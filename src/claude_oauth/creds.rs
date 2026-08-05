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

    /// The last refresh this process performed, overlaid onto the stored
    /// credential by [`overlay`] when it's the newer of the two.
    ///
    /// Load-bearing for `write_back = false`: without it, every later request
    /// re-reads the untouched store, finds the same expired token, and POSTs a
    /// refresh token Anthropic already rotated away — `invalid_grant` on
    /// everything after the first refresh.
    static ref MEMORY_TOKEN: std::sync::Mutex<Option<RefreshedToken>> = std::sync::Mutex::new(None);
}

/// A refresh result held only in this process. `expires_at_ms` is what orders it
/// against the stored credential.
#[derive(Debug, Clone)]
struct RefreshedToken {
    access_token: String,
    refresh_token: String,
    expires_at_ms: u64,
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

/// Parse one source's raw JSON into a credential, or `None` (with a reason
/// logged) when it holds nothing usable.
fn parse_credential(raw: &str, source: Source) -> Option<Credential> {
    let parsed: CredentialFile = match serde_json::from_str(raw) {
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

/// Load the Claude Code credential, or `None` if no usable one exists.
///
/// Each source is parsed independently so an *unusable* first source falls
/// through to the second, not just an absent one. A Keychain item left behind by
/// an older sign-in — truncated, or holding an emptied `claudeAiOauth` — would
/// otherwise permanently shadow a perfectly good `~/.claude/.credentials.json`
/// with no way out but deleting the item by hand.
pub fn load() -> Option<Credential> {
    if let Some(raw) = read_keychain() {
        if let Some(cred) = parse_credential(&raw, Source::Keychain) {
            return Some(cred);
        }
        warn!("claude creds: Keychain item is unusable; falling back to the credentials file");
    }
    let path = credentials_file()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    parse_credential(&raw, Source::File(path))
}

/// [`load`] on the blocking pool. Every credential read on the request path goes
/// through this — `load` shells out to `security` and touches the filesystem, so
/// calling it directly would stall a Tokio worker thread.
pub async fn load_blocking() -> Option<Credential> {
    tokio::task::spawn_blocking(load).await.unwrap_or(None)
}

/// Replace `cred`'s token fields with `mem`'s when `mem` is the newer of the two.
///
/// Ordering by expiry is what makes one rule serve both directions: our own
/// in-memory refresh wins over a store we chose not to write to, and a refresh
/// the real `claude` CLI (or another proxy process) landed *after* ours wins over
/// our cache — correctly, because its rotation spent the refresh token we held.
///
/// This rests on "later refresh ⇒ later expiry", which holds while the endpoint
/// returns a constant `expires_in` but isn't a version number: two refreshes in
/// the same millisecond, a backwards clock step, or a later refresh handed a
/// *shorter* lifetime can each order the pair wrong and leave us overlaying a
/// refresh token the CLI already rotated away. Tracking the parent token instead
/// was considered and rejected: with `write_back = false` the store stays frozen
/// at the token our chain started from, so "overlay only while the store still
/// holds my parent" stops applying after our second refresh and re-breaks the
/// case [`MEMORY_TOKEN`] exists for — correcting that needs a set of every token
/// we've spent, which is a lot of machinery for a mis-order this narrow. What
/// makes the narrow case survivable is that it is no longer *sticky*:
/// [`drop_dead_memory_token`] evicts the cache the first time its refresh token
/// is refused, so a mis-order costs one failed request instead of wedging the
/// process until the CLI refreshes again.
fn overlay(mut cred: Credential, mem: Option<&RefreshedToken>) -> Credential {
    if let Some(mem) = mem.filter(|m| m.expires_at_ms > cred.expires_at_ms) {
        cred.access_token = mem.access_token.clone();
        cred.refresh_token = mem.refresh_token.clone();
        cred.expires_at_ms = mem.expires_at_ms;
    }
    cred
}

/// [`load_blocking`] with [`overlay`] applied — the credential [`ensure_fresh`]
/// actually reasons about.
async fn load_effective() -> Option<Credential> {
    let cred = load_blocking().await?;
    let mem = MEMORY_TOKEN.lock().ok().and_then(|g| g.clone());
    Some(overlay(cred, mem.as_ref()))
}

/// True if the stored access token exists and isn't within the refresh buffer.
fn is_fresh(cred: &Credential) -> bool {
    !cred.access_token.is_empty() && cred.expires_at_ms > now_ms() + EXPIRY_BUFFER_MS
}

/// Whether the credential just read under the refresh lock can be handed back
/// as-is, rather than spending a refresh.
///
/// `rejected` is the access token Anthropic just refused, present only on the
/// post-401 retry. On that path "unexpired" is not sufficient — that token looked
/// unexpired too — but a stored token that *differs* from the rejected one is a
/// newer credential someone else already fetched (a concurrent request, or the
/// real `claude` CLI refreshing alongside us, possibly before this call even
/// began), so it's worth trying before rotating again.
fn stored_token_is_usable(stored: &str, fresh: bool, rejected: Option<&str>) -> bool {
    fresh
        && match rejected {
            Some(rejected) => stored != rejected,
            None => true,
        }
}

/// Whether a failed refresh means "this refresh token is dead" as opposed to
/// "try again later". Same shape the Google path keys on in [`crate::proxy`]:
/// a 400 whose body carries `"error": "invalid_grant"`.
fn is_invalid_grant(status: reqwest::StatusCode, body: &str) -> bool {
    status.as_u16() == 400
        && serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|json| json.get("error")?.as_str().map(String::from))
            .as_deref()
            == Some("invalid_grant")
}

/// Evict [`MEMORY_TOKEN`] when it holds the refresh token that was just refused,
/// so the next request reads the store clean instead of replaying a dead token.
///
/// Without this, a cached pair that loses its race with the real CLI stays
/// authoritative for as long as its expiry beats the store's: every later request
/// overlays it, POSTs the rotated-away refresh token, and fails identically. The
/// process would stay wedged until the CLI refreshed again (pushing the store's
/// expiry past ours) or the proxy restarted.
///
/// Deliberately scoped to `invalid_grant` and to *this* token: a network error or
/// a 5xx says nothing about the token's validity, and with `write_back = false`
/// the cache is the only copy of the live refresh token — clearing it on a
/// transient failure would strand the process instead of unsticking it.
fn drop_dead_memory_token(rejected_refresh: &str) {
    if let Ok(mut slot) = MEMORY_TOKEN.lock() {
        if slot.as_ref().is_some_and(|m| m.refresh_token == rejected_refresh) {
            warn!(
                "claude creds: the cached refresh token was rejected (invalid_grant); dropping it \
                 so the next request re-reads the store"
            );
            *slot = None;
        }
    }
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
/// is expired or nearly so.
///
/// `rejected` selects the mode. `None` is the normal path: a stored token that
/// isn't near expiry is returned untouched. `Some(token)` is the post-401 retry —
/// pass the access token the API just refused, which is what lets this tell
/// "nobody has fixed this yet, refresh" apart from "someone already replaced it,
/// use theirs" (see [`stored_token_is_usable`]).
pub async fn ensure_fresh(write_back: bool, rejected: Option<&str>) -> anyhow::Result<String> {
    // Off the executor: `load` runs a `security` subprocess and/or reads a file,
    // and this is on the request path for every generation.
    let cred = load_effective().await.ok_or_else(|| {
        anyhow::anyhow!(
            "no Claude Code credential found. Sign in with the `claude` CLI, or place the \
             credential JSON in the `{}` Keychain item / ~/.claude/.credentials.json",
            KEYCHAIN_SERVICE
        )
    })?;
    if rejected.is_none() && is_fresh(&cred) {
        return Ok(cred.access_token);
    }

    let _guard = REFRESH_LOCK.lock().await;
    // Always re-read under the lock, retry path included. A retry distrusts the
    // access token's *expiry*, not the stored credential: a refresh that landed
    // while we waited also **rotated the refresh token**, so POSTing the copy
    // captured before the lock fails with `invalid_grant`. That's precisely the
    // 401-storm this path exists to serve, where every in-flight request arrives
    // at once.
    let cred = load_effective().await.unwrap_or(cred);
    if stored_token_is_usable(&cred.access_token, is_fresh(&cred), rejected) {
        return Ok(cred.access_token);
    }
    if cred.refresh_token.is_empty() {
        // Two genuinely different dead ends — the retry path gets here with an
        // access token that may be perfectly unexpired, just refused.
        match rejected {
            Some(_) => anyhow::bail!(
                "Claude Code credential has no refreshToken, and the access token was rejected \
                 upstream — nothing left to recover with. Sign in again with the `claude` CLI."
            ),
            None => anyhow::bail!(
                "Claude Code credential has no refreshToken and the access token is expired"
            ),
        }
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
        if is_invalid_grant(status, &body) {
            drop_dead_memory_token(&cred.refresh_token);
        }
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

    // Cache first, regardless of `write_back`: the rotated refresh token is the
    // one thing a later request can't recover from the store, and with
    // `write_back = true` this also covers a write-back that failed.
    match expiry_at(now_ms(), expires_in) {
        Some(expires_at) => {
            if let Ok(mut slot) = MEMORY_TOKEN.lock() {
                *slot = Some(RefreshedToken {
                    access_token: refreshed.access_token.clone(),
                    refresh_token: new_refresh.clone(),
                    expires_at_ms: expires_at,
                });
            }
        }
        // Same call as `persist` makes: a bogus lifetime must not poison the
        // expiry clock. This request still gets its token; the next one refreshes.
        None => warn!(
            "claude creds: expires_in={} overflows the expiry clock; not caching the refreshed token",
            expires_in
        ),
    }

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

    // Inherit the original's bits; fall back to 0600 rather than the process
    // umask, which would typically publish a bearer token as 0644.
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o777)
            .unwrap_or(0o600)
    };

    // Scoped so the handle is closed before the rename.
    let write_result = (|| -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        // A leftover temp from a killed run would otherwise be reused with
        // whatever mode it already had, so clear the way and demand a fresh
        // create — the mode is then applied by `open` itself, before the file can
        // ever hold credentials.
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)?;
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
    // `sync_all` above makes the *contents* durable; the rename that publishes
    // them is a directory-entry change, and that needs its own fsync or a crash
    // can lose the swap and leave the old file behind. Only a warning if it
    // fails: the rename already succeeded, so the write did happen, and
    // returning an error here would make the caller log a write-back failure for
    // a write that landed.
    if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
        warn!(
            "claude creds: wrote {} but could not fsync {} ({}); the replacement may not survive a \
             crash",
            path.display(),
            dir.display(),
            e
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_path_accepts_any_unexpired_stored_token() {
        assert!(stored_token_is_usable("tok-a", true, None));
        assert!(!stored_token_is_usable("tok-a", false, None));
    }

    #[test]
    fn retry_path_refreshes_when_the_store_still_holds_the_rejected_token() {
        // Unexpired by the clock, but it's the very token the API refused — the
        // whole reason we're on the retry path. Must refresh.
        assert!(!stored_token_is_usable("tok-a", true, Some("tok-a")));
    }

    #[test]
    fn retry_path_reuses_a_token_someone_else_already_rotated_in() {
        // A different stored token means a concurrent request — or the real
        // `claude` CLI, possibly before this call began — already refreshed.
        // Reuse it instead of burning another rotation.
        assert!(stored_token_is_usable("tok-b", true, Some("tok-a")));
        // ...but only if it's actually usable.
        assert!(!stored_token_is_usable("tok-b", false, Some("tok-a")));
    }

    fn stored(access: &str, refresh: &str, expires_at_ms: u64) -> Credential {
        Credential {
            access_token: access.into(),
            refresh_token: refresh.into(),
            expires_at_ms,
            subscription_type: "team".into(),
            source: Source::Keychain,
        }
    }

    #[test]
    fn no_in_memory_refresh_leaves_the_store_untouched() {
        let c = overlay(stored("tok-a", "ref-a", 100), None);
        assert_eq!(c.access_token, "tok-a");
        assert_eq!(c.refresh_token, "ref-a");
        assert_eq!(c.expires_at_ms, 100);
    }

    #[test]
    fn our_refresh_wins_over_a_store_we_did_not_write_to() {
        // `write_back = false`: the store still holds the expired token and the
        // refresh token we already spent. Both must come from memory.
        let mem = RefreshedToken {
            access_token: "tok-b".into(),
            refresh_token: "ref-b".into(),
            expires_at_ms: 500,
        };
        let c = overlay(stored("tok-a", "ref-a", 100), Some(&mem));
        assert_eq!(c.access_token, "tok-b");
        assert_eq!(c.refresh_token, "ref-b");
        assert_eq!(c.expires_at_ms, 500);
        // Everything not part of the refresh is left alone.
        assert_eq!(c.subscription_type, "team");
        assert_eq!(c.source, Source::Keychain);
    }

    #[test]
    fn a_newer_store_wins_over_our_cache() {
        // Someone refreshed after us (the real CLI, or write-back landing), which
        // spent the refresh token we cached — theirs is the only usable pair.
        let mem = RefreshedToken {
            access_token: "tok-b".into(),
            refresh_token: "ref-b".into(),
            expires_at_ms: 500,
        };
        let c = overlay(stored("tok-c", "ref-c", 900), Some(&mem));
        assert_eq!(c.access_token, "tok-c");
        assert_eq!(c.refresh_token, "ref-c");
        assert_eq!(c.expires_at_ms, 900);
        // Equal expiry is not newer: the store is the more authoritative copy.
        let c = overlay(stored("tok-c", "ref-c", 500), Some(&mem));
        assert_eq!(c.access_token, "tok-c");
    }

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

    #[test]
    fn a_brand_new_credential_file_is_created_private() {
        use std::os::unix::fs::PermissionsExt;

        // No original to copy bits from — must land on 0600, not the umask
        // default, since the very first byte written is a bearer token.
        let dir = std::env::temp_dir().join(format!("claude-proxy-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("creds.json");
        assert!(!path.exists());

        write_file_atomic(&path, "{\"claudeAiOauth\":{}}").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "new credential file must not be world-readable");

        std::fs::remove_dir_all(&dir).ok();
    }

    fn oauth_json(access: &str, refresh: &str) -> String {
        format!(
            "{{\"claudeAiOauth\":{{\"accessToken\":\"{access}\",\"refreshToken\":\"{refresh}\"}}}}"
        )
    }

    /// The reason `load` parses each source instead of committing to whichever
    /// one merely *answered*: an unusable first source must not shadow the second.
    #[test]
    fn an_unusable_source_yields_none_so_the_next_one_gets_a_turn() {
        assert!(parse_credential("not json at all", Source::Keychain).is_none());
        assert!(parse_credential("{\"mcpOAuth\":{}}", Source::Keychain).is_none());
        assert!(parse_credential(&oauth_json("", ""), Source::Keychain).is_none());

        // One token is enough to be usable — an expired access token with a live
        // refresh token is the normal case on a cold start.
        let cred = parse_credential(&oauth_json("", "ref-a"), Source::Keychain)
            .expect("a refresh token alone is usable");
        assert_eq!(cred.refresh_token, "ref-a");
        assert_eq!(cred.source, Source::Keychain);
    }

    #[test]
    fn only_a_400_invalid_grant_condemns_the_refresh_token() {
        use reqwest::StatusCode;
        let dead = "{\"error\":\"invalid_grant\"}";
        assert!(is_invalid_grant(StatusCode::BAD_REQUEST, dead));
        // Transient or unrelated failures must not evict the cache — with
        // `write_back = false` it holds the only live refresh token.
        assert!(!is_invalid_grant(StatusCode::INTERNAL_SERVER_ERROR, dead));
        assert!(!is_invalid_grant(StatusCode::BAD_REQUEST, "{\"error\":\"invalid_client\"}"));
        assert!(!is_invalid_grant(StatusCode::BAD_REQUEST, "gateway timeout, not json"));
    }

    /// Eviction is keyed on the *rejected* token so a refusal of the store's
    /// credential can't discard an unrelated cached one.
    #[test]
    fn eviction_only_drops_the_token_that_was_actually_refused() {
        *MEMORY_TOKEN.lock().unwrap() = Some(RefreshedToken {
            access_token: "tok-a".into(),
            refresh_token: "ref-a".into(),
            expires_at_ms: 900,
        });

        drop_dead_memory_token("ref-other");
        assert!(MEMORY_TOKEN.lock().unwrap().is_some(), "someone else's failure");

        drop_dead_memory_token("ref-a");
        assert!(MEMORY_TOKEN.lock().unwrap().is_none(), "our token was refused");
    }
}
