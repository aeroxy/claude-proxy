use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct ProxyConfig {
    pub upstream_proxy: Option<String>,
}

pub fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config/claude-proxy/config.toml"))
}

pub fn load_config(path_override: Option<PathBuf>) -> ProxyConfig {
    if let Ok(env_proxy) = env::var("HTTPS_PROXY") {
        return ProxyConfig {
            upstream_proxy: Some(env_proxy),
        };
    }

    let candidates: Vec<PathBuf> = match path_override {
        Some(p) => vec![p],
        None => [Some(PathBuf::from("config.toml")), default_config_path()]
            .into_iter()
            .flatten()
            .collect(),
    };

    for path in candidates {
        if let Ok(config_str) = fs::read_to_string(&path) {
            if let Ok(config) = toml::from_str::<ProxyConfig>(&config_str) {
                return config;
            }
        }
    }

    ProxyConfig::default()
}
