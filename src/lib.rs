pub mod admin;
pub mod config;
pub mod error;
pub mod health;
pub mod metrics;
pub mod proxy;
pub mod router;
pub mod state;

use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::config::Settings;
use crate::state::AppState;

/// Builds the full app: `/health` and `/__admin/*` are served by this gateway,
/// everything else falls through to the proxy fallback.
pub fn build_app(state: AppState) -> Router {
    Router::<AppState>::new()
        .route("/health", get(root_health))
        .nest("/__admin", admin::admin_router())
        .fallback(proxy::proxy)
        .with_state(state)
}

async fn root_health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Loads the routes file, starts the health checker, and serves until shutdown.
pub async fn run(settings: Settings) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bind = settings.bind.clone();
    let routes_path = settings.routes_file.clone();

    let state = AppState::new(settings)?;
    let routes = config::load_routes(&routes_path)
        .map_err(|e| format!("failed to load routes from {}: {e}", routes_path.display()))?;
    let count = routes.len();
    state.routes.set_all(routes);
    tracing::info!("loaded {count} route(s) from {}", routes_path.display());

    health::spawn(state.clone());

    let app = build_app(state);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    let addr = listener.local_addr()?;
    tracing::info!("top-gateway listening on http://{addr}");
    tracing::info!("admin API at http://{addr}/__admin, docs: GET /__admin/routes");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
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
