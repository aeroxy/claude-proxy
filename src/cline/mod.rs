//! Cline as a built-in provider: serve `POST /v1/chat/completions` against
//! Cline's own API (`api.cline.bot`) using a Cline account credential.
//!
//! This is a near-pure pipe, like [`crate::openai`] and [`crate::claude_oauth`]
//! and unlike the Gemini surfaces — OpenAI in, OpenAI out, no format
//! translation. Three things stand between the client and a verbatim forward:
//!
//! 1. **The credential.** Cline wants `Authorization: Bearer workos:<jwt>`, on a
//!    token that lives about an hour. [`creds`] owns discovery and refresh.
//! 2. **The identity headers.** Cline's API is addressed by its own clients, so
//!    we send the exact header set a real `cline` CLI sends.
//! 3. **The envelope.** A *non-streaming* success comes back wrapped as
//!    `{"data":{…},"success":true}`, which no OpenAI SDK can parse; we unwrap
//!    `data`. Streaming frames are **not** wrapped (measured against
//!    `anthropic/claude-haiku-4.5`: plain `data: {chunk}` lines terminated by
//!    `data: [DONE]`), so the stream is a byte passthrough like every other
//!    surface here. Errors arrive as `{"error":"<string>","success":false}` —
//!    a bare string where SDKs expect an object — so those are reshaped too.
//!
//! Always on, like the Gemini providers: the prefix is the consent, and no
//! credential is read, refreshed or written until a request carries it. A
//! prefixed request with nothing on disk is a 401 with the `login cline` hint.
//!
//! Routing, by transport:
//!
//! - **Origin** (`OPENAI_BASE_URL=http://127.0.0.1:7777`): `cline/<model>`
//!   routes here, and with the `serve_unprefixed = true` opt-in so do bare
//!   model names — but only ones the `[[openai]]` aggregator wouldn't claim, so
//!   this surface can't move existing traffic.
//! - **MITM** of `api.cline.bot`: **only** the explicit `cline/` prefix routes
//!   here. `HTTPS_PROXY` points every client on the machine at us, so the real
//!   `cline` CLI's traffic already passes through this proxy; claiming that host
//!   blindly would hijack the user's own CLI. Same safety crux as
//!   [`crate::claude_oauth`]'s gate, and `routes` is unit-tested on it.

pub mod creds;

use std::path::PathBuf;

use hyper::body::Bytes;
use hyper::{HeaderMap, Method, Response, StatusCode};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::config::{ClineConfig, OpenAIProvider};
use crate::proxy::{full_body, stream_passthrough, ProxyBody};

/// The host we gate MITM interception on.
pub const CLINE_UPSTREAM_HOST: &str = "api.cline.bot";

/// `X-CLIENT-TYPE` / `X-PLATFORM` a real `cline` CLI sends (`cline-<source>`
/// with `source = cli`; see `resolveProviderRequestHeaders` in the cline SDK).
const CLIENT_TYPE: &str = "cline-cli";
const PLATFORM: &str = "cli";

lazy_static::lazy_static! {
    /// `X-Task-ID` for requests that don't carry one. Cline uses it to group a
    /// task's calls, so one value per proxy process is the honest answer: we
    /// have no task boundary to observe. A client that tracks its own sessions
    /// can send the header and we forward it.
    static ref SESSION_ID: String = uuid::Uuid::new_v4().to_string();
}

/// True if `path` is a Chat Completions route we serve. Cline's own API mounts
/// it under `/api/v1`; an OpenAI client pointed at us uses the bare `/v1` form.
pub fn is_chat_completions_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    matches!(path, "/v1/chat/completions" | "/api/v1/chat/completions")
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

/// Whether this request belongs to us, and the upstream model to send.
///
/// `allow_unprefixed` is the transport difference: `true` on the origin branch
/// (where `serve_unprefixed` decides), always `false` over MITM so the real
/// `cline` CLI's own traffic reaches its own API untouched.
///
/// A model the `[[openai]]` aggregator would claim is never taken here, so
/// adding this surface can't change where existing traffic goes.
pub fn routes(
    body: &[u8],
    cfg: &ClineConfig,
    openai_providers: &[OpenAIProvider],
    allow_unprefixed: bool,
) -> Option<String> {
    let model = model_of(body);
    if model.is_empty() {
        return None;
    }
    if let Some(rest) = model.strip_prefix(&format!("{}/", cfg.prefix)) {
        return (!rest.is_empty()).then(|| rest.to_string());
    }
    if !allow_unprefixed || !cfg.serve_unprefixed {
        return None;
    }
    crate::openai::split_model(&model, openai_providers)
        .is_none()
        .then_some(model)
}

/// Handle a Chat Completions request against Cline. Returns `None` only when the
/// path isn't ours — routing is the caller's gate ([`routes`]).
pub async fn try_handle(
    method: &Method,
    path: &str,
    body: Bytes,
    upstream_model: &str,
    client: &reqwest::Client,
    cfg: &ClineConfig,
    auth_dirs: &[PathBuf],
    client_headers: &HeaderMap,
) -> Option<Response<ProxyBody>> {
    if !is_chat_completions_path(path) {
        return None;
    }
    if method != Method::POST {
        return Some(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only POST is supported",
            "invalid_request_error",
        ));
    }
    Some(handle(body, upstream_model, client, cfg, auth_dirs, client_headers).await)
}

/// Apply the client-identity headers a real `cline` CLI sends, plus the bearer.
///
/// An allowlist, not a passthrough: we send exactly this set so a calling SDK
/// can't leak its own fingerprint, or an `Authorization` that would outrank ours.
fn apply_headers(
    builder: reqwest::RequestBuilder,
    token: &str,
    stream: bool,
    cfg: &ClineConfig,
    task_id: &str,
) -> reqwest::RequestBuilder {
    builder
        .header("accept", if stream { "text/event-stream" } else { "application/json" })
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", creds::bearer(token)))
        .header("HTTP-Referer", "https://cline.bot")
        .header("X-Title", "Cline")
        .header("User-Agent", format!("Cline/{}", cfg.client_version))
        .header("X-CLIENT-TYPE", CLIENT_TYPE)
        .header("X-CLIENT-VERSION", &cfg.client_version)
        .header("X-PLATFORM", PLATFORM)
        .header("X-PLATFORM-VERSION", &cfg.client_version)
        .header("X-CORE-VERSION", &cfg.core_version)
        .header("X-IS-MULTIROOT", "false")
        .header("X-Task-ID", task_id)
}

/// Response headers worth returning: rate-limit state (SDKs back off on it), the
/// upstream request id (support/debugging), and `retry-after`. This surface owns
/// a credential it can be throttled on, so unlike [`crate::openai`] it doesn't
/// drop them all. Framing headers are never forwarded — they'd contradict the
/// body we re-frame.
fn forwardable_response_header(name: &str) -> bool {
    name.starts_with("x-ratelimit-") || matches!(name, "retry-after" | "x-request-id")
}

/// The upstream body and whether it asks for a stream.
///
/// Strip our prefix; everything else is forwarded as the client wrote it —
/// except an *absent* `stream`, which is pinned to `false`. The two contracts
/// disagree on the default (OpenAI: false; Cline's API: true), and this is an
/// OpenAI surface, so an SDK that omits the field must get one JSON object, not
/// an event stream.
fn shape_request(body: &[u8], upstream_model: &str) -> Result<(Vec<u8>, bool), String> {
    let mut req = match serde_json::from_slice::<Value>(body) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => return Err("Invalid request body: expected a JSON object".to_string()),
        Err(e) => return Err(format!("Invalid JSON: {e}")),
    };
    req["model"] = json!(upstream_model);
    let stream = match req.get("stream").and_then(|s| s.as_bool()) {
        Some(s) => s,
        None => {
            req["stream"] = json!(false);
            false
        }
    };
    let payload = serde_json::to_vec(&req)
        .map_err(|e| format!("Failed to serialize the upstream request body: {e}"))?;
    Ok((payload, stream))
}

async fn handle(
    body: Bytes,
    upstream_model: &str,
    client: &reqwest::Client,
    cfg: &ClineConfig,
    auth_dirs: &[PathBuf],
    client_headers: &HeaderMap,
) -> Response<ProxyBody> {
    let (payload, stream) = match shape_request(&body, upstream_model) {
        Ok(shaped) => shaped,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, &msg, "invalid_request_error"),
    };

    let task_id = client_headers
        .get("x-task-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(SESSION_ID.as_str())
        .to_string();

    let mut token = match creds::ensure_fresh(cfg, auth_dirs, None).await {
        Ok(t) => t,
        Err(e) => {
            return error_response(
                StatusCode::UNAUTHORIZED,
                &format!("Cline credential unavailable: {e}"),
                "authentication_error",
            )
        }
    };

    let url = format!(
        "{}/api/v1/chat/completions",
        cfg.base_url.trim_end_matches('/')
    );
    info!(
        "cline: chat -> {} model={} (stream={})",
        cfg.base_url, upstream_model, stream
    );

    // The upstream POST reuses the proxy's shared client on purpose: it keeps
    // `upstream_proxy` chaining (Proxyman) working, and api.cline.bot isn't us,
    // so there's no loop. Only the *token refresh* needs `no_proxy()`.
    let send = |token: &str| {
        apply_headers(client.post(&url), token, stream, cfg, &task_id)
            .body(payload.clone())
            .send()
    };

    let mut resp = match send(&token).await {
        Ok(r) => r,
        Err(e) => {
            warn!("cline: upstream request failed: {}", e);
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Upstream error: {e}"),
                "api_error",
            );
        }
    };

    // A 401 on a token we believed was fresh means the stored one was revoked or
    // rotated out from under us (the real `cline` CLI refreshing is enough to do
    // it). Retry once. Passing the *rejected* token lets `ensure_fresh` tell
    // "the store still holds the bad token, so refresh" apart from "someone
    // already replaced it, use theirs".
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        info!("cline: upstream 401; refreshing the token and retrying once");
        match creds::ensure_fresh(cfg, auth_dirs, Some(&token)).await {
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
    let passthrough: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| forwardable_response_header(k.as_str()))
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string())))
        .collect();
    // Branch on the upstream's own framing, not on `stream`: we pin the field
    // above, but Cline's default is the opposite of OpenAI's, and if the two
    // ever disagree the cost is handing a JSON parser an event stream.
    let upstream_is_sse = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/event-stream"));

    if status.is_success() && upstream_is_sse {
        // Measured: Cline's SSE frames are plain OpenAI chunks, not wrapped in
        // the `data`/`success` envelope its JSON responses use. Nothing to
        // rewrite, so this is the same raw byte pump every other surface uses.
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache");
        for (k, v) in &passthrough {
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
                &format!("Failed to read the upstream response body: {e}"),
                "api_error",
            )
        }
    };

    if !status.is_success() {
        warn!(
            "cline: upstream {} for {}: {}",
            status,
            upstream_model,
            String::from_utf8_lossy(&raw)
        );
        return json_with_headers(code, reshape_error(&raw, code), &passthrough);
    }

    json_with_headers(code, unwrap_envelope(&raw), &passthrough)
}

/// Unwrap Cline's `{"data":{…},"success":true}` success envelope.
///
/// Defensive on both sides: a body that *isn't* wrapped is returned untouched
/// (so this keeps working if Cline ever drops the envelope), and a wrapped body
/// whose `data` isn't an object is left alone rather than replaced with
/// something an SDK would choke on differently.
fn unwrap_envelope(raw: &[u8]) -> Vec<u8> {
    let Ok(value) = serde_json::from_slice::<Value>(raw) else {
        return raw.to_vec();
    };
    match value.get("data") {
        Some(data) if data.is_object() && value.get("success").is_some() => {
            serde_json::to_vec(data).unwrap_or_else(|_| raw.to_vec())
        }
        _ => raw.to_vec(),
    }
}

/// Reshape a Cline error into the OpenAI envelope SDKs parse.
///
/// Cline returns `{"error":"empty response content","success":false}` — `error`
/// is a bare **string**, where the OpenAI shape is an object with `message` /
/// `type` / `code`. A client SDK reads `error.message` off that string and gets
/// `undefined`, so the real cause vanishes. An already-object `error` is passed
/// through untouched (some upstreams behind Cline are OpenAI-shaped already).
fn reshape_error(raw: &[u8], status: StatusCode) -> Vec<u8> {
    let value: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        // Not JSON at all — a CDN's HTML 502, say. Labeling that
        // `application/json` is what makes SDKs crash, so wrap it.
        Err(_) => {
            return envelope(
                &format!(
                    "Cline returned a non-JSON {} response: {}",
                    status.as_u16(),
                    String::from_utf8_lossy(raw).chars().take(200).collect::<String>()
                ),
                upstream_error_type(status),
            )
        }
    };
    match value.get("error") {
        Some(Value::Object(_)) => raw.to_vec(),
        Some(Value::String(msg)) => envelope(msg, upstream_error_type(status)),
        _ => envelope(
            &format!("Cline returned {} with no error message", status.as_u16()),
            upstream_error_type(status),
        ),
    }
}

/// Map an HTTP status onto the OpenAI error `type` an SDK branches on.
fn upstream_error_type(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 | 404 | 422 => "invalid_request_error",
        401 | 403 => "authentication_error",
        429 => "rate_limit_error",
        _ => "api_error",
    }
}

fn envelope(message: &str, etype: &str) -> Vec<u8> {
    json!({ "error": { "message": message, "type": etype, "code": null } })
        .to_string()
        .into_bytes()
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

fn error_response(status: StatusCode, message: &str, etype: &str) -> Response<ProxyBody> {
    warn!("cline request failed [{} {}]: {}", status.as_u16(), etype, message);
    json_with_headers(status, envelope(message, etype), &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ClineConfig {
        ClineConfig::default()
    }

    fn body(model: &str) -> Vec<u8> {
        json!({ "model": model, "messages": [] }).to_string().into_bytes()
    }

    fn aggregator(name: &str) -> Vec<OpenAIProvider> {
        vec![OpenAIProvider {
            name: name.to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            ..Default::default()
        }]
    }

    /// The hijack risk, asserted directly: `HTTPS_PROXY` sends the real `cline`
    /// CLI's traffic through this proxy, and over MITM only an explicit
    /// `cline/` prefix may route to us. Everything the CLI actually sends —
    /// bare `provider/model` names — must fall through to its own API.
    #[test]
    fn mitm_never_claims_the_real_cline_clis_traffic() {
        let cfg = cfg();
        for model in ["anthropic/claude-haiku-4.5", "z-ai/glm-5.3-flash", "gpt-5"] {
            assert_eq!(
                routes(&body(model), &cfg, &[], false),
                None,
                "unprefixed model {model} must never route to us over MITM"
            );
        }
        // Even with serve_unprefixed on — that flag is an origin-branch knob and
        // must not widen the MITM gate.
        let permissive = ClineConfig { serve_unprefixed: true, ..cfg };
        assert_eq!(
            routes(&body("anthropic/claude-haiku-4.5"), &permissive, &[], false),
            None
        );
    }

    #[test]
    fn the_prefix_routes_and_is_stripped_on_both_transports() {
        let cfg = cfg();
        for allow_unprefixed in [false, true] {
            assert_eq!(
                routes(&body("cline/anthropic/claude-haiku-4.5"), &cfg, &[], allow_unprefixed),
                Some("anthropic/claude-haiku-4.5".to_string())
            );
        }
        // A bare prefix names no model.
        assert_eq!(routes(&body("cline/"), &cfg, &[], true), None);
        assert_eq!(routes(&body(""), &cfg, &[], true), None);
        assert_eq!(routes(b"not json", &cfg, &[], true), None);
    }

    #[test]
    fn unprefixed_origin_traffic_never_steals_from_the_openai_aggregator() {
        let cfg = ClineConfig { serve_unprefixed: true, ..cfg() };
        let providers = aggregator("anthropic");
        // `anthropic` is a configured `[[openai]]` provider, so it stays theirs.
        assert_eq!(routes(&body("anthropic/claude-haiku-4.5"), &cfg, &providers, true), None);
        // Nothing claims this one, so we serve it.
        assert_eq!(
            routes(&body("z-ai/glm-5.3-flash"), &cfg, &providers, true),
            Some("z-ai/glm-5.3-flash".to_string())
        );
        // ...and the explicit prefix still wins over the aggregator's name.
        assert_eq!(
            routes(&body("cline/anthropic/claude-haiku-4.5"), &cfg, &providers, true),
            Some("anthropic/claude-haiku-4.5".to_string())
        );
    }

    #[test]
    fn serve_unprefixed_off_means_prefix_only_on_origin_too() {
        let cfg = ClineConfig { serve_unprefixed: false, ..cfg() };
        assert_eq!(routes(&body("z-ai/glm-5.3-flash"), &cfg, &[], true), None);
        assert_eq!(
            routes(&body("cline/z-ai/glm-5.3-flash"), &cfg, &[], true),
            Some("z-ai/glm-5.3-flash".to_string())
        );
    }

    #[test]
    fn serves_both_the_bare_and_cline_mounted_paths() {
        assert!(is_chat_completions_path("/v1/chat/completions"));
        assert!(is_chat_completions_path("/api/v1/chat/completions"));
        assert!(is_chat_completions_path("/v1/chat/completions?x=1"));
        assert!(!is_chat_completions_path("/v1/messages"));
        assert!(!is_chat_completions_path("/v1/chat/completions/extra"));
    }

    /// Cline defaults `stream` to true; OpenAI defaults it to false. This is an
    /// OpenAI surface, so an omitted `stream` must reach Cline as `false`, while
    /// an explicit value — and everything else — is forwarded as written.
    #[test]
    fn an_absent_stream_is_pinned_to_false_and_explicit_values_are_kept() {
        let (payload, stream) = shape_request(&body("cline/z-ai/glm-5.3-flash"), "z-ai/glm-5.3-flash").unwrap();
        let sent: Value = serde_json::from_slice(&payload).unwrap();
        assert!(!stream);
        assert_eq!(sent["stream"], false, "absent `stream` is pinned, not left to Cline's default");
        assert_eq!(sent["model"], "z-ai/glm-5.3-flash", "our prefix is stripped");
        assert_eq!(sent["messages"], json!([]), "the rest is forwarded as written");

        let raw = json!({ "model": "cline/x", "stream": true, "max_tokens": 8 }).to_string();
        let (payload, stream) = shape_request(raw.as_bytes(), "x").unwrap();
        let sent: Value = serde_json::from_slice(&payload).unwrap();
        assert!(stream);
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["max_tokens"], 8);

        assert!(shape_request(b"[]", "x").is_err(), "a non-object body is rejected");
        assert!(shape_request(b"not json", "x").is_err());
    }

    #[test]
    fn unwraps_the_success_envelope() {
        // The shape measured against api.cline.bot, trimmed.
        let raw = br#"{"data":{"id":"gen-1","object":"chat.completion",
            "choices":[{"index":0,"message":{"role":"assistant","content":"pong"},
            "finish_reason":"stop"}],"usage":{"completion_tokens":5}},"success":true}"#;
        let out: Value = serde_json::from_slice(&unwrap_envelope(raw)).unwrap();
        assert_eq!(out["choices"][0]["message"]["content"], "pong");
        assert_eq!(out["usage"]["completion_tokens"], 5);
        assert!(out.get("data").is_none(), "the envelope is gone");
    }

    #[test]
    fn an_unwrapped_body_is_left_alone() {
        let raw = br#"{"id":"gen-1","choices":[]}"#;
        assert_eq!(unwrap_envelope(raw), raw.to_vec());
        // `data` that isn't the envelope (no `success` sibling) stays put.
        let raw = br#"{"data":{"x":1}}"#;
        assert_eq!(unwrap_envelope(raw), raw.to_vec());
        assert_eq!(unwrap_envelope(b"not json"), b"not json".to_vec());
    }

    #[test]
    fn reshapes_clines_bare_string_error_into_the_openai_envelope() {
        // Measured: `z-ai/glm-5.3-flash` returns this at 500.
        let raw = br#"{"error":"empty response content","success":false}"#;
        let out: Value =
            serde_json::from_slice(&reshape_error(raw, StatusCode::INTERNAL_SERVER_ERROR)).unwrap();
        assert_eq!(out["error"]["message"], "empty response content");
        assert_eq!(out["error"]["type"], "api_error");
        assert!(out["error"]["code"].is_null());

        let raw = br#"{"error":"model not found","success":false}"#;
        let out: Value = serde_json::from_slice(&reshape_error(raw, StatusCode::NOT_FOUND)).unwrap();
        assert_eq!(out["error"]["message"], "model not found");
        assert_eq!(out["error"]["type"], "invalid_request_error");

        // Measured 401 — no `success` key at all.
        let raw = br#"{"error":"Unauthorized: re-authenticate your Cline account."}"#;
        let out: Value =
            serde_json::from_slice(&reshape_error(raw, StatusCode::UNAUTHORIZED)).unwrap();
        assert_eq!(out["error"]["type"], "authentication_error");
        assert!(out["error"]["message"].as_str().unwrap().starts_with("Unauthorized"));
    }

    #[test]
    fn an_already_openai_shaped_error_passes_through_untouched() {
        let raw = br#"{"error":{"message":"nope","type":"invalid_request_error","code":"x"}}"#;
        assert_eq!(reshape_error(raw, StatusCode::BAD_REQUEST), raw.to_vec());
    }

    #[test]
    fn a_non_json_error_body_still_yields_json() {
        let out: Value = serde_json::from_slice(&reshape_error(
            b"<html>502 Bad Gateway</html>",
            StatusCode::BAD_GATEWAY,
        ))
        .unwrap();
        assert_eq!(out["error"]["type"], "api_error");
        assert!(out["error"]["message"].as_str().unwrap().contains("non-JSON 502"));
    }
}
