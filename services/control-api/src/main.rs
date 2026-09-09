use control_api::{serve, AppConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match std::env::args().nth(1).as_deref() {
        Some("migrate") => control_api::migrate_database_from_env().await,
        Some(command) => anyhow::bail!("unknown control-api command `{command}`"),
        None => {
            let config = AppConfig::from_env()?;
            serve(config).await
        }
    }
}
