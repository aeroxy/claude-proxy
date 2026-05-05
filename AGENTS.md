## What this is

A local Rust HTTPS MITM proxy for the `claude` CLI. It does four specific things and nothing else:

1. **Caches Google OAuth tokens** so `oauth2.googleapis.com/token` round-trips don't happen on every invocation.
2. **Short-circuits Vertex AI heat-up requests** (`max_tokens: 1`, single `"."` user message) so they never burn upstream tokens.
3. **Deduplicates byte-identical concurrent requests** so a duplicate POST results in one upstream call, with both clients receiving the buffered response.
4. **Auto-recovers from expired credentials** — on `invalid_grant` from Google OAuth, opens a browser, runs the consent flow, writes a fresh ADC, and returns a valid token to the client transparently.

Everything else passes through to `reqwest`, optionally chained via `HTTPS_PROXY` / `config.toml` `upstream_proxy` (e.g. Proxyman, mitmproxy at `localhost:9090`).

## Where to read first

- [wiki/architecture.md](wiki/architecture.md) — full module layout, request flow, OAuth caching state machine, Vertex AI heat-up detection, TLS termination details. **Read this before changing anything in `src/`.**
- [wiki/request-dedup.md](wiki/request-dedup.md) — in-flight request dedup: cache key, state machine, RAII guard semantics, header filtering, validation steps.
- [wiki/auto-reauth.md](wiki/auto-reauth.md) — automatic browser-based OAuth re-auth on `invalid_grant`: detection, spawned-task lifecycle, `REAUTH_PROMISE` gating, RAII safety, validation steps.
- [README.md](README.md) — user-facing setup (build, trust CA, set env vars).

## Module map

| File | Responsibility |
| --- | --- |
| [src/main.rs](src/main.rs) | Entry point. Loads config, initializes CA, starts proxy. Tracing config lives here (`RUST_LOG` honored, default `info,claude_proxy=debug`). |
| [src/config.rs](src/config.rs) | Reads `upstream_proxy` from `config.toml` (CLI `--config`, then `./config.toml`, then `~/.config/claude-proxy/config.toml`). Deliberately does **not** read `HTTPS_PROXY` — that var is for clients pointing at us; reading it here would chain the proxy through itself. |
| [src/certs.rs](src/certs.rs) | Root CA at `~/Library/Application Support/claude-proxy/ca.{crt,key}` (rcgen 0.11). Dynamic leaf certs per intercepted host. |
| [src/proxy.rs](src/proxy.rs) | Hyper server, `CONNECT` handling, TLS termination, request routing, upstream forwarding. |
| [src/interceptors.rs](src/interceptors.rs) | OAuth token cache (disk + in-flight dedup), Vertex AI heat-up detection, and the generic request-dedup machinery (`BufferedResponse`, `REQUEST_PROMISES`, `RequestPrimaryGuard`, `handle_dedup_request`). |
| [src/reauth.rs](src/reauth.rs) | Browser OAuth flow triggered on `invalid_grant`. `REAUTH_PROMISE` gate, `ReauthGuard` RAII, callback server, ADC writer, non-proxied token-exchange client. See [wiki/auto-reauth.md](wiki/auto-reauth.md). |
| [src/daemon.rs](src/daemon.rs) | `start` / `stop` / `restart` subcommands. PID files at `~/.config/claude-proxy/pids/{port}.pid`, logs at `~/.config/claude-proxy/log/{epoch}.log`. |

## Key invariants — do not break

- **`PrimaryGuard` is RAII.** The primary OAuth fetcher owns a `PrimaryGuard` whose `Drop` impl removes the in-flight entry from `TOKEN_PROMISES`. This is what prevents secondary waiters from hanging forever if the primary's task is cancelled (e.g., client disconnect). Resolve via `guard.resolve(token)` for normal completion; never call `mem::forget` on it; never re-introduce the old `resolve_token_promise` free function.
- **`RequestPrimaryGuard` is RAII (same pattern).** Mirrors `PrimaryGuard` for the generic request-dedup map (`REQUEST_PROMISES`). Always resolve via `guard.resolve(Some(buf))` on 2xx or `guard.resolve(None)` on non-2xx / upstream error so secondaries fall through to native fetch. The `Err` arm of the upstream send must resolve too — leaking a guard there strands secondaries until Drop fires.
- **`ReauthGuard` is RAII and lives inside a `tokio::spawn`-ed task.** The OAuth browser flow must outlive the request handler that triggered it — clients commonly time out during the 5-minute browser window, and the next retry depends on finding the in-progress flow via `REAUTH_PROMISE`. Keep `run_oauth_flow()` spawned, not inline. See [wiki/auto-reauth.md](wiki/auto-reauth.md).
- **The token-exchange POST in `reauth.rs` uses a `no_proxy()` `reqwest::Client`.** The proxy's main client may chain through `HTTPS_PROXY` (which is us). A proxied exchange would loop back through the very interceptor that triggered re-auth. Don't share the proxy's `reqwest::Client` with `reauth.rs`.
- **Dedup runs after OAuth and heat-up interceptors.** Heat-ups must short-circuit before the dedup map is touched, otherwise a heat-up-shaped body would inflate the in-flight set with synthetic-response candidates. Keep the order in [src/proxy.rs::handle_intercepted_request](src/proxy.rs).
- **Dedup cache key is `format!("{} {}\n{}", method, url, body_str)`.** Method + URL prevent unrelated empty-body GETs from false-deduping; body is the discriminator. If you ever change this, update [wiki/request-dedup.md](wiki/request-dedup.md).
- **Cache key is the request body.** `~/.config/gcloud/application_default_credentials_access_token.json` stores the original `request_body` alongside the token; cache hits require an exact body match. If a caller's body varies between calls (e.g., JWT-bearer flows where `iat`/`exp` change), the cache will miss every time. If you need to support that case, change the key strategy — don't paper over it.
- **Upstream forwarding strips three headers**: `host`, `accept-encoding`, `content-length`. Keep that list in sync if you add header logic; messing with `accept-encoding` will silently break OAuth JSON parsing (we don't enable reqwest's `gzip` feature).
- **`danger_accept_invalid_certs(true)`** on the upstream `reqwest::Client` is intentional — it lets us chain through Proxyman/mitmproxy. Don't remove it.
- **Listening port is `127.0.0.1:6666`**, hardcoded. Change in [src/proxy.rs](src/proxy.rs) if needed.

## Build and test

```bash
cargo build              # debug
cargo build --release    # what users actually run
RUST_LOG=claude_proxy=debug,reqwest=debug,hyper=info target/release/claude-proxy
```

There are no automated tests yet. Validate changes by:
1. Building.
2. Running the proxy.
3. Running `claude` with `HTTPS_PROXY=http://127.0.0.1:6666` and `NODE_EXTRA_CA_CERTS=~/Library/Application\ Support/claude-proxy/ca.crt`.
4. Watching logs for the expected `Cache hit on disk for token` / `Intercepted Vertex AI heat-up request` lines on subsequent invocations.
5. For dedup: fire two byte-identical concurrent requests (e.g. `sed '1 s/^curl /curl -k /' refs/1.sh | bash & sed '1 s/^curl /curl -k /' refs/2.sh | bash & wait`) and confirm the log shows one `We are the primary fetcher` plus one `Waiting on primary in-flight request` followed by `Received response from primary in-flight request`.

## Things to be careful about

- **rcgen is pinned to 0.11.** 0.14 has a different API (no `Certificate::from_params`, `from_ca_cert_pem`); reverting was deliberate. Don't bump it without rewriting [src/certs.rs](src/certs.rs).
- **rand is 0.10**, which uses `RngExt::sample_iter` (not the older `Rng::sample_iter`). Keep the `RngExt` import in [src/interceptors.rs](src/interceptors.rs).
- **Don't add features the proxy doesn't need.** This codebase is intentionally narrow — it intercepts two endpoints and forwards the rest. Resist adding "while we're here" abstractions.
- **README.md must use full GitHub URLs for `wiki/` links.** The `include` list in [Cargo.toml](Cargo.toml) ships `README.md` to crates.io but **not** the `wiki/` directory — relative links like `[wiki/auto-reauth.md](wiki/auto-reauth.md)` render as broken links on the crates.io page. Always link wiki pages from README.md as `https://github.com/aero/claude-proxy/blob/master/wiki/<file>.md`. Internal docs (AGENTS.md, CLAUDE.md, GEMINI.md, other wiki pages) can keep relative paths since they're never published to crates.io.
