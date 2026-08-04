//! Claude Code OAuth credential discovery and refresh.
//!
//! The credential is the one the real `claude` CLI stores. Two sources, in read
//! order:
//!
//! 1. **macOS Keychain** — generic password, service `Claude Code-credentials`,
//!    account `$USER`. Read through the `security` CLI rather than the
//!    Security.framework so there's no new dependency and no code-signing
//!    entanglement (the ACL lands on Apple's signed `security` binary, not ours,
//!    so reads don't raise an authorization prompt).
//! 2. **`~/.claude/.credentials.json`** — same JSON shape, used on platforms
//!    without a Keychain.
//!
//! Shape (other top-level keys — `mcpOAuth`, `pluginSecrets`,
//! `organizationUuid` — exist and must survive a write-back):
//!
//! ```json
//! { "claudeAiOauth": { "accessToken": "sk-ant-oat01-…", "refreshToken": "sk-ant-ort01-…",
//!                      "expiresAt": 1785855177569, "scopes": [...],
//!                      "subscriptionType": "team" } }
//! ```
//!
//! Refresh follows the project-wide invariant ([`crate::reauth`],
//! [`crate::gemini::creds`]): the token POST uses a `no_proxy()` client so it
//! can never loop back through us.

use lazy_static::lazy_static;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, info, warn};

/// Token endpoint the CLI uses.
const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";

/// Claude Code's public OAuth client id.
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Keychain generic-password service name.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Refresh this many ms before the stored expiry.
const EXPIRY_BUFFER_MS: u64 = 60_000;

/// Ceiling on the token-refresh round trip. Held under `REFRESH_LOCK`, so this is
/// also the worst-case stall it can impose on concurrent requests.
const REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

lazy_static! {
    /// Dedicated client for token refresh: `no_proxy()` so a configured
    /// `upstream_proxy` (possibly us) can't create a loop.
    ///
    /// The timeout is load-bearing, not hygiene: the refresh POST runs while
    /// holding `REFRESH_LOCK`, so a server that accepts the connection and never
    /// answers would park every other request behind it indefinitely. Failing
    /// after `REFRESH_TIMEOUT` turns that into one surfaced error per request.
    static ref REFRESH_CLIENT: reqwest::Client = reqwest::Client::builder()
        .no_proxy()
        .timeout(REFRESH_TIMEOUT)
        .build()
        .expect("build claude refresh client");

    /// Serializes refreshes process-wide so concurrent requests hitting an
    /// expired token don't each fire a POST and race the write-back.
    static ref REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());

    /// Account UUID, learned from a refresh response. Only used to fill
    /// `metadata.user_id`; absent until the first refresh of the process.
    static ref ACCOUNT_UUID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
}

/// Where a credential came from, so refreshes write back to the same place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Keychain,
    File(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Credential {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: u64,
    pub subscription_type: String,
    pub source: Source,
}

#[derive(Deserialize)]
struct OauthField {
    #[serde(default, rename = "accessToken")]
    access_token: String,
    #[serde(default, rename = "refreshToken")]
    refresh_token: String,
    #[serde(default, rename = "expiresAt")]
    expires_at: u64,
    #[serde(default, rename = "subscriptionType")]
    subscription_type: String,
}

#[derive(Deserialize)]
struct CredentialFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: OauthField,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `~/.claude/.credentials.json` — the non-Keychain fallback location.
fn credentials_file() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude/.credentials.json"))
}

/// Read the raw Keychain item. Returns `None` when the item is absent, the
/// `security` tool is unavailable, or access was denied.
///
/// Going through the `security` CLI has a useful side effect: the Keychain ACL is
/// evaluated against Apple's signed `security` binary rather than ours, so this
/// does not raise an authorization prompt — verified against a real item. Linking
/// Security.framework directly would put an unsigned binary on the ACL and could
/// block a daemon on an invisible GUI prompt.
fn read_keychain() -> Option<String> {
    let user = std::env::var("USER").ok()?;
    let out = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            &user,
            "-w",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        debug!(
            "claude creds: keychain read failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    let raw = String::from_utf8(out.stdout).ok()?;
    let raw = raw.trim().to_string();
    if raw.is_empty() {
        None
    } else {
        Some(raw)
    }
}

/// Write `contents` back as the Keychain item, replacing it in place (`-U`).
///
/// The JSON travels via argv, which is visible to other local processes. That's
/// consistent with this proxy's documented trusted-local-environment stance (see
/// the "Intentionally trusting" section of AGENTS.md) and with the reference
/// `insert_credentials.sh`; the alternative is linking Security.framework.
fn write_keychain(contents: &str) -> anyhow::Result<()> {
    let user = std::env::var("USER")?;
    let out = Command::new("security")
        .args([
            "add-generic-password",
            "-a",
            &user,
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
            contents,
            "-U",
        ])
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "keychain write failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Read the raw credential JSON from whichever source has it.
fn read_raw() -> Option<(String, Source)> {
    if let Some(raw) = read_keychain() {
        return Some((raw, Source::Keychain));
    }
    let path = credentials_file()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    Some((raw, Source::File(path)))
}

/// Load the Claude Code credential, or `None` if no usable one exists.
pub fn load() -> Option<Credential> {
    let (raw, source) = read_raw()?;
    let parsed: CredentialFile = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "claude creds: {:?} exists but has no usable `claudeAiOauth` object: {}",
                source, e
            );
            return None;
        }
    };
    let o = parsed.claude_ai_oauth;
    if o.access_token.is_empty() && o.refresh_token.is_empty() {
        warn!("claude creds: {:?} has neither an access nor a refresh token", source);
        return None;
    }
    Some(Credential {
        access_token: o.access_token,
        refresh_token: o.refresh_token,
        expires_at_ms: o.expires_at,
        subscription_type: o.subscription_type,
        source,
    })
}

/// [`load`] on the blocking pool. Every credential read on the request path goes
/// through this — `load` shells out to `security` and touches the filesystem, so
/// calling it directly would stall a Tokio worker thread.
pub async fn load_blocking() -> Option<Credential> {
    tokio::task::spawn_blocking(load).await.unwrap_or(None)
}

/// True if the stored access token exists and isn't within the refresh buffer.
fn is_fresh(cred: &Credential) -> bool {
    !cred.access_token.is_empty() && cred.expires_at_ms > now_ms() + EXPIRY_BUFFER_MS
}

/// Account UUID learned from a refresh, if any. Used only for `metadata.user_id`.
pub fn account_uuid() -> Option<String> {
    ACCOUNT_UUID.lock().ok().and_then(|g| g.clone())
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    account: Option<AccountField>,
}

#[derive(Deserialize)]
struct AccountField {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    email_address: String,
}

/// Return a valid access token, refreshing (and writing back) if the stored one
/// is expired or nearly so. `force` skips the freshness check — used to retry
/// once after the API rejects a token we believed was good.
pub async fn ensure_fresh(write_back: bool, force: bool) -> anyhow::Result<String> {
    // Off the executor: `load` runs a `security` subprocess and/or reads a file,
    // and this is on the request path for every generation.
    let cred = load_blocking().await.ok_or_else(|| {
        anyhow::anyhow!(
            "no Claude Code credential found. Sign in with the `claude` CLI, or place the \
             credential JSON in the `{}` Keychain item / ~/.claude/.credentials.json",
            KEYCHAIN_SERVICE
        )
    })?;
    if !force && is_fresh(&cred) {
        return Ok(cred.access_token);
    }

    let _guard = REFRESH_LOCK.lock().await;
    // Always re-read under the lock, `force` included. `force` means "don't trust
    // the access token's expiry", not "don't trust the stored credential": a
    // concurrent refresh that landed while we waited also **rotated the refresh
    // token**, so POSTing the copy captured before the lock fails with
    // `invalid_grant`. That's precisely the 401-storm this path exists to serve,
    // where every in-flight request reaches here at once.
    let previous_access = cred.access_token.clone();
    let cred = load_blocking().await.unwrap_or(cred);
    if is_fresh(&cred) {
        // Under `force` the caller's own token was rejected, so a stored token
        // that merely looks fresh isn't enough — but a token that *differs* from
        // the rejected one is another task's newer refresh, worth returning
        // instead of burning a second rotation. An unchanged one means we really
        // are the first responder and must refresh.
        if !force || cred.access_token != previous_access {
            return Ok(cred.access_token);
        }
    }
    if cred.refresh_token.is_empty() {
        anyhow::bail!("Claude Code credential has no refreshToken and the access token is expired");
    }

    debug!(
        "claude creds: refreshing access token (subscription: {})",
        if cred.subscription_type.is_empty() {
            "unknown"
        } else {
            &cred.subscription_type
        }
    );
    let resp = REFRESH_CLIENT
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": cred.refresh_token,
            "client_id": CLIENT_ID,
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!(
            "Claude token refresh failed ({status}): {body}. Re-run `claude` and sign in again."
        );
    }

    let refreshed: RefreshResponse = resp.json().await?;
    if let Some(account) = &refreshed.account {
        if !account.uuid.is_empty() {
            if let Ok(mut slot) = ACCOUNT_UUID.lock() {
                if slot.as_deref() != Some(account.uuid.as_str()) {
                    info!(
                        "claude creds: refreshed token for {}",
                        if account.email_address.is_empty() {
                            &account.uuid
                        } else {
                            &account.email_address
                        }
                    );
                    *slot = Some(account.uuid.clone());
                }
            }
        }
    }

    let expires_in = refreshed.expires_in.unwrap_or(28_800);
    let new_refresh = refreshed
        .refresh_token
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cred.refresh_token.clone());

    if write_back {
        let access = refreshed.access_token.clone();
        let source = cred.source.clone();
        let _ = tokio::task::spawn_blocking(move || {
            persist(&source, &access, &new_refresh, expires_in)
        })
        .await;
    } else {
        debug!("claude creds: write_back disabled; refreshed token kept in memory only");
    }

    Ok(refreshed.access_token)
}

/// Merge the refreshed token fields back into the credential *without* touching
/// anything else. The Keychain item also carries `mcpOAuth`, `pluginSecrets` and
/// `organizationUuid`, all owned by the real CLI — a whole-object replace would
/// silently destroy them.
fn persist(source: &Source, access: &str, refresh: &str, expires_in: u64) {
    let raw = match source {
        Source::Keychain => read_keychain(),
        Source::File(p) => std::fs::read_to_string(p).ok(),
    };
    let Some(raw) = raw else {
        warn!("claude creds: cannot re-read {:?} for write-back; skipping", source);
        return;
    };
    let mut doc: Value = match serde_json::from_str(&raw) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            warn!("claude creds: {:?} is not a JSON object; skipping write-back", source);
            return;
        }
    };

    // Update in place so unknown sibling keys inside `claudeAiOauth` survive too.
    let slot = doc
        .get_mut("claudeAiOauth")
        .filter(|v| v.is_object())
        .map(|v| v.as_object_mut().expect("checked is_object"));
    let Some(oauth) = slot else {
        warn!("claude creds: {:?} lost its `claudeAiOauth` object; skipping write-back", source);
        return;
    };
    // Overflow here would write an expiry in the past (or wrap to a nonsense
    // value), which turns every later request into a forced refresh — worse than
    // leaving the stored credential alone.
    let Some(expires_at) = expiry_at(now_ms(), expires_in) else {
        warn!(
            "claude creds: expires_in={} overflows the expiry clock; skipping write-back",
            expires_in
        );
        return;
    };
    oauth.insert("accessToken".into(), json!(access));
    oauth.insert("refreshToken".into(), json!(refresh));
    oauth.insert("expiresAt".into(), json!(expires_at));

    let serialized = doc.to_string();
    match source {
        Source::Keychain => match write_keychain(&serialized) {
            Ok(()) => debug!("claude creds: refreshed token written back to Keychain"),
            Err(e) => warn!("claude creds: Keychain write-back failed: {}", e),
        },
        Source::File(path) => match write_file_atomic(path, &serialized) {
            Ok(()) => debug!("claude creds: refreshed token written back to {}", path.display()),
            Err(e) => warn!(
                "claude creds: write-back to {} failed: {}",
                path.display(),
                e
            ),
        },
    }
}

/// Absolute expiry in ms for a token valid `expires_in` seconds from `now_ms`,
/// or `None` if the arithmetic would overflow.
fn expiry_at(now_ms: u64, expires_in_secs: u64) -> Option<u64> {
    expires_in_secs
        .checked_mul(1000)
        .and_then(|ms| now_ms.checked_add(ms))
}

/// Replace `path`'s contents atomically: write a sibling temp file, flush it to
/// disk, then `rename` over the target. A plain `fs::write` truncates in place, so
/// a crash or power loss mid-write leaves a half-written credential file that
/// parses as nothing and locks the user out until they sign in again.
///
/// The temp file is created in the same directory so the rename stays within one
/// filesystem (cross-device renames fail), and inherits the original's permission
/// bits so a 0600 credential file doesn't silently widen to the default umask.
fn write_file_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "credentials".to_string());
    // Same-directory sibling, distinguished by pid so two processes can't collide.
    let tmp = dir.join(format!(".{}.{}.tmp", file_name, std::process::id()));

    let mode = std::fs::metadata(path).ok().map(|m| {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    });

    // Scoped so the handle is closed before the rename.
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(contents.as_bytes())?;
        // Durability: without this the rename can land before the data does, so a
        // power loss could leave an empty file where the credential used to be.
        file.sync_all()
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_is_now_plus_lifetime_in_ms() {
        assert_eq!(expiry_at(1_000, 60), Some(61_000));
        // The value a real refresh returns (8h).
        assert_eq!(expiry_at(1_785_855_177_569, 28_800), Some(1_785_883_977_569));
    }

    #[test]
    fn absurd_lifetime_reports_overflow_instead_of_wrapping() {
        assert_eq!(expiry_at(0, u64::MAX), None);
        assert_eq!(expiry_at(u64::MAX, 1), None);
        // Overflows only in the multiply, not the add.
        assert_eq!(expiry_at(0, u64::MAX / 999), None);
    }

    #[test]
    fn atomic_write_replaces_contents_and_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("claude-proxy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.json");
        std::fs::write(&path, "{\"old\":true}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_file_atomic(&path, "{\"new\":true}").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"new\":true}");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "permission bits must survive the rename");
        // No temp file left behind.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp file leaked: {strays:?}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
