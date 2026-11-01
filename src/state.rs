use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::config::{self, Settings};
use crate::error::ApiError;
use crate::health::HealthState;
use crate::metrics::RouteStats;
use crate::router::RouteTable;

/// Shared state behind every handler: the route table, per-route stats and health
/// records, the HTTP client used for proxying, and the process settings.
///
/// Cloning is cheap: every field is either already an `Arc` or becomes one here,
/// so handlers clone the state instead of wrapping it in another `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub settings: Settings,
    pub client: reqwest::Client,
    pub routes: RouteTable,
    pub(crate) stats: Arc<RwLock<HashMap<String, Arc<RouteStats>>>>,
    pub(crate) health: Arc<RwLock<HashMap<String, HealthState>>>,
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
            stats: Arc::new(RwLock::new(HashMap::new())),
            health: Arc::new(RwLock::new(HashMap::new())),
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

    /// Rewrites the routes file from the current in-memory table. Every admin
    /// mutation that changes routing persists before returning, so a crash or
    /// restart never loses a configuration change.
    pub fn persist(&self) -> Result<(), ApiError> {
        config::save_routes(&self.settings.routes_file, &self.routes.list())
    }
}
