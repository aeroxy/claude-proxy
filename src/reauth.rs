use lazy_static::lazy_static;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

pub(crate) const GOOGLE_CLIENT_ID: &str = "32555940559.apps.googleusercontent.com";
pub(crate) const GOOGLE_CLIENT_SECRET: &str = "ZmssLNjJy2998hD4CTg2ejr2";
pub(crate) const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/auth";
pub(crate) const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(crate) const SCOPES: &[&str] = &[
    "openid",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/cloud-platform",
];
const REAUTH_TIMEOUT_SECS: u64 = 300;

#[derive(Clone, Debug)]
pub struct ReauthResult {
    pub token_response_json: serde_json::Value,
}

lazy_static! {
    static ref REAUTH_PROMISE: Mutex<Option<Arc<broadcast::Sender<Option<ReauthResult>>>>> =
        Mutex::new(None);
}

struct ReauthGuard {
    sender: Arc<broadcast::Sender<Option<ReauthResult>>>,
    resolved: bool,
}

impl ReauthGuard {
    async fn resolve(mut self, result: Option<ReauthResult>) {
        self.resolved = true;
        let mut promise = REAUTH_PROMISE.lock().await;
        *promise = None;
        let n = self.sender.send(result).unwrap_or(0);
        info!(waiters = n, "Resolved re-auth promise");
    }
}

impl Drop for ReauthGuard {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        warn!("ReauthGuard dropped without resolve — re-auth was cancelled.");
        if let Ok(mut promise) = REAUTH_PROMISE.try_lock() {
            *promise = None;
        } else {
            tokio::spawn(async {
                let mut promise = REAUTH_PROMISE.lock().await;
                *promise = None;
            });
        }
    }
}

pub async fn handle_invalid_grant() -> Option<ReauthResult> {
    let mut rx = {
        let mut promise = REAUTH_PROMISE.lock().await;
        if let Some(sender) = promise.as_ref() {
            info!("Re-auth already in progress, waiting on existing flow...");
            let rx = sender.subscribe();
            drop(promise);
            rx
        } else {
            let (tx, rx) = broadcast::channel(1);
            let sender = Arc::new(tx);
            *promise = Some(Arc::clone(&sender));
            drop(promise);

            tokio::spawn(async move {
                let guard = ReauthGuard {
                    sender,
                    resolved: false,
                };
                let result = run_oauth_flow().await;
                guard.resolve(result).await;
            });

            rx
        }
    };

    match rx.recv().await {
        Ok(result) => result,
        Err(broadcast::error::RecvError::Closed) => {
            warn!("Re-auth channel closed without resolution.");
            None
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            warn!("Re-auth broadcast lagged by {}, missed resolution.", n);
            None
        }
    }
}

async fn run_oauth_flow() -> Option<ReauthResult> {
    info!("Starting automatic re-authentication via browser OAuth flow...");

    let cache_path = crate::interceptors::get_token_cache_path();
    let _ = std::fs::remove_file(&cache_path);

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            warn!("Failed to bind callback listener: {}", e);
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    let redirect_uri = format!("http://localhost:{}", port);

    let scopes_str = SCOPES.join(" ");
    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent",
        GOOGLE_AUTH_URL,
        crate::oauth_util::percent_encode(GOOGLE_CLIENT_ID),
        crate::oauth_util::percent_encode(&redirect_uri),
        crate::oauth_util::percent_encode(&scopes_str),
    );

    info!("Opening browser for Google re-authentication...");
    crate::oauth_util::open_browser(&auth_url);

    let auth_code = match tokio::time::timeout(
        Duration::from_secs(REAUTH_TIMEOUT_SECS),
        crate::oauth_util::accept_oauth_callback(&listener),
    )
    .await
    {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => {
            warn!("OAuth callback error: {}", e);
            return None;
        }
        Err(_) => {
            warn!(
                "OAuth flow timed out after {} seconds.",
                REAUTH_TIMEOUT_SECS
            );
            return None;
        }
    };

    info!("Received authorization code, exchanging for tokens...");

    let client = match reqwest::Client::builder().no_proxy().build() {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to build HTTP client for token exchange: {}", e);
            return None;
        }
    };

    let resp = match client
        .post(GOOGLE_TOKEN_URL)
        .form(&[
            ("code", auth_code.as_str()),
            ("client_id", GOOGLE_CLIENT_ID),
            ("client_secret", GOOGLE_CLIENT_SECRET),
            ("redirect_uri", redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Token exchange request failed: {}", e);
            return None;
        }
    };

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!("Token exchange failed ({}): {}", body.len(), body);
        return None;
    }

    let token_json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            warn!("Failed to parse token exchange response: {}", e);
            return None;
        }
    };

    let _refresh_token = match token_json["refresh_token"].as_str() {
        Some(rt) => rt,
        None => {
            warn!("No refresh_token in token exchange response");
            return None;
        }
    };

    write_adc(&token_json);

    info!("Re-authentication completed successfully.");
    Some(ReauthResult {
        token_response_json: token_json,
    })
}

pub(crate) fn write_adc(token_json: &serde_json::Value) {
    let _refresh_token = match token_json["refresh_token"].as_str() {
        Some(rt) => rt,
        None => return,
    };

    let adc = serde_json::json!({
        "client_id": GOOGLE_CLIENT_ID,
        "client_secret": GOOGLE_CLIENT_SECRET,
        "refresh_token": _refresh_token,
        "type": "authorized_user",
    });

    let creds_path = get_adc_path();
    if let Some(parent) = creds_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(&adc) {
        Ok(content) => {
            if let Err(e) = std::fs::write(&creds_path, content) {
                warn!("Failed to write ADC file: {}", e);
            } else {
                info!("Updated ADC credentials at {:?}", creds_path);
            }
        }
        Err(e) => warn!("Failed to serialize ADC: {}", e),
    }
}

pub(crate) fn get_adc_path() -> PathBuf {
    dirs::home_dir()
        .unwrap()
        .join(".config/gcloud/application_default_credentials.json")
}
