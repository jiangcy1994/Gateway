use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::json;

use crate::admin::require_admin;
use crate::error::ApiError;
use crate::llm::app::{App, AppInput, AppPatch};
use crate::llm::ledger::UsageFilter;
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
        .route("/apps", get(list_apps).post(create_app))
        .route(
            "/apps/{id}",
            get(get_app).put(update_app).delete(delete_app),
        )
        .route("/quota", get(get_quota))
        .route("/metrics", get(get_metrics))
        .route("/usage", get(get_usage))
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

async fn list_apps(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(json!({ "apps": state.apps.list() })))
}

async fn get_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let app = state
        .apps
        .get(&id)
        .ok_or_else(|| ApiError::AppNotFound(id.clone()))?;
    Ok(Json(app))
}

async fn create_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<AppInput>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let app: App = input.into();
    app.validate().map_err(ApiError::InvalidApp)?;
    state.apps.add(app.clone())?;
    state.persist_apps()?;
    tracing::info!(
        app = %app.id,
        models = app.models.len(),
        "app added"
    );
    Ok((StatusCode::CREATED, Json(app)))
}

async fn update_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<AppPatch>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let current = state
        .apps
        .get(&id)
        .ok_or_else(|| ApiError::AppNotFound(id.clone()))?;
    let updated = current.apply_patch(patch);
    updated.validate().map_err(ApiError::InvalidApp)?;
    state.apps.replace(&id, updated.clone())?;
    state.persist_apps()?;
    tracing::info!(app = %id, "app updated");
    Ok(Json(updated))
}

async fn delete_app(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    if state.apps.remove(&id).is_none() {
        return Err(ApiError::AppNotFound(id.clone()));
    }
    state.persist_apps()?;
    tracing::info!(app = %id, "app removed");
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

/// `GET /__admin/llm/usage`：按日 × 应用的分桶用量查询（ADR-L20）。只读，
/// 不触发写盘；过滤参数 AND 语义，`from`/`to` 闭区间，非法日期 → 400
/// （`ApiError::InvalidUsageQuery`，消息带参数名与原值）。
async fn get_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<UsageQuery>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let filter = UsageFilter {
        app: params.app.filter(|s| !s.is_empty()),
        date: params
            .date
            .as_deref()
            .map(|value| parse_date("date", value))
            .transpose()?,
        from: params
            .from
            .as_deref()
            .map(|value| parse_date("from", value))
            .transpose()?,
        to: params
            .to
            .as_deref()
            .map(|value| parse_date("to", value))
            .transpose()?,
    };
    Ok::<_, ApiError>(Json(json!({ "usage": state.llm_usage.snapshot(&filter) })))
}

/// `YYYY-MM-DD` 解析；失败回 `InvalidUsageQuery`（400）。`%Y` 单独允许 1-4
/// 位年份（`"26-11-06"` 会被解析成 0026 年），因此解析成功后再按同一格式
/// 回写比对——只有规范 4 位年份的入参才算合法。
fn parse_date(name: &str, value: &str) -> Result<NaiveDate, ApiError> {
    let invalid =
        || ApiError::InvalidUsageQuery(format!("{name}='{value}' is not a valid YYYY-MM-DD date"));
    let parsed = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| invalid())?;
    if parsed.format("%Y-%m-%d").to_string() != value {
        return Err(invalid());
    }
    Ok(parsed)
}

/// `/__admin/llm/usage` 的查询字符串参数：全部可选，全部「空串 = 未传」。
/// 结构非法的 query（axum `Query` 反序列化拒绝）由 axum 回 400，与
/// `ApiError` 的 400 同状态码。
#[derive(Debug, Default, Deserialize)]
struct UsageQuery {
    #[serde(default)]
    app: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}
