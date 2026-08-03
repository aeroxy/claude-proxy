//! Anthropic Messages API surface (`POST /v1/messages`,
//! `POST /v1/messages/count_tokens`) served by the proxy.
//!
//! Routes each request to the `gemini-cli` or `antigravity` upstream by the
//! provider prefix on the body's `model` (`gemini-cli/<m>`, `antigravity/<m>`),
//! translating the Anthropic body to native Gemini and back. The translation
//! lives in [`super::anthropic_translate`]; everything downstream (envelope,
//! upstream POST + SSE pump, creds) is the shared Gemini machinery.
//!
//! A request's model is "ours" one of two ways, both resolved by
//! [`resolve_provider_model`]: it already carries a provider prefix, or it's an
//! exact match in the `[anthropic_model_map]` config (an opt-in, exact-string
//! redirect of a real Anthropic model name, e.g. `claude-sonnet-5`, to a
//! provider-prefixed target — empty by default, so it changes nothing unless
//! configured). This applies uniformly to both transports below — the map is
//! the whole point of the feature: it's how a real, unprefixed model name gets
//! redirected when MITM'ing the real `claude` CLI's traffic.
//!
//! Entry point: [`try_handle`], called from both branches of the proxy. In the
//! plain-HTTP origin branch it serves `/v1/messages` unconditionally; in the
//! TLS-MITM branch the caller additionally gates on `host == api.anthropic.com`
//! **and** [`routed_provider`] so traffic that's neither prefixed nor mapped
//! passes through to the real Anthropic API and the normal `claude` CLI keeps
//! working.

use std::collections::HashMap;
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

/// Resolve a client-supplied `model` string to `(provider, bare_model)`: either
/// it already carries a provider prefix, or it's an exact match in
/// `[anthropic_model_map]` config whose target does. Shared by the MITM gate
/// and both handlers so the two never drift.
pub fn resolve_provider_model<'a>(
    model_full: &'a str,
    map: &'a HashMap<String, String>,
) -> Option<(&'static str, &'a str)> {
    models::split_model(model_full).or_else(|| map.get(model_full).and_then(|m| models::split_model(m)))
}

/// The provider the body's `model` routes to (see [`resolve_provider_model`]),
/// or `None` if it's neither provider-prefixed nor an exact
/// `[anthropic_model_map]` entry — i.e. `Some` is exactly the MITM gate's
/// "routable" condition, so a `None` here means the request falls through to the
/// real Anthropic API.
///
/// The provider name is also the `[compress.providers.<name>]` config key
/// (`gemini-cli` / `antigravity` / `vertex`), which is why the gate resolves it
/// rather than just testing routability: [`crate::compress`] can only derive a
/// provider from a `/`-prefixed model, so a mapped-but-unprefixed name like
/// `claude-sonnet-5` would otherwise silently skip the compression the same
/// request gets when sent with an explicit prefix.
pub fn routed_provider(body: &[u8], map: &HashMap<String, String>) -> Option<&'static str> {
    // Only the `model` field matters here, and this runs on *every* intercepted
    // `api.anthropic.com` request (the MITM gate for the real `claude` CLI), so
    // deserialize just that field as a borrowed `&str` rather than building a
    // full `Value` DOM over a potentially large conversation body.
    #[derive(serde::Deserialize)]
    struct ModelQuery<'a> {
        model: &'a str,
    }
    serde_json::from_slice::<ModelQuery>(body)
        .ok()
        .and_then(|q| resolve_provider_model(q.model, map).map(|(provider, _)| provider))
}

/// `"<key>"` followed by a `:` and the string `"<value>"`, tolerating any amount
/// of JSON whitespace around the colon (serializers differ; ours is compact, the
/// client's needn't be). The cheap pre-check the scrubbers below run before
/// paying for a full DOM parse of a conversation-sized body.
fn has_string_value(body: &[u8], key: &str, value: &str) -> bool {
    let quoted_key = format!("\"{key}\"");
    let quoted_value = format!("\"{value}\"");
    let key = quoted_key.as_bytes();
    let value = quoted_value.as_bytes();
    let skip_ws = |bytes: &[u8], mut i: usize| {
        while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            i += 1;
        }
        i
    };
    body.windows(key.len())
        .enumerate()
        .filter(|(_, w)| *w == key)
        .any(|(start, _)| {
            let i = skip_ws(body, start + key.len());
            if body.get(i) != Some(&b':') {
                return false;
            }
            let i = skip_ws(body, i + 1);
            body.get(i..i + value.len()) == Some(value)
        })
}

/// [`has_string_value`] with an empty string value.
fn has_empty_string_value(body: &[u8], key: &str) -> bool {
    has_string_value(body, key, "")
}

/// Heal transcripts poisoned by an earlier gemini→claude translation bug:
/// [`ClaudeStream`] used to turn Gemini's empty-`text` parts (emitted right
/// before a `functionCall`, usually just carrying a `thoughtSignature`) into
/// empty `text` content blocks. Claude Code stores those in its transcript and
/// resends them on every later turn, so once the session switches back to an
/// unrouted model the real Anthropic API rejects the whole request with
/// `messages: text content blocks must be non-empty` — forever.
///
/// Removes `{"type":"text","text":""}` blocks from **assistant** messages (the
/// only shape we ever produced) and returns the re-serialized body, or `None`
/// when nothing was removed so the caller forwards the original bytes
/// untouched. A message whose content is *only* an empty text block is left
/// alone — dropping it would create an empty `content` array, a different 400,
/// and that shape isn't ours anyway. The substring pre-check keeps the common
/// healthy request free of a full DOM parse (same concern as
/// [`routed_provider`]); a false positive (the pattern inside a string value)
/// just costs the parse and returns `None`.
pub fn scrub_empty_text_blocks(body: &[u8]) -> Option<Vec<u8>> {
    if !has_empty_string_value(body, "text") {
        return None;
    }

    fn is_empty_text_block(block: &Value) -> bool {
        block.get("type").and_then(|t| t.as_str()) == Some("text")
            && block.get("text").and_then(|t| t.as_str()) == Some("")
    }

    let mut root: Value = serde_json::from_slice(body).ok()?;
    let mut changed = false;
    for msg in root.get_mut("messages")?.as_array_mut()? {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        if !blocks.iter().any(|b| !is_empty_text_block(b)) {
            continue;
        }
        let before = blocks.len();
        blocks.retain(|b| !is_empty_text_block(b));
        changed |= blocks.len() != before;
    }
    if changed {
        serde_json::to_vec(&root).ok()
    } else {
        None
    }
}

/// The thinking-block sibling of [`scrub_empty_text_blocks`], same poison, same
/// cure.
///
/// The gemini→claude translation emits Gemini's `thought` parts as Anthropic
/// `thinking` content blocks, but has no `signature` to attach — Gemini's
/// thought signatures are not Anthropic's, and only Anthropic can mint one.
/// Claude Code stores the unsigned block in its transcript as
/// `"signature": ""` and resends it on every later turn, so switching the
/// session back to an unrouted model makes the real Anthropic API reject the
/// whole request with `messages.N.content.M: Invalid \`signature\` in
/// \`thinking\` block` — forever.
///
/// Removes signature-less `thinking` blocks from **assistant** messages. There
/// is nothing to salvage: no value we could put in `signature` would validate,
/// so dropping the block is the only shape the API will accept. Genuine
/// Anthropic thinking blocks carry a non-empty signature and are kept, even
/// when their `thinking` text is empty. Returns `None` when nothing was removed
/// so the caller forwards the original bytes untouched, and leaves a message
/// whose content is *only* such a block alone (dropping it would create an
/// empty `content` array — a different 400).
///
/// Caveat: if the poisoned turn is the one holding the *lastmost* `tool_use`,
/// the API separately demands that turn start with a thinking block, so that
/// request fails either way — dropping just changes which 400. Continuing the
/// session (rather than switching models mid tool-call) heals it.
///
/// The pre-check keys on `"type":"thinking"` rather than on `"signature":""`,
/// because the block we emit has *no* `signature` key at all — a client that
/// resends it verbatim (instead of normalizing it to `""` the way Claude Code
/// does) would otherwise slip past. A signed-and-healthy thinking block matches
/// too and costs a parse that returns `None`; the request-level `thinking`
/// config is `{"type":"enabled"}`, so it doesn't trigger.
pub fn scrub_unsigned_thinking_blocks(body: &[u8]) -> Option<Vec<u8>> {
    if !has_string_value(body, "type", "thinking") {
        return None;
    }

    fn is_unsigned_thinking_block(block: &Value) -> bool {
        block.get("type").and_then(|t| t.as_str()) == Some("thinking")
            && block
                .get("signature")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .is_empty()
    }

    let mut root: Value = serde_json::from_slice(body).ok()?;
    let mut changed = false;
    for msg in root.get_mut("messages")?.as_array_mut()? {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        if !blocks.iter().any(|b| !is_unsigned_thinking_block(b)) {
            continue;
        }
        let before = blocks.len();
        blocks.retain(|b| !is_unsigned_thinking_block(b));
        changed |= blocks.len() != before;
    }
    if changed {
        serde_json::to_vec(&root).ok()
    } else {
        None
    }
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
    if provider == models::VERTEX {
        let access_token = match creds::get_vertex_token().await {
            Ok(t) => t,
            Err(e) => {
                warn!("anthropic: Vertex token fetch failed: {}", e);
                return Err(error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Auth refresh failed: {e}"),
                    "api_error",
                ));
            }
        };
        let gemini_body = atr::claude_to_gemini(req);
        let gemini_bytes = serde_json::to_vec(&gemini_body).unwrap_or_default();
        return Ok((gemini_bytes, access_token));
    }

    let (account, access_token) =
        match creds::resolve_account("anthropic", provider, &state.auth_dirs).await {
            Ok(v) => v,
            Err(creds::AccountError::NoCredential) => {
                return Err(error_response(
                    StatusCode::UNAUTHORIZED,
                    &format!(
                        "No credential for provider '{provider}'. Run `claude-proxy login {}`.",
                        login_name(provider)
                    ),
                    "authentication_error",
                ))
            }
            Err(creds::AccountError::RefreshFailed(msg)) => {
                return Err(error_response(StatusCode::BAD_GATEWAY, &msg, "api_error"))
            }
            Err(creds::AccountError::OnboardFailed(msg)) => {
                return Err(error_response(StatusCode::BAD_GATEWAY, &msg, "api_error"))
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

    Ok((
        serde_json::to_vec(&payload).unwrap_or_default(),
        access_token,
    ))
}

/// Vertex's Claude endpoints (`rawPredict`/`streamRawPredict`/
/// `count-tokens:rawPredict`) reject a `model` field — the model lives in the
/// URL, or is fixed to `count-tokens` — and require `anthropic_version` in the
/// body. Strip/inject accordingly before forwarding the client's Anthropic
/// request upstream.
fn vertex_claude_payload(req: &Value) -> Vec<u8> {
    let mut req = req.clone();
    if let Some(obj) = req.as_object_mut() {
        obj.remove("model");
        obj.entry("anthropic_version")
            .or_insert_with(|| json!("vertex-2023-10-16"));
    }
    serde_json::to_vec(&req).unwrap_or_default()
}

async fn handle_messages(
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

    let (provider_name, bare_model) =
        match resolve_provider_model(model_full, &state.anthropic_model_map) {
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
    if models::split_model(model_full).is_none() {
        info!(
            "Anthropic model map: {} -> {}/{}",
            model_full, provider_name, bare_model
        );
    }

    // Vertex-specific Claude path (completely verbatim Anthropic Messages API -> Vertex rawPredict)
    if provider_name == models::VERTEX {
        let (_, _, model_id) = models::parse_vertex_model(bare_model)
            .unwrap_or_else(|| (String::new(), String::new(), String::new()));
        if model_id.starts_with("claude-") {
            let access_token = match creds::get_vertex_token().await {
                Ok(t) => t,
                Err(e) => {
                    warn!("anthropic: Vertex token fetch failed: {}", e);
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        &format!("Auth refresh failed: {e}"),
                        "api_error",
                    );
                }
            };

            let action = if stream {
                "streamRawPredict"
            } else {
                "rawPredict"
            };
            let payload_bytes = vertex_claude_payload(&req);

            info!(
                "Anthropic messages (Vertex rawPredict) -> model={}",
                bare_model
            );

            let resp = match provider::send_request(
                client,
                provider_name,
                bare_model,
                &access_token,
                payload_bytes,
                action,
                stream,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    warn!("anthropic: Vertex upstream request failed: {}", e);
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
                    "anthropic: Vertex upstream {} for {}: {}",
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
                // Pipe standard Anthropic SSE lines verbatim
                let body = provider::stream_sse(resp, |line| match line {
                    Some(l) => {
                        if !l.is_empty() {
                            vec![format!("{}\n", l)]
                        } else {
                            vec!["\n".to_string()]
                        }
                    }
                    None => Vec::new(),
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

            return json_response(StatusCode::OK, raw.to_vec());
        }
    }

    let action = if stream {
        "streamGenerateContent"
    } else {
        "generateContent"
    };

    let (payload_bytes, access_token) =
        match prepare(&req, provider_name, bare_model, action, state).await {
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
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("anthropic: upstream request failed: {}", e);
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
            "anthropic: upstream {} for {}: {}",
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
                let inner =
                    translate::unwrap_sse_payload(payload).unwrap_or_else(|| payload.to_string());
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

    let gemini_val: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "Upstream returned invalid JSON",
                "api_error",
            );
        }
    };

    let gemini_resp = match gemini_val.get("response") {
        Some(inner) => serde_json::to_vec(inner).unwrap_or_else(|_| raw.to_vec()),
        None => raw.to_vec(),
    };

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
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };
    let model_full = req.get("model").and_then(|m| m.as_str()).unwrap_or("");
    let (provider_name, bare_model) =
        match resolve_provider_model(model_full, &state.anthropic_model_map) {
            Some(p) => p,
            None => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &format!("Model must be prefixed with the provider (got `{model_full}`)."),
                    "not_found_error",
                )
            }
        };
    if models::split_model(model_full).is_none() {
        info!(
            "Anthropic model map: {} -> {}/{}",
            model_full, provider_name, bare_model
        );
    }

    // Vertex-specific Claude path: Anthropic's Vertex API has no per-model
    // countTokens action — token counting goes through a fixed
    // `count-tokens:rawPredict` endpoint that mirrors the real Anthropic
    // `/v1/messages/count_tokens` API (verbatim body in, `{"input_tokens":N}`
    // out), so it bypasses `prepare()`'s Gemini-envelope translation entirely.
    if provider_name == models::VERTEX {
        let model_id = models::parse_vertex_model(bare_model)
            .map(|(_, _, model_id)| model_id)
            .unwrap_or_default();
        if model_id.starts_with("claude-") {
            return handle_vertex_claude_count_tokens(&req, bare_model, client).await;
        }
    }

    let (payload_bytes, access_token) =
        match prepare(&req, provider_name, bare_model, "countTokens", state).await {
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
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("anthropic: countTokens upstream failed: {}", e);
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Upstream error: {e}"),
                "api_error",
            );
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
        .or_else(|| {
            v.get("response")
                .and_then(|r| r.get("totalTokens"))
                .and_then(|t| t.as_i64())
        })
        .unwrap_or(0);
    json_response(
        StatusCode::OK,
        serde_json::to_vec(&json!({ "input_tokens": total })).unwrap_or_default(),
    )
}

/// Vertex Claude token counting: hits the fixed `count-tokens:rawPredict`
/// endpoint with the client's Anthropic body verbatim (same shape the real
/// `/v1/messages/count_tokens` API expects) and passes the `{"input_tokens":N}`
/// response straight through — no Gemini-envelope translation involved.
async fn handle_vertex_claude_count_tokens(
    req: &Value,
    bare_model: &str,
    client: &reqwest::Client,
) -> Response<ProxyBody> {
    let (project_id, region, _) = match models::parse_vertex_model(bare_model) {
        Some(v) => v,
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                &format!("Invalid Vertex model format: `{bare_model}`."),
                "not_found_error",
            )
        }
    };

    let access_token = match creds::get_vertex_token().await {
        Ok(t) => t,
        Err(e) => {
            warn!("anthropic: Vertex token fetch failed: {}", e);
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Auth refresh failed: {e}"),
                "api_error",
            );
        }
    };

    let url = provider::build_vertex_count_tokens_url(&project_id, &region);
    info!(
        "Anthropic count_tokens (Vertex rawPredict) -> model={}",
        bare_model
    );

    let payload_bytes = vertex_claude_payload(req);

    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .body(payload_bytes)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "anthropic: Vertex countTokens upstream request failed: {}",
                e
            );
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
            "anthropic: Vertex countTokens upstream {} for {}: {}",
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

    json_response(StatusCode::OK, raw.to_vec())
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
    warn!(
        "Anthropic request failed [{} {}]: {}",
        status.as_u16(),
        atype,
        message
    );
    let body = json!({
        "type": "error",
        "error": { "type": atype, "message": message },
    });
    json_response(status, body.to_string().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poisoned_body() -> Vec<u8> {
        json!({
            "model": "claude-fable-5",
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hm", "signature": "" },
                    { "type": "text", "text": "" },
                    { "type": "tool_use", "id": "Bash-1", "name": "Bash", "input": {} },
                ]},
            ],
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn scrub_removes_empty_assistant_text_block() {
        let out = scrub_empty_text_blocks(&poisoned_body()).expect("body should change");
        let root: Value = serde_json::from_slice(&out).unwrap();
        let blocks = root["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(root["model"], "claude-fable-5");
    }

    #[test]
    fn scrub_leaves_healthy_body_untouched() {
        let body = json!({
            "model": "claude-fable-5",
            "messages": [
                { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
            ],
        })
        .to_string()
        .into_bytes();
        assert!(scrub_empty_text_blocks(&body).is_none());
    }

    /// A pretty-printed / whitespace-padded body must still be scrubbed — the
    /// pre-check can't assume the client serializes as compactly as we do.
    #[test]
    fn scrub_tolerates_whitespace_around_colon() {
        let body = br#"{
            "model": "claude-fable-5",
            "messages": [
                { "role" : "assistant" , "content" : [
                    { "type" : "text" ,
                      "text"
                          :
                          "" },
                    { "type" : "tool_use", "id": "Bash-1", "name": "Bash", "input": {} }
                ] }
            ]
        }"#;
        let out = scrub_empty_text_blocks(body).expect("body should change");
        let root: Value = serde_json::from_slice(&out).unwrap();
        let blocks = root["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
    }

    /// The pattern inside a *user* turn is the client's own doing, not our
    /// poison — forwarded untouched.
    #[test]
    fn scrub_ignores_user_turns() {
        let body = json!({
            "model": "claude-fable-5",
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "" }] },
            ],
        })
        .to_string()
        .into_bytes();
        assert!(scrub_empty_text_blocks(&body).is_none());
    }

    /// Removing the only block would create an empty `content` array — a
    /// different 400 — so an all-empty assistant message is left alone.
    #[test]
    fn scrub_never_empties_content() {
        let body = json!({
            "model": "claude-fable-5",
            "messages": [
                { "role": "assistant", "content": [{ "type": "text", "text": "" }] },
            ],
        })
        .to_string()
        .into_bytes();
        assert!(scrub_empty_text_blocks(&body).is_none());
    }

    #[test]
    fn thinking_scrub_removes_unsigned_assistant_block() {
        let out = scrub_unsigned_thinking_blocks(&poisoned_body()).expect("body should change");
        let root: Value = serde_json::from_slice(&out).unwrap();
        let blocks = root["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
    }

    /// The shape we actually emit: `{"type":"thinking","thinking":…}` with no
    /// `signature` key at all. A client resending it verbatim (rather than
    /// normalizing it to `""` like Claude Code) must still be healed, which is
    /// why the pre-check keys on `"type":"thinking"`.
    #[test]
    fn thinking_scrub_removes_block_with_missing_signature() {
        let body = json!({
            "model": "claude-fable-5",
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hm" },
                    { "type": "text", "text": "done" },
                ]},
            ],
        })
        .to_string()
        .into_bytes();
        let out = scrub_unsigned_thinking_blocks(&body).expect("body should change");
        let root: Value = serde_json::from_slice(&out).unwrap();
        let blocks = root["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    /// A genuine Anthropic thinking block is kept even when its `thinking`
    /// text is empty — the signature is what makes it valid, not the text.
    #[test]
    fn thinking_scrub_keeps_signed_block() {
        let body = json!({
            "model": "claude-fable-5",
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "", "signature": "CAIS4QQK" },
                    { "type": "tool_use", "id": "Bash-1", "name": "Bash", "input": {} },
                ]},
            ],
        })
        .to_string()
        .into_bytes();
        assert!(scrub_unsigned_thinking_blocks(&body).is_none());
    }

    /// Same whitespace tolerance as the text scrubber's pre-check.
    #[test]
    fn thinking_scrub_tolerates_whitespace_around_colon() {
        let body = br#"{
            "model": "claude-fable-5",
            "messages": [
                { "role" : "assistant" , "content" : [
                    { "type" : "thinking" , "thinking" : "hm" ,
                      "signature"
                          :
                          "" },
                    { "type" : "tool_use", "id": "Bash-1", "name": "Bash", "input": {} }
                ] }
            ]
        }"#;
        let out = scrub_unsigned_thinking_blocks(body).expect("body should change");
        let root: Value = serde_json::from_slice(&out).unwrap();
        let blocks = root["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
    }

    /// Removing the only block would create an empty `content` array — a
    /// different 400 — so a thinking-only assistant message is left alone.
    #[test]
    fn thinking_scrub_never_empties_content() {
        let body = json!({
            "model": "claude-fable-5",
            "messages": [
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "hm", "signature": "" },
                ]},
            ],
        })
        .to_string()
        .into_bytes();
        assert!(scrub_unsigned_thinking_blocks(&body).is_none());
    }
}

