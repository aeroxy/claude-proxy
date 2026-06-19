//! OpenAI Chat Completions API surface (`POST /v1/chat/completions`) driven by
//! the `gemini-cli` / `antigravity` upstreams.
//!
//! Mirrors `anthropic.rs` (the `/v1/messages` surface) but translates the
//! OpenAI Chat Completions envelope instead of the Anthropic one. The provider
//! is selected by a prefix on the body's `model` (`gemini-cli/<model>` or
//! `antigravity/<model>`), parsed by [`super::models::split_model`]. Unprefixed
//! models are not ours — the caller (`openai` aggregator, or the plain-HTTP
//! dispatch in `proxy.rs`) must fall through.
//!
//! Origin mode only: served from the plain-HTTP branch of the proxy. There is
//! no MITM gate (we do not intercept `api.openai.com`).

use std::sync::Arc;

use hyper::body::Bytes;
use hyper::{Method, Response, StatusCode};
use serde_json::Value;
use tracing::{info, warn};

use crate::gemini::{creds, models, provider, translate, GeminiState};
use crate::proxy::{full_body, ProxyBody};

use super::openai_translate::{gemini_to_openai_nonstream, openai_to_gemini, OpenAIStream};

/// True if `path` is the Chat Completions route we serve.
pub fn is_chat_completions_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path == "/v1/chat/completions"
}

/// True if the body's `model` carries one of our provider prefixes. Used to
/// decide whether to hijack the request from the `[[openai]]` aggregator.
pub fn model_has_provider_prefix(body: &[u8]) -> bool {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let model = req.get("model").and_then(|m| m.as_str()).unwrap_or("");
    models::split_model(model).is_some()
}

/// Handle an OpenAI Chat Completions request. Returns `None` if the path isn't
/// ours.
pub async fn try_handle(
    method: &Method,
    path: &str,
    body: Bytes,
    client: &reqwest::Client,
    state: &Arc<GeminiState>,
) -> Option<Response<ProxyBody>> {
    let path_only = path.split('?').next().unwrap_or(path);
    if !is_chat_completions_path(path_only) {
        return None;
    }

    info!("OpenAI Chat Completions (gemini) request: {} {}", method, path);

    if method != Method::POST {
        return Some(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only POST is supported",
            "invalid_request_error",
        ));
    }

    Some(handle_chat_completions(body, client, state).await)
}

/// Pick the account for `provider`, refresh its token, and translate the
/// OpenAI `body` into the provider envelope for `action`. Returns the
/// serialized envelope + a fresh access token, or an OpenAI error response.
async fn prepare(
    req: &Value,
    provider: &str,
    bare_model: &str,
    action: &str,
    state: &Arc<GeminiState>,
) -> Result<(Vec<u8>, String), Response<ProxyBody>> {
    let auth_dirs = state.auth_dirs.clone();
    let account_provider = provider.to_string();
    let account = tokio::task::spawn_blocking(move || creds::pick_account(&account_provider, &auth_dirs))
        .await
        .unwrap_or(None);
    let account = match account {
        Some(a) => a,
        None => {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                &format!(
                    "No credential for provider '{provider}'. Run `claude-proxy login {}`.",
                    login_name(provider)
                ),
                "authentication_error",
            ))
        }
    };

    let access_token = match creds::ensure_fresh(&account).await {
        Ok(t) => t,
        Err(e) => {
            warn!("openai: token refresh failed for {}: {}", account.email, e);
            return Err(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Auth refresh failed: {e}"),
                "api_error",
            ));
        }
    };

    let gemini_body = openai_to_gemini(req);
    let gemini_bytes = serde_json::to_vec(&gemini_body).unwrap_or_default();
    let payload = if provider == models::ANTIGRAVITY {
        translate::gemini_to_antigravity(&gemini_bytes, bare_model, &account.project_id, action)
    } else {
        translate::gemini_to_gemini_cli(&gemini_bytes, bare_model, &account.project_id, action)
    };

    info!(
        "OpenAI chat -> provider={} model={} (account {})",
        provider, bare_model, account.email
    );

    Ok((serde_json::to_vec(&payload).unwrap_or_default(), access_token))
}

async fn handle_chat_completions(
    body: Bytes,
    client: &reqwest::Client,
    state: &Arc<GeminiState>,
) -> Response<ProxyBody> {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };
    let model_full = req.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let (provider_name, bare_model) = match models::split_model(model_full) {
        Some(p) => p,
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                &format!(
                    "Model must be prefixed with the provider, e.g. `gemini-cli/gemini-2.5-pro` or `antigravity/claude-sonnet-4-6` (got `{model_full}`)."
                ),
                "not_found_error",
            )
        }
    };
    let action = if stream { "streamGenerateContent" } else { "generateContent" };

    let (payload_bytes, access_token) = match prepare(&req, provider_name, bare_model, action, state).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let resp = match provider::send_request(
        client,
        provider_name,
        bare_model,
        &access_token,
        payload_bytes,
        action,
        stream,
        &state.antigravity_version,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("openai: upstream request failed: {}", e);
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Upstream error: {e}"),
                "api_error",
            );
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let raw = resp.bytes().await.unwrap_or_default();
        warn!(
            "openai: upstream {} for {}: {}",
            status,
            bare_model,
            String::from_utf8_lossy(&raw)
        );
        let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return error_response(
            code,
            &format!("Upstream error: {}", String::from_utf8_lossy(&raw)),
            upstream_error_type(code),
        );
    }

    if stream {
        let model_echo = model_full.to_string();
        let mut cstate = OpenAIStream::new(model_echo);
        let body = provider::stream_sse(resp, move |line| match line {
            Some(l) => {
                let payload = match l.strip_prefix("data:") {
                    Some(r) => r.trim(),
                    None => return Vec::new(),
                };
                if payload.is_empty() || payload == "[DONE]" {
                    return Vec::new();
                }
                let inner = translate::unwrap_sse_payload(payload).unwrap_or_else(|| payload.to_string());
                cstate.push(inner.as_bytes())
            }
            None => cstate.finish(),
        });
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
                "api_error",
            )
        }
    };

    let gemini_val: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "Upstream returned invalid JSON",
                "api_error",
            );
        }
    };
    let gemini_resp = gemini_val.get("response").unwrap_or(&gemini_val);

    let openai = gemini_to_openai_nonstream(gemini_resp, model_full);
    json_response(StatusCode::OK, openai)
}

fn login_name(provider: &str) -> &'static str {
    match provider {
        models::ANTIGRAVITY => "antigravity",
        _ => "gemini",
    }
}

/// Map an upstream HTTP status to an OpenAI error `type`.
fn upstream_error_type(code: StatusCode) -> &'static str {
    if code.as_u16() >= 500 {
        "api_error"
    } else if code == StatusCode::UNAUTHORIZED {
        "authentication_error"
    } else if code == StatusCode::TOO_MANY_REQUESTS {
        "rate_limit_error"
    } else {
        "invalid_request_error"
    }
}

fn json_response(status: StatusCode, body: Vec<u8>) -> Response<ProxyBody> {
    let bytes = Bytes::from(body);
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(bytes.clone()))
        .unwrap_or_else(|_| Response::new(full_body(bytes)))
}

/// OpenAI error envelope: `{"error":{"message":…,"type":…,"code":null}}`.
fn error_response(status: StatusCode, message: &str, etype: &str) -> Response<ProxyBody> {
    warn!(
        "OpenAI (gemini) request failed [{} {}]: {}",
        status.as_u16(),
        etype,
        message
    );
    let body = serde_json::json!({
        "error": { "message": message, "type": etype, "code": null },
    });
    json_response(status, body.to_string().into_bytes())
}
