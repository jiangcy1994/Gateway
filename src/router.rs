use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::error::ApiError;

fn default_true() -> bool {
    true
}

fn default_health_path() -> String {
    "/health".to_string()
}

/// One mounted sub-gateway: traffic under `prefix` is forwarded to `upstream`.
///
/// The fields round-trip to the TOML routes file, so anything a user can set in the
/// file can also be set through the admin API.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Route {
    pub id: String,
    pub prefix: String,
    pub upstream: String,
    /// Drop the matched prefix before forwarding. `/qqbot/x` -> `/x`.
    #[serde(default = "default_true")]
    pub strip_prefix: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub description: String,
    /// Headers injected into every forwarded request, overriding client values.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Upstream request timeout in seconds; falls back to GATEWAY_DEFAULT_TIMEOUT_SECS.
    pub timeout_secs: Option<u64>,
    /// Path appended to the upstream for health probes. Default `/health`.
    #[serde(default = "default_health_path")]
    pub health_path: String,
}

impl Route {
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("id must not be empty".into());
        }
        if !self.prefix.starts_with('/') {
            return Err(format!("prefix {} must start with '/'", self.prefix));
        }
        if self.prefix.ends_with('/') && self.prefix != "/" {
            return Err(format!("prefix {} must not end with '/'", self.prefix));
        }
        let scheme = self
            .upstream
            .split("://")
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https") {
            return Err(format!("upstream {} must use http or https", self.upstream));
        }
        if self
            .upstream
            .split("://")
            .nth(1)
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            return Err(format!("upstream {} has no host", self.upstream));
        }
        if !self.health_path.starts_with('/') {
            return Err(format!(
                "health_path {} must start with '/'",
                self.health_path
            ));
        }
        for (name, value) in &self.headers {
            if name.trim().is_empty() {
                return Err("header names must not be empty".into());
            }
            let _ = axum::http::HeaderValue::from_str(value)
                .map_err(|_| format!("header {name} value is not a valid header value"))?;
        }
        Ok(())
    }
}

/// Merge semantics for `PUT /__admin/routes/{id}`: only fields present in the patch
/// are changed, everything else is kept from the current route.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutePatch {
    pub prefix: Option<String>,
    pub upstream: Option<String>,
    pub strip_prefix: Option<bool>,
    pub enabled: Option<bool>,
    pub description: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub timeout_secs: Option<u64>,
    pub health_path: Option<String>,
}

impl Route {
    pub fn apply_patch(&self, patch: RoutePatch) -> Route {
        Route {
            id: self.id.clone(),
            prefix: patch.prefix.unwrap_or_else(|| self.prefix.clone()),
            upstream: patch.upstream.unwrap_or_else(|| self.upstream.clone()),
            strip_prefix: patch.strip_prefix.unwrap_or(self.strip_prefix),
            enabled: patch.enabled.unwrap_or(self.enabled),
            description: patch
                .description
                .unwrap_or_else(|| self.description.clone()),
            headers: patch.headers.unwrap_or_else(|| self.headers.clone()),
            timeout_secs: patch.timeout_secs.or(self.timeout_secs),
            health_path: patch
                .health_path
                .unwrap_or_else(|| self.health_path.clone()),
        }
    }
}

/// The route table is read on every proxied request, so reads take a cheap `RwLock`
/// read lock and cloning a matched route releases the lock before the upstream call.
/// With a handful of mounted gateways a linear scan for the longest matching prefix
/// is faster than any index.
#[derive(Debug, Default, Clone)]
pub struct RouteTable {
    routes: Arc<RwLock<Vec<Route>>>,
}

impl RouteTable {
    pub fn set_all(&self, routes: Vec<Route>) {
        *self.routes.write().unwrap() = routes;
    }

    pub fn list(&self) -> Vec<Route> {
        self.routes.read().unwrap().clone()
    }

    pub fn get(&self, id: &str) -> Option<Route> {
        self.routes
            .read()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned()
    }

    /// Longest matching prefix wins. `/qqbot` matches `/qqbot` and `/qqbot/...`;
    /// a route mounted at `/` matches everything.
    pub fn match_route(&self, path: &str) -> Option<Route> {
        let routes = self.routes.read().unwrap();
        let mut best: Option<&Route> = None;
        for route in routes.iter() {
            let matches = route.prefix == "/"
                || path == route.prefix
                || path.starts_with(&format!("{}/", route.prefix));
            if matches && best.is_none_or(|b| route.prefix.len() > b.prefix.len()) {
                best = Some(route);
            }
        }
        best.cloned()
    }

    pub fn add(&self, route: Route) -> Result<(), ApiError> {
        let mut routes = self.routes.write().unwrap();
        if routes.iter().any(|r| r.id == route.id) {
            return Err(ApiError::DuplicateId(route.id));
        }
        if routes.iter().any(|r| r.prefix == route.prefix) {
            return Err(ApiError::DuplicatePrefix(route.prefix));
        }
        routes.push(route);
        Ok(())
    }

    pub fn replace(&self, id: &str, route: Route) -> Result<(), ApiError> {
        let mut routes = self.routes.write().unwrap();
        let Some(pos) = routes.iter().position(|r| r.id == id) else {
            return Err(ApiError::RouteNotFound(id.to_string()));
        };
        if routes
            .iter()
            .enumerate()
            .any(|(i, r)| i != pos && r.prefix == route.prefix)
        {
            return Err(ApiError::DuplicatePrefix(route.prefix));
        }
        routes[pos] = route;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Option<Route> {
        let mut routes = self.routes.write().unwrap();
        let pos = routes.iter().position(|r| r.id == id)?;
        Some(routes.remove(pos))
    }
}
