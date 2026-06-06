mod config;
mod certs;
mod daemon;
mod gemini;
mod interceptors;
mod login;
mod oauth_util;
mod openai;
mod proxy;
mod reauth;

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

    /// Listen port; fails if already in use. Default: 6666
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
    /// Stop then start the proxy (use --port to target a specific instance)
    Restart,
    /// Sign in to a Gemini provider and save account credentials
    Login {
        #[command(subcommand)]
        provider: LoginProvider,
    },
}

#[derive(Subcommand)]
enum LoginProvider {
    /// Google account via Code Assist (the `gemini-cli` provider)
    Gemini {
        /// Use a specific Google Cloud project instead of auto-discovery
        #[arg(long)]
        project: Option<String>,
    },
    /// Antigravity account
    Antigravity,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result: anyhow::Result<()> = match cli.cmd {
        None => run_foreground(cli.config, cli.port),
        Some(Cmd::Start) => daemon::start(cli.config, cli.port),
        Some(Cmd::Stop) => daemon::stop(cli.port),
        Some(Cmd::Restart) => daemon::restart(cli.config, cli.port),
        Some(Cmd::Login { provider }) => run_login(provider),
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
        let resolved_port = config::resolve_port(port, &cfg);
        let ca = match certs::get_or_create_ca(&cfg) {
            Ok(ca) => ca,
            Err(e) => {
                error!("Failed to initialize CA: {}", e);
                return Err(e);
            }
        };
        proxy::run_proxy(ca, cfg, resolved_port).await
    })
}

fn run_login(provider: LoginProvider) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::NONE)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,claude_proxy=info")),
        )
        .init();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        match provider {
            LoginProvider::Gemini { project } => login::login_gemini(project).await,
            LoginProvider::Antigravity => login::login_antigravity().await,
        }
    })
}
