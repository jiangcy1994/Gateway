use std::time::Duration;

use chrono::Utc;
use serde::Serialize;

use crate::error::ApiError;
use crate::router::Route;
use crate::state::AppState;

/// Health of one mounted route, as reported by `GET /__admin/health`.
#[derive(Debug, Clone, Serialize)]
pub struct HealthState {
    pub id: String,
    pub prefix: String,
    pub upstream: String,
    /// `healthy`, `unhealthy`, or `disabled`.
    pub status: String,
    pub last_checked: Option<String>,
    pub latency_ms: Option<u64>,
    pub last_error: Option<String>,
}

/// Spawns the background health checker. The first sweep runs immediately so the
/// admin API reflects upstream reachability right after boot instead of after a
/// full interval.
pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let interval = state.settings.health_interval_secs;
        loop {
            check_all(&state).await;
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    })
}

async fn check_all(state: &AppState) {
    for route in state.routes.list() {
        let health = if route.enabled {
            check_one(state, &route).await
        } else {
            HealthState {
                id: route.id.clone(),
                prefix: route.prefix.clone(),
                upstream: route.upstream.clone(),
                status: "disabled".into(),
                last_checked: None,
                latency_ms: None,
                last_error: None,
            }
        };
        if state
            .health
            .write()
            .unwrap()
            .insert(route.id.clone(), health)
            .is_none()
        {
            tracing::debug!("first health record for route {}", route.id);
        }
    }
}

async fn check_one(state: &AppState, route: &Route) -> HealthState {
    let url = format!(
        "{}{}",
        route.upstream.trim_end_matches('/'),
        route.health_path
    );
    let timeout = Duration::from_secs(state.settings.health_timeout_secs);
    let started = std::time::Instant::now();

    let attempt = tokio::time::timeout(timeout, state.client.get(&url).send()).await;
    let (status, latency_ms, last_error) = match attempt {
        Ok(Ok(resp)) if resp.status().is_success() => (
            "healthy".to_string(),
            Some(started.elapsed().as_millis() as u64),
            None,
        ),
        Ok(Ok(resp)) => (
            "unhealthy".to_string(),
            None,
            Some(format!("health check returned {}", resp.status())),
        ),
        Ok(Err(e)) => ("unhealthy".to_string(), None, Some(format!("{e}"))),
        Err(_) => (
            "unhealthy".to_string(),
            None,
            Some(format!(
                "health check timed out after {}s",
                timeout.as_secs()
            )),
        ),
    };

    HealthState {
        id: route.id.clone(),
        prefix: route.prefix.clone(),
        upstream: route.upstream.clone(),
        status,
        last_checked: Some(Utc::now().to_rfc3339()),
        latency_ms,
        last_error,
    }
}

impl AppState {
    /// Health for every currently mounted route, in route-table order. Entries for
    /// routes removed since the last sweep are dropped, so the view never goes stale.
    pub fn health_snapshot(&self) -> Vec<HealthState> {
        let health = self.health.read().unwrap();
        self.routes
            .list()
            .into_iter()
            .filter_map(|route| health.get(&route.id).cloned())
            .collect()
    }

    /// Probes every mounted route once, regardless of the background interval.
    /// Returns the first failure as an error so callers can report it.
    pub async fn probe_all_now(&self) -> Result<(), ApiError> {
        for route in self.routes.list() {
            if !route.enabled {
                continue;
            }
            let health = check_one(self, &route).await;
            self.health
                .write()
                .unwrap()
                .insert(route.id.clone(), health.clone());
            if health.status != "healthy" {
                return Err(ApiError::Internal(format!(
                    "route {} unhealthy: {}",
                    route.id,
                    health.last_error.unwrap_or_else(|| "unknown".into())
                )));
            }
        }
        Ok(())
    }
}
