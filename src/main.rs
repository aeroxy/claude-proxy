mod certs;
mod claude_oauth;
mod cline;
mod compress;
mod config;
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

    /// Listen port; fails if already in use. Default: 7777
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

        /// Do not open the browser automatically; print URL for manual sign-in and paste code
        #[arg(long)]
        no_browser: bool,
    },
    /// Antigravity account
    Antigravity {
        /// Do not open the browser automatically; print URL for manual sign-in and paste code
        #[arg(long)]
        no_browser: bool,
    },
    /// Google account via gcloud ADC (the `vertex` provider)
    Vertex {
        /// Do not open the browser automatically; print URL for manual sign-in and paste code
        #[arg(long)]
        no_browser: bool,
    },
    /// Cline account (WorkOS device flow — no callback server)
    Cline {
        /// Do not open the browser automatically; print the URL to visit manually
        #[arg(long)]
        no_browser: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result: anyhow::Result<()> = match cli.cmd {
        None => run_foreground(cli.config, cli.port),
        Some(Cmd::Start) => daemon::start(cli.config, cli.port),
        Some(Cmd::Stop) => daemon::stop(cli.port),
        Some(Cmd::Restart) => daemon::restart(cli.config, cli.port),
        Some(Cmd::Login { provider }) => run_login(provider, cli.config),
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

fn run_login(provider: LoginProvider, config_path: Option<PathBuf>) -> anyhow::Result<()> {
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
            LoginProvider::Gemini {
                project,
                no_browser,
            } => login::login_gemini(project, no_browser).await,
            LoginProvider::Antigravity { no_browser } => login::login_antigravity(no_browser).await,
            LoginProvider::Vertex { no_browser } => login::login_vertex(no_browser).await,
            // The only login that talks to a configurable origin: a `[cline]
            // base_url` pointed at staging must register there, not at prod.
            LoginProvider::Cline { no_browser } => {
                let cfg = config::load_config(config_path);
                login::login_cline(no_browser, &cfg.cline).await
            }
        }
    })
}
