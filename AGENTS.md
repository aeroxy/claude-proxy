# AGENTS.md

Guidance for AI agents working on `claude-proxy`.

## What this is

A local Rust HTTPS MITM proxy for the `claude` CLI. It does two specific things and nothing else:

1. **Caches Google OAuth tokens** so `oauth2.googleapis.com/token` round-trips don't happen on every invocation.
2. **Short-circuits Vertex AI heat-up requests** (`max_tokens: 1`, single `"."` user message) so they never burn upstream tokens.

Everything else passes through to `reqwest`, optionally chained via `HTTPS_PROXY` / `config.toml` `upstream_proxy` (e.g. Proxyman, mitmproxy at `localhost:9090`).

## Where to read first

- [wiki/architecture.md](wiki/architecture.md) — full module layout, request flow, OAuth caching state machine, Vertex AI heat-up detection, TLS termination details. **Read this before changing anything in `src/`.**
- [README.md](README.md) — user-facing setup (build, trust CA, set env vars).

## Module map

| File | Responsibility |
| --- | --- |
| [src/main.rs](src/main.rs) | Entry point. Loads config, initializes CA, starts proxy. Tracing config lives here (`RUST_LOG` honored, default `info,claude_proxy=debug`). |
| [src/config.rs](src/config.rs) | Reads `HTTPS_PROXY` env var, falls back to `config.toml`. |
| [src/certs.rs](src/certs.rs) | Root CA at `~/.config/claude-proxy/ca.{crt,key}` (rcgen 0.11). Dynamic leaf certs per intercepted host. |
| [src/proxy.rs](src/proxy.rs) | Hyper server, `CONNECT` handling, TLS termination, request routing, upstream forwarding. |
| [src/interceptors.rs](src/interceptors.rs) | OAuth token cache (disk + in-flight dedup) and Vertex AI heat-up detection. |

## Key invariants — do not break

- **`PrimaryGuard` is RAII.** The primary OAuth fetcher owns a `PrimaryGuard` whose `Drop` impl removes the in-flight entry from `TOKEN_PROMISES`. This is what prevents secondary waiters from hanging forever if the primary's task is cancelled (e.g., client disconnect). Resolve via `guard.resolve(token)` for normal completion; never call `mem::forget` on it; never re-introduce the old `resolve_token_promise` free function.
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
3. Running `claude` with `HTTPS_PROXY=http://127.0.0.1:6666` and `NODE_EXTRA_CA_CERTS=~/.config/claude-proxy/ca.crt`.
4. Watching logs for the expected `Cache hit on disk for token` / `Intercepted Vertex AI heat-up request` lines on subsequent invocations.

## Things to be careful about

- **rcgen is pinned to 0.11.** 0.14 has a different API (no `Certificate::from_params`, `from_ca_cert_pem`); reverting was deliberate. Don't bump it without rewriting [src/certs.rs](src/certs.rs).
- **rand is 0.10**, which uses `RngExt::sample_iter` (not the older `Rng::sample_iter`). Keep the `RngExt` import in [src/interceptors.rs](src/interceptors.rs).
- **Don't add features the proxy doesn't need.** This codebase is intentionally narrow — it intercepts two endpoints and forwards the rest. Resist adding "while we're here" abstractions.
