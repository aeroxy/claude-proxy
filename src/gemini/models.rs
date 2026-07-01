//! Model catalog. Routing is prefix-based (see [`split_model`]); the catalog
//! exists only to render `GET /v1beta/models`. By default that listing fetches
//! the live catalog from CLIProxyAPI's own source (the same two URLs and
//! fallback order as its `model_updater`), cached with a TTL, and falls back to
//! the embedded `models.json` (lifted from CLIProxyAPI's
//! `internal/registry/models/models.json`). A `[gemini] models_file` override
//! pins the listing to a local file and disables remote fetching.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

pub const GEMINI_CLI: &str = "gemini-cli";
pub const ANTIGRAVITY: &str = "antigravity";
pub const VERTEX: &str = "vertex";

const EMBEDDED_MODELS: &str = include_str!("models.json");

/// Remote catalog sources, tried in order (same as CLIProxyAPI's `modelsURLs`).
const MODELS_URLS: &[&str] = &[
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/models.json",
    "https://models.router-for.me/models.json",
];
const MODELS_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a fetched remote catalog is reused before refetching (CLIProxyAPI
/// refreshes every 3h).
const MODELS_TTL: Duration = Duration::from_secs(3 * 3600);

#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default, rename = "inputTokenLimit")]
    pub input_token_limit: Option<u64>,
    #[serde(default, rename = "outputTokenLimit")]
    pub output_token_limit: Option<u64>,
    #[serde(default, rename = "supportedGenerationMethods")]
    pub supported_generation_methods: Vec<String>,
    // antigravity entries use these instead of the camelCase token limits.
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
}

/// Split a requested model into `(provider, upstream_model)` from its prefix.
///
/// Routing is prefix-based, not catalog-based: the provider is encoded in the
/// model name (`gemini-cli/<model>` or `antigravity/<model>`), and the part
/// after the prefix is forwarded upstream verbatim. An optional leading
/// `models/` is tolerated. Returns `None` if there's no recognized prefix.
pub fn split_model(model: &str) -> Option<(&'static str, &str)> {
    let m = model.strip_prefix("models/").unwrap_or(model);
    for provider in [GEMINI_CLI, ANTIGRAVITY, VERTEX] {
        if let Some(after) = m.strip_prefix(provider) {
            // Accept a raw `/` or a percent-encoded one (`%2F`/`%2f`).
            for sep in ["/", "%2F", "%2f"] {
                if let Some(rest) = after.strip_prefix(sep) {
                    if !rest.is_empty() {
                        return Some((provider, rest));
                    }
                }
            }
        }
    }
    None
}

/// Parse a bare model string for the vertex provider.
/// Expected format: `project-id/region-id/model-id`
pub fn parse_vertex_model(bare_model: &str) -> Option<(String, String, String)> {
    let normalized = bare_model.replace("%2F", "/").replace("%2f", "/");
    let mut parts = normalized.splitn(3, '/');
    let project_id = parts.next()?.to_string();
    let region = parts.next()?.to_string();
    let model_id = parts.next()?.to_string();
    if project_id.is_empty() || region.is_empty() || model_id.is_empty() {
        return None;
    }
    Some((project_id, region, model_id))
}

type ModelsByProvider = HashMap<String, Vec<ModelInfo>>;

#[derive(Debug, Default)]
struct RemoteCache {
    fetched_at: Option<Instant>,
    data: Option<ModelsByProvider>,
}

#[derive(Debug)]
pub struct Catalog {
    /// Static fallback: a `models_file` if configured, else the embedded copy.
    fallback: ModelsByProvider,
    /// When true (no `models_file` override), the listing fetches from the
    /// remote source with a TTL; otherwise the fallback is used verbatim.
    use_remote: bool,
    cache: tokio::sync::Mutex<RemoteCache>,
}

impl Catalog {
    /// Build the catalog. With a `models_file` the listing is pinned to that
    /// file (no remote fetch); otherwise it fetches the live remote catalog and
    /// falls back to the embedded copy.
    pub fn load(models_file: Option<&Path>) -> Self {
        let (fallback, use_remote) = match models_file {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(s) => (parse_models(&s), false),
                Err(e) => {
                    warn!("gemini: cannot read models_file {}: {} — using embedded catalog + remote", p.display(), e);
                    (parse_models(EMBEDDED_MODELS), true)
                }
            },
            None => (parse_models(EMBEDDED_MODELS), true),
        };
        Catalog {
            fallback,
            use_remote,
            cache: tokio::sync::Mutex::new(RemoteCache::default()),
        }
    }

    /// Render `{"models":[…]}` in native Gemini list format, restricted to
    /// providers we currently hold credentials for. Each model name is prefixed
    /// with its provider (`models/<provider>/<id>`) so clients request it with
    /// the prefix the router expects. Fetches the remote catalog (cached) unless
    /// pinned to a `models_file`.
    pub async fn list_models_json(
        &self,
        client: &reqwest::Client,
        available_providers: &HashSet<String>,
    ) -> serde_json::Value {
        let models = self.resolve(client).await;
        let mut out = Vec::new();
        for (provider, list) in &models {
            if !available_providers.contains(provider) {
                continue;
            }
            for m in list {
                out.push(model_to_gemini_json(provider, m));
            }
        }
        serde_json::json!({ "models": out })
    }

    /// Resolve the catalog to use for a listing: pinned file, fresh remote
    /// (cached for `MODELS_TTL`), last good remote, or embedded fallback.
    async fn resolve(&self, client: &reqwest::Client) -> ModelsByProvider {
        if !self.use_remote {
            return self.fallback.clone();
        }
        let mut cache = self.cache.lock().await;
        let fresh = cache
            .fetched_at
            .map(|t| t.elapsed() < MODELS_TTL)
            .unwrap_or(false);
        if fresh {
            if let Some(data) = &cache.data {
                return data.clone();
            }
        }
        match fetch_remote(client).await {
            Some(data) => {
                cache.fetched_at = Some(Instant::now());
                cache.data = Some(data.clone());
                data
            }
            None => {
                // Remote fetch failed (offline / blocked / timeout). Stamp the
                // attempt anyway so we don't retry both 30s fetches on every
                // request — serve the last good catalog (or the embedded
                // fallback) and re-attempt only once the TTL lapses. The catalog
                // only feeds the `/v1beta/models` listing; routing never reads it.
                cache.fetched_at = Some(Instant::now());
                cache.data.get_or_insert_with(|| self.fallback.clone()).clone()
            }
        }
    }
}

fn parse_models(raw: &str) -> ModelsByProvider {
    serde_json::from_str(raw).unwrap_or_else(|e| {
        warn!("gemini: failed to parse models catalog: {} — using embedded catalog", e);
        serde_json::from_str(EMBEDDED_MODELS).expect("embedded models.json is valid")
    })
}

/// Fetch the catalog from the remote sources in order (mirrors CLIProxyAPI's
/// `fetchModelsFromRemote`). Returns `None` if every URL fails.
async fn fetch_remote(client: &reqwest::Client) -> Option<ModelsByProvider> {
    for url in MODELS_URLS {
        let resp = client
            .get(*url)
            .header("User-Agent", "CLIProxyAPI-model-updater")
            .timeout(MODELS_FETCH_TIMEOUT)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => match r.json::<ModelsByProvider>().await {
                Ok(m) if !m.is_empty() => {
                    debug!("gemini: fetched model catalog from {}", url);
                    return Some(m);
                }
                Ok(_) => debug!("gemini: empty model catalog from {}", url),
                Err(e) => warn!("gemini: parse model catalog from {} failed: {}", url, e),
            },
            Ok(r) => debug!("gemini: model catalog fetch {} returned {}", url, r.status()),
            Err(e) => debug!("gemini: model catalog fetch {} failed: {}", url, e),
        }
    }
    None
}

fn model_to_gemini_json(provider: &str, m: &ModelInfo) -> serde_json::Value {
    let name = format!("models/{}/{}", provider, m.id);
    let methods = if m.supported_generation_methods.is_empty() {
        vec![
            "generateContent".to_string(),
            "streamGenerateContent".to_string(),
            "countTokens".to_string(),
        ]
    } else {
        m.supported_generation_methods.clone()
    };
    let input_limit = m.input_token_limit.or(m.context_length);
    let output_limit = m.output_token_limit.or(m.max_completion_tokens);

    let mut obj = serde_json::json!({
        "name": name,
        "displayName": if m.display_name.is_empty() { m.id.clone() } else { m.display_name.clone() },
        "description": m.description,
        "supportedGenerationMethods": methods,
    });
    if let Some(v) = &m.version {
        obj["version"] = serde_json::json!(v);
    }
    if let Some(v) = input_limit {
        obj["inputTokenLimit"] = serde_json::json!(v);
    }
    if let Some(v) = output_limit {
        obj["outputTokenLimit"] = serde_json::json!(v);
    }
    obj
}
