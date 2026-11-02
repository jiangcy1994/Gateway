use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::admin::require_admin;
use crate::error::ApiError;
use crate::qqbot::bot::{Bot, BotInput, BotPatch};
use crate::state::AppState;

/// `/qqbot` 的管理端点，挂载于既有 `admin_router()` 内的 `nest("/qqbot", ...)`。
/// 鉴权与路由管理端点完全相同（同 `X-Admin-Key` / `Bearer`，同 `GATEWAY_ADMIN_KEY`）。
pub fn admin_router() -> Router<AppState> {
    Router::new()
        .route("/bots", get(list_bots).post(create_bot))
        .route(
            "/bots/{id}",
            get(get_bot).put(update_bot).delete(delete_bot),
        )
        .route("/ws", get(get_ws))
        .route("/metrics", get(get_metrics))
}

async fn list_bots(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    Ok(Json(json!({ "bots": state.bots.list() })))
}

async fn get_bot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let bot = state
        .bots
        .get(&id)
        .ok_or_else(|| ApiError::BotNotFound(id.clone()))?;
    Ok(Json(bot))
}

async fn create_bot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<BotInput>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let bot: Bot = input.into();
    bot.validate().map_err(ApiError::InvalidBot)?;
    state.bots.add(bot.clone())?;
    state.persist_bots()?;
    // WS 生命周期联动：新形态需要 WS → 起 supervisor（ADR-Q9）。
    state.bot_ws.ensure(&bot, &state);
    tracing::info!(
        bot = %bot.id,
        app_id = %bot.app_id,
        sandbox = bot.sandbox,
        "bot added"
    );
    Ok((StatusCode::CREATED, Json(bot)))
}

async fn update_bot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(patch): Json<BotPatch>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let current = state
        .bots
        .get(&id)
        .ok_or_else(|| ApiError::BotNotFound(id.clone()))?;
    let updated = current.apply_patch(patch);
    updated.validate().map_err(ApiError::InvalidBot)?;
    state.bots.replace(&id, updated.clone())?;
    state.persist_bots()?;
    // 旧 supervisor 必须重连（配置已变）：停掉，需要时重建。
    state.bot_ws.stop(&id);
    if updated.ws_needs_connection() {
        state.bot_ws.ensure(&updated, &state);
    }
    tracing::info!(bot = %id, "bot updated");
    Ok(Json(updated))
}

async fn delete_bot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    if state.bots.remove(&id).is_none() {
        return Err(ApiError::BotNotFound(id.clone()));
    }
    state.persist_bots()?;
    state.bot_ws.stop(&id);
    tracing::info!(bot = %id, "bot removed");
    Ok(Json(json!({ "deleted": id })))
}

/// WS supervisor 快照，按 bot 表序 join（表内无该项的 bot 不出列）。
async fn get_ws(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let statuses = state.bot_ws.snapshot();
    let bots = state.bots.list();
    let mut views = Vec::with_capacity(statuses.len());
    for bot in &bots {
        if let Some(status) = statuses.iter().find(|s| s.bot_id == bot.id) {
            views.push(status);
        }
    }
    Ok::<_, ApiError>(Json(json!({ "bots": views })))
}

/// 按 bot 聚合的透传统计。bot id、route id、provider id 各占一张表，
/// 互不串号（ADR-L9 同构）。
async fn get_metrics(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    require_admin(&state, &headers)?;
    let bots = state.bots.list();
    let mut views = Vec::with_capacity(bots.len());
    for bot in &bots {
        let stats = state.bot_stats_for(&bot.id).snapshot();
        views.push(json!({
            "bot": bot,
            "stats": stats,
        }));
    }
    Ok::<_, ApiError>(Json(json!({ "bots": views })))
}
