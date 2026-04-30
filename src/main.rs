mod config;
mod certs;
mod interceptors;
mod proxy;

use tracing_subscriber::fmt::format::FmtSpan;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::NONE)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,claude_proxy=debug")),
        )
        .init();

    info!("Starting Claude Local Proxy...");

    let config = config::load_config();
    let ca_cert = certs::get_or_create_ca()?;

    proxy::run_proxy(ca_cert, config).await?;

    Ok(())
}
