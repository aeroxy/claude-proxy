use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use tracing::warn;

use crate::compress::CompressConfig;

#[derive(Debug, Deserialize, Default)]
pub struct ProxyConfig {
    /// Listening port. Overridden by the `--port` CLI flag; falls back to
    /// [`crate::proxy::DEFAULT_PORT`] (7777) when neither is set.
    pub port: Option<u16>,
    pub upstream_proxy: Option<String>,
    /// PEM-encoded X.509 CA cert to use for MITM instead of the auto-generated one.
    /// Must be set together with ca_key_path.
    pub ca_cert_path: Option<PathBuf>,
    /// PEM-encoded private key matching ca_cert_path.
    pub ca_key_path: Option<PathBuf>,
    #[serde(default)]
    pub map_local: Vec<MapLocalRule>,
    /// General proxy settings (credential dirs, custom model catalog). All fields
    /// optional — zero config gives working defaults.
    #[serde(default)]
    pub settings: Settings,
    /// OpenAI Chat Completions aggregator backends. Each `[[openai]]` entry is an
    /// OpenAI-compatible upstream selected by a provider prefix on the model
    /// (first `/`-segment); empty disables the `/v1/chat/completions` surface.
    #[serde(default)]
    pub openai: Vec<OpenAIProvider>,
    /// Content compression settings per downstream provider.
    #[serde(default)]
    pub compress: CompressConfig,
    /// Redirects specific Anthropic model names to a provider-prefixed Gemini
    /// model when intercepting `api.anthropic.com`. Key = exact `model` string
    /// the client sends (e.g. a real `claude-*` model); value = a normal
    /// provider-prefixed model (`gemini-cli/...`, `antigravity/...`,
    /// `vertex/...`). Empty by default — no entries means no change in
    /// behavior, since this is an opt-in exception to the provider-prefix gate.
    #[serde(default)]
    pub anthropic_model_map: HashMap<String, String>,
    /// Claude subscription passthrough: serve `/v1/messages` against the *real*
    /// Anthropic API using the Claude Code OAuth credential from the macOS
    /// Keychain. Absent disables the surface entirely; an empty `[claude_oauth]`
    /// table enables it with the defaults below.
    #[serde(default)]
    pub claude_oauth: Option<ClaudeOAuthConfig>,
}

/// `[claude_oauth]` — see [`crate::claude_oauth`]. Every field has a working
/// default, so `[claude_oauth]` on its own is a valid, complete config.
#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeOAuthConfig {
    /// Model prefix for explicit routing (`claude-oauth/claude-opus-5`). This is
    /// the *only* way the surface is reached over MITM of `api.anthropic.com`,
    /// so the real `claude` CLI is never hijacked.
    #[serde(default = "default_claude_prefix")]
    pub prefix: String,
    /// Serve unprefixed `/v1/messages` on the plain-HTTP origin branch, so a
    /// client pointing `ANTHROPIC_BASE_URL` at us works with real model names.
    /// Never affects the MITM branch.
    #[serde(default = "default_true")]
    pub serve_unprefixed: bool,
    /// `cc_version` in the billing system block, and the `claude-cli/<v>`
    /// user-agent. Real values carry a build suffix (`2.1.221.9b8`); the
    /// user-agent uses only the leading `major.minor.patch`.
    #[serde(default = "default_cli_version")]
    pub cli_version: String,
    /// `cc_entrypoint` in the billing system block. `cli` pairs with the plain
    /// identity string; anything else pairs with the Agent SDK variant.
    #[serde(default = "default_entrypoint")]
    pub entrypoint: String,
    /// The `anthropic-beta` header, sent **exactly** as listed on every request.
    /// Client-supplied `anthropic-beta` values are discarded, not merged: the API
    /// 400s on any beta it doesn't recognize, so forwarding a caller's list would
    /// let one stray identifier fail the whole request. `oauth-2025-04-20` is
    /// mandatory for OAuth credentials and is re-added by validation even if
    /// removed here.
    #[serde(default = "default_betas")]
    pub betas: Vec<String>,
    /// Model aliases applied after the prefix is stripped: client model -> real
    /// Anthropic model.
    #[serde(default)]
    pub model_map: HashMap<String, String>,
    /// Write refreshed tokens back to the Keychain. On by default: Anthropic
    /// rotates the refresh token, so keeping ours private would eventually
    /// invalidate the real Claude Code login.
    #[serde(default = "default_true")]
    pub write_back: bool,
    /// Raw JSON object merged into the request body before forwarding. Escape
    /// hatch for CLI-fidelity fields we deliberately don't inject
    /// (`context_management`, `output_config`, …). Client-supplied values win.
    #[serde(default)]
    pub inject: HashMap<String, serde_json::Value>,
}

fn default_claude_prefix() -> String {
    "claude-oauth".to_string()
}
fn default_true() -> bool {
    true
}
fn default_cli_version() -> String {
    "2.1.221.9b8".to_string()
}
fn default_entrypoint() -> String {
    "cli".to_string()
}

/// The `anthropic-beta` list a real `claude-cli` sends, minus
/// `fallback-credit-2026-06-01` — that one authorizes spending API credits when
/// the subscription quota runs out, which shouldn't be enabled implicitly for
/// arbitrary clients. Add it back here explicitly if you want it.
fn default_betas() -> Vec<String> {
    [
        "claude-code-20250219",
        "oauth-2025-04-20",
        "context-1m-2025-08-07",
        "interleaved-thinking-2025-05-14",
        "thinking-token-count-2026-05-13",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
        "mid-conversation-system-2026-04-07",
        "advisor-tool-2026-03-01",
        "advanced-tool-use-2025-11-20",
        "effort-2025-11-24",
        "extended-cache-ttl-2025-04-11",
        "cache-diagnosis-2026-04-07",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for ClaudeOAuthConfig {
    fn default() -> Self {
        Self {
            prefix: default_claude_prefix(),
            serve_unprefixed: true,
            cli_version: default_cli_version(),
            entrypoint: default_entrypoint(),
            betas: default_betas(),
            model_map: HashMap::new(),
            write_back: true,
            inject: HashMap::new(),
        }
    }
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
pub struct Settings {
    /// Credential directories, in read order. Defaults to
    /// `["~/.config/claude-proxy/auths", "~/.cli-proxy-api"]`.
    #[serde(default)]
    pub auth_dirs: Option<Vec<PathBuf>>,
    /// Custom model catalog (`models.json` on disk) used for the `/v1beta/models`
    /// listing when a provider has no live-fetched models.
    #[serde(default)]
    pub models_file: Option<PathBuf>,
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
                validate_compress(&config.compress);
                validate_anthropic_model_map(&config.anthropic_model_map);
                if let Some(claude) = &mut config.claude_oauth {
                    validate_claude_oauth(claude);
                }

                config.settings.auth_dirs = config
                    .settings
                    .auth_dirs
                    .map(|dirs| dirs.into_iter().map(expand_tilde).collect());
                config.settings.models_file = config.settings.models_file.map(expand_tilde);

                return config;
            }
        }
    }

    ProxyConfig::default()
}

fn validate_map_local(rules: &[MapLocalRule]) {
    const VALID_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
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
    let mut seen = std::collections::HashSet::new();
    for p in providers {
        if p.name.is_empty() {
            warn!("[[openai]] entry has empty `name`; it can never be selected by a model prefix");
        } else if !seen.insert(p.name.as_str()) {
            warn!(
                "[[openai]] duplicate provider name '{}'; only the first entry will be reachable",
                p.name
            );
        }
        if p.base_url.is_empty() {
            warn!(
                "[[openai]] entry '{}' has empty `base_url`; requests to it will fail",
                p.name
            );
        }
    }
}

fn validate_anthropic_model_map(map: &HashMap<String, String>) {
    use crate::gemini::models;

    for (from, to) in map {
        if from.trim().is_empty() {
            warn!("[anthropic_model_map] has an empty key; it will never match");
            continue;
        }
        match models::split_model(to) {
            None => {
                warn!(
                    "[anthropic_model_map] '{}' maps to '{}', which isn't routable (no recognized \
                     provider prefix); requests for '{}' will 404 (origin) or fall through to the \
                     real Anthropic API untouched (MITM), since this entry can never resolve",
                    from, to, from
                );
            }
            Some((provider, bare_model)) if provider == models::VERTEX => {
                if models::parse_vertex_model(bare_model).is_none() {
                    warn!(
                        "[anthropic_model_map] '{}' maps to '{}', which needs the form \
                         `vertex/<project>/<region>/<model>`; requests for '{}' will fail once forwarded",
                        from, to, from
                    );
                }
            }
            Some(_) => {}
        }
    }
}

/// Warn on `[claude_oauth]` settings that would silently misbehave, and re-add
/// `oauth-2025-04-20` if it was configured away — without it the OAuth
/// credential is rejected outright, so a missing entry is always a mistake.
fn validate_claude_oauth(cfg: &mut ClaudeOAuthConfig) {
    // Normalize before every later comparison: routing and `resolve_model` both
    // match on `format!("{prefix}/")`, so stray whitespace the checks below already
    // look past would otherwise make the prefix unmatchable.
    if cfg.prefix.trim().len() != cfg.prefix.len() {
        cfg.prefix = cfg.prefix.trim().to_string();
    }
    if cfg.prefix.is_empty() {
        warn!("[claude_oauth] `prefix` is empty; falling back to the default `claude-oauth`");
        cfg.prefix = default_claude_prefix();
    }
    if cfg.prefix.contains('/') {
        warn!(
            "[claude_oauth] `prefix` '{}' contains a `/`; routing splits the model on the first \
             `/`, so this prefix can never match",
            cfg.prefix
        );
    }
    if crate::gemini::models::split_model(&format!("{}/x", cfg.prefix)).is_some() {
        warn!(
            "[claude_oauth] `prefix` '{}' collides with a built-in Gemini provider name; \
             the Gemini surface is checked first, so this surface will never be reached",
            cfg.prefix
        );
    }
    const MANDATORY_BETA: &str = "oauth-2025-04-20";
    if !cfg.betas.iter().any(|b| b == MANDATORY_BETA) {
        warn!(
            "[claude_oauth] `betas` is missing `{}`, which OAuth credentials require; re-adding it",
            MANDATORY_BETA
        );
        cfg.betas.insert(0, MANDATORY_BETA.to_string());
    }
    if !cfg.write_back {
        warn!(
            "[claude_oauth] write_back = false: refreshed tokens stay in memory. Anthropic rotates \
             the refresh token, so the Keychain copy may eventually stop working and Claude Code \
             will ask you to log in again"
        );
    }
    // Drop, don't just warn. These are built by the surface itself, so an
    // injected copy is at best dead weight — except `stream`, which is actively
    // dangerous: `apply_inject` fills only *absent* keys, and `build_payload`
    // reads `stream` into its local before injecting. Injecting it when the
    // client didn't send it makes the upstream stream while our side buffers,
    // handing the caller raw SSE labeled `application/json`. Removing them here
    // is also what makes the message below true.
    for key in RESERVED_INJECT_KEYS {
        if cfg.inject.remove(*key).is_some() {
            warn!(
                "[claude_oauth.inject] sets `{}`, which this surface builds itself; dropping it",
                key
            );
        }
    }
}

/// Body fields `claude_oauth::build_payload` owns. `[claude_oauth.inject]` may
/// not set them — see [`validate_claude_oauth`].
const RESERVED_INJECT_KEYS: &[&str] = &["model", "messages", "system", "metadata", "stream"];

fn validate_compress(config: &crate::compress::CompressConfig) {
    for (name, provider) in &config.providers {
        if let Some(max) = provider.max_tool_chars {
            if max > 0 && max < 400 {
                warn!(
                    "[compress.providers.{}] max_tool_chars={} is very low; \
                     tool results will be truncated to {} chars",
                    name, max, max
                );
            }
        }
        if let Some(bias) = provider.bias {
            if !bias.is_finite() || bias <= 0.0 {
                warn!(
                    "[compress.providers.{}] bias={} is invalid; \
                     bias must be a finite positive number. \
                     Non-finite or non-positive values cause unstable sizing behavior; \
                     invalid value will only be logged but not corrected.",
                    name, bias
                );
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_cannot_smuggle_fields_the_surface_owns() {
        let mut cfg = ClaudeOAuthConfig::default();
        for key in RESERVED_INJECT_KEYS {
            cfg.inject.insert((*key).to_string(), serde_json::json!(true));
        }
        cfg.inject
            .insert("output_config".into(), serde_json::json!({"effort": "high"}));

        validate_claude_oauth(&mut cfg);

        for key in RESERVED_INJECT_KEYS {
            // `stream` above all: left in place it reaches Anthropic while our
            // side buffers, so the caller gets SSE bytes labeled as JSON.
            assert!(!cfg.inject.contains_key(*key), "`{key}` must be dropped");
        }
        assert!(
            cfg.inject.contains_key("output_config"),
            "keys the surface doesn't own must survive"
        );
    }

    /// Routing matches on `format!("{prefix}/")`, so a padded prefix that passed
    /// the blank check unnormalized could never match any model name.
    #[test]
    fn padded_prefix_is_normalized_rather_than_left_unmatchable() {
        let mut cfg = ClaudeOAuthConfig::default();
        cfg.prefix = "  claude-oauth  ".into();
        validate_claude_oauth(&mut cfg);
        assert_eq!(cfg.prefix, "claude-oauth");

        // Whitespace-only still counts as blank and falls back to the default.
        cfg.prefix = "   ".into();
        validate_claude_oauth(&mut cfg);
        assert_eq!(cfg.prefix, default_claude_prefix());
    }

    #[test]
    fn mandatory_oauth_beta_is_restored_when_configured_away() {
        let mut cfg = ClaudeOAuthConfig::default();
        cfg.betas.retain(|b| b != "oauth-2025-04-20");
        validate_claude_oauth(&mut cfg);
        assert!(
            cfg.betas.iter().any(|b| b == "oauth-2025-04-20"),
            "OAuth credentials are rejected without it"
        );
    }
}
