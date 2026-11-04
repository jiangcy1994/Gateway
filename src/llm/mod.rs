pub mod admin;
pub mod balancer;
pub mod forward;
pub mod provider;
pub mod quota;
pub mod usage;

use axum::Router;

use crate::state::AppState;

/// `/llm` 子网关：一个只有 fallback 的路由——所有 `/llm/...` 请求都进
/// `forward::forward`，按**两种入站形态**解析协议（显式协议段
/// `/llm/{protocol}/...`，或 `/llm/v1/{端点}` 经 `infer_protocol` 闭表推断，
/// ADR-L12），再按协议选供应商后透传（ADR-L1/L2/L3/L13）。
pub fn llm_router() -> Router<AppState> {
    Router::new().fallback(forward::forward)
}
