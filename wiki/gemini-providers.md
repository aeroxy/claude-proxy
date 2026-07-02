# Gemini providers (opencode `@ai-sdk/google` + Anthropic Messages API)

`claude-proxy` serves the **native Gemini API surface** (`/v1beta/models…`) and
the **Anthropic Messages API** (`/v1/messages` — see the section near the end),
and routes each request to one of two upstream "providers" — **`gemini-cli`** and
**`antigravity`** — both of which call the Cloud Code Assist endpoint
`https://cloudcode-pa.googleapis.com/v1internal:*`. This lets opencode's
`@ai-sdk/google` provider (native Gemini) and any Anthropic-API client drive
Google/antigravity models through the proxy, the same way
[CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) does. The wire
formats and credential shapes are deliberately CLIProxyAPI-compatible.

## Endpoints served

| Method | Path | Action |
| --- | --- | --- |
| `GET` | `/v1beta/models` | List models fetched live from each provider's own API (filtered to providers with credentials) |
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
  `http://127.0.0.1:7777/v1beta`. Served from the plain-HTTP branch of
  `handle_request`. No CA/TLS needed.
- **MITM:** opencode keeps the default `generativelanguage.googleapis.com`
  endpoint with `HTTPS_PROXY=http://127.0.0.1:7777` + `NODE_EXTRA_CA_CERTS`.
  `handle_intercepted_request` routes the request when the SNI host is
  `generativelanguage.googleapis.com`.

## Routing: provider prefix on the model name

Routing is **prefix-based**, not a lookup table. The provider is encoded as the
first path segment of the model name; everything after it is the real model,
forwarded upstream verbatim (`src/gemini/models.rs::split_model`):

```text
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
forwarded (even one the listing doesn't know about). The listing is used **only**
to render `GET /v1beta/models` and never gates routing.

### `GET /v1beta/models` listing (`src/gemini/models.rs`)

The listing is fetched **live from each provider's own API**, per credential held,
and each model is emitted provider-prefixed (`models/<provider>/<id>`) so clients
request it with the prefix the router expects:

- **gemini-cli** → `POST cloudcode-pa.googleapis.com/v1internal:retrieveUserQuota`
  (`{"project": <project_id>}`). The quota `buckets[].modelId`s are the available
  model IDs. `retrieveUserQuota` returns IDs only, so each entry is emitted with
  just the id unless the optional `models_file` catalog has a richer entry to merge
  (display name / token limits).
- **antigravity** → `POST daily-cloudcode-pa.googleapis.com/v1internal:fetchAvailableModels`
  (`{"project": <project_id>}`). The response `models` map is keyed by model id, and
  each value carries `displayName` / `maxTokens` / `maxOutputTokens`, which are
  mapped straight through.

Both calls send the same hardcoded client headers as the generate path (see
[Per-provider headers](#requestresponse-transform) below). If a provider's live
fetch fails (offline, auth, quota), that provider is served from the `models_file`
catalog instead (or omitted if none is configured).

`[settings] models_file` points at a local `models.json` (same shape as
CLIProxyAPI's `internal/registry/models/models.json`: a `{ "<provider>": [ {id,
display_name, description, inputTokenLimit, outputTokenLimit, …}, … ] }` map). It
is the **fallback / enrichment** source only — there is no remote catalog fetch and
no embedded catalog (both were removed). With no `models_file` and a working live
fetch, the listing is entirely provider-sourced.

## Request/response transform

Inbound native Gemini → Cloud Code Assist envelope (`src/gemini/translate.rs`):

```json
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

**antigravity tool-schema sanitization** (`src/gemini/schema_clean.rs`, ported
from CLIProxyAPI's antigravity executor `buildRequest` — logic that lives outside
the translator and is easy to miss): cloudcode-pa reads tool schemas from
`functionDeclarations[].parameters`, **not** `parametersJsonSchema`, and rejects
unsupported JSON-Schema keywords. So for antigravity we rename
`parametersJsonSchema`→`parameters` and clean each schema —
`CleanJSONSchemaForAntigravity` for `claude`/`gemini-3-pro`/`gemini-3.1-pro`
(strips `$schema`/`$ref`/`additionalProperties`/`format`/`exclusiveMinimum`/… ,
flattens `anyOf`/`oneOf`/`allOf`/`type:[x,null]`, coerces `enum`→strings, and
injects a placeholder required prop into empty object schemas for VALIDATED
mode), `CleanJSONSchemaForGemini` otherwise. Skipping this yields
`tools.0.custom.input_schema: Field required` from the Vertex Anthropic backend.
The **gemini-cli** provider needs none of this — it accepts `parametersJsonSchema`
raw.

Responses arrive wrapped as `{"response":{…}}`; we unwrap `.response`
(non-stream) or rewrite each `data: {"response":{…}}` SSE line to `data: {…}`
(stream) before returning native Gemini to the client.

Per-provider headers (hardcoded to match the real clients, `src/gemini/provider.rs`):
gemini-cli sends `User-Agent: GeminiCLI-tui/0.47.0/<model> (<os>; <arch>; terminal)
google-api-nodejs-client/9.15.1` (the `<model>` segment is per-request; model-less
calls like the listing/login fall back to `gemini-2.5-pro`, gemini-cli's default) plus
`X-Goog-Api-Client: gl-node/25.8.2`. antigravity sends a fixed literal
`User-Agent: antigravity/cli/1.0.15 (aidev_client; os_type=darwin; arch=arm64;
auth_method=consumer)`. These are plain constants/one small builder — there is no
version config knob (version and format change together, so a hand-assembled string
would only drift).

## Credentials

Discovered from (read order) `~/.config/claude-proxy/auths/` then
`~/.cli-proxy-api/` — so credential files written by CLIProxyAPI work
unchanged. Override/extend with `[settings] auth_dirs`. Files are dispatched on
their top-level `type`:

- `type:"gemini"` → `gemini-<email>-<project>.json`:
  `{token:{access_token,token_type,refresh_token,expiry,expires_in,scopes,token_uri,client_id,client_secret,universe_domain},project_id,email,auto,checked,type}`.
  `login gemini` writes the full OAuth client metadata (`client_id`/`client_secret`/`token_uri`/`scopes`) into `token` so CLIProxyAPI-compatible clients, which refresh via the standard google-auth flow, can refresh after expiry. claude-proxy itself only reads `token.{access_token,refresh_token,expiry}` and refreshes via its own constants, so it ignores the rest.
- `type:"antigravity"` → `antigravity-<email>.json`:
  `{type,access_token,refresh_token,expires_in,timestamp,expired,email,project_id}`

Access tokens are refreshed ~60s before expiry against
`https://oauth2.googleapis.com/token` (provider-specific client ID/secret) and
written back to the source file. Like `reauth.rs`, refresh uses a `no_proxy()`
client so it never loops back through the proxy.

## Login

```bash
claude-proxy login gemini [--project <id>] [--no-browser]  # Google / gemini-cli (callback :8085)
claude-proxy login antigravity [--no-browser]              # antigravity (callback :51121)
claude-proxy login vertex [--no-browser]                   # Google Cloud / vertex provider (callback :8085)
```

Each opens a browser consent flow (or prompts for manual code/URL pasting), exchanges the code, fetches the account email, and saves the credentials:
- **`gemini`** and **`antigravity`**: Resolves the Cloud project via `loadCodeAssist` (falling back to `onboardUser`), and writes the credential file into `~/.config/claude-proxy/auths/`.
- **`vertex`**: Generates Google Application Default Credentials (ADC) and writes them to the standard gcloud ADC path `~/.config/gcloud/application_default_credentials.json` as `authorized_user`. This proactively registers your local environment for the Vertex AI provider.

### `--no-browser` Mode

If you are running the proxy on a headless server or over SSH, use the `--no-browser` flag:
- **`gemini`**: Uses Google's out-of-band (OOB) auth flow with PKCE, directing you to `https://codeassist.google.com/authcode`. Copy the clean authorization code from that page and paste it into the terminal.
- **`antigravity`** / **`vertex`**: Google and Antigravity clients do not have an OOB page. The proxy will output the authorization URL using their registered localhost redirect URI, but will run **no server**. Open the URL in your local browser, authorize, and the browser will show a 'Connection Error' (since no local server is listening). Copy the full failed URL (containing `?code=...`) from your browser's address bar and paste it into the terminal; the proxy will parse and exchange the code.

The default (no flag) browser flow opens a browser consent flow and binds a temporary loopback callback listener on localhost. The callback ports/paths match the OAuth clients' registered redirect URIs; the redirect host is `localhost` on the preferred port. If the preferred port is already in use (e.g. CLIProxyAPI or a prior login is holding it), `bind_callback` falls back to an OS-assigned port **and** switches the redirect host to the `127.0.0.1` literal — Google's loopback flow only reliably accepts an arbitrary, unregistered port for an IP-literal host. (For `vertex`, it binds to an ephemeral port `0` and redirects to `http://127.0.0.1:{bound_port}`).

## Config

```toml
[settings]
# Defaults to ["~/.config/claude-proxy/auths", "~/.cli-proxy-api"] when omitted.
auth_dirs = ["~/.config/claude-proxy/auths", "~/.cli-proxy-api"]
# Custom model catalog for the /v1beta/models listing (per-provider fallback when a
# provider has no live-fetched models).
models_file = "~/.config/claude-proxy/models.json"
```

Client `User-Agent`s are hardcoded to match the real clients (gemini-cli embeds the
per-request model; antigravity is a fixed literal) — there is no version knob, since the
version and string format change together and a hand-assembled value would only drift.

There is no model→provider mapping to configure — routing is purely by the
`<provider>/` prefix on the requested model.

## Pipeline placement

Gemini routing sits **right after Map Local** and before OAuth/heat-up/dedup in
both request paths, so a `[[map_local]]` rule on a `/v1beta` URL still wins, but
otherwise Gemini requests bypass the OAuth-token cache and dedup machinery
(which are specific to the `claude` CLI's traffic). See
[architecture.md](architecture.md).

## Anthropic Messages API (`/v1/messages`)

The proxy also serves the **Anthropic Messages API** over the *same* two
upstreams, so any Anthropic-API client (Claude Code via `ANTHROPIC_BASE_URL`,
the Anthropic SDK) can drive Gemini/antigravity models — including antigravity's
`claude-*` models.

**Endpoints:** `POST /v1/messages` (honors `"stream": true`) and
`POST /v1/messages/count_tokens` (→ `{"input_tokens": N}`).

**Routing:** the **body's `model`** carries the provider prefix
(`gemini-cli/<model>`, `antigravity/<model>`) — same `split_model` router as
`/v1beta`. An unprefixed model returns a `not_found_error` envelope in origin
mode.

**Transports:**
- **Origin** — plain HTTP at `127.0.0.1:7777` (`ANTHROPIC_BASE_URL=http://127.0.0.1:7777`); no CA needed.
- **MITM** — intercept `api.anthropic.com`, **gated on the provider prefix**
  (`anthropic::model_has_provider_prefix`). Unprefixed models fall through to the
  real Anthropic API untouched, so the normal `claude` CLI keeps working. This
  gate is the reason MITM of `api.anthropic.com` is safe (unlike Gemini, the
  `claude` CLI's real traffic uses this host).

**Translation (`anthropic.rs` + `anthropic_translate.rs`):** rather than write
direct claude→provider translators, the Anthropic body is translated to a
**native-Gemini** body (`claude_to_gemini`) and fed through the *exact* Gemini
path above (envelope build → `provider::send_request` → `.response` unwrap); the
Gemini reply is translated back to Anthropic (`gemini_to_claude_nonstream`, or
the `ClaudeStream` SSE state machine for streaming). So the only Anthropic-
specific code is the Anthropic↔Gemini boundary. Ported from CLIProxyAPI
`internal/translator/gemini/claude/` + `internal/util`:

- **Request:** `system`/`messages` → `system_instruction`/`contents`;
  `tool_use`→`functionCall`, `tool_result`→`functionResponse` (the `tool_use.id`
  / `tool_use_id` is preserved on `functionCall.id`/`functionResponse.id` — the
  antigravity→Anthropic round-trip needs it to rebuild `tool_use.id`, which Vertex
  requires), `image`(base64)
  →`inline_data`; `tools[].input_schema`→`parametersJsonSchema`; `tool_choice`
  →`toolConfig.functionCallingConfig`;
  `thinking`/`temperature`/`top_p`/`top_k`/`max_tokens` →`generationConfig`
  (`max_tokens`→`maxOutputTokens` — required so the `-thinking` models satisfy the
  backend's `max_tokens > thinking.budget_tokens` rule; antigravity non-claude
  models drop it again). Tool names are sanitized to Gemini's charset; the
  existing `build_envelope` then runs role-normalization + `fix_cli_tool_response`
  grouping + thought-signature injection + default safety.
- **Response:** Gemini `parts` → Anthropic `text`/`thinking`/`tool_use` blocks
  (streaming opens/continues/closes `content_block_*` events, ending with
  `message_delta` + `message_stop`); `finishReason`→`stop_reason`; usage mapped.
  Tool names are restored to the exact client-facing name and `tool_use.id`s are
  sanitized to `^[a-zA-Z0-9_-]+$`.

Streaming reuses the one disconnect-safe `provider::stream_sse` pump.

## Not implemented (deferred)

Daily/sandbox base-URL fallback, multi-account round-robin, thinking-suffix
parsing, and the OpenAI-compatible inbound endpoint. For the Anthropic surface
specifically: the dedicated `antigravity/claude` signature-validation path
(claude-on-antigravity rides the gemini envelope + `VALIDATED` toolConfig tweak +
the schema sanitization above), `stop_sequences` mapping,
prompt-cache fields, `ping` events, and mid-stream upstream-error → Anthropic
error event. (JSON-schema sanitization for antigravity **is** implemented — see
the transform section.)
