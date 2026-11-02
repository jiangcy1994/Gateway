//! QQ Bot 子网关（ADR-Q1：嵌入 top-gateway 进程，与 `/llm` 同构，不独立监听）。
//!
//! 模块划分：
//! - `bot`：配置面——`Bot` / `BotInput` / `BotPatch` / `BotTable` 及 TOML 持久化形态
//! - `admin`：`/__admin/qqbot/*` 管理端点（bot 增删改查、WS 连接快照、指标）
//! - `token`：access_token 缓存与自动刷新（`TokenStore`）
//! - `forward`：`/qqbot/{app_id}/...` OpenAPI 透传
//! - `webhook`：`/qqbot/callback/{app_id}` 回调接收 + 事件分发
//! - `ws`：WebSocket 网关客户端（每 bot 一个监督任务）
//!
//! 实现权威：`docs/sdd/qqbot/interfaces.md`。

pub mod admin;
pub mod bot;
pub mod forward;
pub mod token;
pub mod webhook;

use axum::routing::post;
use axum::Router;

use crate::state::AppState;

/// `/qqbot` 子网关：`/qqbot/callback/{app_id}` 的 webhook 处理器是唯一的
/// 具名路由，其余 `/qqbot/{app_id}/...` 请求都进 `forward::forward`，按
/// app_id 段选 bot 后透传（ADR-Q2）。非 POST 的 callback 路径不匹配具名
/// 路由，落进 fallback 由其 404 兜底。
pub fn qqbot_router() -> Router<AppState> {
    Router::new()
        .route("/callback/{app_id}", post(webhook::webhook))
        .fallback(forward::forward)
}
