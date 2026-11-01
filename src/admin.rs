use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::ApiError;
use crate::router::{Route, RoutePatch};
use crate::state::AppState;

/// Body of `POST /__admin/routes` (and the fields accepted by `PUT`).
#[derive(Debug, Deserialize)]
pub struct RouteInput {
    pub id: Option<String>,
    pub prefix: String,
    pub upstream: String,
    pub strip_prefix: Option<bool>,
    pub enabled: Option<bool>,
    pub description: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub timeout_secs: Option<u64>,
    pub health_path: Option<String>,
}

impl From<RouteInput> for Route {
    fn from(input: RouteInput) -> Self {
        Route {
            id: input.id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            prefix: input.prefix,
            upstream: input.upstream,
            strip_prefix: input.strip_prefix.unwrap_or(true),
            enabled: input.enabled.unwrap_or(true),
            description: input.description.unwrap_or_default(),
            headers: input.headers.unwrap_or_default(),
            timeout_secs: input.timeout_secs,
            health_path: input.health_path.unwrap_or_else(|| "/health".to_string()),
        }
    }
}

/// All admin routes, mounted under `/__admin`. The prefix is deliberately not
/// `/admin` because mounted sub-gateways commonly own that path themselves.
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/routes", get(list_routes).post(create_route))
        .route(
            "/routes/{id}",
            get(get_route).put(update_route).delete(delete_route),
        )
        .route("/health", get(get_health))
        .route("/metrics", get(get_metrics))
        .route("/probe", post(trigger_probe))
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(key) = &state.settings.admin_key else {
        return Ok(());
    };
    let presented = headers
        .get("x-admin-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        });
    match presented {
        Some(p) if constant_time_eq(p.as_bytes(), key.as_bytes()) => Ok(()),
        _ => Err(ApiError::Unauthorized),
    }
}

/// Compares in constant time so an attacker cannot time-verify a key prefix.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn list_routes(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(json!({ "routes": state.routes.list() })))
}

async fn get_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let route = state
        .routes
        .get(&id)
        .ok_or_else(|| ApiError::RouteNotFound(id.clone()))?;
    Ok(Json(route))
}

async fn create_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RouteInput>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let route: Route = input.into();
    route.validate().map_err(ApiError::InvalidRoute)?;
    state.routes.add(route.clone())?;
    state.persist()?;
    tracing::info!(route = %route.id, prefix = %route.prefix, upstream = %route.upstream, "route added");
    Ok((StatusCode::CREATED, Json(route)))
}

async fn update_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<RoutePatch>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let current = state
        .routes
        .get(&id)
        .ok_or_else(|| ApiError::RouteNotFound(id.clone()))?;
    let updated = current.apply_patch(patch);
    updated.validate().map_err(ApiError::InvalidRoute)?;
    state.routes.replace(&id, updated.clone())?;
    state.persist()?;
    tracing::info!(route = %id, "route updated");
    Ok(Json(updated))
}

async fn delete_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    if state.routes.remove(&id).is_none() {
        return Err(ApiError::RouteNotFound(id.clone()));
    }
    state.persist()?;
    tracing::info!(route = %id, "route removed");
    Ok(Json(json!({ "deleted": id })))
}

async fn get_health(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(json!({ "routes": state.health_snapshot() })))
}

async fn get_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let mut routes = Vec::new();
    for route in state.routes.list() {
        let stats = state.stats_for(&route.id).snapshot();
        routes.push(json!({
            "route": route,
            "stats": stats,
        }));
    }
    Ok::<_, ApiError>(Json(json!({ "routes": routes })))
}

async fn trigger_probe(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    state.probe_all_now().await?;
    Ok(Json(json!({ "status": "all routes healthy" })))
}
