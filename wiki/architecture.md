# Claude Proxy Architecture

## Overview
`claude-proxy` is a local, Rust-based MITM (Man-in-the-Middle) HTTPS proxy specifically designed to intercept and optimize traffic for the `claude` CLI. It reduces unnecessary Vertex AI resource consumption ("heat-up" requests) and caches Google OAuth tokens to minimize latency and redundant token generation.

## Core Components

The application is structured into four primary modules:

1. **`main.rs`**: Application entry point, orchestrating configuration loading, certificate initialization, and spawning the async Tokio-based proxy server.
2. **`config.rs`**: Configuration management, reading from environment variables (e.g., `HTTPS_PROXY`) or falling back to a local `config.toml`.
3. **`certs.rs`**: Certificate generation and management using `rcgen` and `rustls`, handling the creation of the local Root CA and dynamic leaf certificates for intercepted domains.
4. **`proxy.rs`**: The core HTTP/HTTPS proxy engine built on `hyper` and `reqwest`. Handles `CONNECT` upgrades, TLS termination, and request routing.
5. **`interceptors.rs`**: The business logic for modifying specific API flows (Google OAuth and Vertex AI).

## Architectural Flow

### 1. Interception and TLS Termination
- The proxy listens on `127.0.0.1:7777`.
- When the `claude` CLI sends an HTTPS request (e.g., to `oauth2.googleapis.com`), it first sends an HTTP `CONNECT` request to the proxy.
- **`proxy::handle_connect`**: The proxy intercepts the `CONNECT` request, dynamically generates a TLS leaf certificate signed by the local Root CA (`~/.config/claude-proxy/ca.crt`), and terminates the TLS connection.
- The decrypted HTTP request is then routed to `handle_intercepted_request`.

### 2. Request Routing & Interception Logic
Within `handle_intercepted_request`, traffic is evaluated in this order: **Map Local → Gemini → Anthropic → Claude-OAuth → Google OAuth caching → Vertex AI heat-up → request dedup → upstream forward.** A hit at any stage short-circuits the rest. The API-surface stages are gated by host and by the body's `model` — see [gemini-providers.md](gemini-providers.md) and [claude-oauth.md](claude-oauth.md); the stages below them are the `interceptors.rs` rules documented here.

The plain-HTTP (origin) branch of `handle_request` runs **Map Local → Gemini → (Claude-OAuth → Anthropic) → OpenAI**. Note the inverted pair: inside the `/v1/messages` block Claude-OAuth is checked *before* `gemini::anthropic::try_handle`, because that handler serves every `/v1/messages` POST (404-ing an unroutable model) rather than declining, and would otherwise shadow the Claude-OAuth surface entirely. Don't reorder it.

#### A. Map Local (runs first)
- **Trigger**: any `[[map_local]]` rule in `config.toml` whose URL pattern (fnmatch-style wildcards) and optional method match the request.
- **Effect**: returns a fixed response (inline `body`, local `file`, or empty) and bypasses every later interceptor — including upstream forwarding. Also runs in the plain-HTTP branch of `handle_request`; that's the only reason non-CONNECT is accepted at all.
- **Detail**: see [map-local.md](map-local.md) — config schema, wildcard semantics, specificity tiebreaker, Content-Type defaulting matrix, error envelope, and the canonical verification cases.

#### B. Google OAuth Token Caching
- **Trigger**: `POST https://oauth2.googleapis.com/token`
- **Logic (`handle_token_request`)**:
  - The proxy reads the request body (containing the refresh token).
  - It checks for a cached token on disk at `~/.config/gcloud/application_default_credentials_access_token.json`.
  - **Disk Cache Hit**: If the cached token matches the request body and is not expired (`expires_on > now`), it calculates `expires_in` dynamically and returns a `200 OK` JSON response directly to the CLI, bypassing the network.
  - **In-Flight Promise Cache**: If multiple requests for the exact same token payload hit the proxy concurrently (before the first one writes to disk), the proxy uses a global Tokio `Mutex<HashMap>` to deduplicate them. The secondary requests subscribe to a `broadcast::channel` and pause execution until the primary request resolves.
  - **Cache Miss**: The primary request is forwarded upstream to Google. Upon a successful HTTP response:
    - The JSON payload is parsed.
    - An `expires_on` timestamp is calculated as `now + expires_in * 1000`.
    - The full token payload, including the original request body, is serialized and saved to `~/.config/gcloud/application_default_credentials_access_token.json` (`save_token_cache`).
    - The newly fetched token is broadcasted (`resolve_token_promise`) to any suspended secondary requests, allowing them to return immediately without hitting the network.

#### C. Vertex AI Heat-Up Blocking
- **Trigger**: `POST https://aiplatform.googleapis.com/.../models/claude-*:rawPredict`
- **Logic (`handle_vertex_heatup`)**:
  - The proxy inspects the JSON payload.
  - If the request is a single "user" message containing only `"."` with `max_tokens: 1`, it is identified as a "heat-up" request.
  - The proxy generates a mock Vertex AI JSON response (e.g., `text: "Hello"`) with a randomly generated Vertex-style ID (`msg_vrtx_...`).
  - This mock response is returned immediately, preventing the burn of Vertex AI resources.

#### D. Request Dedup
See [request-dedup.md](request-dedup.md) for the in-flight dedup machinery (`BufferedResponse`, `REQUEST_PROMISES`, `RequestPrimaryGuard`). Heat-up and Map Local must short-circuit before this stage so synthetic-response candidates don't inflate the in-flight set.

#### E. Upstream Forwarding
- **Trigger**: Any request not matching the interceptor rules.
- **Logic**: The request is forwarded to its original destination using a `reqwest::Client`.
- **Upstream Proxy Support**: If configured (via `HTTPS_PROXY` or `config.toml`), `reqwest` routes traffic through an external proxy (like Proxyman). The client is configured with `.danger_accept_invalid_certs(true)` to gracefully handle upstream MITM proxies without SSL verification errors.

## Configuration & Environment

- **`config.toml`**: Supports the following fields:
  - `upstream_proxy`: Route upstream traffic through another proxy (e.g., `"http://127.0.0.1:9090"`).
  - `ca_cert_path` / `ca_key_path` (optional, must be set together): Use a user-supplied PEM CA cert+key instead of the auto-generated one. Both must point to PEM-encoded files. Tilde expansion is supported. If only one is set, the proxy exits with an error at startup.
  - `[[map_local]]` (zero or more): Map a wildcard URL + optional method to a fixed response (inline `body`, local `file`, or empty). See section "A. Map Local" above and the example config in [README.md](../README.md). Tilde expansion is applied to `file` at config load.
- **CA loading precedence** (`certs::get_or_create_ca`):
  1. User-supplied pair from `config.toml` (`ca_cert_path` + `ca_key_path`) — loaded via `CertificateParams::from_ca_cert_pem`.
  2. Managed CA on disk at `~/.config/claude-proxy/ca.{crt,key}` — same load path.
  3. Generate a new CA cert (saved to the managed path for future restarts).
- **Auto-generated CA extensions**: The CA cert includes `CN=Claude Proxy CA`, `O=Claude Proxy`, `basicConstraints=CA:TRUE`, and `keyUsage=keyCertSign,cRLSign,digitalSignature` — required by strict validators (e.g., macOS Security.framework / libcurl / cargo).
- **Environment Variables**:
  - `NODE_EXTRA_CA_CERTS`: Used by the `claude` CLI (Node.js) to trust the proxy's CA (`~/.config/claude-proxy/ca.crt` or the user-supplied cert).
  - `NODE_TLS_REJECT_UNAUTHORIZED=0`: Alternative to bypass CLI cert validation.
  - `CARGO_HTTP_CAINFO`: Set to the proxy CA cert so `cargo` accepts MITM leaf certs.

## Security Considerations
- The Root CA key (`ca.key`) is stored locally in `~/.config/claude-proxy/` and is used solely for dynamic cert generation during the proxy session.
- Upstream SSL verification is disabled to support chaining with external debugging proxies (e.g., Proxyman), which is safe in this controlled local development environment.