use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{HeaderMap, Method, Response};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

use crate::config::MapLocalRule;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GoogleTokenFile {
    pub request_body: String,
    pub access_token: String,
    pub expires_on: u64,
    pub scope: Option<String>,
    pub token_type: String,
    pub id_token: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VertexHeatUpRequest {
    pub max_tokens: u32,
    pub messages: Vec<Message>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// Broadcast sender for a deduplicated OAuth-token fetch (`TOKEN_PROMISES`).
type TokenSender = broadcast::Sender<Option<GoogleTokenFile>>;
/// Broadcast sender for a deduplicated in-flight request (`REQUEST_PROMISES`).
type ResponseSender = broadcast::Sender<Option<Arc<BufferedResponse>>>;

lazy_static! {
    static ref TOKEN_PROMISES: Mutex<HashMap<String, Arc<TokenSender>>> =
        Mutex::new(HashMap::new());
    static ref REQUEST_PROMISES: Mutex<HashMap<String, Arc<ResponseSender>>> =
        Mutex::new(HashMap::new());
}

/// Hop-by-hop and per-client headers stripped before snapshotting an upstream
/// response for replay to other waiters.
pub const STRIPPED_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "transfer-encoding",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "upgrade",
    "content-length",
    "set-cookie",
];

#[derive(Debug)]
pub struct BufferedResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

pub fn get_token_cache_path() -> PathBuf {
    dirs::home_dir()
        .unwrap()
        .join(".config")
        .join("gcloud")
        .join("application_default_credentials_access_token.json")
}

pub async fn check_disk_token_cache(body: &str) -> Option<GoogleTokenFile> {
    let path = get_token_cache_path();
    if !path.exists() {
        return None;
    }

    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(token_data) = serde_json::from_str::<GoogleTokenFile>(&content) {
            if token_data.request_body == body {
                const EXPIRY_BUFFER_MS: u64 = 60_000; // refresh 60 s before actual expiry
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                if token_data.expires_on > now + EXPIRY_BUFFER_MS {
                    return Some(token_data);
                }
            }
        }
    }
    None
}

pub fn token_file_to_response(token_data: &GoogleTokenFile) -> Response<Full<Bytes>> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // Prevent underflow if it expired right after we checked it
    let expires_in = if token_data.expires_on > now {
        (token_data.expires_on - now) / 1000
    } else {
        0
    };

    let mut response_json = serde_json::json!({
        "access_token": token_data.access_token,
        "expires_in": expires_in,
        "token_type": token_data.token_type,
    });

    if let Some(scope) = &token_data.scope {
        response_json["scope"] = serde_json::json!(scope);
    }
    if let Some(id_token) = &token_data.id_token {
        response_json["id_token"] = serde_json::json!(id_token);
    }

    Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(response_json.to_string())))
        .unwrap()
}

pub enum TokenRequestState {
    Cached(Response<Full<Bytes>>),
    /// Secondary waiter — owns a Receiver subscribed to the primary's broadcast.
    Waiting(broadcast::Receiver<Option<GoogleTokenFile>>),
    /// Primary fetcher — owns the Sender via this guard. On Drop the Sender is
    /// removed from the global map, so cancelled tasks don't strand waiters.
    Primary(PrimaryGuard),
}

pub struct PrimaryGuard {
    body: String,
    sender: Arc<TokenSender>,
    resolved: bool,
}

impl PrimaryGuard {
    /// Resolve the promise with the given token data and remove from the map.
    /// Idempotent — a Drop after this is a no-op.
    pub async fn resolve(mut self, token_data: Option<GoogleTokenFile>) {
        self.resolved = true;
        let mut promises = TOKEN_PROMISES.lock().await;
        promises.remove(&self.body);
        let n = self.sender.send(token_data).unwrap_or(0);
        info!(waiters = n, "Resolved token promise");
    }
}

impl Drop for PrimaryGuard {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        // Task was cancelled before resolve() — synchronously remove from the
        // map and let the Sender drop close the broadcast channel so waiters
        // observe RecvError::Closed instead of hanging forever.
        warn!(
            "PrimaryGuard dropped without resolve — task was cancelled. Removing in-flight entry."
        );
        if let Ok(mut promises) = TOKEN_PROMISES.try_lock() {
            promises.remove(&self.body);
        } else {
            // The lock was contended; spawn a cleanup task.
            let body = std::mem::take(&mut self.body);
            tokio::spawn(async move {
                let mut promises = TOKEN_PROMISES.lock().await;
                promises.remove(&body);
            });
        }
    }
}

pub async fn handle_token_request(body: &str) -> TokenRequestState {
    // 1. Check disk cache first
    if let Some(token_data) = check_disk_token_cache(body).await {
        info!("Cache hit on disk for token");
        return TokenRequestState::Cached(token_file_to_response(&token_data));
    }

    // 2. Check in-flight promises
    let mut promises = TOKEN_PROMISES.lock().await;
    if let Some(tx) = promises.get(body) {
        info!("Token request already in flight, joining existing wait queue.");
        return TokenRequestState::Waiting(tx.subscribe());
    }

    // 3. Register as the one who will fetch
    let (tx, _rx) = broadcast::channel(1);
    let sender = Arc::new(tx);
    promises.insert(body.to_string(), Arc::clone(&sender));
    info!("Registered as the primary fetcher for this token request.");

    TokenRequestState::Primary(PrimaryGuard {
        body: body.to_string(),
        sender,
        resolved: false,
    })
}

pub async fn save_token_cache(
    body: &str,
    response_json: &serde_json::Value,
) -> Option<GoogleTokenFile> {
    if let (Some(access_token), Some(expires_in), Some(token_type)) = (
        response_json.get("access_token").and_then(|v| v.as_str()),
        response_json.get("expires_in").and_then(|v| v.as_u64()),
        response_json.get("token_type").and_then(|v| v.as_str()),
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let expires_on = now + (expires_in * 1000);

        let token_file = GoogleTokenFile {
            request_body: body.to_string(),
            access_token: access_token.to_string(),
            expires_on,
            scope: response_json
                .get("scope")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            token_type: token_type.to_string(),
            id_token: response_json
                .get("id_token")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        let path = get_token_cache_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(content) = serde_json::to_string_pretty(&token_file) {
            if fs::write(&path, content).is_ok() {
                info!("Saved Google OAuth token cache to disk.");
            }
        }

        Some(token_file)
    } else {
        warn!(
            "Failed to extract token fields from Google response: {:?}",
            response_json
        );
        None
    }
}

use rand::{distr::Alphanumeric, RngExt};

pub fn generate_random_id(prefix: &str) -> String {
    let random_chars: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(24)
        .map(char::from)
        .collect();
    format!("{}{}", prefix, random_chars)
}

pub fn handle_vertex_heatup(body: &str, model: &str) -> Option<Response<Full<Bytes>>> {
    if let Ok(req) = serde_json::from_str::<VertexHeatUpRequest>(body) {
        if req.max_tokens == 1 && req.messages.len() == 1 {
            let msg = &req.messages[0];
            if msg.role == "user" && msg.content == "." {
                info!("Intercepted Vertex AI heat-up request for {}", model);

                let random_id = generate_random_id("msg_vrtx_");
                let model_name_formatted = model.replace("@", "-");

                let response_text = "Hello";

                let response_json = serde_json::json!({
                    "model": model_name_formatted,
                    "id": random_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {
                        "type": "text",
                        "text": response_text
                        }
                    ],
                    "stop_reason": "max_tokens",
                    "stop_sequence": null,
                    "stop_details": null,
                    "usage": {
                        "input_tokens": 8,
                        "cache_creation_input_tokens": 0,
                        "cache_read_input_tokens": 0,
                        "cache_creation": {
                        "ephemeral_5m_input_tokens": 0,
                        "ephemeral_1h_input_tokens": 0
                        },
                        "output_tokens": 1
                    }
                });

                return Some(
                    Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(Full::new(Bytes::from(response_json.to_string())))
                        .unwrap(),
                );
            }
        }
    }
    None
}

pub enum RequestDedupState {
    /// Secondary waiter — owns a Receiver subscribed to the primary's broadcast.
    Waiting(broadcast::Receiver<Option<Arc<BufferedResponse>>>),
    /// Primary fetcher — owns the Sender via this guard.
    Primary(RequestPrimaryGuard),
}

pub struct RequestPrimaryGuard {
    key: String,
    sender: Arc<ResponseSender>,
    resolved: bool,
}

impl RequestPrimaryGuard {
    /// Whether any secondary has subscribed to this promise. `handle_dedup_request`
    /// drops its own `_rx` before returning, so a non-zero count means exactly
    /// "a duplicate joined the wait queue". Lets the routed (streaming) path skip
    /// recording a response body nobody will replay.
    pub fn has_waiters(&self) -> bool {
        self.sender.receiver_count() > 0
    }

    pub async fn resolve(mut self, response: Option<Arc<BufferedResponse>>) {
        self.resolved = true;
        let mut promises = REQUEST_PROMISES.lock().await;
        promises.remove(&self.key);
        let n = self.sender.send(response).unwrap_or(0);
        info!(waiters = n, "Resolved request dedup promise");
    }
}

impl Drop for RequestPrimaryGuard {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        warn!(
            "RequestPrimaryGuard dropped without resolve — task was cancelled. Removing in-flight entry."
        );
        if let Ok(mut promises) = REQUEST_PROMISES.try_lock() {
            promises.remove(&self.key);
        } else {
            let key = std::mem::take(&mut self.key);
            tokio::spawn(async move {
                let mut promises = REQUEST_PROMISES.lock().await;
                promises.remove(&key);
            });
        }
    }
}

pub async fn handle_dedup_request(key: &str) -> RequestDedupState {
    let mut promises = REQUEST_PROMISES.lock().await;
    if let Some(tx) = promises.get(key) {
        info!("Request already in flight, joining existing wait queue.");
        return RequestDedupState::Waiting(tx.subscribe());
    }

    let (tx, _rx) = broadcast::channel(1);
    let sender = Arc::new(tx);
    promises.insert(key.to_string(), Arc::clone(&sender));
    info!("Registered as the primary fetcher for this request.");

    RequestDedupState::Primary(RequestPrimaryGuard {
        key: key.to_string(),
        sender,
        resolved: false,
    })
}

pub fn buffered_to_response(buf: &BufferedResponse) -> Response<Full<Bytes>> {
    let mut builder = Response::builder().status(buf.status);
    for (k, v) in buf.headers.iter() {
        builder = builder.header(k.clone(), v.clone());
    }
    builder.body(Full::new(buf.body.clone())).unwrap()
}

// ---------------------------------------------------------------------------
// Map Local: match a configured URL pattern + method, return a fixed response
// (inline literal or local file) instead of forwarding upstream.
// ---------------------------------------------------------------------------

enum BodyKind {
    Inline,
    File(PathBuf),
    Empty,
}

pub fn match_map_local<'a>(
    rules: &'a [MapLocalRule],
    method: &Method,
    url: &str,
) -> Option<&'a MapLocalRule> {
    let mut best: Option<(&MapLocalRule, usize)> = None;
    for rule in rules {
        if rule.url.is_empty() {
            continue;
        }
        if let Some(rule_method) = &rule.method {
            if !rule_method.eq_ignore_ascii_case(method.as_str()) {
                continue;
            }
        }
        if !wildcard_match(&rule.url, url) {
            continue;
        }
        let literal = rule.url.chars().filter(|c| *c != '*' && *c != '?').count();
        let score = literal + if rule.method.is_some() { 1_000_000 } else { 0 };
        if best.as_ref().is_none_or(|(_, s)| score > *s) {
            best = Some((rule, score));
        }
    }
    best.map(|(r, _)| r)
}

pub async fn build_map_local_response(rule: &MapLocalRule) -> Response<Full<Bytes>> {
    let (body_bytes, body_kind): (Bytes, BodyKind) = if let Some(b) = &rule.body {
        (Bytes::from(b.clone()), BodyKind::Inline)
    } else if let Some(p) = &rule.file {
        match tokio::fs::read(p).await {
            Ok(v) => (Bytes::from(v), BodyKind::File(p.clone())),
            Err(e) => {
                warn!("Map Local: cannot read {}: {}", p.display(), e);
                return Response::builder()
                    .status(502)
                    .header("content-type", "text/plain; charset=utf-8")
                    .header("x-map-local-error", "file-unreadable")
                    .body(Full::new(Bytes::from(format!(
                        "Map Local: cannot read {}: {}",
                        p.display(),
                        e
                    ))))
                    .unwrap();
            }
        }
    } else {
        (Bytes::new(), BodyKind::Empty)
    };

    let status = rule.status.unwrap_or(200);
    let mut builder = Response::builder().status(status);

    let ct: Option<String> = match (&rule.content_type, &body_kind, body_bytes.is_empty()) {
        (Some(c), _, _) => Some(c.clone()),
        (None, BodyKind::File(p), _) => {
            Some(guess_mime_from_path(p).unwrap_or_else(|| "application/octet-stream".to_string()))
        }
        (None, BodyKind::Inline, false) => Some("application/json".to_string()),
        _ => None,
    };
    if let Some(c) = ct {
        builder = builder.header("content-type", c);
    }

    for (k, v) in &rule.headers {
        if k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        builder = builder.header(k, v);
    }

    builder = builder.header("x-map-local", "true");
    if let BodyKind::File(p) = &body_kind {
        builder = builder.header("x-map-local-source", p.display().to_string());
    }

    builder.body(Full::new(body_bytes)).unwrap()
}

/// fnmatch-style wildcard: `*` matches zero or more, `?` matches one.
fn wildcard_match(pattern: &str, subject: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = subject.chars().collect();
    let mut dp = vec![vec![false; s.len() + 1]; p.len() + 1];
    dp[0][0] = true;
    for i in 1..=p.len() {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=p.len() {
        for j in 1..=s.len() {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                c => c == s[j - 1] && dp[i - 1][j - 1],
            };
        }
    }
    dp[p.len()][s.len()]
}

fn guess_mime_from_path(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "json" => "application/json",
            "txt" | "log" => "text/plain; charset=utf-8",
            "html" | "htm" => "text/html; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "js" | "mjs" => "application/javascript",
            "xml" => "application/xml",
            "csv" => "text/csv; charset=utf-8",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "pdf" => "application/pdf",
            "zip" => "application/zip",
            "wasm" => "application/wasm",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            _ => return None,
        }
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_basics() {
        assert!(wildcard_match("foo", "foo"));
        assert!(!wildcard_match("foo", "bar"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("a*c", "abc"));
        assert!(wildcard_match("a*c", "axxxc"));
        assert!(!wildcard_match("a*c", "ab"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "ac"));
        assert!(wildcard_match(
            "https://api.example.com/v1/*",
            "https://api.example.com/v1/foo"
        ));
        assert!(wildcard_match(
            "https://api.example.com/v1/*",
            "https://api.example.com/v1/foo?x=1"
        ));
        assert!(!wildcard_match(
            "https://api.example.com/v1/*",
            "https://api.example.com/v2/foo"
        ));
    }

    fn rule(url: &str, method: Option<&str>) -> MapLocalRule {
        MapLocalRule {
            url: url.to_string(),
            method: method.map(|m| m.to_string()),
            body: None,
            file: None,
            status: None,
            content_type: None,
            headers: Default::default(),
        }
    }

    #[test]
    fn match_method_specific_beats_any() {
        let rules = vec![
            rule("https://api.example.com/v1/*", None),
            rule("https://api.example.com/v1/*", Some("POST")),
        ];
        let m = match_map_local(&rules, &Method::POST, "https://api.example.com/v1/foo")
            .expect("should match");
        assert_eq!(m.method.as_deref(), Some("POST"));

        let m = match_map_local(&rules, &Method::PUT, "https://api.example.com/v1/foo")
            .expect("should match");
        assert!(m.method.is_none());
    }

    #[test]
    fn match_method_filter() {
        let rules = vec![rule("https://x/*", Some("get"))];
        assert!(match_map_local(&rules, &Method::GET, "https://x/y").is_some());
        assert!(match_map_local(&rules, &Method::POST, "https://x/y").is_none());
    }

    #[test]
    fn match_no_rules() {
        let rules: Vec<MapLocalRule> = vec![];
        assert!(match_map_local(&rules, &Method::GET, "https://anything").is_none());
    }
}
