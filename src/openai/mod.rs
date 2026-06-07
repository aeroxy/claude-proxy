//! OpenAI Chat Completions aggregator (`POST /v1/chat/completions`).
//!
//! Unlike the Gemini/Anthropic surfaces, this does **no format translation** —
//! OpenAI in, OpenAI out. Its only job is *aggregation*: route one endpoint to
//! many OpenAI-compatible backends, picked by a provider prefix on the model.
//!
//! Routing: the model is split on the **first** `/`. The head is one of our
//! configured `[[openai]]` provider names; the remainder is forwarded verbatim
//! as the upstream `model` (so `opengateway/minimax/minimax-m3` → provider
//! `opengateway`, upstream model `minimax/minimax-m3`). The prefix set is
//! dynamic (config-derived), which is why this can't reuse gemini's fixed-prefix
//! `split_model`.
//!
//! Origin mode only: served from the plain-HTTP branch of the proxy when a
//! client points `OPENAI_BASE_URL` at us. There is no MITM gate.
//!
//! Entry point: [`try_handle`]. Returns `None` when the path isn't ours so the
//! caller falls through.

use std::sync::Arc;

use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use hyper::{Method, Response, StatusCode};
use serde_json::{json, Value};
use tokio_stream::StreamExt;
use tracing::{info, warn};

use crate::config::OpenAIProvider;
use crate::proxy::{full_body, ProxyBody};

/// True if `path` is the Chat Completions route we serve.
pub fn is_chat_completions_path(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    path == "/v1/chat/completions"
}

/// Split a requested model into `(provider, upstream_model)` by its first
/// `/`-segment. The head must match a configured provider `name`; the remainder
/// is the upstream model. Returns `None` when there's no `/` or no match.
pub fn split_model<'m, 'p>(
    model: &'m str,
    providers: &'p [OpenAIProvider],
) -> Option<(&'p OpenAIProvider, &'m str)> {
    let (head, rest) = model.split_once('/')?;
    if rest.is_empty() {
        return None;
    }
    let provider = providers.iter().find(|p| p.name.trim() == head)?;
    Some((provider, rest))
}

/// Handle an OpenAI Chat Completions request. Returns `None` if the path isn't ours.
pub async fn try_handle(
    method: &Method,
    path: &str,
    body: Bytes,
    client: &reqwest::Client,
    providers: &Arc<Vec<OpenAIProvider>>,
    incoming_auth: Option<&str>,
) -> Option<Response<ProxyBody>> {
    if !is_chat_completions_path(path) {
        return None;
    }

    info!("OpenAI API request: {} {}", method, path);

    if method != Method::POST {
        return Some(error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "Only POST is supported",
            "invalid_request_error",
        ));
    }

    Some(handle_chat(body, client, providers, incoming_auth).await)
}

async fn handle_chat(
    body: Bytes,
    client: &reqwest::Client,
    providers: &[OpenAIProvider],
    incoming_auth: Option<&str>,
) -> Response<ProxyBody> {
    let mut req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            )
        }
    };

    let stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    // Resolve routing in a scope so the immutable borrow of `req` (via `model`)
    // is released before we rewrite `req["model"]`. `provider` borrows
    // `providers`, not `req`, so it survives the block.
    let (provider, upstream_model) = {
        let model_full = req.get("model").and_then(|m| m.as_str()).unwrap_or("");
        match split_model(model_full, providers) {
            Some((p, rest)) => (p, rest.to_string()),
            None => {
                return error_response(
                    StatusCode::NOT_FOUND,
                    &format!(
                        "Model must be prefixed with a configured `[[openai]]` provider, e.g. `opengateway/minimax/minimax-m3` (got `{model_full}`)."
                    ),
                    "not_found_error",
                )
            }
        }
    };

    // Rewrite `model` to the bare upstream name (strip our provider prefix).
    req["model"] = json!(upstream_model);
    let payload = serde_json::to_vec(&req).unwrap_or_default();

    let url = format!("{}/chat/completions", provider.base_url.trim_end_matches('/'));

    info!(
        "OpenAI chat -> provider={} model={} (stream={})",
        provider.name, upstream_model, stream
    );

    let mut builder = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header(
            "Accept",
            if stream { "text/event-stream" } else { "application/json" },
        );

    // Config `api_key` wins; otherwise forward the client's own Authorization.
    if let Some(key) = &provider.api_key {
        builder = builder.header("Authorization", format!("Bearer {key}"));
    } else if let Some(auth) = incoming_auth {
        builder = builder.header("Authorization", auth);
    }
    for (k, v) in &provider.headers {
        builder = builder.header(k, v);
    }

    let resp = match builder.body(payload).send().await {
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
    let code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);

    if !status.is_success() {
        // Already OpenAI-shaped — pass the upstream error through verbatim.
        let raw = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Failed to read upstream error response body: {e}"),
                    "api_error",
                );
            }
        };
        warn!("openai: upstream {} for {}: {}", status, upstream_model, String::from_utf8_lossy(&raw));
        return json_response(code, raw.to_vec());
    }

    if stream {
        let body = stream_passthrough(resp);
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
    json_response(code, raw.to_vec())
}

/// Disconnect-safe raw-byte passthrough: stream the upstream `reqwest::Response`
/// body into a [`ProxyBody`] unchanged. Mirrors the `biased`
/// `tokio::select!`-on-`tx.closed()` contract of [`crate::gemini`]'s `stream_sse`
/// so a client disconnect promptly tears down the upstream connection — but
/// forwards raw bytes (not line-reframed) so OpenAI SSE framing is byte-exact.
fn stream_passthrough(resp: reqwest::Response) -> ProxyBody {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(16);

    tokio::spawn(async move {
        let mut upstream = Box::pin(resp.bytes_stream());
        loop {
            let chunk = tokio::select! {
                biased;
                _ = tx.closed() => return, // client gone — drop upstream + channel
                next = upstream.next() => next,
            };
            match chunk {
                Some(Ok(c)) => {
                    if tx.send(Ok(Frame::data(c))).await.is_err() {
                        return; // client gone
                    }
                }
                Some(Err(e)) => {
                    warn!("openai stream: upstream read error: {}", e);
                    // Surface as a broken body rather than a clean EOF.
                    let _ = tx.send(Err(std::io::Error::other(e))).await;
                    return;
                }
                None => break, // upstream finished
            }
        }
    });

    StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx)).boxed()
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
    warn!("OpenAI request failed [{} {}]: {}", status.as_u16(), etype, message);
    let body = json!({
        "error": { "message": message, "type": etype, "code": null },
    });
    json_response(status, body.to_string().into_bytes())
}
