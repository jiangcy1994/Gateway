use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::ApiError;

/// 按 `proxy_url` 复用的 reqwest client 缓存（ADR-L19）。
///
/// `proxy_url = None` 的供应商永远用共享直连 client（`AppState.client`），不进
/// 缓存表；每个代理 url 对应一个自带连接池的 client，避免每请求新建连接。
/// 缓存只增不删：供应商改 / 删代理 url 后旧 client 自然淘汰（无持有者时由
/// reqwest 连接池回收），代理 url 变更是低频管理操作，不做显式失效。
///
/// 并发模型与 `AppState::stats_for` 同构：读路径（命中）只取读锁；未命中再取
/// 写锁构建并写入。锁中毒 panic（与 `RouteTable` 同策略）。
#[derive(Debug, Default, Clone)]
pub struct ClientCache {
    clients: Arc<RwLock<HashMap<String, reqwest::Client>>>,
}

impl ClientCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取转发用的 client：`None` → 共享直连 client；`Some(url)` → 命中缓存返回
    /// 克隆，未命中构建 `Client::builder().proxy(Proxy::all(url))` 并写入缓存。
    /// 构建失败 → `ApiError::Internal`（500）——`Provider::validate` 已把绝大
    /// 多数非法 url 拦在配置时（400），此处是防御性兜底。
    pub fn get(
        &self,
        proxy_url: Option<&str>,
        direct: &reqwest::Client,
    ) -> Result<reqwest::Client, ApiError> {
        let Some(url) = proxy_url else {
            return Ok(direct.clone());
        };
        if let Some(client) = self.clients.read().unwrap().get(url) {
            return Ok(client.clone());
        }
        let mut clients = self.clients.write().unwrap();
        if let Some(client) = clients.get(url) {
            return Ok(client.clone());
        }
        let proxy = reqwest::Proxy::all(url)
            .map_err(|e| ApiError::Internal(format!("invalid proxy url '{url}': {e}")))?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .build()
            .map_err(|e| ApiError::Internal(format!("build proxy client for '{url}': {e}")))?;
        clients.insert(url.to_string(), client.clone());
        Ok(client)
    }
}
