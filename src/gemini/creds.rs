//! Credential discovery, parsing, and refresh for the `gemini-cli` and
//! `antigravity` providers.
//!
//! Files are read from our own dir (`~/.config/claude-proxy/auths/`) and any
//! existing CLIProxyAPI dir (`~/.cli-proxy-api/`), so CLIProxyAPI users work
//! unchanged. Two on-disk shapes are supported, dispatched on the top-level
//! `type` field:
//!
//! - `type:"gemini"`     → `{token:{access_token,token_type,refresh_token,expiry},project_id,email,…}`
//! - `type:"antigravity"`→ `{access_token,refresh_token,expires_in,timestamp,expired,email,project_id,…}`
//!
//! Refresh follows the same invariant as [`crate::reauth`]: the token-exchange
//! POST uses a `no_proxy()` client so it never loops back through us.

use lazy_static::lazy_static;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

use super::models::{ANTIGRAVITY, GEMINI_CLI};

pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

// --- gemini (gemini-cli) OAuth client -------------------------------------
pub const GEMINI_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
pub const GEMINI_CLIENT_SECRET: &str = "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";
pub const GEMINI_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

// --- antigravity OAuth client ---------------------------------------------
pub const ANTIGRAVITY_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub const ANTIGRAVITY_CLIENT_SECRET: &str = "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";
pub const ANTIGRAVITY_SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Refresh this many ms before the stored expiry.
const EXPIRY_BUFFER_MS: u64 = 60_000;

lazy_static! {
    /// Dedicated client for token refresh. `no_proxy()` so a configured
    /// `upstream_proxy` (possibly us) can't create a loop, mirroring reauth.rs.
    static ref REFRESH_CLIENT: reqwest::Client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build refresh client");

    /// Serializes credential refreshes process-wide (see [`ensure_fresh`]).
    static ref REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());

    /// Serializes lazy onboarding process-wide (see [`lazy_onboard`]). Kept
    /// separate from `REFRESH_LOCK` because onboarding can poll upstream for
    /// tens of seconds (`ONBOARD_TIMEOUT_SECS` in login.rs) and must not stall
    /// unrelated token refreshes for that long.
    static ref ONBOARD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());

    /// In-memory cache for Vertex tokens: Some((access_token, expires_at_ms))
    static ref VERTEX_TOKEN_CACHE: tokio::sync::Mutex<Option<(String, u64)>> = tokio::sync::Mutex::new(None);
}

/// Default credential directories, in read order: ours first, then CLIProxyAPI's.
pub fn default_auth_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    vec![
        home.join(".config/claude-proxy/auths"),
        home.join(".cli-proxy-api"),
    ]
}

/// Our own credential dir (where `login` writes).
pub fn our_auth_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".config/claude-proxy/auths")
}

#[derive(Debug, Clone)]
pub struct Account {
    /// Resolved provider name: `gemini-cli` or `antigravity`.
    pub provider: String,
    pub email: String,
    pub project_id: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix ms when the access token expires (0 if unknown).
    pub expires_at_ms: u64,
    pub file_path: PathBuf,
}

#[derive(Deserialize)]
struct GeminiTokenField {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expiry: Option<String>,
}

#[derive(Deserialize)]
struct GeminiCred {
    #[serde(default)]
    token: Option<GeminiTokenField>,
    #[serde(default)]
    project_id: String,
    #[serde(default)]
    email: String,
}

#[derive(Deserialize)]
struct AntigravityCred {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    timestamp: Option<u64>,
    #[serde(default)]
    email: String,
    #[serde(default)]
    project_id: String,
}

fn now_ms() -> u64 {
    // `unwrap_or_default` (→ 0) on the impossible pre-1970 clock case just makes
    // a token look expired and triggers a harmless refresh — never a panic.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn rfc3339_to_ms(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis().max(0) as u64)
}

/// Scan all `auth_dirs` for `*.json` credential files and parse the ones we
/// understand. Unreadable/unknown files are skipped with a debug log.
pub fn discover_accounts(auth_dirs: &[PathBuf]) -> Vec<Account> {
    let mut accounts = Vec::new();
    for dir in auth_dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue, // dir absent — fine
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match parse_account(&path) {
                Some(acc) => accounts.push(acc),
                None => debug!("gemini creds: skipped unrecognized file {}", path.display()),
            }
        }
    }
    accounts
}

fn parse_account(path: &Path) -> Option<Account> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match kind {
        "gemini" => {
            let c: GeminiCred = serde_json::from_value(value).ok()?;
            let token = c.token?;
            let expires_at_ms = token.expiry.as_deref().and_then(rfc3339_to_ms).unwrap_or(0);
            Some(Account {
                provider: GEMINI_CLI.to_string(),
                email: c.email,
                project_id: c.project_id,
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                expires_at_ms,
                file_path: path.to_path_buf(),
            })
        }
        "antigravity" => {
            let c: AntigravityCred = serde_json::from_value(value).ok()?;
            let expires_at_ms = match (c.timestamp, c.expires_in) {
                (Some(ts), Some(exp)) => ts + exp * 1000,
                _ => 0,
            };
            Some(Account {
                provider: ANTIGRAVITY.to_string(),
                email: c.email,
                project_id: c.project_id,
                access_token: c.access_token,
                refresh_token: c.refresh_token,
                expires_at_ms,
                file_path: path.to_path_buf(),
            })
        }
        _ => None,
    }
}

/// Pick the first credential for `provider` found across `auth_dirs`.
pub fn pick_account(provider: &str, auth_dirs: &[PathBuf]) -> Option<Account> {
    discover_accounts(auth_dirs)
        .into_iter()
        .find(|a| a.provider == provider)
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// True if `account`'s access token is present and not within the refresh
/// buffer of its expiry.
fn is_fresh(account: &Account) -> bool {
    !account.access_token.is_empty() && account.expires_at_ms > now_ms() + EXPIRY_BUFFER_MS
}

/// Return a valid access token for `account`, refreshing and writing back to
/// the source file if the stored one is expired (or about to be).
pub async fn ensure_fresh(account: &Account) -> anyhow::Result<String> {
    if is_fresh(account) {
        return Ok(account.access_token.clone());
    }
    if account.refresh_token.is_empty() {
        anyhow::bail!(
            "credential {} has no refresh_token and the access token is expired",
            account.file_path.display()
        );
    }

    // Serialize refreshes process-wide: concurrent requests hitting an expired
    // token must not each fire a refresh POST and race the cred-file write
    // (which could corrupt it or needlessly churn the refresh token). Refreshes
    // are rare (~once/hour/account) and brief, and fresh tokens return above
    // without ever taking this lock, so contention is negligible.
    let _guard = REFRESH_LOCK.lock().await;
    // Another task may have refreshed while we waited on the lock — re-read from
    // disk and reuse its token instead of refreshing again.
    let path = account.file_path.clone();
    if let Some(reloaded) = tokio::task::spawn_blocking(move || parse_account(&path))
        .await
        .unwrap_or(None)
    {
        if is_fresh(&reloaded) {
            return Ok(reloaded.access_token);
        }
    }

    let (client_id, client_secret) = match account.provider.as_str() {
        GEMINI_CLI => (GEMINI_CLIENT_ID, GEMINI_CLIENT_SECRET),
        ANTIGRAVITY => (ANTIGRAVITY_CLIENT_ID, ANTIGRAVITY_CLIENT_SECRET),
        other => anyhow::bail!("unknown provider {other}"),
    };

    debug!(
        "gemini creds: refreshing {} token for {}",
        account.provider, account.email
    );
    let resp = REFRESH_CLIENT
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", account.refresh_token.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("token refresh failed ({status}): {body}");
    }

    let refreshed: RefreshResponse = resp.json().await?;
    let expires_in = refreshed.expires_in.unwrap_or(3600);
    let new_refresh = refreshed
        .refresh_token
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| account.refresh_token.clone());

    let account_clone = account.clone();
    let access_token_clone = refreshed.access_token.clone();
    let new_refresh_clone = new_refresh.clone();
    let _ = tokio::task::spawn_blocking(move || {
        write_back_token(
            &account_clone,
            &access_token_clone,
            &new_refresh_clone,
            expires_in,
        );
    })
    .await;
    Ok(refreshed.access_token)
}

/// Merge the refreshed token back into the credential file, preserving all
/// other fields. Best-effort: a write failure only costs an extra refresh next
/// time.
fn write_back_token(account: &Account, access_token: &str, refresh_token: &str, expires_in: u64) {
    let raw = match std::fs::read_to_string(&account.file_path) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "gemini creds: cannot re-read {} for write-back: {}",
                account.file_path.display(),
                e
            );
            return;
        }
    };
    let mut value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "gemini creds: cannot parse {} for write-back: {}",
                account.file_path.display(),
                e
            );
            return;
        }
    };

    if !value.is_object() {
        warn!(
            "gemini creds: credential file {} is not a JSON object",
            account.file_path.display()
        );
        return;
    }

    let now = now_ms();
    // Saturating arithmetic: a bogus huge `expires_in` must not overflow-panic
    // (debug) or wrap (release). `now` stays the single clock read shared with
    // the antigravity `timestamp` field below so the two remain consistent.
    let expiry_ms = now.saturating_add(expires_in.saturating_mul(1000));
    let expiry_rfc3339 = chrono::DateTime::<chrono::Utc>::from(
        UNIX_EPOCH + std::time::Duration::from_millis(expiry_ms),
    )
    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    match account.provider.as_str() {
        GEMINI_CLI => {
            // `value` is confirmed an object above, so `value["token"] = ...`
            // can't panic on the outer level — but indexing a *second* level
            // into `token` panics if it already holds a non-object, non-null
            // value (serde_json only auto-vivifies null/missing), so that
            // case is guarded explicitly instead of indexing blind.
            if !value["token"].is_object() && !value["token"].is_null() {
                warn!(
                    "gemini creds: 'token' in {} is not an object",
                    account.file_path.display()
                );
                return;
            }
            value["token"]["access_token"] = serde_json::json!(access_token);
            value["token"]["refresh_token"] = serde_json::json!(refresh_token);
            value["token"]["expiry"] = serde_json::json!(expiry_rfc3339);
        }
        ANTIGRAVITY => {
            value["access_token"] = serde_json::json!(access_token);
            value["refresh_token"] = serde_json::json!(refresh_token);
            value["expires_in"] = serde_json::json!(expires_in);
            value["timestamp"] = serde_json::json!(now);
            value["expired"] = serde_json::json!(expiry_rfc3339);
        }
        _ => {}
    }

    if let Ok(serialized) = serde_json::to_string_pretty(&value) {
        // Write to a temp file and rename so a crash mid-write can't corrupt the
        // credential. Same dir → same filesystem → the rename is atomic, and the
        // `.tmp` is ignored by the `*.json`-only discovery. Refreshes are
        // serialized by REFRESH_LOCK, so the temp path can't race a concurrent one.
        let tmp_path = account.file_path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp_path, serialized) {
            warn!(
                "gemini creds: failed to write refreshed token to {}: {}",
                tmp_path.display(),
                e
            );
        } else if let Err(e) = std::fs::rename(&tmp_path, &account.file_path) {
            warn!(
                "gemini creds: failed to rename refreshed token to {}: {}",
                account.file_path.display(),
                e
            );
        }
    }
}

/// Merge the resolved project_id back into the credential file, preserving all
/// other fields. Best-effort: a write failure only means we might try again next time.
pub fn write_back_project(account: &Account, project_id: &str) {
    let raw = match std::fs::read_to_string(&account.file_path) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "gemini creds: cannot re-read {} for project write-back: {}",
                account.file_path.display(),
                e
            );
            return;
        }
    };
    let mut value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "gemini creds: cannot parse {} for project write-back: {}",
                account.file_path.display(),
                e
            );
            return;
        }
    };

    if !value.is_object() {
        warn!(
            "gemini creds: credential file {} is not a JSON object",
            account.file_path.display()
        );
        return;
    }
    value["project_id"] = serde_json::json!(project_id);

    if let Ok(serialized) = serde_json::to_string_pretty(&value) {
        let tmp_path = account.file_path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp_path, serialized) {
            warn!(
                "gemini creds: failed to write project to {}: {}",
                tmp_path.display(),
                e
            );
        } else if let Err(e) = std::fs::rename(&tmp_path, &account.file_path) {
            warn!(
                "gemini creds: failed to rename project to {}: {}",
                account.file_path.display(),
                e
            );
        }
    }
}

/// Lazily resolve and finalize a project ID for the given account if it's missing,
/// writing it back to the credential file and updating the in-memory account struct.
pub async fn lazy_onboard(account: &mut Account, access_token: &str) -> anyhow::Result<()> {
    if !account.project_id.is_empty() {
        return Ok(());
    }

    let _guard = ONBOARD_LOCK.lock().await;
    // Re-read from disk to see if another thread already onboarded this account
    let path = account.file_path.clone();
    if let Some(reloaded) = tokio::task::spawn_blocking(move || parse_account(&path))
        .await
        .unwrap_or(None)
    {
        if !reloaded.project_id.is_empty() {
            account.project_id = reloaded.project_id;
            return Ok(());
        }
    }

    let (user_agent, metadata, load_base, onboard_base) = match account.provider.as_str() {
        GEMINI_CLI => (
            crate::login::GEMINI_LOGIN_USER_AGENT,
            serde_json::json!({
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI",
            }),
            crate::login::CODE_ASSIST,
            crate::login::CODE_ASSIST,
        ),
        ANTIGRAVITY => (
            crate::login::ANTIGRAVITY_USER_AGENT,
            serde_json::json!({ "ideType": "ANTIGRAVITY" }),
            crate::login::CODE_ASSIST_DAILY,
            crate::login::CODE_ASSIST_DAILY,
        ),
        other => anyhow::bail!("unknown provider {other} for lazy onboarding"),
    };

    // Use the dedicated REFRESH_CLIENT (no_proxy) to avoid any proxy loop
    let project_id = crate::login::fetch_project_id(
        &REFRESH_CLIENT,
        access_token,
        user_agent,
        metadata,
        None,
        load_base,
        onboard_base,
        false,
    )
    .await?;

    if project_id.is_empty() {
        anyhow::bail!("lazy onboarding resolved an empty project ID");
    }

    // Write back to the file under REFRESH_LOCK to prevent races with token refreshes
    {
        let _refresh_guard = REFRESH_LOCK.lock().await;
        let account_clone = account.clone();
        let project_id_clone = project_id.clone();
        tokio::task::spawn_blocking(move || {
            write_back_project(&account_clone, &project_id_clone);
        })
        .await?;
    }

    account.project_id = project_id;
    Ok(())
}

/// Failure modes of [`resolve_account`]. Callers map each variant onto their
/// own (provider-specific) error envelope; the `String` payloads are already
/// formatted, user-facing messages.
pub enum AccountError {
    NoCredential,
    RefreshFailed(String),
    OnboardFailed(String),
}

/// Pick the account for `provider`, refresh its token, and lazily onboard a
/// missing Code Assist project id. Shared by the Gemini, Anthropic, and
/// OpenAI request handlers — each surface has its own error envelope, so this
/// only returns the resolved account + token (or an [`AccountError`] for the
/// caller to format). `caller` tags log lines (e.g. `"gemini"`, `"anthropic"`).
pub async fn resolve_account(
    caller: &str,
    provider: &str,
    auth_dirs: &[PathBuf],
) -> Result<(Account, String), AccountError> {
    let dirs = auth_dirs.to_vec();
    let account_provider = provider.to_string();
    let account = tokio::task::spawn_blocking(move || pick_account(&account_provider, &dirs))
        .await
        .unwrap_or(None);
    let mut account = account.ok_or(AccountError::NoCredential)?;

    let access_token = ensure_fresh(&account).await.map_err(|e| {
        warn!(
            "{caller}: token refresh failed for {}: {}",
            account.email, e
        );
        AccountError::RefreshFailed(format!("Auth refresh failed: {e}"))
    })?;

    if account.project_id.is_empty() {
        info!(
            "Project ID not set for provider '{}' (account {}). Attempting lazy onboarding...",
            provider, account.email
        );
        if let Err(e) = lazy_onboard(&mut account, &access_token).await {
            warn!(
                "{caller}: lazy onboarding failed for {}: {}",
                account.email, e
            );
            // Onboarding failures are network/upstream issues (Code Assist
            // unreachable, provisioning timeout, etc.), not bad credentials —
            // a missing credential is already handled above as `NoCredential`.
            return Err(AccountError::OnboardFailed(format!(
                "Lazy onboarding failed: {e}"
            )));
        }
    }

    Ok((account, access_token))
}

/// Get a fresh access token for the Vertex AI provider.
/// Reads local Google Application Default Credentials (ADC), refreshes against Google's token endpoint
/// using in-memory caching and disk-based sync (updating standard application_default_credentials_access_token.json).
pub async fn get_vertex_token() -> anyhow::Result<String> {
    let now = now_ms();

    // 1. Check in-memory cache first
    {
        let cache = VERTEX_TOKEN_CACHE.lock().await;
        if let Some((access_token, expires_at_ms)) = &*cache {
            if *expires_at_ms > now.saturating_add(EXPIRY_BUFFER_MS) {
                return Ok(access_token.clone());
            }
        }
    }

    // 2. Lock refresh process-wide (reuses REFRESH_LOCK to avoid concurrent file system and OAuth races)
    let _guard = REFRESH_LOCK.lock().await;

    // Double check cache in case another thread did the work while we waited on the lock
    {
        let cache = VERTEX_TOKEN_CACHE.lock().await;
        if let Some((access_token, expires_at_ms)) = &*cache {
            if *expires_at_ms > now.saturating_add(EXPIRY_BUFFER_MS) {
                return Ok(access_token.clone());
            }
        }
    }

    // 3. Load standard Google ADC file
    let adc_path = crate::reauth::get_adc_path();
    let adc_raw = match tokio::fs::read_to_string(&adc_path).await {
        Ok(s) => s,
        Err(_) => {
            anyhow::bail!(
                "No Google Application Default Credentials found at {:?}. Run `claude-proxy login vertex` first.",
                adc_path
            );
        }
    };
    let adc: serde_json::Value = serde_json::from_str(&adc_raw)?;

    let refresh_token = adc["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("refresh_token missing from ADC file {:?}", adc_path))?;

    let client_id = adc["client_id"]
        .as_str()
        .unwrap_or(crate::reauth::GOOGLE_CLIENT_ID);
    let client_secret = adc["client_secret"]
        .as_str()
        .unwrap_or(crate::reauth::GOOGLE_CLIENT_SECRET);

    debug!("gemini creds: refreshing vertex token");
    let resp = REFRESH_CLIENT
        .post(TOKEN_ENDPOINT)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Google OAuth refresh returned status {status}: {body}");
    }

    let token_json: serde_json::Value = resp.json().await?;
    let access_token = token_json["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("access_token missing in Google OAuth response"))?
        .to_string();
    let expires_in = token_json["expires_in"].as_u64().unwrap_or(3600);

    let expires_on_ms = now.saturating_add(expires_in.saturating_mul(1000));

    // 4. Update in-memory cache
    {
        let mut cache = VERTEX_TOKEN_CACHE.lock().await;
        *cache = Some((access_token.clone(), expires_on_ms));
    }

    // 5. Update disk-based CLI-compatible token cache so that `claude` CLI and our other interceptors
    // can reuse this fresh token without re-hitting Google.
    // Construct the request body string that matches what our interceptors would key on:
    let request_body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
        crate::oauth_util::percent_encode(refresh_token),
        crate::oauth_util::percent_encode(client_id),
        crate::oauth_util::percent_encode(client_secret)
    );
    let _ = crate::interceptors::save_token_cache(&request_body, &token_json).await;

    Ok(access_token)
}
