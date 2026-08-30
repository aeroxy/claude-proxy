//! Gemini Enterprise / AntiGravity team coding seat, served over the
//! `businessaicode` API as the `aicode/<experience>` provider.
//!
//! A different upstream from the `antigravity` provider next door, despite the
//! shared client identity: `businessaicode.googleapis.com` (or
//! `businessaicode.<location>.rep.googleapis.com`), a
//! `/v1beta/projects/<p>/locations/<l>:streamGenerateContent` path, and a
//! **flat, field-allowlisted** native-Gemini body with no `model` field at all —
//! the model is `aicode.experience`.
//!
//! Three identities have to line up, and only the first comes from a credential:
//!
//! 1. **account** — which Google identity holds the seat. We borrow a stored
//!    `gemini-cli` credential (this provider has no login of its own), selected
//!    by `[aicode] account_email`. Nothing else can supply it: the licence is
//!    invisible from the credential file, and a credential's `project_id` is a
//!    Code Assist project that need not equal the licence project.
//! 2. **licence project + location + tier** — discovered together from
//!    `:fetchLicenses` ([`fetch_licences`]), with `[aicode]` overriding any
//!    field. They travel as a set because the location is a property of the
//!    project: it lands in the *hostname*, and a regional licence sent to the
//!    global host is a 403, not a redirect.
//! 3. **experience** — the part after `aicode/`.
//!
//! Load-bearing protocol details, each taken from a captured request:
//!
//! * The body is an allowlist, not a passthrough — unknown names are a 400,
//!   notably `project`, which both sibling translators inject. See
//!   [`super::translate::gemini_to_aicode`].
//! * `entitlement.userTier` is mandatory on the wire. Optional in config only
//!   because it is discoverable; if discovery fails and config is silent, this
//!   module errors rather than sending the request without it.
//! * `x-goog-user-project` is required **for us** even though the real client
//!   sends none: we authenticate with gemini-cli's *public* OAuth client, so
//!   without it Google bills the call to that client's own project and answers
//!   403 `SERVICE_DISABLED` — which reads like the API being switched off.
//! * Ordinary Google OAuth only. The real client's workforce/STS sign-in yields
//!   a refresh token that inherits the pool's session lifetime and dies within
//!   hours, so `login gemini`'s credential is the one to borrow.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use lazy_static::lazy_static;
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::creds::{self, Account};
use super::models::GEMINI_CLI;
use crate::config::AicodeConfig;

/// Client we impersonate. Sent verbatim on every `businessaicode` call;
/// `auth_method=gcp` is part of the identity, not decoration.
pub const USER_AGENT: &str =
    "antigravity/cli/1.1.12 (aidev_client; os_type=darwin; arch=arm64; cl=962369648; auth_method=gcp)";

/// Where `:fetchLicenses` lives — always the global host, whatever the licence's
/// own location turns out to be.
const GLOBAL_BASE: &str = "https://businessaicode.googleapis.com/v1beta";

const DEFAULT_LOCATION: &str = "global";

lazy_static! {
    /// Discovered licences, keyed by account email. Per-process only — one GET
    /// per proxy start. Deliberately not persisted: writing it to disk would
    /// reopen the "which file owns this" question that keeps this provider's
    /// state out of the shared credential file.
    static ref LICENCE_CACHE: tokio::sync::Mutex<HashMap<String, Licence>> =
        tokio::sync::Mutex::new(HashMap::new());

    /// Serializes discovery so concurrent first requests don't each fetch.
    /// Global rather than per-account **on purpose**: one `[aicode]` table means
    /// one seat, so there is never a second account to contend with, and it is
    /// held once per process for the length of a single GET. If multi-licence
    /// support ever lands, this becomes a set of in-flight emails.
    static ref DISCOVERY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::new(());
}

/// Counter for `X-Aicode-Request-Id`'s `<trajectory>-<n>`. **Process-wide, not
/// per-trajectory**: two concurrent conversations interleave (`A-0`, `B-1`,
/// `A-2`). The id needs to be unique, not monotonic within a trajectory, and a
/// per-trajectory counter would mean keeping state per conversation for no gain.
static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// One Gemini Enterprise licence, as `:fetchLicenses` reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Licence {
    #[serde(rename = "projectId", default)]
    pub project: String,
    #[serde(default)]
    pub location: String,
    #[serde(rename = "userTier", default)]
    pub user_tier: String,
    #[serde(rename = "tierDisplayName", default)]
    pub tier_display_name: String,
}

#[derive(Deserialize)]
struct LicencesResponse {
    #[serde(default)]
    licenses: Vec<Licence>,
}

/// Everything one request needs, after account selection, discovery and config
/// overrides have all been applied.
#[derive(Debug, Clone)]
pub struct Target {
    pub project: String,
    pub location: String,
    pub user_tier: String,
    /// Which credential backs it — logged, never sent.
    pub email: String,
}

/// Why an `aicode` request can't proceed. Each surface formats these into its
/// own error envelope, so this carries the message and not the status.
#[derive(Debug)]
pub enum AicodeError {
    /// No `[aicode]` in config: the provider is off, not broken.
    Disabled,
    /// No usable `gemini-cli` credential, or an ambiguous choice between several.
    Credential(String),
    /// Token refresh failed.
    Refresh(String),
    /// Discovery ran but produced nothing usable, and config didn't fill the gap.
    Licence(String),
}

impl AicodeError {
    /// The HTTP status, owned here so the two surfaces can't drift apart when a
    /// variant is added. Each surface still picks its own envelope *wording* —
    /// that is the part that legitimately differs between the Gemini and
    /// Anthropic error shapes.
    pub fn http_status(&self) -> hyper::StatusCode {
        match self {
            // Not configured is "no such model", not a failure.
            AicodeError::Disabled => hyper::StatusCode::NOT_FOUND,
            AicodeError::Credential(_) => hyper::StatusCode::UNAUTHORIZED,
            AicodeError::Refresh(_) | AicodeError::Licence(_) => {
                hyper::StatusCode::BAD_GATEWAY
            }
        }
    }
}

impl std::fmt::Display for AicodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AicodeError::Disabled => write!(
                f,
                "The `aicode` provider is not configured. Add an `[aicode]` table to config.toml."
            ),
            AicodeError::Credential(m)
            | AicodeError::Refresh(m)
            | AicodeError::Licence(m) => write!(f, "{m}"),
        }
    }
}

/// A config field that is present, non-empty and trimmed — an empty string in
/// TOML means "unset", not "the empty value".
fn configured(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// Regional endpoint for `location`: the multi-region deployments live on their
/// own hosts and only `global` sits on the bare one. Sending a regional
/// licence's traffic to the global host is not a redirect — it's a different
/// deployment, and it answers 403.
pub fn api_base(location: &str) -> String {
    if location == DEFAULT_LOCATION {
        GLOBAL_BASE.to_string()
    } else {
        format!("https://businessaicode.{location}.rep.googleapis.com/v1beta")
    }
}

/// `{base}/projects/{project}/locations/{location}:{action}` (+ `?alt=sse`).
/// Note that project and location each appear twice — in the authority and
/// again in the path. That duplication is the API's, which is why they are
/// resolved as one set rather than two independent knobs.
pub fn build_url(project: &str, location: &str, action: &str, stream: bool) -> String {
    let mut url = format!(
        "{}/projects/{project}/locations/{location}:{action}",
        api_base(location)
    );
    if stream {
        url.push_str("?alt=sse");
    }
    url
}

/// Non-empty and free of anything that could restructure a URL or a header
/// value: no `/`, `?`, `#`, `@`, `:`, no whitespace, no control characters.
/// Deliberately a charset rule rather than an allowlist of known values, so a
/// new region or project naming scheme doesn't need a code change.
fn safe_component(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `location` becomes part of the API *hostname*, so it is validated before use
/// — including when it arrives from the wire via `:fetchLicenses`, which is the
/// more important call site: config is operator-typed, a wire value is not.
pub fn valid_location(location: &str) -> bool {
    safe_component(location)
}

/// `project` is validated for the same reason and against the same charset, but
/// for a different exposure: it lands in **two URL path segments** and in the
/// `x-goog-user-project` **header value**. A `/` would restructure the path
/// (`projects/a/locations/b` addresses a different resource), a `?` or `#`
/// would truncate it, and a newline in a header is a request-splitting shape —
/// `reqwest` rejects that one at build time, but as an opaque error rather than
/// a diagnosis. Real GCP project ids are `[a-z0-9-]` and project *numbers* are
/// digits, so this charset accepts every legitimate value.
pub fn valid_project(project: &str) -> bool {
    safe_component(project)
}

/// Whether a `:fetchLicenses` failure means "this billing handle is wrong, try
/// another" rather than "the service is unhappy". Only 403 qualifies: it covers
/// both shapes we have observed — `SERVICE_DISABLED` when the API is not enabled
/// on the named project, and *"Caller does not have required permission to use
/// project"* when the account has no access to it. Anything else (5xx, 429, a
/// network-level failure) is about the call, not the handle, and must surface
/// as itself.
fn wrong_billing_handle(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::FORBIDDEN
}

/// Billing handles to try for the `:fetchLicenses` metadata GET, most specific
/// first. Neither is the licence project — that is what the call discovers.
/// The credential's own `project_id` earns second place because it is a project
/// this account demonstrably has, which is all the header needs.
fn candidate_projects<'a>(cfg_project: Option<&'a str>, account_project: &'a str) -> Vec<&'a str> {
    let mut out = Vec::new();
    if let Some(p) = cfg_project {
        out.push(p);
    }
    if !account_project.is_empty() && Some(account_project) != cfg_project {
        out.push(account_project);
    }
    out
}

/// Flatten a `reqwest` error chain. Its own `Display` stops at "error sending
/// request for url (…)", which can't distinguish a host that failed to resolve
/// (a wrong region) from one that refused a connection — precisely the
/// distinction worth having here.
pub fn cause_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        src = cause.source();
    }
    out
}

/// Pick the `gemini-cli` credential that holds the seat.
///
/// Deliberately not [`creds::resolve_account`]: that lazy-onboards a Code
/// Assist project when `project_id` is empty, which this path never needs and
/// which **writes to the credential file** — putting a licence project into the
/// field the `gemini-cli` provider reads would break plain `gemini-cli/*`
/// requests in a way that outlives the process.
///
/// And deliberately not [`creds::pick_account`]: that takes the first match
/// from an unsorted `read_dir` walk across both auth dirs. With one credential
/// that's correct; with two it's whichever file the filesystem happens to
/// yield, which can differ between machines and between runs. Picking wrong is
/// usually a 403, but if both accounts hold seats it silently spends the wrong
/// org's — so an ambiguous choice is refused, not guessed.
pub async fn resolve_account(
    cfg: &AicodeConfig,
    auth_dirs: &[PathBuf],
) -> Result<Account, AicodeError> {
    let dirs = auth_dirs.to_vec();
    let accounts = tokio::task::spawn_blocking(move || creds::discover_accounts(&dirs))
        .await
        .unwrap_or_default();
    let mut candidates: Vec<Account> = accounts
        .into_iter()
        .filter(|a| a.provider == GEMINI_CLI)
        .collect();

    if let Some(want) = cfg.account_email.as_deref().map(str::trim) {
        if !want.is_empty() {
            return candidates
                .into_iter()
                .find(|a| a.email.eq_ignore_ascii_case(want))
                .ok_or_else(|| {
                    AicodeError::Credential(format!(
                        "No gemini credential for `[aicode] account_email = \"{want}\"`. \
                         Run `claude-proxy login gemini` with that account."
                    ))
                });
        }
    }

    match candidates.len() {
        0 => Err(AicodeError::Credential(
            "No gemini credential found. Run `claude-proxy login gemini` with the account that \
             holds the Gemini Enterprise seat."
                .to_string(),
        )),
        1 => Ok(candidates.remove(0)),
        _ => {
            let emails: Vec<&str> = candidates.iter().map(|a| a.email.as_str()).collect();
            Err(AicodeError::Credential(format!(
                "{} gemini credentials on disk ({}) and `[aicode] account_email` is not set. \
                 Set it to the account holding the seat — picking one arbitrarily could spend \
                 the wrong organisation's licence.",
                candidates.len(),
                emails.join(", ")
            )))
        }
    }
}

/// `GET :fetchLicenses` on the global host.
///
/// The real client sends no `x-goog-user-project`, and that is load-bearing for
/// *it*: the `serviceusage.services.use` check this endpoint is known for is
/// enforced against whatever project that header names, so sending it invites a
/// failure that omitting it avoids. But we ride gemini-cli's **public** OAuth
/// client, so a bare call is billed to that client's own project and comes back
/// 403 `SERVICE_DISABLED` — verified, not theorised.
///
/// Hence `user_projects`: candidate handles to retry with, in order. Each is
/// only a billing/enablement handle for this one metadata GET — **never** the
/// licence project, which is what the call is trying to discover. The
/// credential's own `project_id` earns its place in that list precisely because
/// it is a project this account demonstrably has, which is all the header needs.
pub async fn fetch_licences(
    client: &reqwest::Client,
    access_token: &str,
    user_projects: &[&str],
) -> anyhow::Result<Vec<Licence>> {
    let url = format!("{GLOBAL_BASE}:fetchLicenses");
    let send = |user_project: Option<&str>| {
        let mut req = client
            .get(&url)
            .bearer_auth(access_token)
            .header("User-Agent", USER_AGENT);
        if let Some(p) = user_project {
            req = req.header("x-goog-user-project", p);
        }
        req.send()
    };

    // Candidates first. With our *public* OAuth client a header-less call is
    // billed to that client's own project (`681255809395`), where the Business
    // AI Code API is not enabled and never will be — so leading with it is a
    // guaranteed wasted round-trip, not a graceful default.
    let mut refused = None;
    for project in user_projects {
        let r = send(Some(project))
            .await
            .map_err(|e| anyhow::anyhow!("fetchLicenses failed: {}", cause_chain(&e)))?;
        if r.status().is_success() {
            return Ok(r.json::<LicencesResponse>().await?.licenses);
        }
        if !wrong_billing_handle(r.status()) {
            // Not a "try a different project" answer — a 500 or a 429 is about
            // the service, not the handle. Advancing would spend an unrelated
            // project's quota on the same outage and then report the *second*
            // failure, hiding the first.
            refused = Some(r);
            break;
        }
        warn!(
            "aicode: :fetchLicenses refused billed to {} ({}); trying the next candidate",
            project,
            r.status()
        );
        refused = Some(r);
    }

    // The header-less form is what the real client sends, and it is the only
    // option when nothing supplied a candidate project. Skipped entirely when
    // candidates existed, since it cannot do better than they did.
    let resp = match refused {
        Some(r) => r,
        None => send(None)
            .await
            .map_err(|e| anyhow::anyhow!("fetchLicenses failed: {}", cause_chain(&e)))?,
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("fetchLicenses returned {status}: {body}");
    }

    let parsed: LicencesResponse = resp.json().await?;
    Ok(parsed.licenses)
}

/// Choose among the licences an account holds. One is unambiguous; several need
/// `[aicode] project` to say which, because spending the wrong one is a billing
/// decision, not a routing detail.
fn select_licence(licences: Vec<Licence>, want_project: Option<&str>) -> Result<Licence, String> {
    if let Some(want) = want_project {
        return licences
            .into_iter()
            .find(|l| l.project == want)
            .ok_or_else(|| format!("no Gemini Enterprise licence for project `{want}`"));
    }
    let mut it = licences.into_iter();
    let first = it
        .next()
        .ok_or_else(|| "account holds no Gemini Enterprise licence".to_string())?;
    let rest: Vec<Licence> = it.collect();
    if rest.is_empty() {
        return Ok(first);
    }
    let mut names: Vec<String> = vec![first.project.clone()];
    names.extend(rest.into_iter().map(|l| l.project));
    Err(format!(
        "account holds {} licences ({}); set `[aicode] project` to pick one",
        names.len(),
        names.join(", ")
    ))
}

/// Resolve the account, its token, and the licence triple for one request.
/// Config wins over discovery on every field, so a stale or multi-licence
/// answer is always overridable; discovery is an optimisation, never the only
/// path.
pub async fn resolve(
    client: &reqwest::Client,
    cfg: &AicodeConfig,
    auth_dirs: &[PathBuf],
) -> Result<(Target, String), AicodeError> {
    let account = resolve_account(cfg, auth_dirs).await?;
    let token = creds::ensure_fresh(&account).await.map_err(|e| {
        warn!("aicode: token refresh failed for {}: {}", account.email, e);
        AicodeError::Refresh(format!("Auth refresh failed: {e}"))
    })?;

    // Config alone can fully specify the target — skip discovery entirely then,
    // which is also the escape hatch if our client can't call :fetchLicenses.
    let cfg_project = configured(&cfg.project);
    let cfg_region = configured(&cfg.region);
    let cfg_tier = configured(&cfg.user_tier);

    let licence = if cfg_project.is_some() && cfg_region.is_some() && cfg_tier.is_some() {
        None
    } else {
        Some(discover_licence(client, cfg, &account, &token, cfg_project).await?)
    };

    let project = cfg_project
        .map(str::to_string)
        .or_else(|| licence.as_ref().map(|l| l.project.clone()))
        .filter(|p| !p.is_empty())
        .ok_or_else(|| {
            AicodeError::Licence(
                "no licence project: discovery returned none and `[aicode] project` is unset"
                    .to_string(),
            )
        })?;

    // Same check as the location's, one call site later, and for the same
    // reason: whichever of config or `:fetchLicenses` supplied it, this value
    // is about to become two URL path segments and a header value.
    if !valid_project(&project) {
        return Err(AicodeError::Licence(format!(
            "licence project {project:?} contains characters that cannot appear in a URL path \
             or a header value; set `[aicode] project` to override"
        )));
    }

    let location = cfg_region
        .map(str::to_string)
        .or_else(|| licence.as_ref().map(|l| l.location.clone()))
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| DEFAULT_LOCATION.to_string());

    // The wire value gets the same charset check as the config one — this is
    // the call site that matters, since nobody typed it.
    if !valid_location(&location) {
        return Err(AicodeError::Licence(format!(
            "licence location {location:?} is not hostname-safe; set `[aicode] region` to override"
        )));
    }

    let user_tier = cfg_tier
        .map(str::to_string)
        .or_else(|| licence.as_ref().map(|l| l.user_tier.clone()))
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            AicodeError::Licence(
                "no `entitlement.userTier`: discovery returned none and `[aicode] user_tier` is \
                 unset. It is mandatory upstream, so the request is not sent without it."
                    .to_string(),
            )
        })?;

    Ok((
        Target {
            project,
            location,
            user_tier,
            email: account.email.clone(),
        },
        token,
    ))
}

/// Cached `:fetchLicenses`, one call per account per process.
async fn discover_licence(
    client: &reqwest::Client,
    cfg: &AicodeConfig,
    account: &Account,
    token: &str,
    cfg_project: Option<&str>,
) -> Result<Licence, AicodeError> {
    if let Some(hit) = LICENCE_CACHE.lock().await.get(&account.email) {
        return Ok(hit.clone());
    }

    let _guard = DISCOVERY_LOCK.lock().await;
    // Another task may have discovered while we waited for the lock.
    if let Some(hit) = LICENCE_CACHE.lock().await.get(&account.email) {
        return Ok(hit.clone());
    }

    let user_projects = candidate_projects(cfg_project, &account.project_id);
    let licences = fetch_licences(client, token, &user_projects)
        .await
        .map_err(|e| {
            AicodeError::Licence(format!(
                "could not discover the licence for {}: {e}. Set `[aicode] project`, `region` and \
                 `user_tier` in config to skip discovery.",
                account.email
            ))
        })?;

    let licence = select_licence(licences, cfg_project).map_err(|msg| {
        AicodeError::Licence(format!(
            "{msg} (account {}). {}",
            account.email,
            if cfg.account_email.is_none() {
                "If the seat is on a different account, set `[aicode] account_email`."
            } else {
                "Check that this account holds the seat."
            }
        ))
    })?;

    info!(
        "aicode: licence {} → project={} location={} tier={}{}",
        account.email,
        licence.project,
        licence.location,
        licence.user_tier,
        if licence.tier_display_name.is_empty() {
            String::new()
        } else {
            format!(" ({})", licence.tier_display_name)
        }
    );

    LICENCE_CACHE
        .lock()
        .await
        .insert(account.email.clone(), licence.clone());
    Ok(licence)
}

/// Send an already-translated payload to the seat's regional endpoint.
///
/// Dedicated rather than an arm inside [`super::provider::send_request`]:
/// this provider needs a project header and the two trajectory headers, which
/// that signature has nowhere to put, and widening it would touch four
/// unrelated call sites.
///
/// `trajectory` groups a multi-turn conversation into one trajectory instead of
/// N unrelated ones from a single identity — the real client uses a per-session
/// UUID, and the conversation-derived id gives us the same property. The
/// request id's counter is global, not per-trajectory; see [`REQUEST_SEQ`].
pub async fn send_request(
    client: &reqwest::Client,
    target: &Target,
    access_token: &str,
    payload: Vec<u8>,
    action: &str,
    stream: bool,
    trajectory: &str,
) -> reqwest::Result<reqwest::Response> {
    let url = build_url(&target.project, &target.location, action, stream);
    let seq = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed);

    client
        .post(&url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header(
            "Accept",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .header("User-Agent", USER_AGENT)
        // Required for us and not for the real client: we ride gemini-cli's
        // public OAuth client, whose own project we don't own.
        .header("x-goog-user-project", &target.project)
        .header("X-Aicode-Trajectory-Id", trajectory)
        .header("X-Aicode-Request-Id", format!("{trajectory}-{seq}"))
        .body(payload)
        .send()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two hosts are different deployments, not aliases — the global host
    /// rejects a regional licence outright ("The selected license is not
    /// valid"), so getting this mapping wrong is a 403 per request rather than
    /// anything obviously URL-shaped.
    #[test]
    fn global_uses_the_bare_host() {
        assert_eq!(api_base("global"), "https://businessaicode.googleapis.com/v1beta");
    }

    #[test]
    fn a_region_uses_its_own_host() {
        assert_eq!(
            api_base("us"),
            "https://businessaicode.us.rep.googleapis.com/v1beta"
        );
    }

    /// Both host forms, end to end. Real captures of each shape exist (a
    /// `global` licence and a `us` one, from the same client); the project ids
    /// here are placeholders, since the URL construction is what's under test.
    #[test]
    fn generate_urls_match_the_captures() {
        assert_eq!(
            build_url("example-ge-dev", "global", "streamGenerateContent", true),
            "https://businessaicode.googleapis.com/v1beta/projects/example-ge-dev\
             /locations/global:streamGenerateContent?alt=sse"
        );
        assert_eq!(
            build_url("example-ge-prod", "us", "streamGenerateContent", true),
            "https://businessaicode.us.rep.googleapis.com/v1beta/projects/\
             example-ge-prod/locations/us:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn non_stream_url_has_no_alt_sse() {
        assert!(!build_url("p", "us", "generateContent", false).contains("alt=sse"));
    }

    /// `location` lands in the hostname, so anything that could reshape the
    /// authority is refused — whether it came from config or from the wire.
    #[test]
    fn location_rejects_anything_not_hostname_safe() {
        assert!(valid_location("global"));
        assert!(valid_location("us"));
        assert!(valid_location("us-central1"));
        assert!(!valid_location(""));
        assert!(!valid_location("us/../evil"));
        assert!(!valid_location("us evil"));
        assert!(!valid_location("us.evil.com"));
        assert!(!valid_location("us\n"));
        assert!(!valid_location("us:8080"));
        assert!(!valid_location("us@evil"));
    }

    /// `project` lands in two URL path segments **and** in the
    /// `x-goog-user-project` header value, so it needs the same gate as the
    /// location — a `/` would address a different resource, and a newline is a
    /// request-splitting shape.
    #[test]
    fn project_rejects_path_and_header_metacharacters() {
        assert!(valid_project("example-ge-prod"));
        assert!(valid_project("681255809395")); // a project *number* is legal too
        assert!(!valid_project(""));
        assert!(!valid_project("foo/../bar"));
        assert!(!valid_project("foo/locations/evil"));
        assert!(!valid_project("foo?alt=sse"));
        assert!(!valid_project("foo#frag"));
        assert!(!valid_project("foo\nHeader-Injection: bar"));
        assert!(!valid_project("foo\r\nX: y"));
        assert!(!valid_project("foo bar"));
        assert!(!valid_project("foo@bar"));
    }

    /// The gate has to hold for a value that arrived from `:fetchLicenses`, not
    /// only for one an operator typed — that is the whole reason it exists,
    /// since a wire value is the one nobody reviewed.
    #[test]
    fn a_hostile_discovered_project_never_reaches_a_url() {
        let hostile = "p/locations/global:generateContent?x=";
        assert!(!valid_project(hostile));
        // Demonstrates what the gate prevents: the path would address a
        // different resource entirely.
        let url = build_url(hostile, "us", "generateContent", false);
        assert!(url.contains("locations/global:generateContent"), "{url}");
    }

    /// Only a 403 means "wrong billing handle, try the next project". An
    /// earlier version advanced on any non-2xx, which turned a transient 500 on
    /// the first candidate into a second call against an unrelated project —
    /// and then reported that second failure, hiding the outage.
    #[test]
    fn only_403_advances_to_the_next_billing_handle() {
        use reqwest::StatusCode;
        assert!(wrong_billing_handle(StatusCode::FORBIDDEN));
        for other in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::NOT_FOUND,
            StatusCode::UNAUTHORIZED,
        ] {
            assert!(!wrong_billing_handle(other), "{other} must not advance");
        }
    }

    /// Config first, then the credential's own project. Neither is the licence
    /// project; both are only billing handles for the metadata GET.
    #[test]
    fn candidates_are_config_first_then_the_credential() {
        assert_eq!(
            candidate_projects(Some("from-config"), "from-credential"),
            vec!["from-config", "from-credential"]
        );
        assert_eq!(candidate_projects(None, "from-credential"), vec!["from-credential"]);
        assert_eq!(candidate_projects(Some("only-config"), ""), vec!["only-config"]);
    }

    /// An empty list is what sends the header-less form, which is the only
    /// shape left when nothing supplied a handle — never a default we lead with.
    #[test]
    fn no_candidates_means_no_header_attempt_to_make() {
        assert!(candidate_projects(None, "").is_empty());
    }

    /// The same value from both sources is one attempt, not two.
    #[test]
    fn a_duplicate_candidate_is_not_tried_twice() {
        assert_eq!(candidate_projects(Some("same"), "same"), vec!["same"]);
    }

    fn licence(project: &str, location: &str) -> Licence {
        Licence {
            project: project.into(),
            location: location.into(),
            user_tier: "gcp-ge-plus-tier".into(),
            tier_display_name: "Gemini Enterprise Plus".into(),
        }
    }

    #[test]
    fn one_licence_needs_no_config() {
        let got = select_licence(vec![licence("p", "us")], None).unwrap();
        assert_eq!(got.project, "p");
        assert_eq!(got.location, "us");
    }

    /// An account with no seat is a diagnosable state, not a panic — and not
    /// the same as the 403 a wrong region gives.
    #[test]
    fn no_licence_is_an_error_naming_the_problem() {
        let err = select_licence(vec![], None).unwrap_err();
        assert!(err.contains("no Gemini Enterprise licence"), "{err}");
    }

    #[test]
    fn several_licences_need_config_project() {
        let err = select_licence(vec![licence("a", "us"), licence("b", "global")], None)
            .unwrap_err();
        assert!(err.contains('a') && err.contains('b'), "{err}");
        let got = select_licence(vec![licence("a", "us"), licence("b", "global")], Some("b"))
            .unwrap();
        assert_eq!(got.location, "global");
    }

    #[test]
    fn config_project_with_no_matching_licence_is_an_error() {
        let err = select_licence(vec![licence("a", "us")], Some("zzz")).unwrap_err();
        assert!(err.contains("zzz"), "{err}");
    }

    /// Parses the captured `:fetchLicenses` body verbatim.
    #[test]
    fn licence_response_parses_the_capture() {
        let body = r#"{"licenses":[{"userTier":"gcp-ge-plus-tier",
            "tierDisplayName":"Gemini Enterprise Plus",
            "projectId":"example-ge-dev","location":"global"}]}"#;
        let parsed: LicencesResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.licenses.len(), 1);
        assert_eq!(parsed.licenses[0].project, "example-ge-dev");
        assert_eq!(parsed.licenses[0].location, "global");
        assert_eq!(parsed.licenses[0].user_tier, "gcp-ge-plus-tier");
    }
}
