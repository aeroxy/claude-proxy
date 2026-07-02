//! Model catalog. Routing is prefix-based (see [`split_model`]); the catalog
//! exists only to render `GET /v1beta/models`.
//! A `[settings] models_file` override supplies a local catalog.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::warn;

use super::provider::{gemini_cli_user_agent, ANTIGRAVITY_USER_AGENT};

pub const GEMINI_CLI: &str = "gemini-cli";
pub const ANTIGRAVITY: &str = "antigravity";
pub const VERTEX: &str = "vertex";

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
            // Accept a raw `/` or a percent-encoded one (`%2F`/%2f`).
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
    if project_id.contains('?')
        || project_id.contains('#')
        || project_id.contains('/')
        || project_id.contains('@')
        || region.contains('?')
        || region.contains('#')
        || region.contains('/')
        || region.contains('@')
        || model_id.contains('?')
        || model_id.contains('#')
        || model_id.contains('/')
        || model_id.contains('@')
    {
        return None;
    }
    Some((project_id, region, model_id))
}

type ModelsByProvider = HashMap<String, Vec<ModelInfo>>;

#[derive(Debug)]
pub struct Catalog {
    pub models: ModelsByProvider,
}

impl Catalog {
    /// Build the catalog. With a `models_file` the listing is pinned to that file.
    pub fn load(models_file: Option<&Path>) -> Self {
        let models = match models_file {
            Some(p) => {
                match std::fs::read_to_string(p) {
                    Ok(s) => parse_models(&s),
                    Err(e) => {
                        warn!("gemini: cannot read models_file {}: {}", p.display(), e);
                        HashMap::new()
                    }
                }
            }
            None => HashMap::new(),
        };
        Catalog { models }
    }

    /// Render `{"models":[…]}` in native Gemini list format, restricted to
    /// providers we currently hold credentials for. Each model name is prefixed
    /// with its provider (`models/<provider>/<id>`) so clients request it with
    /// the prefix the router expects.
    pub async fn list_models_json(
        &self,
        _client: &reqwest::Client,
        available_providers: &HashSet<String>,
    ) -> serde_json::Value {
        let mut out = Vec::new();
        for (provider, list) in &self.models {
            if !available_providers.contains(provider) {
                continue;
            }
            for m in list {
                out.push(model_to_gemini_json(provider, m));
            }
        }
        serde_json::json!({ "models": out })
    }
}

fn parse_models(raw: &str) -> ModelsByProvider {
    serde_json::from_str(raw).unwrap_or_else(|e| {
        warn!("gemini: failed to parse models catalog: {}", e);
        HashMap::new()
    })
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

/// Fetch real Gemini models dynamically from Google Code Assist's `retrieveUserQuota` endpoint
/// using an active OAuth access token and project ID. Returns them mapped with our `gemini-cli/` prefix.
pub async fn fetch_real_gemini_models(
    client: &reqwest::Client,
    project_id: &str,
    access_token: &str,
    fallback_catalog: &Catalog,
) -> anyhow::Result<serde_json::Value> {
    let url = "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota";
    let body = serde_json::json!({
        "project": project_id
    });
    
    let resp = client
        .post(url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("User-Agent", gemini_cli_user_agent(""))
        .header("X-Goog-Api-Client", "gl-node/25.8.2")
        .json(&body)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("retrieveUserQuota returned status {status}: {body}");
    }

    let quota_json: serde_json::Value = resp.json().await?;
    
    let mut out_models = Vec::new();
    let static_gemini_models = fallback_catalog.models.get(GEMINI_CLI);

    if let Some(buckets) = quota_json.get("buckets").and_then(|b| b.as_array()) {
        for bucket in buckets {
            if let Some(model_id) = bucket.get("modelId").and_then(|m| m.as_str()) {
                if model_id.is_empty() || model_id == "all" {
                    continue;
                }
                
                // Find this model in our static/embedded list to preserve descriptions/limits
                let matched_static = static_gemini_models.and_then(|list| {
                    list.iter().find(|m| m.id == model_id)
                });

                let model_obj = match matched_static {
                    Some(m) => {
                        model_to_gemini_json(GEMINI_CLI, m)
                    }
                    None => {
                        let name = format!("models/{GEMINI_CLI}/{model_id}");
                        serde_json::json!({
                            "name": name,
                            "displayName": model_id.to_string(),
                            "description": format!("Dynamically discovered Gemini model: {model_id}"),
                            "supportedGenerationMethods": vec![
                                "generateContent".to_string(),
                                "streamGenerateContent".to_string(),
                                "countTokens".to_string()
                            ]
                        })
                    }
                };
                
                out_models.push(model_obj);
            }
        }
    }

    Ok(serde_json::json!({ "models": out_models }))
}

/// Fetch real Antigravity models dynamically from Antigravity's daily `fetchAvailableModels` endpoint
/// using an active OAuth access token and project ID. Returns them mapped with our `antigravity/` prefix.
pub async fn fetch_real_antigravity_models(
    client: &reqwest::Client,
    project_id: &str,
    access_token: &str,
    fallback_catalog: &Catalog,
) -> anyhow::Result<serde_json::Value> {
    let url = "https://daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels";
    
    let resp = client
        .post(url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("User-Agent", ANTIGRAVITY_USER_AGENT)
        .json(&serde_json::json!({
            "project": project_id
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("fetchAvailableModels returned status {status}: {body}");
    }

    let raw_json: serde_json::Value = resp.json().await?;
    
    let mut out_models = Vec::new();
    let static_antigravity_models = fallback_catalog.models.get(ANTIGRAVITY);

    if let Some(models_map) = raw_json.get("models").and_then(|m| m.as_object()) {
        for (model_id, info) in models_map {
            if model_id.is_empty() || model_id == "all" {
                continue;
            }
            
            // Extract attributes from Google's response if available
            let display_name = info.get("displayName")
                .and_then(|v| v.as_str())
                .unwrap_or(model_id);
            let description = info.get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
                
            let input_limit = info.get("maxTokens")
                .and_then(|v| v.as_u64());
            let output_limit = info.get("maxOutputTokens")
                .and_then(|v| v.as_u64());

            // Check if we have additional properties in our static catalog (if any exists)
            let matched_static = static_antigravity_models.and_then(|list| {
                list.iter().find(|m| m.id == *model_id)
            });

            let model_obj = match matched_static {
                Some(m) => {
                    model_to_gemini_json(ANTIGRAVITY, m)
                }
                None => {
                    let name = format!("models/{ANTIGRAVITY}/{model_id}");
                    let mut obj = serde_json::json!({
                        "name": name,
                        "displayName": display_name,
                        "description": if description.is_empty() {
                            format!("Dynamically discovered Antigravity model: {model_id}")
                        } else {
                            description.to_string()
                        },
                        "supportedGenerationMethods": vec![
                            "generateContent".to_string(),
                            "streamGenerateContent".to_string(),
                            "countTokens".to_string()
                        ]
                    });
                    
                    if let Some(lim) = input_limit {
                        obj["inputTokenLimit"] = serde_json::json!(lim);
                    }
                    if let Some(lim) = output_limit {
                        obj["outputTokenLimit"] = serde_json::json!(lim);
                    }
                    obj
                }
            };
            
            out_models.push(model_obj);
        }
    }

    Ok(serde_json::json!({ "models": out_models }))
}
