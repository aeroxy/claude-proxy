use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct ProxyConfig {
    pub upstream_proxy: Option<String>,
    /// PEM-encoded X.509 CA cert to use for MITM instead of the auto-generated one.
    /// Must be set together with ca_key_path.
    pub ca_cert_path: Option<PathBuf>,
    /// PEM-encoded private key matching ca_cert_path.
    pub ca_key_path: Option<PathBuf>,
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config/claude-proxy/config.toml"))
}

pub fn load_config(path_override: Option<PathBuf>) -> ProxyConfig {
    let candidates: Vec<PathBuf> = match path_override {
        Some(p) => vec![p],
        None => [Some(PathBuf::from("config.toml")), default_config_path()]
            .into_iter()
            .flatten()
            .collect(),
    };

    for path in candidates {
        if let Ok(config_str) = fs::read_to_string(&path) {
            if let Ok(mut config) = toml::from_str::<ProxyConfig>(&config_str) {
                config.ca_cert_path = config.ca_cert_path.map(expand_tilde);
                config.ca_key_path = config.ca_key_path.map(expand_tilde);

                match (&config.ca_cert_path, &config.ca_key_path) {
                    (Some(_), None) | (None, Some(_)) => {
                        eprintln!(
                            "claude-proxy: ca_cert_path and ca_key_path must be set together"
                        );
                        std::process::exit(1);
                    }
                    _ => {}
                }

                return config;
            }
        }
    }

    ProxyConfig::default()
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(&s[2..]);
        }
    }
    path
}
