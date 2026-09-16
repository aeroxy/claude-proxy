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
//! credential is refreshed or written until a request carries it (startup reads
//! the store once, read-only, to log which account is in play). A prefixed
//! request with nothing on disk is a 401 with the `login cline` hint.
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

/// True if `path` is Cline's own `/api/v1` mount — the one route among the
/// origin `/v1/chat/completions` surfaces that only this module serves.
pub fn is_cline_only_path(path: &str) -> bool {
    is_chat_completions_path(path) && !crate::openai::is_chat_completions_path(path)
}

/// The OpenAI-shaped refusal for a body on the Cline-only mount that
/// [`routes`] declined: nothing else serves that path, so say why here rather
/// than letting the request fall to the generic plain-HTTP 500.
pub fn unroutable_response(cfg: &ClineConfig) -> Response<ProxyBody> {
    error_response(
        StatusCode::NOT_FOUND,
        &format!(
            "/api/v1/chat/completions is served for Cline models only: use `{}/<model>`, or set \
             `[cline] serve_unprefixed = true` for bare model names",
            cfg.prefix
        ),
        "invalid_request_error",
    )
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
        return Some(method_not_allowed("POST"));
    }
    Some(handle(body, upstream_model, client, cfg, auth_dirs, client_headers).await)
}

/// True if `path` is the model listing an OpenAI client asks us for.
///
/// Only the bare `/v1` form. Cline's own `/api/v1/models` is a real upstream
/// route, and over MITM of `api.cline.bot` it has to keep reaching the real API
/// — claiming it here would answer the `cline` CLI's own catalog fetch with our
/// prefixed rewrite, which is exactly the hijack the routing gate exists to
/// prevent.
pub fn is_models_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    // One trailing slash is tolerated, so a client that normalizes to `/v1/models/`
    // gets this surface's 404/405 instead of the generic plain-HTTP 500, which reads
    // as proxy breakage. Deliberately *not* done for `is_chat_completions_path`:
    // that matcher is the MITM gate on `api.cline.bot`, so anything it accepts is
    // traffic taken from the real `cline` CLI. This route is origin-only — the
    // single call site is in the plain-HTTP branch — so there is nothing to widen.
    // Cline's own `/api/v1/models` stays excluded either way.
    path.strip_suffix('/').unwrap_or(path) == "/v1/models"
}

/// Serve `GET /v1/models` from Cline's catalog. `None` when the path isn't ours.
///
/// Gated on a Cline credential *existing*, not on config: this surface is always
/// on, so there is no `enabled` flag to read, and listing hundreds of models the
/// caller has no account to spend would be the misleading answer. The catalog
/// itself is public — we send no `Authorization`, so listing never refreshes the
/// credential and never touches the account.
pub async fn try_handle_models(
    method: &Method,
    path: &str,
    client: &reqwest::Client,
    cfg: &ClineConfig,
    auth_dirs: &[PathBuf],
) -> Option<Response<ProxyBody>> {
    if !is_models_path(path) {
        return None;
    }
    if method != Method::GET {
        return Some(method_not_allowed("GET"));
    }
    Some(list_models(client, cfg, auth_dirs).await)
}

async fn list_models(
    client: &reqwest::Client,
    cfg: &ClineConfig,
    auth_dirs: &[PathBuf],
) -> Response<ProxyBody> {
    // Existence, not freshness, and deliberately the raw store read rather than
    // the `MEMORY_TOKENS` overlay: the catalog below needs no credential at all,
    // so this asks only "does this machine have a Cline account?" before
    // offering a catalog whose ids all route to it. Nothing here is spent, so an
    // expired or even a mid-rotation credential is still a yes.
    if creds::load_blocking(cfg, auth_dirs).await.is_none() {
        // `info!`, not the `warn!` every other error on this surface gets: clients
        // poll model discovery on startup, so on a machine with no Cline login this
        // is the steady state, not an anomaly. A warning per client launch would
        // cost `warn!` its signal value.
        let message = "`GET /v1/models` lists Cline's catalog only, and no Cline credential \
                       was found: run `claude-proxy login cline`";
        info!("cline: models -> 404 (no credential on this machine)");
        return json_with_headers(
            StatusCode::NOT_FOUND,
            envelope(message, "invalid_request_error"),
            &[],
        );
    }

    // Any query the client sent is dropped, not forwarded: OpenAI's model listing
    // takes no parameters, so there is nothing to honor, and `is_models_path`
    // tolerates a query only so a client appending one still reaches us rather
    // than falling to the generic 500.
    let url = format!("{}/api/v1/models", cfg.base_url.trim_end_matches('/'));
    info!("cline: models -> {}", url);

    // Shared client on purpose, like the chat path: it keeps `upstream_proxy`
    // chaining working, and api.cline.bot isn't us, so there's no loop.
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Could not reach the Cline model catalog: {e}"),
                "api_error",
            )
        }
    };
    let code = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    // Captured before the body consumes `resp`. Same allowlist the chat path uses:
    // a 429 here is IP-based rather than per-account, but `retry-after` is exactly
    // what tells a throttled client when to come back, and dropping it turns one
    // 429 into a retry storm.
    let passthrough: Vec<(String, String)> = resp
        .headers()
        .iter()
        .filter(|(k, _)| forwardable_response_header(k.as_str()))
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();
    let raw = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Failed to read the Cline model catalog: {e}"),
                "api_error",
            )
        }
    };
    if !code.is_success() {
        return json_with_headers(code, reshape_error(&raw, code), &passthrough);
    }
    match prefix_model_ids(&raw, &cfg.prefix) {
        // Success is always 200, whatever 2xx the catalog answered with — unlike the
        // chat path, which preserves the upstream's per-request status. A listing has
        // one correct success code in the OpenAI contract, and the body we return is
        // ours (ids rewritten) rather than the upstream's verbatim.
        Some(body) => json_with_headers(StatusCode::OK, body, &passthrough),
        None => error_response(
            StatusCode::BAD_GATEWAY,
            "The Cline model catalog was not an OpenAI-shaped `data` list",
            "api_error",
        ),
    }
}

/// Rewrite every `id` in the catalog to `<prefix>/<id>`.
///
/// Cline names models as its own upstreams do (`anthropic/claude-haiku-4.5`).
/// Handed back verbatim, a client that picks one and POSTs it to
/// `/v1/chat/completions` misses this surface entirely: `serve_unprefixed` is
/// off by default, so the `[[openai]]` aggregator claims the bare name and 400s
/// on it. Prefixed, the round trip routes here in both modes.
fn prefix_model_ids(raw: &[u8], prefix: &str) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(raw).ok()?;
    let data = value.get_mut("data")?.as_array_mut()?;
    for model in data.iter_mut() {
        let Some(obj) = model.as_object_mut() else {
            continue;
        };
        let Some(id) = obj.get("id").and_then(Value::as_str) else {
            continue;
        };
        let prefixed = format!("{}/{}", prefix, id);
        obj.insert("id".to_string(), Value::String(prefixed));
    }
    serde_json::to_vec(&value).ok()
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

/// A 405 that names what the resource accepts, as RFC 9110 §15.5.6 requires.
///
/// Separate from [`error_response`] rather than folded into it: `Allow` is
/// meaningful only on a 405, and every other error on this surface goes through
/// that helper without wanting a header attached.
fn method_not_allowed(allow: &'static str) -> Response<ProxyBody> {
    let mut resp = error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        &format!("Only {allow} is supported"),
        "invalid_request_error",
    );
    resp.headers_mut().insert(
        hyper::header::ALLOW,
        hyper::header::HeaderValue::from_static(allow),
    );
    resp
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

    /// Only the `/api/v1` mount is ours alone; the bare `/v1` path is shared
    /// with the Gemini and `[[openai]]` surfaces and must not be claimed.
    #[test]
    fn only_the_api_v1_mount_is_cline_only() {
        assert!(is_cline_only_path("/api/v1/chat/completions"));
        assert!(is_cline_only_path("/api/v1/chat/completions?x=1"));
        assert!(!is_cline_only_path("/v1/chat/completions"));
        assert!(!is_cline_only_path("/v1/messages"));
    }

    /// A listed id must be one the client can send straight back to us. Cline
    /// names models the way its own upstreams do, and those bare names belong
    /// to the `[[openai]]` aggregator on this path, so an unprefixed listing
    /// would hand out ids that route away from the surface that served them.
    #[test]
    fn listed_ids_carry_the_prefix_that_routes_them_back_here() {
        let raw = json!({
            "object": "list",
            "data": [
                { "id": "anthropic/claude-haiku-4.5", "object": "model", "owned_by": "anthropic" },
                { "id": "~openai/gpt-astra-latest", "object": "model", "owned_by": "~openai" },
            ]
        })
        .to_string();

        let out = prefix_model_ids(raw.as_bytes(), "cline").expect("catalog rewritten");
        let value: Value = serde_json::from_slice(&out).unwrap();
        let ids: Vec<&str> = value["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "cline/anthropic/claude-haiku-4.5",
                "cline/~openai/gpt-astra-latest"
            ]
        );
        // Everything else about each entry survives untouched.
        assert_eq!(value["data"][0]["owned_by"], "anthropic");
        assert_eq!(value["object"], "list");

        // And the round trip holds: a listed id routes back to this surface.
        assert_eq!(
            routes(&body(ids[0]), &cfg(), &[], false),
            Some("anthropic/claude-haiku-4.5".to_string())
        );

        // The prefix is applied unconditionally, and that is what keeps the round
        // trip exact even for an upstream id that already begins with it: `routes`
        // strips exactly one layer, so the id Cline gets back is the one it named.
        // Skipping already-prefixed ids would send `foo` upstream for `cline/foo`.
        let collide = json!({ "data": [{ "id": "cline/foo" }] }).to_string();
        let out = prefix_model_ids(collide.as_bytes(), "cline").unwrap();
        let listed = serde_json::from_slice::<Value>(&out).unwrap()["data"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(listed, "cline/cline/foo");
        assert_eq!(
            routes(&body(&listed), &cfg(), &[], false),
            Some("cline/foo".to_string()),
            "what Cline gets back must be the id Cline listed"
        );
    }

    #[test]
    fn a_catalog_that_isnt_an_openai_list_is_refused_rather_than_mangled() {
        assert!(prefix_model_ids(b"not json at all", "cline").is_none());
        assert!(prefix_model_ids(br#"{"models":[]}"#, "cline").is_none());
        assert!(prefix_model_ids(br#"{"data":{}}"#, "cline").is_none());
        // An entry with no `id` is skipped, not fatal.
        let out = prefix_model_ids(br#"{"data":[{"object":"model"}]}"#, "cline").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"data":[{"object":"model"}]}"#
        );
    }

    /// The two answers a caller sees before any catalog is fetched. Both are the
    /// documented contract for a machine with no Cline account, so neither should
    /// be able to change without a test noticing.
    #[tokio::test]
    async fn the_listing_refuses_a_non_get_before_doing_any_work() {
        let resp = try_handle_models(
            &Method::POST,
            "/v1/models",
            &reqwest::Client::new(),
            &cfg(),
            &[],
        )
        .await
        .expect("the path is ours, so this is served, not declined");
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            resp.headers()
                .get(hyper::header::ALLOW)
                .map(|v| v.as_bytes()),
            Some(&b"GET"[..]),
            "a 405 must name the methods the resource accepts"
        );
    }

    /// The chat path's own method gate, which shares the 405 helper. Asserted
    /// separately because the two surfaces accept opposite methods, and a helper
    /// that hardcoded either one would still pass the other surface's test.
    #[tokio::test]
    async fn the_chat_path_refuses_a_non_post_and_says_so() {
        let resp = try_handle(
            &Method::GET,
            "/v1/chat/completions",
            Bytes::new(),
            "anthropic/claude-haiku-4.5",
            &reqwest::Client::new(),
            &cfg(),
            &[],
            &HeaderMap::new(),
        )
        .await
        .expect("the path is ours, so this is served, not declined");
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            resp.headers()
                .get(hyper::header::ALLOW)
                .map(|v| v.as_bytes()),
            Some(&b"POST"[..]),
            "a 405 must name the methods the resource accepts"
        );
    }

    #[tokio::test]
    async fn no_credential_is_a_404_rather_than_an_empty_list() {
        // Hermetic: a `settings_path` that doesn't exist short-circuits the real
        // CLI store lookup (no `~/.cline` fallback), and empty `auth_dirs` leaves
        // nothing of our own — so this never reads the developer's own login, and
        // returns before the catalog fetch, so it never reaches the network.
        let cfg = ClineConfig {
            settings_path: Some(PathBuf::from("/nonexistent/claude-proxy-test/providers.json")),
            ..ClineConfig::default()
        };
        let resp = try_handle_models(
            &Method::GET,
            "/v1/models",
            &reqwest::Client::new(),
            &cfg,
            &[],
        )
        .await
        .expect("the path is ours");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Only the bare `/v1` form. Cline mounts its own catalog at
    /// `/api/v1/models`, and over MITM that request is the real `cline` CLI
    /// fetching its model list — claiming it would feed the CLI our prefixed
    /// rewrite of its own names.
    #[test]
    fn the_models_route_never_claims_clines_own_mount() {
        assert!(is_models_path("/v1/models"));
        assert!(is_models_path("/v1/models?limit=10"));
        // A normalizing client's trailing slash reaches us rather than the
        // generic 500; the `/api/v1` exclusion survives it.
        assert!(is_models_path("/v1/models/"));
        assert!(is_models_path("/v1/models/?limit=10"));
        assert!(!is_models_path("/api/v1/models/"));
        assert!(!is_models_path("/v1/models//"));
        assert!(!is_models_path("/api/v1/models"));
        assert!(!is_models_path("/v1beta/models"));
        assert!(!is_models_path("/v1/models/gpt-5"));
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
