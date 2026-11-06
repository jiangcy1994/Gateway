use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::config::{self, Settings};
use crate::error::ApiError;
use crate::health::HealthState;
use crate::llm::app::AppTable;
use crate::llm::client::ClientCache;
use crate::llm::ledger::UsageLedger;
use crate::llm::provider::ProviderTable;
use crate::llm::quota::QuotaTracker;
use crate::metrics::RouteStats;
use crate::qqbot::bot::BotTable;
use crate::qqbot::token::TokenStore;
use crate::qqbot::ws::WsTable;
use crate::router::RouteTable;

/// Shared state behind every handler: the route table, provider table, per-route
/// stats and health records, the HTTP client used for proxying, and the process
/// settings.
///
/// Cloning is cheap: every field is either already an `Arc` or becomes one here,
/// so handlers clone the state instead of wrapping it in another `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub settings: Settings,
    pub client: reqwest::Client,
    pub routes: RouteTable,
    pub providers: ProviderTable,
    pub apps: AppTable,
    /// 按日 × 应用的 LLM 用量账本（ADR-L20），周期 flush 到 `settings.usage_file`。
    pub llm_usage: UsageLedger,
    pub bots: BotTable,
    pub bot_tokens: TokenStore,
    pub bot_ws: WsTable,
    /// 按 `proxy_url` 复用的代理 client（ADR-L19）；`None` 的供应商用 `client`。
    pub llm_clients: ClientCache,
    pub(crate) stats: Arc<RwLock<HashMap<String, Arc<RouteStats>>>>,
    pub(crate) health: Arc<RwLock<HashMap<String, HealthState>>>,
    pub(crate) llm_stats: Arc<RwLock<HashMap<String, Arc<RouteStats>>>>,
    pub(crate) llm_quotas: Arc<RwLock<HashMap<String, Arc<QuotaTracker>>>>,
    pub(crate) bot_stats: Arc<RwLock<HashMap<String, Arc<RouteStats>>>>,
}

impl AppState {
    pub fn new(settings: Settings) -> Result<Self, ApiError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| ApiError::Internal(format!("build HTTP client: {e}")))?;
        Ok(Self {
            settings,
            client,
            routes: RouteTable::default(),
            providers: ProviderTable::default(),
            apps: AppTable::default(),
            llm_usage: UsageLedger::new(),
            bots: BotTable::default(),
            bot_tokens: TokenStore::new(),
            bot_ws: WsTable::new(),
            llm_clients: ClientCache::new(),
            stats: Arc::new(RwLock::new(HashMap::new())),
            health: Arc::new(RwLock::new(HashMap::new())),
            llm_stats: Arc::new(RwLock::new(HashMap::new())),
            llm_quotas: Arc::new(RwLock::new(HashMap::new())),
            bot_stats: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Stats for a route, creating the counters on first use so a route mounted
    /// before it sees traffic reports zeros instead of being missing from metrics.
    pub fn stats_for(&self, id: &str) -> Arc<RouteStats> {
        if let Some(stats) = self.stats.read().unwrap().get(id) {
            return stats.clone();
        }
        let mut stats = self.stats.write().unwrap();
        stats
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(RouteStats::default()))
            .clone()
    }

    /// Stats for an LLM provider. Kept in a map separate from routes so route ids
    /// and provider ids cannot collide（ADR-L9）。
    pub fn llm_stats_for(&self, id: &str) -> Arc<RouteStats> {
        if let Some(stats) = self.llm_stats.read().unwrap().get(id) {
            return stats.clone();
        }
        let mut stats = self.llm_stats.write().unwrap();
        stats
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(RouteStats::default()))
            .clone()
    }

    /// Stats for a QQ bot. Kept in a map separate from routes and providers so
    /// route ids, provider ids and bot ids cannot collide（ADR-L9 同构）。
    pub fn bot_stats_for(&self, id: &str) -> Arc<RouteStats> {
        if let Some(stats) = self.bot_stats.read().unwrap().get(id) {
            return stats.clone();
        }
        let mut stats = self.bot_stats.write().unwrap();
        stats
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(RouteStats::default()))
            .clone()
    }

    /// Quota tracker for a provider, creating it on first use. Trackers survive
    /// provider deletion so history is not lost; the quota view only joins the
    /// current table.
    pub fn quota_for(&self, id: &str) -> Arc<QuotaTracker> {
        if let Some(tracker) = self.llm_quotas.read().unwrap().get(id) {
            return tracker.clone();
        }
        let mut trackers = self.llm_quotas.write().unwrap();
        trackers
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(QuotaTracker::new()))
            .clone()
    }

    /// Rewrites the routes file from the current in-memory table. Every admin
    /// mutation that changes routing persists before returning, so a crash or
    /// restart never loses a configuration change.
    pub fn persist(&self) -> Result<(), ApiError> {
        config::save_routes(&self.settings.routes_file, &self.routes.list())
    }

    /// Same contract as `persist`, for the providers table（ADR-L10：独立文件）。
    pub fn persist_providers(&self) -> Result<(), ApiError> {
        config::save_providers(&self.settings.providers_file, &self.providers.list())
    }

    /// Same contract as `persist`, for the bots table（ADR-Q1：独立配置面）。
    pub fn persist_bots(&self) -> Result<(), ApiError> {
        config::save_bots(&self.settings.bots_file, &self.bots.list())
    }

    /// Same contract as `persist`, for the apps table（ADR-L18：独立配置面）。
    pub fn persist_apps(&self) -> Result<(), ApiError> {
        config::save_apps(&self.settings.apps_file, &self.apps.list())
    }
}
