# Claude Proxy

A local HTTPS MITM proxy specifically designed to optimize the `claude` CLI tool's behavior.

[![crates.io](https://img.shields.io/crates/v/claude-proxy.svg)](https://crates.io/crates/claude-proxy)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](./LICENSE)

## Features
- Caches Google OAuth tokens locally to speed up execution
- Blocks unnecessary Vertex AI heat-up calls natively
- Deduplicates byte-identical concurrent requests so duplicates don't burn upstream tokens
- Auto-recovers from expired credentials: when Google returns `invalid_grant`, opens a browser, runs the consent flow, writes a fresh ADC, and resumes the in-flight request transparently (see [wiki/auto-reauth.md](https://github.com/aero/claude-proxy/blob/master/wiki/auto-reauth.md))
- **Map Local**: return a fixed response (inline body or local file) for a configured URL pattern + method instead of forwarding upstream — silence telemetry, neuter update checks, replay fixtures
- **Gemini for opencode**: serves the native Gemini API (`/v1beta/models…`) for opencode's `@ai-sdk/google`, routing each model to the `gemini-cli` or `antigravity` upstream, with `login` for each (see [Gemini models for opencode](#gemini-models-for-opencode-ai-sdkgoogle) below and [wiki/gemini-providers.md](https://github.com/aero/claude-proxy/blob/master/wiki/gemini-providers.md))
- **Anthropic API**: serves `POST /v1/messages` (+ `count_tokens`) so Claude Code / the Anthropic SDK can drive the same `gemini-cli`/`antigravity` models by a provider-prefixed model name; MITM of `api.anthropic.com` is prefix-gated so normal Claude usage passes through untouched (see [Anthropic API](#anthropic-api-v1messages-for-claude-code--the-anthropic-sdk) below)
- **OpenAI aggregator**: serves `POST /v1/chat/completions` and fans it out to multiple OpenAI-compatible backends (configured under `[[openai]]`), routing by a provider prefix on the model — a near-pure passthrough, no format translation (see [OpenAI aggregator](#openai-aggregator-v1chatcompletions) below)
- Transparently routes other traffic via existing Proxies (like Proxyman)

## How to use it

1. Build the proxy:
   ```bash
   cargo build --release
   ```

2. Trust the local CA for Node.js and cargo:
   ```bash
   export NODE_EXTRA_CA_CERTS=~/Library/Application\ Support/claude-proxy/ca.crt
   export CARGO_HTTP_CAINFO=~/Library/Application\ Support/claude-proxy/ca.crt
   export HTTPS_PROXY=http://127.0.0.1:7777
   ```

3. Run the CLI:
   ```bash
   claude
   ```

> **Upgrading from an older build?** Delete the old CA and re-import the new one — earlier builds generated a CA cert missing required X.509 extensions (`keyCertSign`, proper subject DN), which caused strict TLS validators such as `cargo` to reject it.
> ```bash
> rm ~/Library/Application\ Support/claude-proxy/ca.{crt,key}
> # Start the proxy once to regenerate, then re-import ca.crt into your trust store.
> sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain \
>   ~/Library/Application\ Support/claude-proxy/ca.crt
> ```

## Running as a daemon

```bash
claude-proxy start                    # daemonize on 7777 (or next free port up to 7786)
claude-proxy --port 7000 start        # pick a starting port
claude-proxy stop                     # SIGTERM all running daemons
claude-proxy --port 7000 stop         # stop a specific instance
claude-proxy restart                  # stop + start (no-op stop if nothing running)
claude-proxy --port 7000 restart      # restart a specific instance
```

Logs are written to `~/.config/claude-proxy/log/{epoch}.log`, one file per `start`. PID files live at `~/.config/claude-proxy/pids/{port}.pid`.

## Configuration

Config lookup order (first match wins):

1. `--config <path>` if provided
2. `./config.toml` in the current working directory
3. `~/.config/claude-proxy/config.toml`

`HTTPS_PROXY` is **not** read for `upstream_proxy` — it's a client-side var meant to point clients at this proxy, and reading it here would make the proxy chain through itself when `HTTPS_PROXY=http://127.0.0.1:7777` is set in the same shell. Configure chained proxies (Proxyman, mitmproxy) explicitly via `upstream_proxy = "..."` in `config.toml`.

### Listening port

The port defaults to `7777`. Set it in `config.toml` with a top-level `port`, or override per-invocation with `--port`. Precedence is **`--port` (CLI) > `port` (config) > `7777`**.

```toml
# ~/.config/claude-proxy/config.toml
port = 7000
```

### Using a custom CA

If you already manage a CA (e.g. one signed by a corporate root already in your trust store), you can point the proxy at it instead of using the auto-generated one. The proxy needs both the cert **and** the private key to sign leaf certs for each intercepted host — a public cert alone (`.cer` file) is not sufficient.

```toml
# ~/.config/claude-proxy/config.toml
upstream_proxy = "http://127.0.0.1:9090"  # optional

# Both must be set together. PEM format. Tilde expansion supported.
ca_cert_path = "~/.certs/my-ca.crt"
ca_key_path  = "~/.certs/my-ca.key"
```

If only one of the two fields is set, the proxy will exit with an error at startup.

### Map Local

Make the proxy return a fixed response for a URL match — useful for silencing telemetry, blocking update checks, or replaying canned fixtures without sending upstream traffic. Each `[[map_local]]` rule needs a `url` (with `*` and `?` wildcards allowed) and may pin a `method`. The body comes from one of: an inline `body` string, a `file` path on disk, or neither (empty body). When the body is a file, the proxy reads it live on every matching request, so editing the file is reflected immediately without restarting the proxy.

When several rules could match the same request, the most specific one wins (more literal characters in `url` outranks a fuzzier pattern; rules that pin a `method` outrank any-method rules). `Content-Type` defaults intelligently: explicit `content_type` wins, otherwise it's derived from the file extension for file-backed rules, or `application/json` for non-empty inline bodies, or omitted entirely for empty responses. The proxy always adds `X-Map-Local: true` (and `X-Map-Local-Source: <path>` for file-backed rules) so you can tell mocked responses from real ones.

```toml
# Datadog log intake — return 202 with an empty JSON object so the client thinks it succeeded.
[[map_local]]
url    = "https://*.datadoghq.com/api/v2/logs"
method = "POST"
status = 202
body   = "{}"

# Plain HTTP works too — useful for internal telemetry endpoints.
[[map_local]]
url    = "http://192.168.0.1/x/y/z"
method = "POST"
body   = "{}"

# Anthropic event-logging batches — pretend the server accepted everything.
[[map_local]]
url    = "https://api.anthropic.com/api/event_logging/v2/batch"
method = "POST"
body   = '{"accepted_count":100,"rejected_count":0}'

# Claude Code update check — return 200 with no body and no Content-Type header.
[[map_local]]
url    = "https://downloads.claude.ai/claude-code-releases/latest"
method = "GET"

# File-backed rule. Edit the file at any time — next matching request sees the change.
# Content-Type is auto-derived from the extension (.json -> application/json).
[[map_local]]
url    = "https://api.anthropic.com/v1/messages"
method = "POST"
file   = "~/dev/mocks/messages.json"
[map_local.headers]
"x-mock-source" = "fixture"
```

If a rule's `file` is missing or unreadable at request time the proxy returns `502` with `X-Map-Local-Error: file-unreadable` and a body explaining the path that failed — loud failure beats a silent passthrough that hides "why isn't my mock working?".

Full reference (specificity tiebreaker, Content-Type defaulting matrix, plain-HTTP support, error envelope, regression-test recipes): see [wiki/map-local.md](https://github.com/aero/claude-proxy/blob/master/wiki/map-local.md).

## Gemini models for opencode (`@ai-sdk/google`)

The proxy serves the native Gemini API and routes each model to one of two Google Cloud Code Assist backends — **`gemini-cli`** and **`antigravity`** — the same way [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) does. Credential files are compatible: anything in `~/.cli-proxy-api/` is read as-is, and `login` writes new ones to `~/.config/claude-proxy/auths/`.

1. **Sign in** (opens a browser):
   ```bash
   claude-proxy login gemini            # Google account (Code Assist) → gemini-cli provider
   claude-proxy login gemini --project my-gcp-project   # skip project auto-discovery
   claude-proxy login antigravity       # antigravity account
   ```

2. **Point opencode at the proxy.** Either transport works:

   - **Origin (simplest, no CA):** set the Google provider `baseURL` to `http://127.0.0.1:7777/v1beta` and any dummy API key.
   - **MITM (no opencode config):** keep the default Google endpoint and run opencode with `HTTPS_PROXY=http://127.0.0.1:7777` and `NODE_EXTRA_CA_CERTS=~/Library/Application\ Support/claude-proxy/ca.crt`.

3. **Pick a model by provider prefix.** The provider is the first segment of the model name: `gemini-cli/<model>` or `antigravity/<model>` — e.g. `gemini-cli/gemini-2.5-pro`, `gemini-cli/gemini-2.5-flash`, `antigravity/claude-sonnet-4-6`, `antigravity/gemini-3-pro-high`. The part after the prefix is sent upstream as-is, so any model your account can serve works (not just catalogued ones). `GET /v1beta/models` lists the known models (provider-prefixed) for the providers you have credentials for. Streaming (`:streamGenerateContent`) and `:countTokens` are supported.

Optional `config.toml` knobs:

```toml
[gemini]
# Defaults to ["~/.config/claude-proxy/auths", "~/.cli-proxy-api"] when omitted.
auth_dirs = ["~/.config/claude-proxy/auths", "~/.cli-proxy-api"]
```

Full reference (endpoints, prefix routing, request/response envelope, credential formats, `login` flow internals): see [wiki/gemini-providers.md](https://github.com/aero/claude-proxy/blob/master/wiki/gemini-providers.md).

## Anthropic API (`/v1/messages`) for Claude Code & the Anthropic SDK

The same `gemini-cli` / `antigravity` backends are also exposed through the **Anthropic Messages API**, so any Anthropic-API client can drive Gemini (and antigravity's `claude-*`) models. Sign in once with `claude-proxy login …` as above, then:

- **Origin (simplest, no CA):** point your client's base URL at `http://127.0.0.1:7777` (e.g. `ANTHROPIC_BASE_URL=http://127.0.0.1:7777` for Claude Code) and use any dummy API key.
- **MITM (no client config):** run the client with `HTTPS_PROXY=http://127.0.0.1:7777` and `NODE_EXTRA_CA_CERTS=~/Library/Application\ Support/claude-proxy/ca.crt`. Interception of `api.anthropic.com` is **gated on the provider prefix** — requests whose `model` is *not* `gemini-cli/…` or `antigravity/…` pass straight through to the real Anthropic API, so normal Claude usage is unaffected.

Set the request `model` to a provider-prefixed name (e.g. `gemini-cli/gemini-2.5-pro`, `antigravity/claude-sonnet-4-6`). `POST /v1/messages` (streaming and non-streaming) and `POST /v1/messages/count_tokens` are supported.

```bash
curl -s http://127.0.0.1:7777/v1/messages -H 'content-type: application/json' \
  -d '{"model":"gemini-cli/gemini-2.5-pro","max_tokens":1024,
       "messages":[{"role":"user","content":"hi"}]}'
```

## OpenAI aggregator (`/v1/chat/completions`)

Serves the OpenAI Chat Completions API and fans it out to one or more OpenAI-compatible
backends. There is **no format translation** — OpenAI in, OpenAI out; the proxy only picks
the backend, rewrites the `model`, and pipes the response through (streaming included).

Configure each backend under `[[openai]]` in `config.toml`:

```toml
# ~/.config/claude-proxy/config.toml
[[openai]]
name = "opengateway"                       # the provider prefix
base_url = "https://opengateway.example/v1" # POSTed to {base_url}/chat/completions
api_key = "sk-..."                          # optional; if omitted, your client's
                                            # Authorization header is forwarded instead
  [openai.headers]                          # optional extra upstream headers
  X-Title = "claude-proxy"

[[openai]]
name = "openrouter"
base_url = "https://openrouter.ai/api/v1"
api_key = "sk-or-v1-..."
```

**Origin mode only** — point your OpenAI client at the proxy
(`OPENAI_BASE_URL=http://127.0.0.1:7777`), no CA trust needed. There is no MITM of
`api.openai.com`.

The request `model` is `<provider>/<upstream-model>`: the first `/`-segment selects the
`[[openai]]` provider; everything after it is forwarded verbatim as the upstream model. So
`opengateway/minimax/minimax-m3` routes to `opengateway` and asks it for `minimax/minimax-m3`.

```bash
curl -s http://127.0.0.1:7777/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"opengateway/minimax/minimax-m3",
       "messages":[{"role":"user","content":"hi"}]}'
```
