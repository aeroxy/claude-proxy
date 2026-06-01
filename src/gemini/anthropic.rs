//! Anthropic Messages API surface (`POST /v1/messages`,
//! `POST /v1/messages/count_tokens`) served by the proxy.
//!
//! Routes each request to the `gemini-cli` or `antigravity` upstream by the
//! provider prefix on the body's `model` (`gemini-cli/<m>`, `antigravity/<m>`),
//! translating the Anthropic body to native Gemini and back. The translation
//! lives in [`super::anthropic_translate`]; everything downstream (envelope,
//! upstream POST + SSE pump, creds) is the shared Gemini machinery.
//!
//! Entry point: [`try_handle`], called from both branches of the proxy. In the
//! plain-HTTP origin branch it serves `/v1/messages` unconditionally; in the
//! TLS-MITM branch the caller additionally gates on `host == api.anthropic.com`
//! **and** [`model_has_provider_prefix`] so unprefixed traffic passes through to
//! the real Anthropic API and the normal `claude` CLI keeps working.

use std::sync::Arc;

use hyper::body::Bytes;
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use tracing::{info, warn};

use super::anthropic_translate::{self as atr, ClaudeStream, ToolMaps};
use super::{creds, models, provider, translate, GeminiState};
use crate::proxy::{full_body, ProxyBody};

/// The host the real Anthropic API lives at — the MITM gate target.
pub const ANTHROPIC_UPSTREAM_HOST: &str = "api.anthropic.com";

/// True if `path` is an Anthropic Messages route we serve.
pub fn is_messages_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path == "/v1/messages" || path == "/v1/messages/count_tokens"
}

/// True if the body's `model` carries one of our provider prefixes. Used to gate
/// MITM interception of `api.anthropic.com` so only requests meant for us are
/// hijacked; everything else falls through to the real Anthropic API.
pub fn model_has_provider_prefix(body: &[u8]) -> bool {
    // Only the `model` field matters here, and this runs on *every* intercepted
    // `api.anthropic.com` request (the MITM gate for the real `claude` CLI), so
    // deserialize just that field as a borrowed `&str` rather than building a
    // full `Value` DOM over a potentially large conversation body.
    #[derive(serde::Deserialize)]
    struct ModelQuery<'a> {
        model: &'a str,
    }
    serde_json::from_slice::<ModelQuery>(body)
        .map(|q| models::split_model(q.model).is_some())
        .unwrap_or(false)
}

/// Handle an Anthropic Messages request. Returns `None` if the path isn't ours.
pub async fn try_handle(
    method: &Method,
    path: &str,
    body: Bytes,
    client: &reqwest::Client,
    state: &Arc<GeminiState>,
) -> Option<Response<ProxyBody>> {
    let path_only = path.split('?').next().unwrap_or(path);
    if !is_messages_path(path_only) {
        return None;
    }

    info!("Anthropic API request: {} {}", method, path);

    if method != Method::POST {
        return Some(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only POST is supported",
            "invalid_request_error",
        ));
    }

    if path_only == "/v1/messages/count_tokens" {
        Some(handle_count_tokens(body, client, state).await)
    } else {
        Some(handle_messages(body, client, state).await)
    }
}

/// Pick the account for `provider`, refresh its token, and translate the
/// Anthropic `body` into the provider envelope for `action`. Returns the
/// serialized envelope + a fresh access token, or an Anthropic error response.
/// Shared by messages + count_tokens.
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
            warn!("anthropic: token refresh failed for {}: {}", account.email, e);
            return Err(error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Auth refresh failed: {e}"),
                "api_error",
            ));
        }
    };

    let gemini_body = atr::claude_to_gemini(req);
    let gemini_bytes = serde_json::to_vec(&gemini_body).unwrap_or_default();
    let payload = if provider == models::ANTIGRAVITY {
        translate::gemini_to_antigravity(&gemini_bytes, bare_model, &account.project_id, action)
    } else {
        translate::gemini_to_gemini_cli(&gemini_bytes, bare_model, &account.project_id, action)
    };

    info!(
        "Anthropic {} -> provider={} model={} (account {})",
        action, provider, bare_model, account.email
    );

    Ok((serde_json::to_vec(&payload).unwrap_or_default(), access_token))
}

async fn handle_messages(
    body: Bytes,
    client: &reqwest::Client,
    state: &Arc<GeminiState>,
) -> Response<ProxyBody> {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {e}"), "invalid_request_error")
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
    let maps = ToolMaps::from_request(&req);

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
            warn!("anthropic: upstream request failed: {}", e);
            return error_response(StatusCode::BAD_GATEWAY, &format!("Upstream error: {e}"), "api_error");
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let raw = resp.bytes().await.unwrap_or_default();
        warn!("anthropic: upstream {} for {}: {}", status, bare_model, String::from_utf8_lossy(&raw));
        let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        return error_response(
            code,
            &format!("Upstream error: {}", String::from_utf8_lossy(&raw)),
            upstream_error_type(code),
        );
    }

    if stream {
        let mut cstate = ClaudeStream::new(maps);
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
            );
        }
    };

    if serde_json::from_slice::<serde_json::Value>(&raw).is_err() {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "Upstream returned invalid JSON",
            "api_error",
        );
    }

    let gemini_resp = translate::unwrap_response_nonstream(&raw);
    let claude = atr::gemini_to_claude_nonstream(&gemini_resp, &maps);
    json_response(StatusCode::OK, claude)
}

async fn handle_count_tokens(
    body: Bytes,
    client: &reqwest::Client,
    state: &Arc<GeminiState>,
) -> Response<ProxyBody> {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, &format!("Invalid JSON: {e}"), "invalid_request_error")
        }
    };
    let model_full = req.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let (provider_name, bare_model) = match models::split_model(model_full) {
        Some(p) => p,
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                &format!("Model must be prefixed with the provider (got `{model_full}`)."),
                "not_found_error",
            )
        }
    };

    let (payload_bytes, access_token) = match prepare(&req, provider_name, bare_model, "countTokens", state).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let resp = match provider::send_request(
        client,
        provider_name,
        bare_model,
        &access_token,
        payload_bytes,
        "countTokens",
        false,
        &state.antigravity_version,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("anthropic: countTokens upstream failed: {}", e);
            return error_response(StatusCode::BAD_GATEWAY, &format!("Upstream error: {e}"), "api_error");
        }
    };

    if !resp.status().is_success() {
        let code = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let raw = resp.bytes().await.unwrap_or_default();
        return error_response(
            code,
            &format!("Upstream error: {}", String::from_utf8_lossy(&raw)),
            upstream_error_type(code),
        );
    }

    let raw = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to read upstream response body: {e}"),
                "api_error",
            );
        }
    };
    let v: Value = serde_json::from_slice(&raw).unwrap_or_else(|_| json!({}));
    // countTokens upstream is bare `{"totalTokens":N}` (no `.response` wrapper),
    // but tolerate a wrapped shape too.
    let total = v
        .get("totalTokens")
        .and_then(|t| t.as_i64())
        .or_else(|| v.get("response").and_then(|r| r.get("totalTokens")).and_then(|t| t.as_i64()))
        .unwrap_or(0);
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&json!({ "input_tokens": total })).unwrap_or_default(),
    )
}

fn login_name(provider: &str) -> &'static str {
    if provider == models::ANTIGRAVITY {
        "antigravity"
    } else {
        "gemini"
    }
}

/// Map an upstream HTTP status to an Anthropic error `type`.
fn upstream_error_type(code: StatusCode) -> &'static str {
    match code.as_u16() {
        400 => "invalid_request_error",
        401 | 403 => "authentication_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        529 => "overloaded_error",
        _ => "api_error",
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

/// Anthropic error envelope: `{"type":"error","error":{"type":…,"message":…}}`.
fn error_response(status: StatusCode, message: &str, atype: &str) -> Response<ProxyBody> {
    warn!("Anthropic request failed [{} {}]: {}", status.as_u16(), atype, message);
    let body = json!({
        "type": "error",
        "error": { "type": atype, "message": message },
    });
    json_response(status, body.to_string().into_bytes())
}
