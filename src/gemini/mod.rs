//! Gemini API surface (`/v1beta/models…`) served by the proxy for opencode's
//! `@ai-sdk/google` provider. Routes each request to the `gemini-cli` or
//! `antigravity` upstream by model ID, translating the native Gemini body to
//! the Cloud Code Assist envelope and back.
//!
//! Entry point: [`try_handle`], called from both branches of the proxy (plain
//! HTTP origin and TLS MITM). Returns `None` when the path is not a Gemini
//! route, so the caller can fall through to normal proxying.

pub mod anthropic;
mod anthropic_translate;
pub mod creds;
pub mod models;
pub mod openai;
mod openai_translate;
mod provider;
mod schema_clean;
mod translate;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use hyper::body::Bytes;
use hyper::{Method, Response, StatusCode};
use tracing::{info, warn};

use crate::proxy::{full_body, ProxyBody};

/// Host the `@ai-sdk/google` default endpoint resolves to; the only host we
/// intercept for Gemini routing in the TLS-MITM branch.
pub const GEMINI_UPSTREAM_HOST: &str = "generativelanguage.googleapis.com";

/// Config-derived state shared across requests.
#[derive(Debug)]
pub struct GeminiState {
    pub auth_dirs: Vec<PathBuf>,
    pub catalog: models::Catalog,
    pub antigravity_version: String,
}

impl GeminiState {
    pub fn new(
        auth_dirs: Vec<PathBuf>,
        models_file: Option<PathBuf>,
        antigravity_version: String,
    ) -> Self {
        let catalog = models::Catalog::load(models_file.as_deref());
        GeminiState {
            auth_dirs,
            catalog,
            antigravity_version,
        }
    }
}

/// True if `path` looks like a Gemini API route we serve. Accepts both the
/// canonical `/v1beta/models/{model}…` shape and the `models/`-less form that
/// `@ai-sdk/google` emits (`/v1beta/{model}:{action}`, where `{model}` carries
/// the provider prefix).
pub fn is_gemini_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path == "/v1beta/models"
        || path.starts_with("/v1beta/models/")
        || (path.starts_with("/v1beta/") && path.contains(':'))
}

/// Handle a Gemini API request. Returns `None` if the path isn't ours.
pub async fn try_handle(
    method: &Method,
    path: &str,
    body: Bytes,
    client: &reqwest::Client,
    state: &Arc<GeminiState>,
) -> Option<Response<ProxyBody>> {
    let path_only = path.split('?').next().unwrap_or(path);
    if !is_gemini_path(path_only) {
        return None;
    }

    info!("Gemini API request: {} {}", method, path);

    let rest = path_only.strip_prefix("/v1beta/")?;

    // GET /v1beta/models  → list catalog
    if method == Method::GET && rest == "models" {
        return Some(list_models(client, state).await);
    }

    // The `models/` segment is optional: canonical Gemini uses it, @ai-sdk/google
    // omits it. After stripping it, `spec` is `{model}:{action}` or `{model}`.
    let spec = rest.strip_prefix("models/").unwrap_or(rest);

    // POST .../{model}:{action}
    if let Some((model, action)) = spec.split_once(':') {
        return Some(handle_generate(model, action, body, client, state).await);
    }

    // GET .../{model} → single model metadata
    if method == Method::GET {
        return Some(get_model(spec, client, state).await);
    }

    Some(error_response(
        StatusCode::NOT_FOUND,
        &format!("Unsupported Gemini route: {path_only}"),
        "NOT_FOUND",
    ))
}

/// Providers we currently hold a credential for (empty set falls back to all).
fn available_providers(state: &GeminiState) -> HashSet<String> {
    let mut set: HashSet<String> = creds::discover_accounts(&state.auth_dirs)
        .into_iter()
        .map(|a| a.provider)
        .collect();
    if set.is_empty() {
        set.insert(models::GEMINI_CLI.to_string());
        set.insert(models::ANTIGRAVITY.to_string());
    }
    set
}

async fn list_models(client: &reqwest::Client, state: &Arc<GeminiState>) -> Response<ProxyBody> {
    let state_clone = state.clone();
    let providers =
        match tokio::task::spawn_blocking(move || available_providers(&state_clone)).await {
            Ok(providers) => providers,
            Err(e) => {
                tracing::warn!("gemini: provider discovery failed: {}", e);
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to discover available providers",
                    "INTERNAL",
                );
            }
        };

    let json = state.catalog.list_models_json(client, &providers).await;
    json_response(StatusCode::OK, json.to_string().into_bytes())
}

async fn get_model(
    model: &str,
    client: &reqwest::Client,
    state: &Arc<GeminiState>,
) -> Response<ProxyBody> {
    let state_clone = state.clone();
    let providers =
        match tokio::task::spawn_blocking(move || available_providers(&state_clone)).await {
            Ok(providers) => providers,
            Err(e) => {
                tracing::warn!("gemini: provider discovery failed: {}", e);
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to discover available providers",
                    "INTERNAL",
                );
            }
        };
    let list = state.catalog.list_models_json(client, &providers).await;
    if let Some(arr) = list.get("models").and_then(|m| m.as_array()) {
        // Tolerate a percent-encoded provider separator
        // (`gemini-cli%2Fgemini-2.5-pro`), matching `split_model`; catalog
        // names are emitted with a literal `/`.
        let decoded = model.replace("%2F", "/").replace("%2f", "/");
        let want = format!(
            "models/{}",
            decoded.strip_prefix("models/").unwrap_or(&decoded)
        );
        if let Some(found) = arr
            .iter()
            .find(|m| m.get("name").and_then(|n| n.as_str()) == Some(&want))
        {
            return json_response(StatusCode::OK, found.to_string().into_bytes());
        }
    }
    error_response(
        StatusCode::NOT_FOUND,
        &format!("Model not found: {model}"),
        "NOT_FOUND",
    )
}

async fn handle_generate(
    model: &str,
    action: &str,
    body: Bytes,
    client: &reqwest::Client,
    state: &Arc<GeminiState>,
) -> Response<ProxyBody> {
    let (upstream_action, stream) = match action {
        "generateContent" => ("generateContent", false),
        "streamGenerateContent" => ("streamGenerateContent", true),
        "countTokens" => ("countTokens", false),
        other => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Unsupported method: {other}"),
                "INVALID_ARGUMENT",
            )
        }
    };

    // Prefix-based routing: `gemini-cli/<model>` or `antigravity/<model>`.
    let (provider, model) = match models::split_model(model) {
        Some((p, m)) => (p, m),
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                &format!(
                    "Model must be prefixed with the provider, e.g. `gemini-cli/gemini-2.5-pro` or `antigravity/claude-sonnet-4-6` (got `{model}`)."
                ),
                "NOT_FOUND",
            )
        }
    };

    // Vertex-specific path (completely verbatim native Gemini)
    if provider == models::VERTEX {
        let access_token = match creds::get_vertex_token().await {
            Ok(t) => t,
            Err(e) => {
                warn!("gemini: Vertex token fetch failed: {}", e);
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Auth refresh failed: {e}"),
                    "UNAVAILABLE",
                );
            }
        };

        let payload_bytes = body.to_vec();

        info!("Gemini {} (Vertex) -> model={}", action, model);

        let resp = match provider::send_request(
            client,
            provider,
            model,
            &access_token,
            payload_bytes,
            upstream_action,
            stream,
            &state.antigravity_version,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("gemini: Vertex upstream request failed: {}", e);
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Upstream error: {e}"),
                    "UNAVAILABLE",
                );
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let raw = resp.bytes().await.unwrap_or_default();
            warn!(
                "gemini: Vertex upstream {} for {}: {}",
                status,
                model,
                String::from_utf8_lossy(&raw)
            );
            return json_response(code, raw.to_vec());
        }

        if stream {
            let body = provider::stream_body_from_response(resp);
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .body(body)
                .unwrap_or_else(|_| Response::new(full_body(Bytes::new())));
        }

        let raw = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Failed to read upstream response body: {e}"),
                    "UNAVAILABLE",
                );
            }
        };

        let out = if upstream_action == "countTokens" {
            raw.to_vec()
        } else {
            translate::unwrap_response_nonstream(&raw)
        };
        return json_response(StatusCode::OK, out);
    }

    let (account, access_token) =
        match creds::resolve_account("gemini", provider, &state.auth_dirs).await {
            Ok(v) => v,
            Err(creds::AccountError::NoCredential) => {
                return error_response(
                    StatusCode::UNAUTHORIZED,
                    &format!(
                        "No credential for provider '{provider}'. Run `claude-proxy login {}`.",
                        if provider == models::ANTIGRAVITY {
                            "antigravity"
                        } else {
                            "gemini"
                        }
                    ),
                    "UNAUTHENTICATED",
                )
            }
            Err(creds::AccountError::RefreshFailed(msg)) => {
                return error_response(StatusCode::BAD_GATEWAY, &msg, "UNAVAILABLE")
            }
            Err(creds::AccountError::OnboardFailed(msg)) => {
                return error_response(StatusCode::BAD_GATEWAY, &msg, "UNAVAILABLE")
            }
        };

    let payload = if provider == models::ANTIGRAVITY {
        translate::gemini_to_antigravity(&body, model, &account.project_id, upstream_action)
    } else {
        translate::gemini_to_gemini_cli(&body, model, &account.project_id, upstream_action)
    };
    let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default();

    info!(
        "Gemini {} -> provider={} model={} (account {})",
        action, provider, model, account.email
    );

    let resp = match provider::send_request(
        client,
        provider,
        model,
        &access_token,
        payload_bytes,
        upstream_action,
        stream,
        &state.antigravity_version,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("gemini: upstream request failed: {}", e);
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Upstream error: {e}"),
                "UNAVAILABLE",
            );
        }
    };

    let status = resp.status();

    if !status.is_success() {
        let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let raw = resp.bytes().await.unwrap_or_default();
        warn!(
            "gemini: upstream {} for {}: {}",
            status,
            model,
            String::from_utf8_lossy(&raw)
        );
        return json_response(code, raw.to_vec());
    }

    if stream {
        let body = provider::stream_body_from_response(resp);
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)
            // The builder only errors on invalid header name/value, none of
            // which are dynamic here — but fall back rather than panic.
            .unwrap_or_else(|_| Response::new(full_body(Bytes::new())));
    }

    let raw = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to read upstream response body: {e}"),
                "UNAVAILABLE",
            );
        }
    };
    // `generateContent` is wrapped as `{"response": {…}}` and must be unwrapped.
    // `countTokens` is NOT wrapped — upstream returns `{"totalTokens": N}`
    // directly (CLIProxyAPI reads top-level `totalTokens`) — so pass it through.
    let out = if upstream_action == "countTokens" {
        raw.to_vec()
    } else {
        translate::unwrap_response_nonstream(&raw)
    };
    json_response(StatusCode::OK, out)
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response<ProxyBody> {
    let bytes = Bytes::from(body);
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(bytes.clone()))
        // Header/status here are always valid; never panic in the request path.
        .unwrap_or_else(|_| Response::new(full_body(bytes)))
}

fn error_response(status: StatusCode, message: &str, gstatus: &str) -> Response<ProxyBody> {
    warn!(
        "Gemini request failed [{} {}]: {}",
        status.as_u16(),
        gstatus,
        message
    );
    let body = serde_json::json!({
        "error": {
            "code": status.as_u16(),
            "message": message,
            "status": gstatus,
        }
    });
    json_response(status, body.to_string().into_bytes())
}
