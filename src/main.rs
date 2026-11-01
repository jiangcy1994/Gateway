use top_gateway::config::Settings;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "top_gateway=info,warn".into()),
        )
        .init();

    let settings = Settings::from_env();
    top_gateway::run(settings).await
}
