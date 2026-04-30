mod config;
mod certs;
mod daemon;
mod interceptors;
mod proxy;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::fmt::format::FmtSpan;

#[derive(Parser)]
#[command(name = "claude-proxy", version, about)]
struct Cli {
    /// Path to config.toml (default: ~/.config/claude-proxy/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Starting port (auto-shifts +9 if taken). Default: 6666
    #[arg(long, global = true)]
    port: Option<u16>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the proxy as a background daemon
    Start,
    /// Stop running daemon(s); use --port to target a specific instance
    Stop,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result: anyhow::Result<()> = match cli.cmd {
        None => run_foreground(cli.config, cli.port),
        Some(Cmd::Start) => daemon::start(cli.config, cli.port),
        Some(Cmd::Stop) => daemon::stop(cli.port),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("claude-proxy: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_foreground(config_path: Option<PathBuf>, port: Option<u16>) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::NONE)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,claude_proxy=debug")),
        )
        .init();

    info!("Starting Claude Local Proxy...");

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let cfg = config::load_config(config_path);
        let ca = match certs::get_or_create_ca() {
            Ok(ca) => ca,
            Err(e) => {
                error!("Failed to initialize CA: {}", e);
                return Err(e);
            }
        };
        proxy::run_proxy(ca, cfg, port.unwrap_or(proxy::DEFAULT_PORT)).await
    })
}
