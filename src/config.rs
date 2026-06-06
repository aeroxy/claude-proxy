use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tracing::warn;

#[derive(Debug, Deserialize, Default)]
pub struct ProxyConfig {
    /// Listening port. Overridden by the `--port` CLI flag; falls back to
    /// [`crate::proxy::DEFAULT_PORT`] (6666) when neither is set.
    pub port: Option<u16>,
    pub upstream_proxy: Option<String>,
    /// PEM-encoded X.509 CA cert to use for MITM instead of the auto-generated one.
    /// Must be set together with ca_key_path.
    pub ca_cert_path: Option<PathBuf>,
    /// PEM-encoded private key matching ca_cert_path.
    pub ca_key_path: Option<PathBuf>,
    #[serde(default)]
    pub map_local: Vec<MapLocalRule>,
    /// Gemini provider settings (opencode `@ai-sdk/google` support). All fields
    /// optional — zero config gives working defaults.
    #[serde(default)]
    pub gemini: GeminiConfig,
    /// OpenAI Chat Completions aggregator backends. Each `[[openai]]` entry is an
    /// OpenAI-compatible upstream selected by a provider prefix on the model
    /// (first `/`-segment); empty disables the `/v1/chat/completions` surface.
    #[serde(default)]
    pub openai: Vec<OpenAIProvider>,
}

/// One OpenAI-compatible upstream the aggregator can route to. The `[[openai]]`
/// entry's `name` is the provider prefix (`<name>/<upstream-model>`); the part
/// after the first `/` is forwarded verbatim as the upstream `model`.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct OpenAIProvider {
    /// Provider prefix, e.g. `opengateway` in `opengateway/minimax/minimax-m3`.
    pub name: String,
    /// Upstream base URL, e.g. `https://opengateway.example/v1`. The request is
    /// POSTed to `{base_url}/chat/completions`.
    pub base_url: String,
    /// Bearer token sent upstream. When absent, the client's incoming
    /// `Authorization` header (if any) is forwarded instead.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Extra headers added to the upstream request.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct GeminiConfig {
    /// Credential directories, in read order. Defaults to
    /// `["~/.config/claude-proxy/auths", "~/.cli-proxy-api"]`.
    #[serde(default)]
    pub auth_dirs: Option<Vec<PathBuf>>,
    /// Override the embedded model catalog with a `models.json` on disk.
    #[serde(default)]
    pub models_file: Option<PathBuf>,
    /// Version string used in the antigravity `User-Agent`.
    #[serde(default)]
    pub antigravity_version: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MapLocalRule {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub file: Option<PathBuf>,
    #[serde(default)]
    pub status: Option<u16>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
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

                for rule in &mut config.map_local {
                    if let Some(p) = rule.file.take() {
                        rule.file = Some(expand_tilde(p));
                    }
                }
                validate_map_local(&config.map_local);
                validate_openai(&config.openai);

                config.gemini.auth_dirs = config.gemini.auth_dirs.map(|dirs| {
                    dirs.into_iter().map(expand_tilde).collect()
                });
                config.gemini.models_file = config.gemini.models_file.map(expand_tilde);

                return config;
            }
        }
    }

    ProxyConfig::default()
}

fn validate_map_local(rules: &[MapLocalRule]) {
    const VALID_METHODS: &[&str] =
        &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    for rule in rules {
        if rule.url.trim().is_empty() {
            warn!("Map Local rule has empty url; it will never match");
            continue;
        }
        if let Some(m) = &rule.method {
            let upper = m.trim().to_ascii_uppercase();
            if !VALID_METHODS.contains(&upper.as_str()) {
                warn!(
                    "Map Local rule for {} has unrecognized method '{}'; it will only match this exact verb",
                    rule.url, m
                );
            }
        }
        if rule.body.is_some() && rule.file.is_some() {
            warn!(
                "Map Local rule for {} sets both `body` and `file`; `body` wins, `file` ignored",
                rule.url
            );
        }
        if rule.body.is_none() {
            if let Some(p) = &rule.file {
                if !p.exists() {
                    warn!(
                        "Map Local rule for {} points at {} which does not exist (will return 502 until the file appears)",
                        rule.url,
                        p.display()
                    );
                }
            }
        }
    }
}

/// Resolve the effective listening port with precedence: CLI `--port` > config
/// `port` > [`crate::proxy::DEFAULT_PORT`].
pub fn resolve_port(cli_port: Option<u16>, cfg: &ProxyConfig) -> u16 {
    cli_port.or(cfg.port).unwrap_or(crate::proxy::DEFAULT_PORT)
}

fn validate_openai(providers: &[OpenAIProvider]) {
    for p in providers {
        if p.name.trim().is_empty() {
            warn!("[[openai]] entry has empty `name`; it can never be selected by a model prefix");
        }
        if p.base_url.trim().is_empty() {
            warn!("[[openai]] entry '{}' has empty `base_url`; requests to it will fail", p.name);
        }
    }
}

fn expand_tilde(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path
}
