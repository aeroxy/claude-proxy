# Claude Proxy

A local HTTPS MITM proxy specifically designed to optimize the `claude` CLI tool's behavior.

## Features
- Caches Google OAuth tokens locally to speed up execution
- Blocks unnecessary Vertex AI heat-up calls natively
- Deduplicates byte-identical concurrent requests so duplicates don't burn upstream tokens
- Auto-recovers from expired credentials: when Google returns `invalid_grant`, opens a browser, runs the consent flow, writes a fresh ADC, and resumes the in-flight request transparently (see [wiki/auto-reauth.md](https://github.com/aero/claude-proxy/blob/master/wiki/auto-reauth.md))
- **Map Local**: return a fixed response (inline body or local file) for a configured URL pattern + method instead of forwarding upstream — silence telemetry, neuter update checks, replay fixtures
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
   export HTTPS_PROXY=http://127.0.0.1:6666
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
claude-proxy start                    # daemonize on 6666 (or next free port up to 6675)
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

`HTTPS_PROXY` is **not** read for `upstream_proxy` — it's a client-side var meant to point clients at this proxy, and reading it here would make the proxy chain through itself when `HTTPS_PROXY=http://127.0.0.1:6666` is set in the same shell. Configure chained proxies (Proxyman, mitmproxy) explicitly via `upstream_proxy = "..."` in `config.toml`.

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
