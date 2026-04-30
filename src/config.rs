use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct ProxyConfig {
    pub upstream_proxy: Option<String>,
}

pub fn load_config() -> ProxyConfig {
    // Check HTTPS_PROXY env var first
    if let Ok(env_proxy) = env::var("HTTPS_PROXY") {
        return ProxyConfig {
            upstream_proxy: Some(env_proxy),
        };
    }

    let config_path = PathBuf::from("config.toml");
    if let Ok(config_str) = fs::read_to_string(&config_path) {
        if let Ok(config) = toml::from_str::<ProxyConfig>(&config_str) {
            return config;
        }
    }

    ProxyConfig::default()
}
