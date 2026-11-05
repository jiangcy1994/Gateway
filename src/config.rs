use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::llm::app::{App, AppsFile};
use crate::llm::provider::{Provider, ProvidersFile};
use crate::qqbot::bot::{Bot, BotsFile};
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
    /// Path of the TOML file holding the LLM providers (ADR-L10：独立于 routes 文件）。
    pub providers_file: std::path::PathBuf,
    /// Path of the TOML file holding the QQ bots（ADR-Q1：独立配置面）。
    pub bots_file: std::path::PathBuf,
    /// Path of the TOML file holding the LLM apps（ADR-L18：独立配置面）。
    pub apps_file: std::path::PathBuf,
    /// Interval between background health checks of every enabled route.
    pub health_interval_secs: u64,
    /// Per-request timeout for a single health probe.
    pub health_timeout_secs: u64,
    /// Default upstream request timeout, used when a route has no `timeout_secs`.
    pub default_timeout_secs: u64,
}

/// Directory that relative config-file paths resolve against: `GATEWAY_BASE_DIR`
/// if set, otherwise the current directory. Console behavior is unchanged; the
/// Windows service shell pins this to its own base directory before settings
/// are constructed, because a service's working directory is `C:\Windows\System32`.
pub fn base_dir() -> std::path::PathBuf {
    base_dir_or(&std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}

/// `GATEWAY_BASE_DIR` if set (non-empty), else `default`. Shared by the console
/// default (cwd) and the service default (`%ProgramData%\top-gateway`).
pub fn base_dir_or(default: &std::path::Path) -> std::path::PathBuf {
    std::env::var("GATEWAY_BASE_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| default.to_path_buf())
}

fn resolve_path(env_name: &str, default: &str, base: &std::path::Path) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(std::env::var(env_name).unwrap_or_else(|_| default.into()));
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

/// Applies a `KEY=VALUE` file to the process environment. A missing file is a
/// no-op; unparseable lines are warned about and skipped, never fatal. Used in
/// Windows service mode, where the process does not inherit the installer's
/// environment — the service reads `GATEWAY_*` settings from `gateway.env` in
/// the base directory instead.
pub fn load_env_file(path: &std::path::Path) -> Result<(), std::io::Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            tracing::warn!("{}:{}: no '=' — line ignored", path.display(), idx + 1);
            continue;
        };
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        if key.is_empty() {
            tracing::warn!("{}:{}: empty key — line ignored", path.display(), idx + 1);
            continue;
        }
        std::env::set_var(key, value);
    }
    Ok(())
}

impl Settings {
    pub fn from_env() -> Self {
        let base = base_dir();
        let admin_key = std::env::var("GATEWAY_ADMIN_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        if admin_key.is_none() {
            tracing::warn!("GATEWAY_ADMIN_KEY is not set; /__admin/* endpoints are open");
        }
        Self {
            bind: std::env::var("GATEWAY_BIND").unwrap_or_else(|_| "127.0.0.1:8000".into()),
            admin_key,
            routes_file: resolve_path("GATEWAY_ROUTES_FILE", "routes.toml", &base),
            providers_file: resolve_path("GATEWAY_PROVIDERS_FILE", "providers.toml", &base),
            bots_file: resolve_path("GATEWAY_BOTS_FILE", "bots.toml", &base),
            apps_file: resolve_path("GATEWAY_APPS_FILE", "apps.toml", &base),
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

/// Loads providers from the TOML file. A missing file is not an error: startup
/// proceeds with no providers (与 `load_routes` 同策略）。重复 id 只告警、全部保留。
pub fn load_providers(path: &std::path::Path) -> Result<Vec<Provider>, ApiError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: ProvidersFile = toml::from_str(&text)
                .map_err(|e| ApiError::ConfigLoad(format!("{}: {e}", path.display())))?;
            let mut seen = std::collections::HashSet::new();
            for provider in &file.providers {
                if !seen.insert(provider.id.clone()) {
                    tracing::warn!(
                        "duplicate provider id {} in {}, keeping both",
                        provider.id,
                        path.display()
                    );
                }
            }
            Ok(file.providers)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "providers file {} not found; starting with no providers",
                path.display()
            );
            Ok(Vec::new())
        }
        Err(e) => Err(ApiError::ConfigLoad(format!("{}: {e}", path.display()))),
    }
}

/// Serializes all providers and replaces the providers file atomically (与
/// `save_routes` 同策略；`api_key` 明文落盘，见 ADR-L11）。
pub fn save_providers(path: &std::path::Path, providers: &[Provider]) -> Result<(), ApiError> {
    let text = toml::to_string_pretty(&ProvidersFile {
        providers: providers.to_vec(),
    })
    .map_err(|e| ApiError::Persist(format!("serialize providers: {e}")))?;
    let body = format!("# top-gateway providers - managed file, rewrites drop comments\n{text}");
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

/// Loads bots from the TOML file. A missing file is not an error: startup
/// proceeds with no bots（与 `load_routes` / `load_providers` 同策略）。
/// 重复 id 或 app_id 只告警、全部保留；每行经 `Bot::normalize` 收敛
/// TOML round-trip 的 Option 形态。
pub fn load_bots(path: &std::path::Path) -> Result<Vec<Bot>, ApiError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: BotsFile = toml::from_str(&text)
                .map_err(|e| ApiError::ConfigLoad(format!("{}: {e}", path.display())))?;
            let mut seen_ids = std::collections::HashSet::new();
            let mut seen_app_ids = std::collections::HashSet::new();
            for bot in &file.bots {
                if !seen_ids.insert(bot.id.clone()) {
                    tracing::warn!(
                        "duplicate bot id {} in {}, keeping both",
                        bot.id,
                        path.display()
                    );
                }
                if !seen_app_ids.insert(bot.app_id.clone()) {
                    tracing::warn!(
                        "duplicate bot app_id {} in {}, keeping both",
                        bot.app_id,
                        path.display()
                    );
                }
            }
            Ok(file.bots.into_iter().map(Bot::normalize).collect())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "bots file {} not found; starting with no bots",
                path.display()
            );
            Ok(Vec::new())
        }
        Err(e) => Err(ApiError::ConfigLoad(format!("{}: {e}", path.display()))),
    }
}

/// Serializes all bots and replaces the bots file atomically（与
/// `save_routes` / `save_providers` 同策略；`app_secret` 明文落盘，
/// 见 ADR-Q11）。
pub fn save_bots(path: &std::path::Path, bots: &[Bot]) -> Result<(), ApiError> {
    let text = toml::to_string_pretty(&BotsFile {
        bots: bots.to_vec(),
    })
    .map_err(|e| ApiError::Persist(format!("serialize bots: {e}")))?;
    let body = format!("# top-gateway bots - managed file, rewrites drop comments\n{text}");
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

/// Loads apps from the TOML file. A missing file is not an error: startup
/// proceeds with no apps（与 `load_routes`/`load_providers`/`load_bots` 同策略，
/// ADR-L18）。重复 id 或 key 只告警、全部保留。
pub fn load_apps(path: &std::path::Path) -> Result<Vec<App>, ApiError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: AppsFile = toml::from_str(&text)
                .map_err(|e| ApiError::ConfigLoad(format!("{}: {e}", path.display())))?;
            let mut seen_ids = std::collections::HashSet::new();
            let mut seen_keys = std::collections::HashSet::new();
            for app in &file.apps {
                if !seen_ids.insert(app.id.clone()) {
                    tracing::warn!(
                        "duplicate app id {} in {}, keeping both",
                        app.id,
                        path.display()
                    );
                }
                if !seen_keys.insert(app.key.clone()) {
                    tracing::warn!("duplicate app key in {}, keeping both", path.display());
                }
            }
            Ok(file.apps)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                "apps file {} not found; starting with no apps",
                path.display()
            );
            Ok(Vec::new())
        }
        Err(e) => Err(ApiError::ConfigLoad(format!("{}: {e}", path.display()))),
    }
}

/// Serializes all apps and replaces the apps file atomically（与
/// `save_bots` 同策略；`key` 明文落盘——它是凭证，与 providers 的
/// `api_key` 同级处理）。
pub fn save_apps(path: &std::path::Path, apps: &[App]) -> Result<(), ApiError> {
    let text = toml::to_string_pretty(&AppsFile {
        apps: apps.to_vec(),
    })
    .map_err(|e| ApiError::Persist(format!("serialize apps: {e}")))?;
    let body = format!("# top-gateway apps - managed file, rewrites drop comments\n{text}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_applies_values_and_skips_noise() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.env");
        std::fs::write(
            &path,
            "# comment\n\n   \nGATEWAY_UNIT_TEST_KEY=abc\nGATEWAY_UNIT_TEST_QUOTED=\"with spaces\"\nNO_EQUALS_HERE\n=GATEWAY_UNIT_TEST_BAD\nGATEWAY_UNIT_TEST_EMPTY=\n",
        )
        .unwrap();
        load_env_file(&path).unwrap();
        assert_eq!(std::env::var("GATEWAY_UNIT_TEST_KEY").unwrap(), "abc");
        assert_eq!(
            std::env::var("GATEWAY_UNIT_TEST_QUOTED").unwrap(),
            "with spaces"
        );
        assert!(std::env::var("GATEWAY_UNIT_TEST_BAD").is_err());
        assert_eq!(std::env::var("GATEWAY_UNIT_TEST_EMPTY").unwrap(), "");
    }

    #[test]
    fn env_file_missing_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        load_env_file(&dir.path().join("does-not-exist.env")).unwrap();
    }

    #[test]
    fn relative_paths_resolve_against_base() {
        let base = std::path::Path::new("/gw/conf");
        assert_eq!(
            resolve_path("GATEWAY_ROUTES_FILE", "routes.toml", base),
            base.join("routes.toml")
        );
    }

    #[test]
    fn absolute_paths_stay_absolute() {
        let base = std::path::Path::new("/gw/conf");
        std::env::set_var("GATEWAY_UNIT_TEST_ABS", "/etc/routes.toml");
        assert_eq!(
            resolve_path("GATEWAY_UNIT_TEST_ABS", "routes.toml", base),
            std::path::PathBuf::from("/etc/routes.toml")
        );
        std::env::remove_var("GATEWAY_UNIT_TEST_ABS");
    }
}
