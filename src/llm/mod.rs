pub mod admin;
pub mod balancer;
pub mod forward;
pub mod provider;
pub mod quota;
pub mod usage;

use axum::Router;

use crate::state::AppState;

/// `/llm` 子网关：一个只有 fallback 的路由——所有 `/llm/{protocol}/...` 请求
/// 都进 `forward::forward`，按协议段选供应商后透传（ADR-L1/L2/L3）。
pub fn llm_router() -> Router<AppState> {
    Router::new().fallback(forward::forward)
}
