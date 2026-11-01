use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::router::Route;

/// On-disk layout of the routes file: one `[[routes]]` table per mounted sub-gateway.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RoutesFile {
    #[serde(default)]
    pub routes: Vec<Route>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    /// Address the top-level gateway listens on.
    pub bind: String,
    /// Key required for `/__admin/*`. `None` means admin endpoints are open (a warning is logged).
    pub admin_key: Option<String>,
    /// Path of the TOML file holding the mounted routes.
    pub routes_file: std::path::PathBuf,
    /// Interval between background health checks of every enabled route.
    pub health_interval_secs: u64,
    /// Per-request timeout for a single health probe.
    pub health_timeout_secs: u64,
    /// Default upstream request timeout, used when a route has no `timeout_secs`.
    pub default_timeout_secs: u64,
}

impl Settings {
    pub fn from_env() -> Self {
        let admin_key = std::env::var("GATEWAY_ADMIN_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        if admin_key.is_none() {
            tracing::warn!("GATEWAY_ADMIN_KEY is not set; /__admin/* endpoints are open");
        }
        Self {
            bind: std::env::var("GATEWAY_BIND").unwrap_or_else(|_| "127.0.0.1:8000".into()),
            admin_key,
            routes_file: std::path::PathBuf::from(
                std::env::var("GATEWAY_ROUTES_FILE").unwrap_or_else(|_| "routes.toml".into()),
            ),
            health_interval_secs: env_u64("GATEWAY_HEALTH_INTERVAL_SECS", 10),
            health_timeout_secs: env_u64("GATEWAY_HEALTH_TIMEOUT_SECS", 2),
            default_timeout_secs: env_u64("GATEWAY_DEFAULT_TIMEOUT_SECS", 30),
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &u64| *v > 0)
        .unwrap_or(default)
}

/// Loads routes from the TOML file. A missing file is not an error: startup proceeds
/// with no routes and the file is created on the first admin mutation.
pub fn load_routes(path: &std::path::Path) -> Result<Vec<Route>, ApiError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: RoutesFile = toml::from_str(&text)
                .map_err(|e| ApiError::ConfigLoad(format!("{}: {e}", path.display())))?;
            let mut seen = std::collections::HashSet::new();
            for route in &file.routes {
                if !seen.insert(route.prefix.clone()) {
                    tracing::warn!(
                        "duplicate prefix {} in {}, keeping both",
                        route.prefix,
                        path.display()
                    );
                }
            }
            Ok(file.routes)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "routes file {} not found; starting with no routes",
                path.display()
            );
            Ok(Vec::new())
        }
        Err(e) => Err(ApiError::ConfigLoad(format!("{}: {e}", path.display()))),
    }
}

/// Serializes all routes and replaces the routes file atomically (temp file in the
/// same directory, then rename). Comments in the file do not survive a rewrite.
pub fn save_routes(path: &std::path::Path, routes: &[Route]) -> Result<(), ApiError> {
    let text = toml::to_string_pretty(&RoutesFile {
        routes: routes.to_vec(),
    })
    .map_err(|e| ApiError::Persist(format!("serialize routes: {e}")))?;
    let body = format!("# top-gateway routes - managed file, rewrites drop comments\n{text}");
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| ApiError::Persist(format!("create temp file in {}: {e}", dir.display())))?;
    use std::io::Write;
    tmp.write_all(body.as_bytes())
        .map_err(|e| ApiError::Persist(format!("write temp file: {e}")))?;
    tmp.persist(path)
        .map_err(|e| ApiError::Persist(format!("replace {}: {e}", path.display())))?;
    Ok(())
}
