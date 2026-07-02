//! `claude-proxy login gemini` / `login antigravity` — interactive browser
//! OAuth that mints the per-account credential files our Gemini providers read.
//! Files are written to our own auth dir (`~/.config/claude-proxy/auths/`) in
//! the same on-disk shapes CLIProxyAPI uses, so the two are interchangeable.
//!
//! Every HTTP call here uses a `no_proxy()` client: `login` runs in the user's
//! shell where `HTTPS_PROXY` points at this very proxy, so a default client
//! would loop back through our own MITM.

use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::gemini::creds;
use crate::oauth_util;

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const USERINFO_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";
pub(crate) const CODE_ASSIST: &str = "https://cloudcode-pa.googleapis.com";
pub(crate) const CODE_ASSIST_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
const CODE_ASSIST_VERSION: &str = "v1internal";
const LOGIN_TIMEOUT_SECS: u64 = 300;
const ONBOARD_TIMEOUT_SECS: u64 = 30;

const GEMINI_CALLBACK_PORT: u16 = 8085;
const GEMINI_CALLBACK_PATH: &str = "/oauth2callback";
const ANTIGRAVITY_CALLBACK_PORT: u16 = 51121;
const ANTIGRAVITY_CALLBACK_PATH: &str = "/oauth-callback";
pub(crate) const ANTIGRAVITY_USER_AGENT: &str = "antigravity/cli/1.0.9 darwin/arm64";
pub(crate) const GEMINI_LOGIN_USER_AGENT: &str =
    "GeminiCLI-tui/0.47.0/unknown (darwin; arm64; terminal) google-api-nodejs-client/9.15.1";

struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

fn no_proxy_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .context("build no-proxy HTTP client")
}

/// Bind the OAuth callback listener, preferring `preferred_port` (the port the
/// OAuth client's redirect URI is registered with) and falling back to an
/// OS-assigned loopback port if it's already in use. Returns the listener and
/// the actual bound port.
async fn bind_callback(preferred_port: u16) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    match tokio::net::TcpListener::bind(("127.0.0.1", preferred_port)).await {
        Ok(l) => {
            let port = l.local_addr()?.port();
            Ok((l, port))
        }
        Err(e) => {
            warn!("OAuth callback port {preferred_port} unavailable ({e}); using a random loopback port");
            let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .context("bind OAuth callback on a random loopback port")?;
            let port = l.local_addr()?.port();
            Ok((l, port))
        }
    }
}

// ---------------------------------------------------------------------------
// login gemini
// ---------------------------------------------------------------------------

pub async fn login_gemini(
    requested_project: Option<String>,
    no_browser: bool,
) -> anyhow::Result<()> {
    let client = no_proxy_client()?;
    let token = if no_browser {
        manual_oauth(
            creds::GEMINI_CLIENT_ID,
            creds::GEMINI_CLIENT_SECRET,
            creds::GEMINI_SCOPES,
            "https://codeassist.google.com/authcode",
            true, // use PKCE for gemini-cli OOB flow
            "Gemini (Google Code Assist)",
            &client,
        )
        .await?
    } else {
        loopback_oauth(
            creds::GEMINI_CLIENT_ID,
            creds::GEMINI_CLIENT_SECRET,
            creds::GEMINI_SCOPES,
            GEMINI_CALLBACK_PORT,
            GEMINI_CALLBACK_PATH,
            &client,
        )
        .await?
    };

    let email = fetch_email(&client, &token.access_token).await?;
    info!("Authenticated as {}", email);

    let metadata = json!({
        "ideType": "IDE_UNSPECIFIED",
        "platform": "PLATFORM_UNSPECIFIED",
        "pluginType": "GEMINI",
    });
    let project_id = fetch_project_id(
        &client,
        &token.access_token,
        GEMINI_LOGIN_USER_AGENT,
        metadata,
        requested_project.as_deref(),
        CODE_ASSIST,
        CODE_ASSIST,
        true,
    )
    .await?;
    info!("Using project {}", project_id);

    let expiry = rfc3339_from_now(token.expires_in);
    let cred = json!({
        "token": {
            "access_token": token.access_token,
            "token_type": "Bearer",
            "refresh_token": token.refresh_token,
            "expiry": expiry,
            "expires_in": token.expires_in,
            // OAuth client metadata, so CLIProxyAPI-compatible clients (which
            // refresh via the standard google-auth flow) can refresh this
            // credential after the access token expires. claude-proxy itself
            // refreshes via the hardcoded `creds` constants and never reads
            // these back, but the on-disk format must carry them.
            "scopes": creds::GEMINI_SCOPES,
            "token_uri": creds::TOKEN_ENDPOINT,
            "client_id": creds::GEMINI_CLIENT_ID,
            "client_secret": creds::GEMINI_CLIENT_SECRET,
            "universe_domain": "googleapis.com",
        },
        "project_id": project_id,
        "email": email,
        "auto": requested_project.is_none(),
        "checked": true,
        "type": "gemini",
    });

    let filename = format!("gemini-{}-{}.json", email, project_id);
    write_cred(&filename, &cred)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// login antigravity
// ---------------------------------------------------------------------------

pub async fn login_antigravity(no_browser: bool) -> anyhow::Result<()> {
    let client = no_proxy_client()?;
    let token = if no_browser {
        manual_oauth(
            creds::ANTIGRAVITY_CLIENT_ID,
            creds::ANTIGRAVITY_CLIENT_SECRET,
            creds::ANTIGRAVITY_SCOPES,
            "http://localhost:51121/oauth-callback",
            false, // antigravity standard flow, no PKCE required
            "Antigravity",
            &client,
        )
        .await?
    } else {
        loopback_oauth(
            creds::ANTIGRAVITY_CLIENT_ID,
            creds::ANTIGRAVITY_CLIENT_SECRET,
            creds::ANTIGRAVITY_SCOPES,
            ANTIGRAVITY_CALLBACK_PORT,
            ANTIGRAVITY_CALLBACK_PATH,
            &client,
        )
        .await?
    };

    let email = fetch_email(&client, &token.access_token).await?;
    info!("Authenticated as {}", email);

    let metadata = json!({ "ideType": "ANTIGRAVITY" });
    let project_id = fetch_project_id(
        &client,
        &token.access_token,
        ANTIGRAVITY_USER_AGENT,
        metadata,
        None,
        CODE_ASSIST_DAILY,
        CODE_ASSIST_DAILY,
        true,
    )
    .await?;
    info!("Using project {}", project_id);

    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let cred = json!({
        "type": "antigravity",
        "access_token": token.access_token,
        "refresh_token": token.refresh_token,
        "expires_in": token.expires_in,
        "timestamp": now_ms,
        "expired": rfc3339_from_now(token.expires_in),
        "email": email,
        "project_id": project_id,
    });

    let filename = format!("antigravity-{}.json", email);
    write_cred(&filename, &cred)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// login vertex
// ---------------------------------------------------------------------------

pub async fn login_vertex(no_browser: bool) -> anyhow::Result<()> {
    use crate::reauth;

    let client = no_proxy_client()?;
    let token = if no_browser {
        manual_oauth(
            reauth::GOOGLE_CLIENT_ID,
            reauth::GOOGLE_CLIENT_SECRET,
            reauth::SCOPES,
            "http://localhost:8085",
            false,
            "Vertex (Google Cloud)",
            &client,
        )
        .await?
    } else {
        loopback_oauth(
            reauth::GOOGLE_CLIENT_ID,
            reauth::GOOGLE_CLIENT_SECRET,
            reauth::SCOPES,
            8085,
            "",
            &client,
        )
        .await?
    };

    // Clean out any stale cached access token in our disk cache.
    // When logging in with a fresh ADC, we want to force any subsequent
    // proxy request to mint a fresh access token from the new ADC.
    let cache_path = crate::interceptors::get_token_cache_path();
    let _ = std::fs::remove_file(&cache_path);

    let email = fetch_email(&client, &token.access_token).await?;
    info!("Authenticated as {}", email);

    let token_json = json!({
        "access_token": token.access_token,
        "refresh_token": token.refresh_token,
        "expires_in": token.expires_in,
        "token_type": "Bearer",
    });

    reauth::write_adc(&token_json);
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared flow
// ---------------------------------------------------------------------------

async fn loopback_oauth(
    client_id: &str,
    client_secret: &str,
    scopes: &[&str],
    port: u16,
    path: &str,
    client: &reqwest::Client,
) -> anyhow::Result<TokenResponse> {
    // Prefer the canonical port (matches the OAuth client's registered redirect),
    // but fall back to an OS-assigned port if it's taken (e.g. CLIProxyAPI or a
    // previous login is holding it).
    let (listener, bound_port) = bind_callback(port).await?;

    // Redirect host: on the preferred port keep `localhost` — the value gemini-cli
    // and CLIProxyAPI use for these clients (antigravity is only verified to accept
    // `localhost:51121`), and Google treats `localhost`/`127.0.0.1` as distinct
    // redirect values. If we fell back to an OS-assigned port, use the `127.0.0.1`
    // literal: Google's loopback flow only reliably accepts an arbitrary,
    // unregistered port for an IP-literal host (gemini-cli pairs `127.0.0.1` with a
    // dynamic port for exactly this reason).
    let host = if bound_port == port {
        "localhost"
    } else {
        "127.0.0.1"
    };
    let redirect_uri = format!("http://{host}:{bound_port}{path}");
    let scope_str = scopes.join(" ");
    let state: String = format!("{:x}", rand::random::<u64>());
    let auth_url = format!(
        "{AUTH_ENDPOINT}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={}",
        oauth_util::percent_encode(client_id),
        oauth_util::percent_encode(&redirect_uri),
        oauth_util::percent_encode(&scope_str),
        oauth_util::percent_encode(&state),
    );

    println!("Opening browser for sign-in. If it doesn't open, visit:\n  {auth_url}\n");
    oauth_util::open_browser(&auth_url);

    let code = tokio::time::timeout(
        Duration::from_secs(LOGIN_TIMEOUT_SECS),
        oauth_util::accept_oauth_callback(&listener),
    )
    .await
    .context("OAuth flow timed out")??;

    exchange_code(client, client_id, client_secret, &code, &redirect_uri, None).await
}

async fn manual_oauth(
    client_id: &str,
    client_secret: &str,
    scopes: &[&str],
    redirect_uri: &str,
    use_pkce: bool,
    provider_label: &str,
    client: &reqwest::Client,
) -> anyhow::Result<TokenResponse> {
    use rand::distr::Alphanumeric;
    use rand::RngExt;
    use sha2::{Digest, Sha256};

    // 1. Generate verifier and S256 challenge if PKCE is requested
    let mut code_verifier = None;
    let mut extra_params = String::new();

    if use_pkce {
        let verifier: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect();

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let challenge_hash = hasher.finalize();

        // base64url-encode challenge without padding
        let challenge = base64url_encode(&challenge_hash);

        extra_params = format!(
            "&code_challenge_method=S256&code_challenge={}",
            oauth_util::percent_encode(&challenge)
        );
        code_verifier = Some(verifier);
    }

    let scope_str = scopes.join(" ");
    let state: String = format!("{:x}", rand::random::<u64>());
    let auth_url = format!(
        "{AUTH_ENDPOINT}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={}{}",
        oauth_util::percent_encode(client_id),
        oauth_util::percent_encode(redirect_uri),
        oauth_util::percent_encode(&scope_str),
        oauth_util::percent_encode(&state),
        extra_params,
    );

    println!("--- {} Sign-In ---", provider_label);
    println!(
        "Please visit the following URL in any browser to authorize:\n\n  {}\n",
        auth_url
    );

    if redirect_uri.contains("localhost") || redirect_uri.contains("127.0.0.1") {
        println!("Note: Since this is a manual flow, after authorizing in your browser,");
        println!("your browser will show a 'Connection Error' page (e.g. unable to connect to localhost).");
        println!("This is expected! Please copy the full URL from your browser's address bar");
        println!("and paste it below.\n");
    }

    let code = tokio::time::timeout(Duration::from_secs(LOGIN_TIMEOUT_SECS), async {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);

        loop {
            print!("Paste the authorization code or redirect URL: ");
            use std::io::Write;
            let _ = std::io::stdout().flush();

            let mut line = String::new();
            // read_line returns Ok(0) on EOF (e.g. stdin closed / piped input
            // exhausted). Without this guard the loop would spin at 100% CPU on
            // an empty line until the outer timeout fires.
            if reader
                .read_line(&mut line)
                .await
                .context("read from stdin")?
                == 0
            {
                anyhow::bail!("stdin closed (EOF) before an authorization code was provided");
            }
            let code_or_url = line.trim();
            if code_or_url.is_empty() {
                continue;
            }

            match oauth_util::parse_callback_code(code_or_url) {
                Ok(c) => return Ok::<_, anyhow::Error>(c),
                Err(e) => {
                    println!("Error: {}. Please try again.\n", e);
                }
            }
        }
    })
    .await
    .context("OAuth flow timed out waiting for input")??;

    exchange_code(
        client,
        client_id,
        client_secret,
        &code,
        redirect_uri,
        code_verifier.as_deref(),
    )
    .await
}

async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: Option<&str>,
) -> anyhow::Result<TokenResponse> {
    let mut params = vec![
        ("code", code),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("redirect_uri", redirect_uri),
        ("grant_type", "authorization_code"),
    ];
    if let Some(verifier) = code_verifier {
        params.push(("code_verifier", verifier));
    }

    let resp = client
        .post(creds::TOKEN_ENDPOINT)
        .form(&params)
        .send()
        .await
        .context("token exchange request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("token exchange failed ({status}): {body}");
    }

    let v: Value = resp.json().await.context("parse token response")?;
    let access_token = v["access_token"].as_str().unwrap_or_default().to_string();
    let refresh_token = v["refresh_token"].as_str().unwrap_or_default().to_string();
    if access_token.is_empty() || refresh_token.is_empty() {
        anyhow::bail!("token response missing access_token/refresh_token: {v}");
    }
    Ok(TokenResponse {
        access_token,
        refresh_token,
        expires_in: v["expires_in"].as_u64().unwrap_or(3600),
    })
}

fn base64url_encode(data: &[u8]) -> String {
    let mut result = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = if i + 1 < data.len() { data[i + 1] } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] } else { 0 };

        let c0 = b0 >> 2;
        let c1 = ((b0 & 3) << 4) | (b1 >> 4);
        let c2 = ((b1 & 15) << 2) | (b2 >> 6);
        let c3 = b2 & 63;

        let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        result.push(chars[c0 as usize] as char);
        result.push(chars[c1 as usize] as char);
        if i + 1 < data.len() {
            result.push(chars[c2 as usize] as char);
        }
        if i + 2 < data.len() {
            result.push(chars[c3 as usize] as char);
        }
        i += 3;
    }
    result
}

async fn fetch_email(client: &reqwest::Client, access_token: &str) -> anyhow::Result<String> {
    let v: Value = client
        .get(USERINFO_ENDPOINT)
        .bearer_auth(access_token)
        .send()
        .await
        .context("userinfo request")?
        .json()
        .await
        .context("parse userinfo")?;
    v["email"]
        .as_str()
        .map(|s| s.to_string())
        .context("userinfo response had no email")
}

/// Resolve the Cloud Code Assist project via loadCodeAssist, falling back to
/// onboardUser. Mirrors CLIProxyAPI's discovery for both providers.
pub(crate) async fn fetch_project_id(
    client: &reqwest::Client,
    access_token: &str,
    user_agent: &str,
    metadata: Value,
    requested_project: Option<&str>,
    load_base: &str,
    onboard_base: &str,
    interactive: bool,
) -> anyhow::Result<String> {
    let mut load_body = json!({ "metadata": metadata });
    if let Some(p) = requested_project {
        load_body["cloudaicompanionProject"] = json!(p);
    }
    let load_resp = code_assist(
        client,
        load_base,
        "loadCodeAssist",
        &load_body,
        access_token,
        user_agent,
    )
    .await?;

    let tier_id = load_resp
        .get("allowedTiers")
        .and_then(|t| t.as_array())
        .and_then(|tiers| {
            tiers.iter().find(|t| {
                t.get("isDefault")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false)
            })
        })
        .and_then(|t| t.get("id").and_then(|i| i.as_str()))
        .unwrap_or("legacy-tier")
        .to_string();

    let mut project_id = requested_project
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();

    if project_id.is_empty() {
        project_id = extract_project(&load_resp).unwrap_or_default();
    }

    if project_id.is_empty() && interactive {
        println!("Listing your active Google Cloud projects...");
        let projects = list_projects(client, access_token, user_agent).await;
        if !projects.is_empty() {
            println!("\nAvailable Google Cloud projects:");
            for (i, p) in projects.iter().enumerate() {
                let id = p.get("id").and_then(|x| x.as_str()).unwrap_or("");
                let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("");
                println!("  [{}] {} ({})", i + 1, id, name);
            }
            println!("  [0] Auto-discover / Auto-provision");

            use std::io::Write;
            use tokio::io::{AsyncBufReadExt, BufReader};

            let picked = tokio::time::timeout(Duration::from_secs(LOGIN_TIMEOUT_SECS), async {
                let stdin = tokio::io::stdin();
                let mut reader = BufReader::new(stdin);
                loop {
                    print!("\nSelect a project (0-{}): ", projects.len());
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    match reader.read_line(&mut line).await {
                        Ok(0) => return None,
                        Ok(_) => {
                            let choice_str = line.trim();
                            if choice_str == "0" || choice_str.is_empty() {
                                return None;
                            }
                            if let Ok(num) = choice_str.parse::<usize>() {
                                if num > 0 && num <= projects.len() {
                                    if let Some(selected) = projects.get(num - 1) {
                                        if let Some(selected_id) =
                                            selected.get("id").and_then(|x| x.as_str())
                                        {
                                            return Some(selected_id.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to read from stdin: {e}");
                            return None;
                        }
                    }
                    println!("Invalid choice. Please try again.");
                }
            })
            .await;

            match picked {
                Ok(Some(selected_id)) => project_id = selected_id,
                Ok(None) => {}
                Err(_) => {
                    warn!(
                        "Timed out after {}s waiting for project selection; falling back to auto-discover/auto-provision.",
                        LOGIN_TIMEOUT_SECS
                    );
                }
            }
        }
    }

    if project_id.is_empty() {
        // Auto-provision via onboardUser polling.
        let onboard_body = json!({ "tierId": tier_id, "metadata": metadata });
        project_id = onboard_poll(
            client,
            onboard_base,
            &onboard_body,
            access_token,
            user_agent,
        )
        .await?
        .unwrap_or_default();
    }

    if project_id.is_empty() {
        anyhow::bail!(
            "could not determine a Cloud project. Pass --project <id> (gemini) or ensure Code Assist is enabled for this account."
        );
    }

    // Finalize: register the project so Code Assist is enabled for it.
    let finalize_body = json!({
        "tierId": tier_id,
        "metadata": metadata,
        "cloudaicompanionProject": project_id,
    });
    match onboard_poll(
        client,
        onboard_base,
        &finalize_body,
        access_token,
        user_agent,
    )
    .await
    {
        Ok(Some(p)) => {
            if !p.is_empty() {
                project_id = p;
            }
        }
        Ok(None) => {
            // Onboarding completed successfully, but response had no project ID.
            // That's fine, we keep the original project_id.
        }
        Err(e) => {
            anyhow::bail!("Failed to onboard Code Assist for project '{project_id}': {e:#}");
        }
    }

    Ok(project_id)
}

/// POST `{base}/v1internal:{method}` with a Bearer token, returning the JSON.
async fn code_assist(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    body: &Value,
    access_token: &str,
    user_agent: &str,
) -> anyhow::Result<Value> {
    let url = format!("{base}/{CODE_ASSIST_VERSION}:{method}");
    let resp = client
        .post(&url)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent)
        .json(body)
        .send()
        .await
        .with_context(|| format!("{method} request"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("{method} failed ({status}): {text}");
    }
    resp.json()
        .await
        .with_context(|| format!("parse {method} response"))
}

/// Poll onboardUser until `done:true`, then extract the project ID.
async fn onboard_poll(
    client: &reqwest::Client,
    base: &str,
    body: &Value,
    access_token: &str,
    user_agent: &str,
) -> anyhow::Result<Option<String>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(ONBOARD_TIMEOUT_SECS);
    loop {
        let resp = code_assist(client, base, "onboardUser", body, access_token, user_agent).await?;
        if resp.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
            let project = resp.get("response").and_then(extract_project);
            return Ok(project);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("Onboarding timed out after {ONBOARD_TIMEOUT_SECS} seconds");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// `cloudaicompanionProject` may be a bare string or an object with `id`.
fn extract_project(v: &Value) -> Option<String> {
    v.get("cloudaicompanionProject").and_then(value_to_project)
}

fn value_to_project(p: &Value) -> Option<String> {
    if let Some(s) = p.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    p.get("id")
        .and_then(|i| i.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn rfc3339_from_now(expires_in: u64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(expires_in as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

async fn list_projects(
    client: &reqwest::Client,
    access_token: &str,
    user_agent: &str,
) -> Vec<Value> {
    const BASE_URL: &str = "https://cloudresourcemanager.googleapis.com/v1/projects?pageSize=300&filter=lifecycleState%3AACTIVE";
    let mut out: Vec<Value> = Vec::new();
    let mut page_token: Option<String> = None;
    loop {
        let url = match &page_token {
            Some(t) => format!("{BASE_URL}&pageToken={}", oauth_util::percent_encode(t)),
            None => BASE_URL.to_string(),
        };
        let resp = match client
            .get(&url)
            .bearer_auth(access_token)
            .header("User-Agent", user_agent)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("gemini login: projects.list transport error: {e}");
                break;
            }
        };
        if !resp.status().is_success() {
            warn!("gemini login: projects.list returned {}", resp.status());
            break;
        }
        let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));
        if let Some(arr) = body.get("projects").and_then(|p| p.as_array()) {
            out.extend(arr.iter().filter_map(|p| {
                let id = p.get("projectId").and_then(|x| x.as_str())?;
                let name = p
                    .get("name")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(id);
                Some(json!({ "id": id, "name": name }))
            }));
        }
        page_token = body
            .get("nextPageToken")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        if page_token.is_none() {
            break;
        }
    }
    out.sort_by(|a, b| {
        a["id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["id"].as_str().unwrap_or(""))
    });
    out
}

fn write_cred(filename: &str, cred: &Value) -> anyhow::Result<()> {
    let dir = creds::our_auth_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create auth dir {}", dir.display()))?;
    let path = dir.join(filename);
    let content = serde_json::to_string_pretty(cred)?;
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    println!("Saved credentials to {}", path.display());
    Ok(())
}
