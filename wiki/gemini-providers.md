# Gemini providers (opencode `@ai-sdk/google`)

`claude-proxy` serves the **native Gemini API surface** (`/v1beta/models…`) and
routes each request to one of two upstream "providers" — **`gemini-cli`** and
**`antigravity`** — both of which call the Cloud Code Assist endpoint
`https://cloudcode-pa.googleapis.com/v1internal:*`. This lets opencode's
`@ai-sdk/google` provider drive Google models through the proxy, the same way
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) does. The wire
formats and credential shapes are deliberately CLIProxyAPI-compatible.

## Endpoints served

| Method | Path | Action |
| --- | --- | --- |
| `GET` | `/v1beta/models` | List catalog (fetched live from CLIProxyAPI's source, filtered to providers with credentials) |
| `GET` | `/v1beta/models/{model}` | Single-model metadata |
| `POST` | `/v1beta/models/{model}:generateContent` | Non-streaming generation |
| `POST` | `/v1beta/models/{model}:streamGenerateContent` | SSE streaming |
| `POST` | `/v1beta/models/{model}:countTokens` | Token count |

The `models/` segment is **optional** on the generate/stream/count routes:
`@ai-sdk/google` emits `/v1beta/{model}:{action}` (no `models/`), which is
accepted identically to the canonical `/v1beta/models/{model}:{action}`.

The inbound API key (`x-goog-api-key` / `?key=`) is **ignored** — real auth is
the OAuth credential we hold for the routed provider. This is a local-trust
proxy; set any dummy key in opencode.

## Two transports (both work, same handler)

- **Origin (plain HTTP):** point `@ai-sdk/google` `baseURL` at
  `http://127.0.0.1:6666/v1beta`. Served from the plain-HTTP branch of
  `handle_request`. No CA/TLS needed.
- **MITM:** opencode keeps the default `generativelanguage.googleapis.com`
  endpoint with `HTTPS_PROXY=http://127.0.0.1:6666` + `NODE_EXTRA_CA_CERTS`.
  `handle_intercepted_request` routes the request when the SNI host is
  `generativelanguage.googleapis.com`.

## Routing: provider prefix on the model name

Routing is **prefix-based**, not a lookup table. The provider is encoded as the
first path segment of the model name; everything after it is the real model,
forwarded upstream verbatim (`src/gemini/models.rs::split_model`):

```
gemini-cli/<model>   → gemini-cli provider, upstream model = <model>
antigravity/<model>  → antigravity provider, upstream model = <model>
```

e.g. `gemini-cli/gemini-2.5-pro` calls cloudcode-pa with model `gemini-2.5-pro`
via gemini-cli creds; `antigravity/claude-sonnet-4-6` calls it with
`claude-sonnet-4-6` via antigravity creds. The request URL is therefore
`/v1beta/models/gemini-cli/gemini-2.5-pro:generateContent`. A leading `models/`
and a percent-encoded slash (`%2F`) are tolerated. A model with no recognized
prefix returns `404`.

Because routing never consults a table, **any** model under a provider prefix is
forwarded (even one newer than the catalog). The catalog is used **only** to
render `GET /v1beta/models`. That listing is fetched live from CLIProxyAPI's own
source — the same two URLs and fallback order as its `model_updater`:

1. `https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/models.json`
2. `https://models.router-for.me/models.json`

The result is cached for 3h (matching CLIProxyAPI's refresh interval); on fetch
failure it falls back to the last good result, then to the embedded
`src/gemini/models.json` (lifted from CLIProxyAPI). Setting `[gemini]
models_file` pins the listing to a local file and disables remote fetching. The
embedded/remote catalogs currently list:

| provider | catalogued model IDs (request as `<provider>/<id>`) |
| --- | --- |
| **gemini-cli** | `gemini-2.5-pro`, `gemini-2.5-flash`, `gemini-2.5-flash-lite`, `gemini-3-pro-preview`, `gemini-3.1-pro-preview`, `gemini-3-flash-preview`, `gemini-3.1-flash-lite-preview` |
| **antigravity** | `claude-opus-4-6-thinking`, `claude-sonnet-4-6`, `gemini-3-flash`, `gemini-3-flash-agent`, `gemini-3-pro-high`, `gemini-3-pro-low`, `gemini-3.1-flash-image`, `gemini-pro-agent`, `gemini-3.1-pro-low`, `gpt-oss-120b-medium`, `gemini-3.1-flash-lite`, `gemini-3.5-flash-low` |

## Request/response transform

Inbound native Gemini → Cloud Code Assist envelope (`src/gemini/translate.rs`):

```
{ "project": "<project_id>", "model": "<model>", "request": { …gemini body… } }
```

Shared normalizations (ported from CLIProxyAPI's request translators):
`fixCLIToolResponse` (groups function calls with their responses under a
`role:"function"` content), `system_instruction`→`systemInstruction`, role
normalization, `thoughtSignature` = `skip_thought_signature_validator` on model
function-call parts, empty-`parts` filtering, and default safety settings (all
`OFF`). `countTokens` drops `project`/`model`/`safetySettings`.

**antigravity** adds: `userAgent:"antigravity"`, `requestType:"agent"`
(`image_gen` for image models), `requestId`, a stable `request.sessionId`,
removes `request.safetySettings`, sets `functionCallingConfig.mode:"VALIDATED"`
for `claude-*` models.

Responses arrive wrapped as `{"response":{…}}`; we unwrap `.response`
(non-stream) or rewrite each `data: {"response":{…}}` SSE line to `data: {…}`
(stream) before returning native Gemini to the client.

Per-provider headers: gemini-cli sends `User-Agent: GeminiCLI/0.34.0/<model>
(<os>; <arch>; terminal)` + `X-Goog-Api-Client`; antigravity sends
`User-Agent: antigravity/<version> darwin/arm64`.

## Credentials

Discovered from (read order) `~/.config/claude-proxy/auths/` then
`~/.cli-proxy-api/` — so credential files written by CLIProxyAPI work
unchanged. Override/extend with `[gemini] auth_dirs`. Files are dispatched on
their top-level `type`:

- `type:"gemini"` → `gemini-<email>-<project>.json`:
  `{token:{access_token,token_type,refresh_token,expiry},project_id,email,auto,checked,type}`
- `type:"antigravity"` → `antigravity-<email>.json`:
  `{type,access_token,refresh_token,expires_in,timestamp,expired,email,project_id}`

Access tokens are refreshed ~60s before expiry against
`https://oauth2.googleapis.com/token` (provider-specific client ID/secret) and
written back to the source file. Like `reauth.rs`, refresh uses a `no_proxy()`
client so it never loops back through the proxy.

## Login

```bash
claude-proxy login gemini [--project <id>]   # Google / gemini-cli (callback :8085)
claude-proxy login antigravity               # antigravity (callback :51121)
```

Each opens a browser consent flow, exchanges the code, fetches the account
email, resolves the Cloud project via `loadCodeAssist` (falling back to
`onboardUser`), and writes the credential file into
`~/.config/claude-proxy/auths/`. Callback ports/paths match the OAuth clients'
registered redirect URIs, so they must be free during login.

## Config

```toml
[gemini]
# Defaults to ["~/.config/claude-proxy/auths", "~/.cli-proxy-api"] when omitted.
auth_dirs = ["~/.config/claude-proxy/auths", "~/.cli-proxy-api"]
# Override the embedded model catalog used for the /v1beta/models listing.
models_file = "~/.config/claude-proxy/models.json"
# User-Agent version for antigravity requests.
antigravity_version = "1.21.9"
```

There is no model→provider mapping to configure — routing is purely by the
`<provider>/` prefix on the requested model.

## Pipeline placement

Gemini routing sits **right after Map Local** and before OAuth/heat-up/dedup in
both request paths, so a `[[map_local]]` rule on a `/v1beta` URL still wins, but
otherwise Gemini requests bypass the OAuth-token cache and dedup machinery
(which are specific to the `claude` CLI's traffic). See
[architecture.md](architecture.md).

## Not implemented (deferred)

Daily/sandbox base-URL fallback, multi-account round-robin, full JSON-schema
sanitizers, thinking-suffix parsing, and OpenAI/Anthropic-compatible inbound
endpoints. opencode's `@ai-sdk/google` speaks native Gemini, so only the
Gemini↔provider transform is implemented.
