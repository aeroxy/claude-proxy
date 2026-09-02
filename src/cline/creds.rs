//! Cline credential store: discovery, refresh, and merged write-back.
//!
//! The bearer Cline wants is `workos:<jwt>` — a literal prefix on a WorkOS
//! access token minted (or renewed) by Cline's own API. Tokens live about an
//! hour, so refresh is on the request path, not a login-time nicety.
//!
//! Two stores are read, in this order:
//!
//! 1. **The real `cline` CLI's** `providers.json`, so a machine that already
//!    runs Cline needs no `claude-proxy login cline` at all.
//! 2. **Ours**, `~/.config/claude-proxy/auths/cline-*.json`, written by
//!    `login cline`.
//!
//! Same shape as [`crate::claude_oauth::creds`] reading the Keychain before its
//! file fallback, and it carries the same four invariants, each of which was
//! paid for once already there:
//!
//! - the refresh POST uses a `no_proxy()` client, so it can't loop back through
//!   us when `HTTPS_PROXY` points a client at this proxy;
//! - [`REFRESH_LOCK`] serializes refreshes process-wide, so concurrent requests
//!   don't each spend the (rotating) refresh token;
//! - [`MEMORY_TOKENS`] overlays the last refresh this process performed (per
//!   store, so two accounts never trade tokens), so `write_back = false`
//!   doesn't replay a token Cline already rotated away;
//! - **transient ≠ rejected**: only an `invalid_grant`-shaped refusal is treated
//!   as "this credential is dead". A network blip or a 5xx keeps the stored
//!   credential and retries. The Cline client carries a scar from exactly this
//!   — a blip landing just after expiry was read as a rejection and logged out
//!   every Cline process on the machine.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lazy_static::lazy_static;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::config::ClineConfig;

/// Literal prefix Cline's API expects in front of the WorkOS access token.
/// Stored credentials may or may not carry it; we strip on read and add on send.
pub const WORKOS_PREFIX: &str = "workos:";

/// WorkOS public client id for Cline, hardcoded in the cline repo
/// (`sdk/packages/shared/src/runtime/cline-environment.ts`).
pub const WORKOS_CLIENT_ID: &str = "client_01K3A541FN8TA3EPPHTD2325AR";

/// WorkOS device-flow endpoints, used by `login cline`.
pub const WORKOS_DEVICE_AUTH_URL: &str = "https://api.workos.com/user_management/authorize/device";
pub const WORKOS_AUTHENTICATE_URL: &str = "https://api.workos.com/user_management/authenticate";

/// Refresh this many ms before the stored expiry.
const EXPIRY_BUFFER_MS: u64 = 60_000;

/// Ceiling on the refresh round trip. Held under [`REFRESH_LOCK`], so it's also
/// the worst-case stall a hung Cline API can impose on concurrent requests.
const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

lazy_static! {
    /// Dedicated client for token refresh: `no_proxy()` so a configured
    /// `upstream_proxy` (possibly us) can't create a loop, a timeout because
    /// this POST runs while holding [`REFRESH_LOCK`], and no redirects — every
    /// body it sends is a refresh token, and a redirect would replay that body
    /// wherever the response pointed.
    static ref REFRESH_CLIENT: reqwest::Client = reqwest::Client::builder()
        .no_proxy()
        .timeout(REFRESH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build cline refresh client");

    /// Serializes refreshes process-wide so concurrent requests hitting an
    /// expired token don't each fire a POST and race the write-back.
    static ref REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());

    /// The last refresh this process performed **per store**, overlaid onto the
    /// credential read from that store by [`overlay`] when the store is behind.
    /// Keyed by [`Source`] so two `cline-*.json` accounts never trade tokens.
    static ref MEMORY_TOKENS: std::sync::Mutex<HashMap<Source, RefreshedToken>> =
        std::sync::Mutex::new(HashMap::new());
}

#[derive(Debug, Clone)]
struct RefreshedToken {
    access_token: String,
    refresh_token: String,
    /// `0` when Cline's response carried no usable `expiresAt`.
    expires_at_ms: u64,
    /// The refresh token this one was exchanged *for* — a store still holding
    /// it is behind us, whatever its expiry says.
    spent_refresh_token: String,
}

/// Where a credential came from, so a refresh writes back to the same place.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Source {
    /// The real `cline` CLI's `providers.json`. Write-back must **merge**.
    ClineCli(PathBuf),
    /// One of our own `cline-*.json` files under an auth dir.
    Ours(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Credential {
    /// Normalized: the bare JWT, with any `workos:` prefix stripped.
    pub access_token: String,
    pub refresh_token: String,
    /// Unix ms when the access token expires (0 = unknown, i.e. refresh now).
    pub expires_at_ms: u64,
    pub email: String,
    pub source: Source,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Strip the `workos:` prefix if present. Cline's own client normalizes both
/// directions, so either form can be sitting in a store.
pub fn strip_prefix(token: &str) -> &str {
    let t = token.trim();
    match t.split_at_checked(WORKOS_PREFIX.len()) {
        Some((head, rest)) if head.eq_ignore_ascii_case(WORKOS_PREFIX) => rest,
        _ => t,
    }
}

/// The bearer value Cline's API expects.
pub fn bearer(access_token: &str) -> String {
    format!("{WORKOS_PREFIX}{}", strip_prefix(access_token))
}

/// The real CLI's provider settings file, resolved the way the CLI resolves it
/// (`sdk/packages/shared/src/storage/paths.ts`): an explicit path env var wins,
/// then an explicit data dir, then `~/.cline/data`.
pub fn cline_cli_settings_path(cfg: &ClineConfig) -> Option<PathBuf> {
    if let Some(p) = &cfg.settings_path {
        return Some(p.clone());
    }
    if let Some(p) = env_path("CLINE_PROVIDER_SETTINGS_PATH") {
        return Some(p);
    }
    let data_dir = match env_path("CLINE_DATA_DIR") {
        Some(d) => d,
        None => env_path("CLINE_DIR")
            .or_else(|| dirs::home_dir().map(|h| h.join(".cline")))?
            .join("data"),
    };
    Some(data_dir.join("settings").join("providers.json"))
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The `auth` block the CLI persists under `providers.cline.settings`.
#[derive(Deserialize, Default)]
struct AuthField {
    #[serde(default, rename = "accessToken")]
    access_token: String,
    #[serde(default, rename = "refreshToken")]
    refresh_token: String,
    /// Unix **ms**. Absent means "unknown expiry" — treat as expired so the next
    /// resolution refreshes, which is what the CLI does too.
    #[serde(default, rename = "expiresAt")]
    expires_at: u64,
    #[serde(default)]
    metadata: Option<Value>,
}

/// Read the `cline` provider out of the CLI's `providers.json`.
fn parse_cline_cli_store(raw: &str, path: &Path) -> Option<Credential> {
    let root: Value = serde_json::from_str(raw).ok()?;
    let settings = root.pointer("/providers/cline/settings")?;
    let auth: AuthField = serde_json::from_value(settings.get("auth")?.clone()).ok()?;
    let access = strip_prefix(&auth.access_token).to_string();
    if access.is_empty() || auth.refresh_token.trim().is_empty() {
        return None;
    }
    let email = auth
        .metadata
        .as_ref()
        .and_then(|m| m.pointer("/userInfo/email")?.as_str().map(String::from))
        .unwrap_or_default();
    Some(Credential {
        access_token: access,
        refresh_token: auth.refresh_token.trim().to_string(),
        expires_at_ms: auth.expires_at,
        email,
        source: Source::ClineCli(path.to_path_buf()),
    })
}

/// Our own credential file: `{"type":"cline", ...}` in an auth dir.
#[derive(Deserialize)]
struct OurCred {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_at: u64,
}

fn parse_our_cred(raw: &str, path: &Path) -> Option<Credential> {
    let c: OurCred = serde_json::from_str(raw).ok()?;
    if c.kind != "cline" {
        return None;
    }
    let access = strip_prefix(&c.access_token).to_string();
    if access.is_empty() || c.refresh_token.trim().is_empty() {
        return None;
    }
    Some(Credential {
        access_token: access,
        refresh_token: c.refresh_token.trim().to_string(),
        expires_at_ms: c.expires_at,
        email: c.email,
        source: Source::Ours(path.to_path_buf()),
    })
}

/// Filename `login cline` writes to, under [`crate::gemini::creds::our_auth_dir`].
pub fn our_cred_filename(email: &str) -> String {
    let slug: String = email
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' })
        .collect();
    if slug.is_empty() {
        "cline.json".to_string()
    } else {
        format!("cline-{slug}.json")
    }
}

/// Read the stored credential: the real CLI's store first, then ours.
pub fn load(cfg: &ClineConfig, auth_dirs: &[PathBuf]) -> Option<Credential> {
    if let Some(path) = cline_cli_settings_path(cfg) {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            match parse_cline_cli_store(&raw, &path) {
                Some(cred) => return Some(cred),
                None => debug!(
                    "cline creds: {} has no usable `cline` provider credential",
                    path.display()
                ),
            }
        }
    }
    for dir in auth_dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue, // dir absent — fine
        };
        // Sorted, so two `login cline` accounts side by side resolve to the same
        // one on every request rather than in `read_dir` order (which is
        // filesystem-dependent and not stable across platforms).
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect();
        paths.sort();
        for path in paths {
            if let Some(cred) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| parse_our_cred(&raw, &path))
            {
                return Some(cred);
            }
        }
    }
    None
}

/// [`load`] on the blocking pool — it reads files, and this is on the request path.
pub async fn load_blocking(cfg: &ClineConfig, auth_dirs: &[PathBuf]) -> Option<Credential> {
    let cfg = cfg.clone();
    let dirs = auth_dirs.to_vec();
    tokio::task::spawn_blocking(move || load(&cfg, &dirs))
        .await
        .unwrap_or(None)
}

/// Replace `cred`'s token fields with `mem`'s when the store is behind us.
///
/// Two signals, either sufficient. The store still holding the refresh token we
/// already **spent** is the exact one: that token is dead upstream, so the store
/// is stale whatever its expiry says — and this is what carries a rotation whose
/// response had no `expiresAt`. Otherwise expiry orders the two, and serves both
/// directions: our refresh wins over a store we chose not to write to, and a
/// refresh the real `cline` CLI landed *after* ours wins over our cache —
/// correctly, since its rotation spent the refresh token we were holding.
fn overlay(mut cred: Credential, mem: Option<&RefreshedToken>) -> Credential {
    let store_is_behind = |m: &&RefreshedToken| {
        m.spent_refresh_token == cred.refresh_token || m.expires_at_ms > cred.expires_at_ms
    };
    if let Some(mem) = mem.filter(store_is_behind) {
        cred.access_token = mem.access_token.clone();
        cred.refresh_token = mem.refresh_token.clone();
        cred.expires_at_ms = mem.expires_at_ms;
    }
    cred
}

async fn load_effective(cfg: &ClineConfig, auth_dirs: &[PathBuf]) -> Option<Credential> {
    let cred = load_blocking(cfg, auth_dirs).await?;
    let mem = MEMORY_TOKENS
        .lock()
        .ok()
        .and_then(|m| m.get(&cred.source).cloned());
    Some(overlay(cred, mem.as_ref()))
}

/// True if the access token exists and isn't inside the refresh buffer. An
/// unknown expiry (`0`) is deliberately *not* fresh: the CLI treats it the same
/// way, so an unrecognized store shape costs one refresh rather than a 401.
fn is_fresh(cred: &Credential) -> bool {
    !cred.access_token.is_empty() && cred.expires_at_ms > now_ms() + EXPIRY_BUFFER_MS
}

/// Whether the credential read under the lock can be handed back as-is.
///
/// `rejected` is the access token Cline just refused, present only on the
/// post-401 retry. There, "unexpired" isn't sufficient — the rejected token
/// looked unexpired too — but a stored token that *differs* is a newer
/// credential someone else already fetched, worth trying before rotating again.
fn stored_token_is_usable(stored: &str, fresh: bool, rejected: Option<&str>) -> bool {
    fresh
        && match rejected {
            Some(rejected) => strip_prefix(stored) != strip_prefix(rejected),
            None => true,
        }
}

/// Whether a failed refresh means "this refresh token is dead" rather than "try
/// again later".
///
/// Mirrors `ClineOAuthTokenError.isLikelyInvalidGrant` in the cline SDK: an
/// explicit grant/token error code, or a 400/401/403 whose message reads like a
/// rejection. Everything else — timeouts, 5xx, a 429 — is transient, and the
/// caller keeps the stored credential.
fn is_invalid_grant(status: reqwest::StatusCode, body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if lower.contains("invalid_grant") || lower.contains("invalid_token") {
        return true;
    }
    matches!(status.as_u16(), 400 | 401 | 403)
        && ["invalid", "expired", "revoked", "unauthorized"]
            .iter()
            .any(|needle| lower.contains(needle))
}

/// Evict the [`MEMORY_TOKENS`] entry for `source` when it holds the refresh token
/// that was just refused, so the next request reads the store clean instead of
/// replaying a dead token.
///
/// Deliberately scoped to `invalid_grant` and to *this* token: a network error
/// or a 5xx says nothing about validity, and with `write_back = false` the cache
/// is the only copy of the live refresh token — clearing it on a transient
/// failure would strand the process instead of unsticking it.
fn drop_dead_memory_token(source: &Source, rejected_refresh: &str) {
    if let Ok(mut map) = MEMORY_TOKENS.lock() {
        if map.get(source).is_some_and(|m| m.refresh_token == rejected_refresh) {
            warn!(
                "cline creds: the cached refresh token was rejected; dropping it so the next \
                 request re-reads the store"
            );
            map.remove(source);
        }
    }
}

/// `{"success":true,"data":{...}}` — Cline wraps its auth responses the same way
/// it wraps chat completions.
#[derive(Deserialize)]
struct AuthEnvelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<AuthData>,
}

#[derive(Deserialize)]
pub struct AuthData {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(default, rename = "refreshToken")]
    pub refresh_token: Option<String>,
    /// RFC 3339, e.g. `2026-09-01T18:38:50Z`.
    #[serde(default, rename = "expiresAt")]
    pub expires_at: Option<String>,
    #[serde(default, rename = "userInfo")]
    pub user_info: Option<Value>,
}

impl AuthData {
    pub fn email(&self) -> String {
        self.user_info
            .as_ref()
            .and_then(|u| u.get("email")?.as_str().map(String::from))
            .unwrap_or_default()
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp_millis().max(0) as u64)
            .unwrap_or(0)
    }
}

fn unwrap_auth(raw: &str) -> anyhow::Result<AuthData> {
    let env: AuthEnvelope = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("unparseable Cline auth response: {e}"))?;
    match env.data {
        Some(data) if env.success && !data.access_token.is_empty() => Ok(data),
        _ => anyhow::bail!("Cline auth response carried no access token: {raw}"),
    }
}

/// Exchange a pair of WorkOS tokens for Cline credentials. Used by `login cline`
/// after the device flow completes.
pub async fn register(
    base_url: &str,
    workos_access: &str,
    workos_refresh: &str,
) -> anyhow::Result<AuthData> {
    let url = format!("{}/api/v1/auth/register", base_url.trim_end_matches('/'));
    let resp = REFRESH_CLIENT
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&json!({ "accessToken": workos_access, "refreshToken": workos_refresh }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("Cline token registration failed ({status}): {body}");
    }
    unwrap_auth(&body)
}

/// Write a freshly registered credential into `dir`, returning the path.
///
/// Lives here rather than in `login.rs` deliberately: this is the one writer of
/// the shape [`parse_our_cred`] reads, and splitting the two across modules is
/// exactly how a renamed field survives review — nothing else would catch it.
pub fn persist_login(dir: &Path, data: &AuthData) -> anyhow::Result<PathBuf> {
    let refresh = data
        .refresh_token
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("Cline returned no refresh token; nothing to keep the login alive with")
        })?;
    let email = data.email();
    let cred = json!({
        "type": "cline",
        "email": email,
        "access_token": strip_prefix(&data.access_token),
        "refresh_token": refresh,
        "expires_at": data.expires_at_ms(),
    });
    std::fs::create_dir_all(dir)
        .map_err(|e| anyhow::anyhow!("create auth dir {}: {e}", dir.display()))?;
    let path = dir.join(our_cred_filename(&email));
    write_atomic(&path, &cred)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Return a usable **bare** access token, refreshing if needed.
///
/// `rejected` is the token Cline just refused (post-401 retry only). Errors are
/// the caller's cue to surface a 401; the stored credential is left alone unless
/// the refusal was `invalid_grant`-shaped.
pub async fn ensure_fresh(
    cfg: &ClineConfig,
    auth_dirs: &[PathBuf],
    rejected: Option<&str>,
) -> anyhow::Result<String> {
    let cred = load_effective(cfg, auth_dirs).await.ok_or_else(|| {
        anyhow::anyhow!(
            "no Cline credential found. Run `claude-proxy login cline`, or sign in with the \
             `cline` CLI (its credential is read from providers.json)"
        )
    })?;
    if rejected.is_none() && is_fresh(&cred) {
        return Ok(cred.access_token);
    }

    let _guard = REFRESH_LOCK.lock().await;
    // Always re-read under the lock, retry path included: a refresh that landed
    // while we waited also **rotated the refresh token**, so POSTing the copy
    // captured before the lock would fail. That's exactly the 401 storm this
    // path exists to serve, where every in-flight request arrives at once.
    let cred = load_effective(cfg, auth_dirs).await.unwrap_or(cred);
    if stored_token_is_usable(&cred.access_token, is_fresh(&cred), rejected) {
        return Ok(cred.access_token);
    }

    debug!(
        "cline creds: refreshing access token{}",
        if cred.email.is_empty() { String::new() } else { format!(" for {}", cred.email) }
    );
    let url = format!("{}/api/v1/auth/refresh", cfg.base_url.trim_end_matches('/'));
    let resp = REFRESH_CLIENT
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&json!({ "refreshToken": cred.refresh_token, "grantType": "refresh_token" }))
        .send()
        .await?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Transient ≠ rejected. Only a rejection-shaped failure invalidates what
        // we hold; a 5xx or a timeout leaves the store alone for the next try.
        if is_invalid_grant(status, &body) {
            drop_dead_memory_token(&cred.source, &cred.refresh_token);
            anyhow::bail!(
                "Cline refresh token was rejected ({status}): {body}. Run \
                 `claude-proxy login cline` to sign in again."
            );
        }
        anyhow::bail!("Cline token refresh failed ({status}): {body}");
    }

    let data = unwrap_auth(&body)?;
    let access = strip_prefix(&data.access_token).to_string();
    let new_refresh = data
        .refresh_token
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| cred.refresh_token.clone());
    let expires_at_ms = data.expires_at_ms();

    // Cache first, regardless of `write_back` — and regardless of whether the
    // response carried an expiry. The rotated refresh token is the one thing a
    // later request can't recover from the store, and with `write_back = true`
    // this also covers a write-back that failed. Without an expiry the entry is
    // never *fresh*, so every request refreshes again, but each one does so
    // with the live token instead of the one this call just spent.
    if expires_at_ms == 0 {
        warn!(
            "cline creds: refresh response carried no usable expiresAt; the token will be \
             refreshed again on the next request"
        );
    }
    if let Ok(mut map) = MEMORY_TOKENS.lock() {
        map.insert(
            cred.source.clone(),
            RefreshedToken {
                access_token: access.clone(),
                refresh_token: new_refresh.clone(),
                expires_at_ms,
                spent_refresh_token: cred.refresh_token.clone(),
            },
        );
    }

    if cfg.write_back {
        let source = cred.source.clone();
        let (a, r) = (access.clone(), new_refresh.clone());
        let email = if data.email().is_empty() { cred.email.clone() } else { data.email() };
        let _ = tokio::task::spawn_blocking(move || {
            persist(&source, &a, &r, expires_at_ms, &email)
        })
        .await;
    } else {
        debug!("cline creds: write_back disabled; refreshed token kept in memory only");
    }

    Ok(access)
}

/// Merge the refreshed tokens back into the store they came from.
///
/// For the real CLI's `providers.json` this **must** be a merge, never a
/// replace: the file also holds the selected model, the model catalog, custom
/// headers, and every other provider the user has configured. Same invariant as
/// [`crate::claude_oauth::creds::persist`] and the Keychain item.
fn persist(source: &Source, access: &str, refresh: &str, expires_at_ms: u64, email: &str) {
    match source {
        Source::ClineCli(path) => {
            let Ok(raw) = std::fs::read_to_string(path) else {
                warn!("cline creds: cannot re-read {} for write-back; skipping", path.display());
                return;
            };
            let Ok(mut root) = serde_json::from_str::<Value>(&raw) else {
                warn!("cline creds: {} is not valid JSON; skipping write-back", path.display());
                return;
            };
            let Some(auth) = root.pointer_mut("/providers/cline/settings/auth") else {
                warn!(
                    "cline creds: {} no longer has providers.cline.settings.auth; skipping \
                     write-back",
                    path.display()
                );
                return;
            };
            // The CLI stores the prefixed form here; keep it byte-identical to
            // what it writes so a round trip through us is invisible to it.
            auth["accessToken"] = json!(bearer(access));
            auth["refreshToken"] = json!(refresh);
            if expires_at_ms > 0 {
                auth["expiresAt"] = json!(expires_at_ms);
            }
            log_write(path, write_atomic(path, &root));
        }
        Source::Ours(path) => {
            let mut value = std::fs::read_to_string(path)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .filter(|v| v.is_object())
                .unwrap_or_else(|| json!({}));
            value["type"] = json!("cline");
            value["email"] = json!(email);
            value["access_token"] = json!(access);
            value["refresh_token"] = json!(refresh);
            if expires_at_ms > 0 {
                value["expires_at"] = json!(expires_at_ms);
            }
            log_write(path, write_atomic(path, &value));
        }
    }
}

fn log_write(path: &Path, result: std::io::Result<()>) {
    if let Err(e) = result {
        warn!("cline creds: write-back to {} failed: {}", path.display(), e);
    }
}

/// Serialize and atomically replace `path`, via the hardened writer
/// [`crate::claude_oauth::creds::write_file_atomic`]: same-dir temp, fsync,
/// then rename — and the target's permission bits are **inherited** (0600 for a
/// new file). That last part matters twice here: `login cline` writes a refresh
/// token, and the request-path write-back renames over the real `cline` CLI's
/// own `providers.json`, whose mode is the CLI's to set, not ours to widen.
///
/// Returns the error rather than only logging it: `login cline` must not print
/// "Saved credentials" over a write that didn't happen. The request-path
/// write-back ([`persist`]) logs and carries on, since the token is cached.
pub fn write_atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    let serialized = serde_json::to_string_pretty(value)?;
    crate::claude_oauth::creds::write_file_atomic(path, &serialized)
}

/// Log line for startup, so an operator can see which store is in play.
pub fn describe(cred: &Credential) -> String {
    let who = if cred.email.is_empty() { "unknown account" } else { &cred.email };
    match &cred.source {
        Source::ClineCli(p) => format!("{who} (from the cline CLI store {})", p.display()),
        Source::Ours(p) => format!("{who} (from {})", p.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(expires_at_ms: u64) -> Credential {
        Credential {
            access_token: "tok".into(),
            refresh_token: "r".into(),
            expires_at_ms,
            email: String::new(),
            source: Source::Ours(PathBuf::from("/x.json")),
        }
    }

    /// Opt-in live check of the `login cline` tail, **without a browser**.
    ///
    /// A device flow needs a human for exactly one thing: obtaining the first
    /// WorkOS token pair. Everything after it — renew the pair on a refresh
    /// grant, register with Cline, write our credential file, read it back
    /// through the real loader — runs unattended, so the part of `login cline`
    /// that a browser would otherwise gate is checkable here.
    ///
    /// `#[ignore]`d: it needs a live Cline account and the network, so it never
    /// runs in a plain `cargo test`. WorkOS **rotates** the refresh token on
    /// every call, so the test prints the next one to use — a stale value gives
    /// `invalid_grant`, not a code failure.
    ///
    ///   CLINE_TEST_WORKOS_REFRESH=<token> cargo test login_tail -- --ignored --nocapture
    #[test]
    #[ignore]
    fn login_tail_round_trips_against_the_live_api() {
        let Ok(workos_refresh) = std::env::var("CLINE_TEST_WORKOS_REFRESH") else {
            panic!("set CLINE_TEST_WORKOS_REFRESH");
        };
        let cfg = ClineConfig::default();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            // 1. Renew the WorkOS pair — no browser, this is a plain refresh grant.
            let resp = REFRESH_CLIENT
                .post(WORKOS_AUTHENTICATE_URL)
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", workos_refresh.as_str()),
                    ("client_id", WORKOS_CLIENT_ID),
                ])
                .send()
                .await
                .expect("workos refresh");
            let status = resp.status();
            let w: Value = resp.json().await.expect("workos json");
            assert!(status.is_success(), "workos refresh failed: {status} {w}");
            let wa = w["access_token"].as_str().expect("workos access_token");
            let wr = w["refresh_token"].as_str().expect("workos refresh_token");
            println!("workos renewed; next CLINE_TEST_WORKOS_REFRESH={wr}");

            // 2. Exchange for Cline credentials — the step after the poll.
            let data = register(&cfg.base_url, wa, wr).await.expect("cline register");
            println!("registered as {} expiresAt {:?}", data.email(), data.expires_at);

            // 3. Write the credential file exactly as `login cline` does.
            let dir = std::env::temp_dir().join(format!("cline-login-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let path = persist_login(&dir, &data).expect("persist");
            println!("wrote {}", path.display());

            // 4. Read it back through the real loader, with the CLI store absent.
            let cfg = ClineConfig {
                settings_path: Some(dir.join("no-such-providers.json")),
                ..cfg
            };
            let cred = load(&cfg, std::slice::from_ref(&dir)).expect("credential is discoverable");
            assert_eq!(cred.email, data.email());
            assert!(!cred.refresh_token.is_empty(), "refresh token persisted");
            assert!(
                !cred.access_token.starts_with("workos:"),
                "stored bare, prefixed on send"
            );
            assert!(
                cred.expires_at_ms > now_ms() + EXPIRY_BUFFER_MS,
                "expiry is in the future: {} vs now {}",
                cred.expires_at_ms,
                now_ms()
            );
            assert!(is_fresh(&cred), "usable without a refresh");
            println!("round trip ok; expires_at_ms={}", cred.expires_at_ms);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn strips_the_workos_prefix_in_either_case() {
        assert_eq!(strip_prefix("workos:abc"), "abc");
        assert_eq!(strip_prefix("WorkOS:abc"), "abc");
        assert_eq!(strip_prefix("abc"), "abc");
        assert_eq!(bearer("workos:abc"), "workos:abc");
        assert_eq!(bearer("abc"), "workos:abc");
    }

    #[test]
    fn unknown_expiry_is_not_fresh() {
        assert!(!is_fresh(&cred(0)));
        assert!(is_fresh(&cred(now_ms() + 10 * 60_000)));
        assert!(!is_fresh(&cred(now_ms() + 1_000)), "inside the refresh buffer");
    }

    #[test]
    fn a_transient_failure_is_not_a_rejection() {
        use reqwest::StatusCode as S;
        // The scar case: a 5xx or a timeout must never read as "logged out".
        assert!(!is_invalid_grant(S::INTERNAL_SERVER_ERROR, "upstream boom"));
        assert!(!is_invalid_grant(S::BAD_GATEWAY, ""));
        assert!(!is_invalid_grant(S::TOO_MANY_REQUESTS, "slow down"));
        // Cline's own 500 for a model that produced nothing — not an auth problem.
        assert!(!is_invalid_grant(
            S::INTERNAL_SERVER_ERROR,
            r#"{"error":"empty response content","success":false}"#
        ));

        assert!(is_invalid_grant(S::BAD_REQUEST, r#"{"error":"invalid_grant"}"#));
        assert!(is_invalid_grant(
            S::UNAUTHORIZED,
            "Unauthorized: Please make sure you're using the latest version of Cline"
        ));
        assert!(is_invalid_grant(S::FORBIDDEN, "refresh token revoked"));
    }

    #[test]
    fn the_memory_overlay_wins_when_the_store_is_behind() {
        let mem = RefreshedToken {
            access_token: "new".into(),
            refresh_token: "new-r".into(),
            expires_at_ms: 2_000,
            spent_refresh_token: "spent".into(),
        };
        // Expiry ordering, both directions.
        assert_eq!(overlay(cred(1_000), Some(&mem)).access_token, "new");
        assert_eq!(overlay(cred(3_000), Some(&mem)).access_token, "tok");
        assert_eq!(overlay(cred(0), None).access_token, "tok");

        // The store still holds the refresh token we exchanged: it is behind us
        // no matter what its expiry says — even when ours is unknown.
        let no_expiry = RefreshedToken { expires_at_ms: 0, ..mem.clone() };
        let stale = Credential { refresh_token: "spent".into(), ..cred(9_000) };
        let out = overlay(stale, Some(&no_expiry));
        assert_eq!(out.access_token, "new");
        assert_eq!(out.refresh_token, "new-r", "the live rotation, not the dead one");
        // ...but a store holding some *other* token is not overridden by an
        // expiry-less entry: it may be a fresher rotation by the real CLI.
        assert_eq!(overlay(cred(9_000), Some(&no_expiry)).access_token, "tok");
    }

    /// Two accounts on disk must never trade tokens: the overlay is per store.
    #[test]
    fn the_memory_overlay_is_scoped_to_the_store_it_came_from() {
        let a = Source::Ours(PathBuf::from("/a.json"));
        let b = Source::Ours(PathBuf::from("/b.json"));
        let mut map = MEMORY_TOKENS.lock().unwrap();
        map.insert(
            a.clone(),
            RefreshedToken {
                access_token: "a-new".into(),
                refresh_token: "a-r2".into(),
                expires_at_ms: u64::MAX,
                spent_refresh_token: "a-r1".into(),
            },
        );
        let entry_for = |s: &Source| map.get(s).cloned();
        assert_eq!(overlay(Credential { source: a.clone(), ..cred(1) }, entry_for(&a).as_ref()).access_token, "a-new");
        assert_eq!(overlay(Credential { source: b.clone(), ..cred(1) }, entry_for(&b).as_ref()).access_token, "tok");
        map.remove(&a);
    }

    #[test]
    fn the_retry_path_refuses_the_token_that_was_just_rejected() {
        assert!(stored_token_is_usable("a", true, None));
        assert!(!stored_token_is_usable("a", false, None));
        assert!(!stored_token_is_usable("a", true, Some("a")));
        // Prefixed and bare forms of the same token are the same token.
        assert!(!stored_token_is_usable("a", true, Some("workos:a")));
        // A different stored token is someone else's fresher credential.
        assert!(stored_token_is_usable("b", true, Some("a")));
    }

    #[test]
    fn reads_the_cline_cli_store_shape() {
        let raw = r#"{
            "version": 1,
            "lastUsedProvider": "cline",
            "providers": {
                "cline": {
                    "settings": {
                        "provider": "cline",
                        "auth": {
                            "accessToken": "workos:jwt",
                            "refreshToken": "rt",
                            "expiresAt": 1899578924000,
                            "accountId": "testId",
                            "metadata": { "userInfo": { "email": "a@b.c" } }
                        },
                        "model": "anthropic/claude-sonnet-4.6"
                    }
                }
            }
        }"#;
        let c = parse_cline_cli_store(raw, Path::new("/p.json")).expect("parses");
        assert_eq!(c.access_token, "jwt", "the workos: prefix is normalized off");
        assert_eq!(c.refresh_token, "rt");
        assert_eq!(c.expires_at_ms, 1_899_578_924_000);
        assert_eq!(c.email, "a@b.c");
    }

    #[test]
    fn a_store_without_a_cline_login_is_not_a_credential() {
        assert!(parse_cline_cli_store(r#"{"providers":{}}"#, Path::new("/p.json")).is_none());
        assert!(parse_cline_cli_store(
            r#"{"providers":{"cline":{"settings":{"auth":{"accessToken":"workos:j"}}}}}"#,
            Path::new("/p.json")
        )
        .is_none(), "no refresh token means nothing to keep alive");
    }

    #[test]
    fn write_back_to_the_cli_store_preserves_everything_else() {
        let dir = std::env::temp_dir().join(format!("cline-cred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("providers.json");
        std::fs::write(
            &path,
            r#"{"version":1,"lastUsedProvider":"cline","providers":{
                "cline":{"settings":{"provider":"cline","model":"anthropic/x",
                    "auth":{"accessToken":"workos:old","refreshToken":"old-r","expiresAt":1,
                            "accountId":"keep-me"}},
                 "tokenSource":"oauth"},
                "openrouter":{"settings":{"apiKey":"sk-keep"}}}}"#,
        )
        .unwrap();

        persist(&Source::ClineCli(path.clone()), "new", "new-r", 42, "a@b.c");

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let auth = v.pointer("/providers/cline/settings/auth").unwrap();
        assert_eq!(auth["accessToken"], "workos:new", "stored in the CLI's own form");
        assert_eq!(auth["refreshToken"], "new-r");
        assert_eq!(auth["expiresAt"], 42);
        assert_eq!(auth["accountId"], "keep-me", "untouched sibling keys survive");
        assert_eq!(v["providers"]["cline"]["settings"]["model"], "anthropic/x");
        assert_eq!(v["providers"]["cline"]["tokenSource"], "oauth");
        assert_eq!(v["providers"]["openrouter"]["settings"]["apiKey"], "sk-keep");
        assert_eq!(v["lastUsedProvider"], "cline");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `providers.json` is the real CLI's file: a write-back must leave its mode
    /// as the CLI set it, and a file `login cline` creates must not be
    /// world-readable — it holds a refresh token.
    #[test]
    fn write_back_keeps_the_stores_mode_and_new_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("cline-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let cli = dir.join("providers.json");
        std::fs::write(&cli, r#"{"providers":{"cline":{"settings":{"auth":{}}}}}"#).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o600)).unwrap();
        persist(&Source::ClineCli(cli.clone()), "a", "r", 1, "");
        let mode = std::fs::metadata(&cli).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the CLI's own mode must survive our rename");

        let ours = dir.join("cline-new.json");
        write_atomic(&ours, &json!({"type": "cline"})).unwrap();
        let mode = std::fs::metadata(&ours).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a new credential file must not follow the umask");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two accounts on disk resolve to the same one every time, not to whichever
    /// `read_dir` happens to yield first.
    #[test]
    fn multiple_own_credentials_resolve_deterministically() {
        let dir = std::env::temp_dir().join(format!("cline-multi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, email) in [("cline-zed.json", "z@x"), ("cline-amy.json", "a@x")] {
            std::fs::write(
                dir.join(name),
                json!({"type":"cline","email":email,"access_token":"t","refresh_token":"r"})
                    .to_string(),
            )
            .unwrap();
        }
        let cfg = ClineConfig {
            settings_path: Some(dir.join("no-such-providers.json")),
            ..ClineConfig::default()
        };
        let picked = load(&cfg, std::slice::from_ref(&dir)).expect("a credential");
        assert_eq!(picked.email, "a@x", "alphabetical by file name");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
