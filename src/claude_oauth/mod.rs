//! Claude subscription passthrough: serve `POST /v1/messages` (+
//! `/count_tokens`) against the **real** Anthropic API using the Claude Code
//! OAuth credential from the macOS Keychain.
//!
//! This is a near-pure pipe, like [`crate::openai`] and unlike the Gemini
//! surfaces — Anthropic in, Anthropic out, no format translation. The work is
//! all in the two layers beside it: [`creds`] (Keychain read + refresh) and
//! [`disguise`] (making the request look like `claude-cli`, which Anthropic
//! requires before it will honor an OAuth credential for inference).
//!
//! Routing, by transport:
//!
//! - **Origin** (`ANTHROPIC_BASE_URL=http://127.0.0.1:7777`): the model may
//!   carry the `claude-oauth/` prefix, and with `serve_unprefixed = true`
//!   (default) plain real model names are served too — a client pointing at us
//!   wants us to serve it. Models the Gemini surface would claim are left to it.
//! - **MITM** of `api.anthropic.com`: **only** the explicit prefix routes here.
//!   Everything else falls through to the real API untouched, so the normal
//!   `claude` CLI is never hijacked — same safety crux as
//!   [`crate::gemini::anthropic`]'s gate.
//!
//! Dedup: registered by the caller in `REQUEST_PROMISES` exactly like the routed
//! Gemini-Anthropic path, since this early-returns past the shared dedup block.

pub mod creds;
pub mod disguise;

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use hyper::body::Bytes;
use hyper::{HeaderMap, Method, Response, StatusCode};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::ClaudeOAuthConfig;
use crate::proxy::{full_body, stream_passthrough, ProxyBody};

/// Base URL of the real Anthropic API — where this surface forwards to.
const UPSTREAM_BASE: &str = "https://api.anthropic.com";

/// Response headers worth returning to the client: rate-limit state (SDKs use it
/// to back off), the upstream request id (support/debugging), and `retry-after`.
/// Everything else — including framing headers — is dropped and re-synthesized.
fn forwardable_response_header(name: &str) -> bool {
    name.starts_with("anthropic-ratelimit-")
        || matches!(
            name,
            "request-id" | "retry-after" | "anthropic-organization-id" | "x-should-retry"
        )
}

/// Upstream `request-id` of the last `/v1/messages` call per session, feeding the
/// next call's `cc_prev_req`. In-memory only: a fresh process (like a fresh
/// conversation) has no previous request, and the billing block then omits the
/// field. Only generation calls are recorded — a `count_tokens` id is not what
/// the CLI chains.
static PREV_REQUEST_IDS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Ceiling on remembered sessions. The daemon runs for months and every Claude
/// Code conversation is a new session id, so the map only ever grows; a client
/// minting a fresh id per request would grow it per request. At the cap the map
/// is cleared rather than LRU-evicted: the value is one cosmetic billing field,
/// and forgetting it costs exactly one `cc_prev_req`-less request per session.
const MAX_REMEMBERED_SESSIONS: usize = 4096;

fn prev_request_id(session: &str) -> Option<String> {
    PREV_REQUEST_IDS.lock().ok()?.get(session).cloned()
}

fn record_request_id(session: &str, headers: &reqwest::header::HeaderMap) {
    let Some(id) = headers.get("request-id").and_then(|v| v.to_str().ok()) else {
        return;
    };
    if let Ok(mut map) = PREV_REQUEST_IDS.lock() {
        remember(&mut map, session, id);
    }
}

/// Insert under [`MAX_REMEMBERED_SESSIONS`]; see the constant for the policy.
/// A session id is a UUID from the CLI (36 bytes); one far longer is a client
/// stuffing the header, and is simply not remembered rather than stored.
fn remember(map: &mut HashMap<String, String>, session: &str, id: &str) {
    const MAX_SESSION_ID_LEN: usize = 128;
    if session.len() > MAX_SESSION_ID_LEN {
        return;
    }
    if !map.contains_key(session) && map.len() >= MAX_REMEMBERED_SESSIONS {
        map.clear();
    }
    map.insert(session.to_string(), id.to_string());
}

/// The body's `model`, or `""`.
fn model_of(body: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct ModelQuery {
        model: String,
    }
    serde_json::from_slice::<ModelQuery>(body)
        .map(|q| q.model)
        .unwrap_or_default()
}

/// Whether this request belongs to us.
///
/// `allow_unprefixed` is the transport difference: `true` on the origin branch
/// (where `serve_unprefixed` decides), always `false` over MITM so unprefixed
/// traffic reaches the real Anthropic API.
///
/// A model the Gemini-Anthropic surface would serve is never claimed here, so
/// adding this surface can't change where existing traffic goes.
pub fn routes(
    body: &[u8],
    cfg: &ClaudeOAuthConfig,
    gemini_model_map: &HashMap<String, String>,
    allow_unprefixed: bool,
) -> bool {
    let model = model_of(body);
    if model.is_empty() {
        return false;
    }
    if model.starts_with(&format!("{}/", cfg.prefix)) {
        return true;
    }
    if !allow_unprefixed || !cfg.serve_unprefixed {
        return false;
    }
    crate::gemini::anthropic::resolve_provider_model(&model, gemini_model_map).is_none()
}

/// Handle a `/v1/messages` (or `/count_tokens`) request against the real
/// Anthropic API. Returns `None` only when the path isn't ours — routing is the
/// caller's gate ([`routes`]).
pub async fn try_handle(
    method: &Method,
    path: &str,
    body: Bytes,
    client: &reqwest::Client,
    cfg: &ClaudeOAuthConfig,
    client_headers: &HeaderMap,
) -> Option<Response<ProxyBody>> {
    let path_only = path.split('?').next().unwrap_or(path);
    if !crate::gemini::anthropic::is_messages_path(path_only) {
        return None;
    }

    if method != Method::POST {
        return Some(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only POST is supported",
            "invalid_request_error",
        ));
    }

    Some(handle(path_only, heal_transcript(body), client, cfg, client_headers).await)
}

/// Strip the assistant content blocks an earlier gemini→claude translation wrote
/// into the transcript that the real Anthropic API rejects outright — empty
/// `text` blocks and signature-less `thinking` blocks.
///
/// This surface's upstream *is* the real Anthropic API, so the poison is exactly
/// as fatal here as on the unrouted fall-through in [`crate::proxy`], and reached
/// the same way: a Claude Code session that switches between a provider-backed
/// model and `claude-oauth/…` resends the poisoned turns forever. Both scrubbers
/// substring-pre-check and return `None` unless a block was actually removed, so
/// a healthy body is handed on unchanged. (Compression stays skipped on this
/// path — that mangles content the client meant to send; this only removes blocks
/// the API would refuse.)
fn heal_transcript(mut body: Bytes) -> Bytes {
    if let Some(scrubbed) = crate::gemini::anthropic::scrub_empty_text_blocks(&body) {
        info!("claude-oauth: scrubbed empty assistant text block(s) from the request body");
        body = Bytes::from(scrubbed);
    }
    if let Some(scrubbed) = crate::gemini::anthropic::scrub_unsigned_thinking_blocks(&body) {
        info!("claude-oauth: scrubbed unsigned assistant thinking block(s) from the request body");
        body = Bytes::from(scrubbed);
    }
    body
}

/// Fields `POST /v1/messages/count_tokens` accepts. Its schema is strict —
/// anything else fails with `Extra inputs are not permitted` — so on that route
/// the body is reduced to this set instead of being decorated like a real
/// generation request.
const COUNT_TOKENS_FIELDS: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "tool_choice",
    "thinking",
    "mcp_servers",
];

/// Build the disguised upstream payload: real model name, CLI-shaped `system`,
/// cosmetic fields, config injections. Returns the serialized body plus the
/// stream flag and the model we're sending, for logging.
///
/// `session` is derived once by the caller and shared with the
/// `x-claude-code-session-id` header, so the body and the header can't disagree.
fn build_payload(
    req: &mut Value,
    cfg: &ClaudeOAuthConfig,
    session: &str,
    count_tokens: bool,
) -> Result<(Vec<u8>, bool, String), Response<ProxyBody>> {
    let model_full = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    let model = disguise::resolve_model(&model_full, cfg);
    req["model"] = json!(model.clone());

    let stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let prompt_id = disguise::prompt_id(req, session);
    let prev_req = prev_request_id(session);

    // count_tokens still needs the system-prompt disguise (the OAuth gate applies
    // to every route), but none of the cosmetic decoration — and it rejects even
    // the fields a client may legitimately have sent, so trim to the schema.
    if count_tokens {
        if let Some(obj) = req.as_object_mut() {
            obj.retain(|key, _| COUNT_TOKENS_FIELDS.contains(&key.as_str()));
        }
        disguise::normalize_system(req, cfg, &prompt_id, prev_req.as_deref());
        return Ok((serialize_payload(req)?, false, model));
    }

    disguise::normalize_system(req, cfg, &prompt_id, prev_req.as_deref());
    disguise::apply_metadata(req, session, creds::account_uuid().as_deref());
    disguise::apply_cosmetic_fields(req);
    disguise::apply_inject(req, cfg);
    if disguise::ensure_max_tokens(req) {
        info!(
            "claude-oauth: client sent no max_tokens; substituting a default for {}",
            model
        );
    }

    Ok((serialize_payload(req)?, stream, model))
}

/// Serialize the disguised body, surfacing a failure instead of POSTing nothing.
///
/// Serializing a `Value` that we assembled ourselves shouldn't fail, but
/// `unwrap_or_default()` here would send an **empty body** upstream and surface as
/// a baffling Anthropic 400 about missing fields. A 500 naming the real cause is
/// worth the branch.
fn serialize_payload(req: &Value) -> Result<Vec<u8>, Response<ProxyBody>> {
    serde_json::to_vec(req).map_err(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to serialize the upstream request body: {e}"),
            "api_error",
        )
    })
}

/// Apply the header disguise to an outgoing request builder. `token` is the
/// Bearer credential; the client's own auth headers never reach upstream.
fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    token: &str,
    stream: bool,
    cfg: &ClaudeOAuthConfig,
    session: &str,
) -> reqwest::RequestBuilder {
    // Allowlist, not denylist: we send exactly this set, so a calling SDK can't
    // leak its own fingerprint (or an `x-api-key` that would outrank our Bearer).
    builder = builder
        .header("accept", if stream { "text/event-stream" } else { "application/json" })
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-dangerous-direct-browser-access", "true")
        .header("x-app", "cli")
        .header("user-agent", disguise::user_agent(cfg))
        .header("x-claude-code-session-id", session)
        .header("x-client-request-id", uuid::Uuid::new_v4().to_string())
        .header("anthropic-beta", disguise::beta_header(cfg));
    for (name, value) in disguise::STAINLESS_HEADERS {
        builder = builder.header(*name, *value);
    }
    builder
}

async fn handle(
    path: &str,
    body: Bytes,
    client: &reqwest::Client,
    cfg: &ClaudeOAuthConfig,
    client_headers: &HeaderMap,
) -> Response<ProxyBody> {
    let mut req: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "Invalid request body: expected a JSON object",
                "invalid_request_error",
            )
        }
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };

    let session = disguise::session_id(
        &req,
        client_headers
            .get("x-claude-code-session-id")
            .and_then(|v| v.to_str().ok()),
    );
    let count_tokens = path.ends_with("/count_tokens");
    let (payload, stream, model) = match build_payload(&mut req, cfg, &session, count_tokens) {
        Ok(built) => built,
        Err(resp) => return resp,
    };

    // `?beta=true` is what the CLI sends on this route.
    let url = format!("{UPSTREAM_BASE}{path}?beta=true");

    let mut token = match creds::ensure_fresh(cfg.write_back, None).await {
        Ok(t) => t,
        Err(e) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                &format!("Claude credential unavailable: {e}"),
                "authentication_error",
            )
        }
    };

    info!(
        "claude-oauth: {} -> api.anthropic.com model={} (stream={})",
        path, model, stream
    );

    // The upstream POST reuses the proxy's shared client on purpose: it keeps
    // `upstream_proxy` chaining (Proxyman) working, and api.anthropic.com isn't
    // us, so there's no loop. Only the *token refresh* needs `no_proxy()`.
    let dropped = disguise::dropped_client_betas(
        client_headers
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok()),
        cfg,
    );
    if !dropped.is_empty() {
        warn!(
            "claude-oauth: not forwarding client-requested beta(s) [{}] — Anthropic 400s on values \
             it doesn't recognize, so only `[claude_oauth] betas` is sent. Add them there if the \
             request needs them.",
            dropped.join(", ")
        );
    }

    let send = |token: &str| {
        apply_headers(client.post(&url), token, stream, cfg, &session)
            .body(payload.clone())
            .send()
    };

    let mut resp = match send(&token).await {
        Ok(r) => r,
        Err(e) => {
            warn!("claude-oauth: upstream request failed: {}", e);
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Upstream error: {e}"),
                "api_error",
            );
        }
    };

    // A 401 on a token we believed was fresh means the stored one was revoked or
    // rotated out from under us (the real CLI refreshing is enough to do it).
    // Retry once. Passing the *rejected* token is what lets `ensure_fresh` tell
    // "the store still holds the bad token, so refresh" apart from "someone
    // already replaced it, use theirs" — without it, a token another process
    // rotated in before this request even started would still cost a refresh.
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        info!("claude-oauth: upstream 401; refreshing the token and retrying once");
        // Bound the borrow of `token` to this statement so the arm below can
        // reassign it.
        let refreshed = creds::ensure_fresh(cfg.write_back, Some(&token)).await;
        match refreshed {
            Ok(fresh) => {
                token = fresh;
                match send(&token).await {
                    Ok(r) => resp = r,
                    Err(e) => {
                        return error_response(
                            StatusCode::BAD_GATEWAY,
                            &format!("Upstream error after token refresh: {e}"),
                            "api_error",
                        )
                    }
                }
            }
            Err(e) => {
                return error_response(
                    StatusCode::UNAUTHORIZED,
                    &format!("Token refresh after 401 failed: {e}"),
                    "authentication_error",
                )
            }
        }
    }

    let status = resp.status();
    let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !count_tokens {
        record_request_id(&session, resp.headers());
    }
    let passthrough_headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| forwardable_response_header(k.as_str()))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();

    if !status.is_success() {
        let upstream_is_json = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.to_ascii_lowercase().contains("json"));
        let raw = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Failed to read upstream error body: {e}"),
                    "api_error",
                )
            }
        };
        let text = String::from_utf8_lossy(&raw);
        warn!("claude-oauth: upstream {} for {}: {}", status, model, text);
        // The one failure mode specific to this surface: the credential is fine
        // but Anthropic didn't accept the request as Claude Code traffic.
        if (status.as_u16() == 401 || status.as_u16() == 403) && text.contains("Claude Code") {
            warn!(
                "claude-oauth: upstream rejected the request as non-Claude-Code traffic. The \
                 system-prompt disguise may be stale — check `[claude_oauth] cli_version` and that \
                 the identity block is still block 1 of `system`."
            );
        }
        if upstream_is_json {
            return json_with_headers(code, raw.to_vec(), &passthrough_headers);
        }
        return error_response(
            code,
            &format!("Upstream returned a non-JSON {} response", status.as_u16()),
            upstream_error_type(code),
        );
    }

    if stream {
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache");
        for (k, v) in &passthrough_headers {
            builder = builder.header(k, v);
        }
        return builder
            .body(stream_passthrough(resp))
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
    json_with_headers(code, raw.to_vec(), &passthrough_headers)
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

fn json_with_headers(
    status: StatusCode,
    body: Vec<u8>,
    headers: &[(String, String)],
) -> Response<ProxyBody> {
    let bytes = Bytes::from(body);
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    for (k, v) in headers {
        builder = builder.header(k, v);
    }
    builder
        .body(full_body(bytes.clone()))
        .unwrap_or_else(|_| Response::new(full_body(bytes)))
}

/// Anthropic error envelope: `{"type":"error","error":{"type":…,"message":…}}`.
fn error_response(status: StatusCode, message: &str, atype: &str) -> Response<ProxyBody> {
    warn!(
        "claude-oauth request failed [{} {}]: {}",
        status.as_u16(),
        atype,
        message
    );
    let body = json!({
        "type": "error",
        "error": { "type": atype, "message": message },
    });
    json_with_headers(status, body.to_string().into_bytes(), &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ClaudeOAuthConfig {
        ClaudeOAuthConfig::default()
    }

    fn body(model: &str) -> Vec<u8> {
        json!({"model": model, "messages": []}).to_string().into_bytes()
    }

    #[test]
    fn prefixed_model_routes_on_both_transports() {
        let map = HashMap::new();
        assert!(routes(&body("claude-oauth/claude-opus-5"), &cfg(), &map, false));
        assert!(routes(&body("claude-oauth/claude-opus-5"), &cfg(), &map, true));
    }

    #[test]
    fn unprefixed_model_never_routes_over_mitm() {
        // The safety crux: the real `claude` CLI must reach the real API.
        assert!(!routes(&body("claude-opus-5"), &cfg(), &HashMap::new(), false));
    }

    #[test]
    fn unprefixed_model_routes_on_origin_by_default() {
        assert!(routes(&body("claude-opus-5"), &cfg(), &HashMap::new(), true));
    }

    #[test]
    fn serve_unprefixed_false_requires_the_prefix_everywhere() {
        let mut c = cfg();
        c.serve_unprefixed = false;
        assert!(!routes(&body("claude-opus-5"), &c, &HashMap::new(), true));
        assert!(routes(&body("claude-oauth/claude-opus-5"), &c, &HashMap::new(), true));
    }

    #[test]
    fn gemini_traffic_is_never_stolen() {
        let map = HashMap::new();
        assert!(!routes(&body("gemini-cli/gemini-3.5-flash"), &cfg(), &map, true));
        // ...including a model redirected by [anthropic_model_map].
        let mut mapped = HashMap::new();
        mapped.insert("claude-sonnet-5".to_string(), "gemini-cli/x".to_string());
        assert!(!routes(&body("claude-sonnet-5"), &cfg(), &mapped, true));
    }

    #[test]
    fn missing_or_unparseable_model_does_not_route() {
        assert!(!routes(b"{}", &cfg(), &HashMap::new(), true));
        assert!(!routes(b"not json", &cfg(), &HashMap::new(), true));
        assert!(!routes(&body(""), &cfg(), &HashMap::new(), true));
    }

    #[test]
    fn payload_keeps_the_client_prompt_and_strips_the_prefix() {
        let mut req = json!({
            "model": "claude-oauth/claude-opus-5",
            "system": "You are a pirate.",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
        });
        let Ok((payload, stream, model)) = build_payload(&mut req, &cfg(), "sess-1", false) else {
            panic!("a hand-built body must serialize");
        };
        assert_eq!(model, "claude-opus-5");
        assert!(!stream);
        let sent: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(sent["model"], "claude-opus-5");
        let system = sent["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert_eq!(system[2]["text"], "You are a pirate.");
        assert_eq!(sent["max_tokens"], 100);
        // Cosmetic decoration belongs on generation requests.
        assert!(sent["metadata"]["user_id"].is_string());
        assert!(sent.get("diagnostics").is_some());
    }

    #[test]
    fn count_tokens_payload_is_trimmed_to_the_strict_schema() {
        // The endpoint 400s on anything outside its schema ("Extra inputs are not
        // permitted"), including fields the client itself supplied.
        let mut req = json!({
            "model": "claude-oauth/claude-opus-5",
            "system": "You are a pirate.",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
            "stream": true,
            "metadata": {"user_id": "mine"},
            "context_management": {"edits": []},
        });
        let Ok((payload, stream, model)) = build_payload(&mut req, &cfg(), "sess-1", true) else {
            panic!("a hand-built body must serialize");
        };
        assert_eq!(model, "claude-opus-5");
        assert!(!stream);
        let sent: Value = serde_json::from_slice(&payload).unwrap();
        for rejected in ["max_tokens", "stream", "metadata", "context_management", "diagnostics"] {
            assert!(sent.get(rejected).is_none(), "{rejected} should be stripped");
        }
        // ...but the auth-critical system disguise still applies.
        let system = sent["system"].as_array().unwrap();
        assert!(system[0]["text"].as_str().unwrap().starts_with(disguise::BILLING_PREFIX));
        assert_eq!(system[1]["text"], disguise::IDENTITY_CLI);
        assert_eq!(system[2]["text"], "You are a pirate.");
    }

    /// The previous-request map is fed by a client-chosen header on a daemon
    /// that runs for months, so it must not grow without bound.
    #[test]
    fn remembered_sessions_are_capped_and_oversized_ids_are_ignored() {
        let mut map = HashMap::new();
        for i in 0..MAX_REMEMBERED_SESSIONS {
            remember(&mut map, &format!("s{i}"), "req");
        }
        assert_eq!(map.len(), MAX_REMEMBERED_SESSIONS);
        // Updating a known session at the cap doesn't evict anyone.
        remember(&mut map, "s0", "req-2");
        assert_eq!(map.len(), MAX_REMEMBERED_SESSIONS);
        assert_eq!(map["s0"], "req-2");
        // A new session at the cap resets the map and is itself remembered.
        remember(&mut map, "fresh", "req-3");
        assert_eq!(map.len(), 1);
        assert_eq!(map["fresh"], "req-3");
        // A header-stuffed id is not stored at all.
        remember(&mut map, &"x".repeat(129), "req-4");
        assert_eq!(map.len(), 1);
    }
}
