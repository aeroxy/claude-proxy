use hyper::body::Bytes;
use hyper::Response;
use http_body_util::Full;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use tokio::sync::{Mutex, broadcast};
use std::sync::Arc;
use lazy_static::lazy_static;
use std::collections::HashMap;

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

lazy_static! {
    static ref TOKEN_PROMISES: Mutex<HashMap<String, Arc<broadcast::Sender<Option<GoogleTokenFile>>>>> = Mutex::new(HashMap::new());
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
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
                if token_data.expires_on > now {
                    return Some(token_data);
                }
            }
        }
    }
    None
}

pub fn token_file_to_response(token_data: &GoogleTokenFile) -> Response<Full<Bytes>> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
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
    sender: Arc<broadcast::Sender<Option<GoogleTokenFile>>>,
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

pub async fn save_token_cache(body: &str, response_json: &serde_json::Value) -> Option<GoogleTokenFile> {
    if let (Some(access_token), Some(expires_in), Some(token_type)) = (
        response_json.get("access_token").and_then(|v| v.as_str()),
        response_json.get("expires_in").and_then(|v| v.as_u64()),
        response_json.get("token_type").and_then(|v| v.as_str()),
    ) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let expires_on = now + (expires_in * 1000);
        
        let token_file = GoogleTokenFile {
            request_body: body.to_string(),
            access_token: access_token.to_string(),
            expires_on,
            scope: response_json.get("scope").and_then(|v| v.as_str()).map(|s| s.to_string()),
            token_type: token_type.to_string(),
            id_token: response_json.get("id_token").and_then(|v| v.as_str()).map(|s| s.to_string()),
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
        warn!("Failed to extract token fields from Google response: {:?}", response_json);
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

                return Some(Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(response_json.to_string())))
                    .unwrap());
            }
        }
    }
    None
}
