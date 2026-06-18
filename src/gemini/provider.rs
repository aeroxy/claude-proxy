//! Upstream calls to the Cloud Code Assist endpoint for both providers.
//! Antigravity uses `daily-cloudcode-pa`; gemini-cli uses `cloudcode-pa`.
//! Headers (User-Agent / api-client) also differ per provider.
//! Builds the SSE streaming body for `:streamGenerateContent`.

use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Bytes, Frame};
use tokio_stream::StreamExt;
use tracing::warn;

use super::models::{ANTIGRAVITY, GEMINI_CLI};
use super::translate;
use crate::proxy::ProxyBody;

pub const CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub const CODE_ASSIST_DAILY_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const CODE_ASSIST_VERSION: &str = "v1internal";

const GEMINI_CLI_VERSION: &str = "0.47.0";
const GEMINI_CLI_API_CLIENT: &str = "gl-node/25.8.2";

/// `{base}/v1internal:{action}` (+ `?alt=sse` when streaming).
/// Antigravity uses the daily endpoint; gemini-cli uses the standard endpoint.
pub fn build_url(provider: &str, action: &str, stream: bool) -> String {
    let base = match provider {
        ANTIGRAVITY => CODE_ASSIST_DAILY_ENDPOINT,
        _ => CODE_ASSIST_ENDPOINT,
    };
    let mut url = format!("{base}/{CODE_ASSIST_VERSION}:{action}");
    if stream {
        url.push_str("?alt=sse");
    }
    url
}

fn node_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

fn node_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "x86",
        other => other,
    }
}

fn gemini_cli_user_agent(model: &str) -> String {
    let model = if model.is_empty() { "unknown" } else { model };
    format!("GeminiCLI-tui/{GEMINI_CLI_VERSION}/{model} ({}; {}; terminal) google-api-nodejs-client/9.15.1", node_os(), node_arch())
}

/// Send the (already-translated) `payload` to the upstream for `provider`.
#[allow(clippy::too_many_arguments)]
pub async fn send_request(
    client: &reqwest::Client,
    provider: &str,
    model: &str,
    access_token: &str,
    payload: Vec<u8>,
    action: &str,
    stream: bool,
    antigravity_version: &str,
) -> reqwest::Result<reqwest::Response> {
    let url = build_url(provider, action, stream);
    let mut req = client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .header("Accept", if stream { "text/event-stream" } else { "application/json" });

    req = match provider {
        GEMINI_CLI => req
            .header("User-Agent", gemini_cli_user_agent(model))
            .header("X-Goog-Api-Client", GEMINI_CLI_API_CLIENT),
        ANTIGRAVITY => req.header("User-Agent", format!("antigravity/{antigravity_version} darwin/arm64")),
        _ => req,
    };

    req.body(payload).send().await
}

/// Generic SSE pump: stream an upstream `reqwest::Response` line-by-line into a
/// [`ProxyBody`], forwarding whatever `on_line` produces. `on_line(Some(line))`
/// is called per complete upstream line (CR/LF trimmed); `on_line(None)` is
/// called once at upstream EOF as a finalizer. Each returned `String` is sent as
/// one body frame.
///
/// The Gemini path ([`stream_body_from_response`]) and the Anthropic path
/// (`gemini::anthropic`) both build on this, so the disconnect-safety lives in
/// one place.
pub fn stream_sse<F>(resp: reqwest::Response, mut on_line: F) -> ProxyBody
where
    F: FnMut(Option<&str>) -> Vec<String> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, std::io::Error>>(16);

    tokio::spawn(async move {
        let mut upstream = Box::pin(resp.bytes_stream());
        let mut buf: Vec<u8> = Vec::new();

        loop {
            // Stop promptly if the client disconnected — `tx.closed()` fires
            // when the StreamBody (and its receiver) is dropped, even while the
            // upstream is idle. Without this we could park on `upstream.next()`
            // indefinitely, leaking the task and the upstream connection.
            let chunk = tokio::select! {
                biased;
                _ = tx.closed() => return,
                next = upstream.next() => next,
            };
            let chunk = match chunk {
                Some(Ok(c)) => c,
                Some(Err(e)) => {
                    warn!("gemini stream: upstream read error: {}", e);
                    // Surface the failure as a broken body rather than a clean
                    // EOF — otherwise the truncated stream looks like a completed
                    // response. Return without running the finalizer so we don't
                    // also emit a synthetic completion event (e.g. the Anthropic
                    // `message_stop`) after the error.
                    let _ = tx.send(Err(std::io::Error::other(e))).await;
                    return;
                }
                None => break, // upstream finished
            };
            buf.extend_from_slice(&chunk);

            // Emit each complete line, keep the trailing partial in `buf`.
            // Split on the raw `\n` byte (which never appears inside a
            // multi-byte UTF-8 sequence) and decode only complete lines, so a
            // character straddling a chunk boundary is never corrupted.
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                // Borrow the line up to (not including) the `\n` — no per-line
                // allocation — decode and forward it, then drain once the borrow
                // is done. A trailing CRLF `\r` is trimmed (`\n` already excluded).
                let line = String::from_utf8_lossy(&buf[..nl]);
                for frame in on_line(Some(line.trim_end_matches(['\r', '\n']))) {
                    if tx.send(Ok(Frame::data(Bytes::from(frame)))).await.is_err() {
                        return; // client gone
                    }
                }
                buf.drain(..=nl);
            }
        }

        // Flush any final partial line, then the finalizer. On client-gone the
        // send simply errors and is dropped; returning drops `upstream` (closing
        // the upstream connection) and `tx` (closing the channel) — nothing leaks.
        let tail = String::from_utf8_lossy(&buf);
        let tail = tail.trim();
        if !tail.is_empty() {
            for frame in on_line(Some(tail)) {
                if tx.send(Ok(Frame::data(Bytes::from(frame)))).await.is_err() {
                    return;
                }
            }
        }
        for frame in on_line(None) {
            let _ = tx.send(Ok(Frame::data(Bytes::from(frame)))).await;
        }
    });

    StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx)).boxed()
}

/// Convert an upstream SSE response into a native-Gemini SSE [`ProxyBody`],
/// unwrapping each `data: {"response":{…}}` chunk to `data: {…}` on the fly.
pub fn stream_body_from_response(resp: reqwest::Response) -> ProxyBody {
    stream_sse(resp, |line| match line {
        Some(l) => transform_sse_line(l).into_iter().collect(),
        None => Vec::new(),
    })
}

/// Translate one upstream SSE line into the line we forward to the client.
/// Returns `None` for blank/comment lines.
fn transform_sse_line(line: &str) -> Option<String> {
    if line.is_empty() {
        return None;
    }
    let payload = match line.strip_prefix("data:") {
        Some(rest) => rest.trim(),
        None => return None, // ignore non-data lines (event:/:keepalive/etc.)
    };
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    // Unwrap `.response`; fall back to forwarding the original payload.
    let out = translate::unwrap_sse_payload(payload).unwrap_or_else(|| payload.to_string());
    Some(format!("data: {out}\n\n"))
}
