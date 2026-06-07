# Automatic OAuth Re-authentication

## What it does

When `oauth2.googleapis.com/token` returns HTTP 400 with `"error": "invalid_grant"` (typically because the RAPT or refresh token has expired and Google demands re-consent), the proxy:

1. Detects the error in the upstream response body.
2. Deletes the stale access-token disk cache.
3. Spawns a browser-based OAuth flow (same client ID as the `gcloud` CLI).
4. Captures the authorization code on a localhost callback, exchanges it for tokens.
5. Writes a fresh `~/.config/gcloud/application_default_credentials.json`.
6. Returns a synthetic 200 token response to the original client — the client never sees the 400.

The user just sees a browser tab open, signs in, and their `claude` invocation continues as if nothing happened.

## Scope

Only `invalid_grant` on a 400 response triggers re-auth. Other OAuth errors (5xx, network failures, malformed JSON) fall through to the existing pass-through path.

The detection lives in [src/proxy.rs::handle_intercepted_request](../src/proxy.rs) inside the `if let Some(guard) = primary_guard` non-success branch — i.e., it runs only on the OAuth interceptor path, never on generic forwarded traffic.

## State machine

`reauth::handle_invalid_grant() -> Option<ReauthResult>`:

- **Empty `REAUTH_PROMISE`** — caller becomes primary. Creates a `broadcast::channel`, stores the `Sender` in the static, **`tokio::spawn`s** `run_oauth_flow()` as a detached task, then subscribes to the channel and awaits.
- **Occupied `REAUTH_PROMISE`** — caller subscribes to the existing broadcast and awaits. No second browser window opens.

The broadcast carries `Option<ReauthResult>` where `ReauthResult { token_response_json }` holds the raw JSON from Google's code-exchange response. Callers turn that into a `GoogleTokenFile` via the existing `interceptors::save_token_cache()`.

## Why the OAuth flow is spawned

`run_oauth_flow()` lives inside a `tokio::spawn`-ed task, not inline in the request handler. This is the key to surviving client disconnects.

If the original request handler is dropped (the `claude` CLI's HTTP client times out and closes the connection during the 5-minute browser window):

1. The handler's future is dropped.
2. `PrimaryGuard` (TOKEN_PROMISES) drops with `resolve=false` — broadcasts `None` to TOKEN_PROMISES secondaries, who fall through to native fetch.
3. **The spawned re-auth task continues** — it owns the `ReauthGuard` and the broadcast `Sender`.
4. When the user retries (e.g., re-runs `claude`), the new token request forwards to Google → still gets 400 → calls `handle_invalid_grant()` → finds `REAUTH_PROMISE` occupied → subscribes and waits.
5. When the spawned flow finishes, the retry receives the fresh token.

If `run_oauth_flow` were inline, dropping the handler would drop the OAuth flow itself and the next retry would re-prompt the browser.

## Resolution rules

| Outcome | Primary broadcasts | Caller behavior |
| --- | --- | --- |
| User signs in, code exchange succeeds | `Some(ReauthResult)` | Caller writes new ADC, caches access token, returns synthetic 200 with the fresh token. |
| User cancels / browser fails to open / timeout (5 min) | `None` | Caller falls through and returns the original 400 to the client. |
| Token exchange returns non-2xx | `None` | Same as above. |
| Spawned task panics | `ReauthGuard::drop` clears `REAUTH_PROMISE`; `Sender` drops; secondaries see `RecvError::Closed` | Falls through to original 400. A subsequent `invalid_grant` can trigger a fresh re-auth. |

## Why a single `Mutex<Option<...>>` (not a `HashMap`)

Unlike OAuth token requests (keyed by request body, since different callers may want different scopes) and generic dedup (keyed by `method+url+body`), re-auth is a **global** operation: there is only one ADC file, and only one in-flight browser flow can be meaningful at a time. The single-slot `Mutex<Option<Arc<Sender<...>>>>` matches that domain shape exactly.

## RAII cancellation safety

`ReauthGuard` mirrors the OAuth `PrimaryGuard` pattern from [src/interceptors.rs](../src/interceptors.rs). On drop without a prior `resolve()`:

1. `try_lock` `REAUTH_PROMISE` and clear it synchronously, OR
2. If contended, spawn a cleanup task that does the same.

The guard lives inside the spawned task. If the task panics or is aborted, Drop fires and clears the static so the next `invalid_grant` can start a fresh flow. Never call `mem::forget` on a `ReauthGuard`.

## Critical: non-proxied client for the token exchange

The proxy's main `reqwest::Client` chains through any configured upstream proxy, and `HTTPS_PROXY` typically points to `127.0.0.1:7777` (us). The token-exchange POST inside `run_oauth_flow()` therefore builds a fresh `reqwest::Client::builder().no_proxy().build()` — without it, the exchange would route back through the proxy and hit the very interceptor that triggered re-auth, creating a loop.

## OAuth flow details

- **Client ID / secret**: Google's public desktop OAuth client (same values `gcloud auth application-default login` uses). They are not secrets.
- **Scopes**: `openid`, `userinfo.email`, `cloud-platform`. The third covers Vertex AI access; the others let us identify the user.
- **Callback**: random local port (`127.0.0.1:0`), bare HTTP, single connection accepted, query string parsed for `code` or `error`.
- **Browser opening**: `std::process::Command::new("open").arg(url).spawn()` — macOS-only. If cross-platform support is needed later, swap in the `open` crate.
- **Timeout**: 5 minutes (`REAUTH_TIMEOUT_SECS`). After that, the spawned task resolves with `None`.

## Things deliberately not done

- **No retry of the original token request.** The code-exchange response itself contains an `access_token` that is identical in shape to a refresh response, so we synthesize the success response directly. One round-trip instead of two.
- **No `gcloud` shell-out.** Implementing the OAuth flow in-process avoids a hard dependency on the `gcloud` CLI being installed and in `$PATH`.
- **No detection of `invalid_rapt` as a separate code.** Google returns `invalid_grant` for both. Adding `invalid_rapt` matching would be dead code on the current API surface.
- **No persistence of the refresh-token across re-auths beyond the ADC file.** That file is the source of truth — the proxy never holds long-lived credentials in memory.

## Validation

1. Corrupt the refresh token in `~/.config/gcloud/application_default_credentials.json` (e.g., flip a few characters).
2. Run the proxy in foreground: `RUST_LOG=claude_proxy=debug target/debug/claude-proxy`.
3. Run `claude` with `HTTPS_PROXY=http://127.0.0.1:7777`.
4. Expected proxy log:

   ```
   Intercepted: POST https://oauth2.googleapis.com/token
   Upstream response status for https://oauth2.googleapis.com/token: 400 Bad Request
   Google OAuth upstream returned status 400 Bad Request. Body: {"error":"invalid_grant", ...}
   Detected invalid_grant — initiating automatic re-authentication
   Starting automatic re-authentication via browser OAuth flow...
   Opening browser for Google re-authentication...
   ```

5. A browser tab opens to Google's consent screen. Sign in.
6. Expected continuation:

   ```
   Received authorization code, exchanging for tokens...
   Updated ADC credentials at "~/.config/gcloud/application_default_credentials.json"
   Re-authentication completed successfully.
   Resolved re-auth promise waiters=1
   Re-auth succeeded. Returning fresh token to client.
   ```

7. The `claude` invocation completes normally.
8. Verify `~/.config/gcloud/application_default_credentials.json` has a new `refresh_token`.
9. Verify `~/.config/gcloud/application_default_credentials_access_token.json` has a fresh entry.
10. Re-run `claude` — no browser, just `Cache hit on disk for token`.
