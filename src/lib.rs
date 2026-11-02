pub mod admin;
pub mod config;
pub mod error;
pub mod health;
pub mod llm;
pub mod logging;
pub mod metrics;
pub mod proxy;
pub mod qqbot;
pub mod router;
pub mod state;

/// Windows service shell (`src/service.rs`): SCM entry, status reporting, and
/// the `install`/`start`/`stop`/`uninstall`/`status` management commands. Absent
/// on other targets, where the binary is a plain console app.
#[cfg(windows)]
pub mod service;

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::config::Settings;
use crate::state::AppState;

/// Builds the full app: `/health` and `/__admin/*` are served by this gateway,
/// `/llm/*` by the embedded LLM sub-gateway, `/qqbot/*` by the embedded QQ Bot
/// sub-gateway, everything else falls through to the proxy fallback. `/llm` and
/// `/qqbot` are reserved prefixes: routes mounted under either in `routes.toml`
/// never win over the nest (ADR-L5 / ADR-Q5).
pub fn build_app(state: AppState) -> Router {
    Router::<AppState>::new()
        .route("/health", get(root_health))
        .nest("/__admin", admin::admin_router())
        .nest("/llm", llm::llm_router())
        .nest("/qqbot", qqbot::qqbot_router())
        .fallback(proxy::proxy)
        .with_state(state)
}

async fn root_health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Loads the routes and providers files, starts the health checker, and serves
/// until Ctrl+C (or SIGTERM on Unix). See `run_with_shutdown` — the Windows
/// service shell reuses the latter to drive shutdown from the service control
/// manager.
pub async fn run(settings: Settings) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_with_shutdown(settings, Box::pin(shutdown_signal())).await
}

/// Same as `run`, but the caller supplies the shutdown trigger. The future must
/// resolve exactly once; on resolution `axum::serve` stops accepting and waits
/// for in-flight connections to finish (graceful shutdown).
pub async fn run_with_shutdown(
    settings: Settings,
    shutdown: futures_util::future::BoxFuture<'static, ()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bind = settings.bind.clone();
    let routes_path = settings.routes_file.clone();
    let providers_path = settings.providers_file.clone();
    let bots_path = settings.bots_file.clone();

    let state = AppState::new(settings)?;
    let routes = config::load_routes(&routes_path)
        .map_err(|e| format!("failed to load routes from {}: {e}", routes_path.display()))?;
    let count = routes.len();
    state.routes.set_all(routes);
    tracing::info!("loaded {count} route(s) from {}", routes_path.display());

    let providers = config::load_providers(&providers_path).map_err(|e| {
        format!(
            "failed to load providers from {}: {e}",
            providers_path.display()
        )
    })?;
    let count = providers.len();
    state.providers.set_all(providers);
    tracing::info!(
        "loaded {count} provider(s) from {}",
        providers_path.display()
    );

    let bots = config::load_bots(&bots_path)
        .map_err(|e| format!("failed to load bots from {}: {e}", bots_path.display()))?;
    let count = bots.len();
    state.bots.set_all(bots);
    tracing::info!("loaded {count} bot(s) from {}", bots_path.display());

    health::spawn(state.clone());

    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let addr = listener.local_addr()?;
    tracing::info!("top-gateway listening on http://{addr}");
    tracing::info!("admin API at http://{addr}/__admin, docs: GET /__admin/routes");
    tracing::info!("LLM sub-gateway at http://{addr}/llm/{{openai,anthropic,systemone}}/...");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    tracing::info!("shutdown complete");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("shutdown signal received");
}
