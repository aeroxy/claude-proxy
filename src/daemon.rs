use std::fs::{self, File};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use daemonize::Daemonize;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tracing::info;
use tracing_subscriber::fmt::format::FmtSpan;

use crate::{certs, config, proxy};

fn state_dir() -> anyhow::Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("could not resolve $HOME"))?
        .join(".config/claude-proxy"))
}

fn log_dir() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("log"))
}

fn pid_dir() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join("pids"))
}

pub fn start(config_path: Option<PathBuf>, port: Option<u16>) -> anyhow::Result<()> {
    let log_dir = log_dir()?;
    let pid_dir = pid_dir()?;
    fs::create_dir_all(&log_dir)?;
    fs::create_dir_all(&pid_dir)?;

    let (listener, port) = proxy::bind_listener(port.unwrap_or(proxy::DEFAULT_PORT))?;

    let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let log_path = log_dir.join(format!("{epoch}.log"));
    let pid_path = pid_dir.join(format!("{port}.pid"));

    if pid_path.exists() {
        return Err(anyhow!(
            "pidfile {} already exists; another daemon may be running on port {}",
            pid_path.display(),
            port
        ));
    }

    let log_file = File::create(&log_path)
        .with_context(|| format!("creating log file {}", log_path.display()))?;
    let err_file = log_file.try_clone()?;

    println!("claude-proxy: listening on 127.0.0.1:{port}");
    println!("claude-proxy: log -> {}", log_path.display());
    println!("claude-proxy: pid -> {}", pid_path.display());

    let cwd = std::env::current_dir()?;

    Daemonize::new()
        .pid_file(&pid_path)
        .chown_pid_file(false)
        .working_directory(&cwd)
        .stdout(log_file)
        .stderr(err_file)
        .start()
        .context("daemonize failed")?;

    // ---- We are now in the child. ----

    tracing_subscriber::fmt()
        .with_span_events(FmtSpan::NONE)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,claude_proxy=debug")),
        )
        .init();

    info!("Starting Claude Local Proxy (daemon, pid={})", std::process::id());

    let result = (|| -> anyhow::Result<()> {
        let cfg = config::load_config(config_path);
        let ca = certs::get_or_create_ca(&cfg)?;
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(proxy::run_proxy_with_listener(listener, ca, cfg))
    })();

    let _ = fs::remove_file(&pid_path);
    result
}

pub fn stop(port: Option<u16>) -> anyhow::Result<()> {
    let pid_dir = pid_dir()?;
    if !pid_dir.exists() {
        return Err(anyhow!("no pid directory at {}", pid_dir.display()));
    }

    let mut killed = 0usize;
    for entry in fs::read_dir(&pid_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let stem = match name.strip_suffix(".pid") {
            Some(s) => s,
            None => continue,
        };
        let entry_port: u16 = match stem.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Some(want) = port {
            if entry_port != want {
                continue;
            }
        }

        let pid_str = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let pid: i32 = pid_str.trim().parse()
            .with_context(|| format!("parsing pid in {}", path.display()))?;

        match kill(Pid::from_raw(pid), Signal::SIGTERM) {
            Ok(()) => {
                println!("claude-proxy: stopped pid {} (port {})", pid, entry_port);
                killed += 1;
                let _ = fs::remove_file(&path);
            }
            Err(nix::errno::Errno::ESRCH) => {
                println!(
                    "claude-proxy: stale pidfile {} (no such process); removing",
                    path.display()
                );
                let _ = fs::remove_file(&path);
            }
            Err(e) => {
                eprintln!("claude-proxy: failed to signal pid {}: {}", pid, e);
            }
        }
    }

    if killed == 0 {
        return Err(anyhow!(
            "no running daemon{} found",
            port.map(|p| format!(" on port {p}")).unwrap_or_default()
        ));
    }
    Ok(())
}

pub fn restart(config_path: Option<PathBuf>, port: Option<u16>) -> anyhow::Result<()> {
    match stop(port) {
        Ok(()) => {}
        Err(e) => {
            // Not fatal — daemon may simply not be running. Log and continue.
            eprintln!("claude-proxy: stop step skipped ({e})");
        }
    }

    // Wait for the OS to release the listening socket(s) before re-binding.
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let probe_port = port.unwrap_or(proxy::DEFAULT_PORT);
        if std::net::TcpListener::bind(("127.0.0.1", probe_port)).is_ok() {
            break;
        }
    }

    start(config_path, port)
}
