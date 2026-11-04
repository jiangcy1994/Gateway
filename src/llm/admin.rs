use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::admin::require_admin;
use crate::error::ApiError;
use crate::llm::provider::{Provider, ProviderInput, ProviderPatch};
use crate::state::AppState;

/// `/llm` 的管理端点，挂载于既有 `admin_router()` 内的 `nest("/llm", ...)`。
/// 鉴权与路由管理端点完全相同（同 `X-Admin-Key` / `Bearer`，同 `GATEWAY_ADMIN_KEY`）。
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/providers", get(list_providers).post(create_provider))
        .route(
            "/providers/{id}",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/quota", get(get_quota))
        .route("/metrics", get(get_metrics))
}

async fn list_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(json!({ "providers": state.providers.list() })))
}

async fn get_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let provider = state
        .providers
        .get(&id)
        .ok_or_else(|| ApiError::ProviderNotFound(id.clone()))?;
    Ok(Json(provider))
}

async fn create_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ProviderInput>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let provider: Provider = input.into();
    provider.validate().map_err(ApiError::InvalidProvider)?;
    state.providers.add(provider.clone())?;
    state.persist_providers()?;
    tracing::info!(
        provider = %provider.id,
        protocols = ?provider.protocols,
        base_url = %provider.base_url,
        "provider added"
    );
    Ok((StatusCode::CREATED, Json(provider)))
}

async fn update_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<ProviderPatch>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let current = state
        .providers
        .get(&id)
        .ok_or_else(|| ApiError::ProviderNotFound(id.clone()))?;
    let updated = current.apply_patch(patch);
    updated.validate().map_err(ApiError::InvalidProvider)?;
    state.providers.replace(&id, updated.clone())?;
    state.persist_providers()?;
    tracing::info!(provider = %id, "provider updated");
    Ok(Json(updated))
}

async fn delete_provider(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    if state.providers.remove(&id).is_none() {
        return Err(ApiError::ProviderNotFound(id.clone()));
    }
    state.persist_providers()?;
    tracing::info!(provider = %id, "provider removed");
    Ok(Json(json!({ "deleted": id })))
}

async fn get_quota(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let providers = state.providers.list();
    let mut views = Vec::with_capacity(providers.len());
    for provider in &providers {
        let tracker = state.quota_for(&provider.id);
        views.push(tracker.snapshot(
            &provider.id,
            &provider.protocols,
            provider.enabled,
            provider.rpm_limit,
            provider.tpm_limit,
        ));
    }
    Ok::<_, ApiError>(Json(json!({ "providers": views })))
}

async fn get_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let providers = state.providers.list();
    let mut views = Vec::with_capacity(providers.len());
    for provider in &providers {
        let stats = state.llm_stats_for(&provider.id).snapshot();
        views.push(json!({
            "provider": provider,
            "stats": stats,
        }));
    }
    Ok::<_, ApiError>(Json(json!({ "providers": views })))
}
