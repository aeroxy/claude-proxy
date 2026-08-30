# Gemini providers (opencode `@ai-sdk/google` + Anthropic Messages API)

`claude-proxy` serves the **native Gemini API surface** (`/v1beta/models…`) and
the **Anthropic Messages API** (`/v1/messages` — see the section near the end),
and routes each request to an upstream "provider" picked by a prefix on the
model name: **`gemini-cli`** and **`antigravity`**, both of which call the Cloud
Code Assist endpoint `https://cloudcode-pa.googleapis.com/v1internal:*`, plus
**`aicode`**, a Gemini Enterprise seat on its own `businessaicode` host (see its
section below). This lets opencode's
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
gemini-cli/<model>        → gemini-cli provider, upstream model = <model>
antigravity/<model>       → antigravity provider, upstream model = <model>
aicode/<experience>       → Gemini Enterprise seat, upstream aicode.experience
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

There is no model→provider mapping to configure for `/v1beta` — routing there is
purely by the `<provider>/` prefix on the requested model. The Anthropic surface
below additionally supports an opt-in exact-string model map.

## `aicode/` — the Gemini Enterprise / AntiGravity team seat

A fourth provider (`src/gemini/aicode.rs`) on a **different upstream** from the
`antigravity` one next door, despite sharing its client identity:

| | `antigravity` | `aicode` |
| --- | --- | --- |
| host | `daily-cloudcode-pa.googleapis.com` | `businessaicode.googleapis.com`, or `businessaicode.<location>.rep.googleapis.com` |
| path | `/v1internal:streamGenerateContent` | `/v1beta/projects/<p>/locations/<l>:streamGenerateContent` |
| body | `{project, model, request:{…}}` envelope | **flat, field-allowlisted** native Gemini |
| model selector | `model` | `aicode.experience` — there is no `model` field |
| licence | consumer OAuth quota | Gemini Enterprise seat; `entitlement.userTier` mandatory |
| UA | `antigravity/cli/1.0.15 (… auth_method=consumer)` | `antigravity/cli/1.1.12 (… auth_method=gcp)` |

### Three identities, only one of them a credential

1. **account** — a stored `gemini-cli` credential, borrowed (this provider has
   no login of its own) and selected by `account_email`. Nothing else can
   supply it: the licence is invisible from the credential file, and a
   credential's `project_id` is a Code Assist project that need not equal the
   licence project.
2. **licence project + location + tier** — discovered together from
   `GET businessaicode…/v1beta:fetchLicenses`, config overriding any field.
   They travel as a set because the location lands in the *hostname*: `global`
   uses the bare host, anything else `<loc>.rep`, and a regional licence sent
   to the global host is a 403, not a redirect. The location is charset-checked
   (`[A-Za-z0-9-]+`) before it can reshape an authority — the check that matters
   is on the *discovered* value, since nobody typed it.
3. **experience** — the part after `aicode/`, forwarded verbatim.

### The `x-goog-user-project` twist

The real client sends **no** such header, and that is load-bearing for it: the
`serviceusage.services.use` check `:fetchLicenses` is known for is enforced
against whatever project the header names, so sending it invites the failure
that omitting it avoids. We ride gemini-cli's **public** OAuth client, so a bare
call is billed to that client's own project and comes back
403 `SERVICE_DISABLED` (verified). `fetch_licences` therefore retries with
candidate projects in order — config `project`, then the credential's own
`project_id` — each being a billing handle *for that one metadata GET*, never
the licence. Generate calls always send the header, set to the licence project.

### Body allowlist

`gemini_to_aicode` runs the shared `build_envelope` (role normalization,
`systemInstruction` rename, tool-response grouping, thought-signature injection,
empty-part filtering) and then keeps only `contents`, `systemInstruction`,
`tools`, `toolConfig`, `generationConfig`, `labels`, adding `aicode.experience`
and `entitlement.userTier`. `project`, `model` and `safetySettings` fall out by
construction — the API rejects unknown names outright, so this is an allowlist
rather than a list of things to delete. Tool schemas are renamed
`parametersJsonSchema` → `parameters` and cleaned, same as antigravity.

`X-Aicode-Trajectory-Id` comes from `stable_session_id` (a hash of the first
user message), so a multi-turn conversation groups as one trajectory instead of
N unrelated ones; `X-Aicode-Request-Id` is `<trajectory>-<n>`.

### Thinking eats `max_tokens`

The experiences think by default, and thinking tokens count against the
client's `max_tokens`. A small budget therefore comes back `stop_reason:
max_tokens` with **empty** content (observed: `max_tokens: 32` → 32 output
tokens, none of them text). We forward the client's `generationConfig` /
`thinking` as sent and never invent one — same stance as the Claude-OAuth
surface's semantic fields — so the fix is a realistic `max_tokens`, or an
explicit `thinkingConfig` from the caller.

### countTokens

`businessaicode` has no such action. `aicode` countTokens is served by
**gemini-cli's** `v1internal:countTokens` on the same credential —
`strip_for_count_tokens` removes `model` and `project`, so the experience name
never reaches the wire and no licence is spent.

### The listing comes from config, and cannot come from anywhere else

There is no live experience catalogue for this provider, and that is settled
rather than assumed. `cloudcode-pa:fetchAvailableModels` is the right endpoint —
the real client uses it even for the business seat — but reaching it needs an
identity the proxy cannot hold:

| credential | `fetchAvailableModels` |
| --- | --- |
| the seat's `gemini-cli` token (any User-Agent) | 403 `The caller does not have permission` |
| the same, plus `x-goog-user-project` | 403 `Cloud Code Private API … disabled` on the licence project |
| an ordinary `antigravity` OAuth token (`ya29.a0…`) | 200, but the **consumer** catalogue — identical for every `project` sent, including a nonexistent one, and identical across two different accounts |
| the real client's workforce token (`ya29.d…`) | 200 with the seat's true list |

That last row is the only one that answers correctly, and it comes from
`sts.googleapis.com/v1/oauthtoken` through a SAML workforce pool — a token class
whose refresh dies within hours, which is exactly why the generation path
deliberately avoids it. Note the asymmetry: `businessaicode` **generation**
accepts the plain `gemini-cli` token happily, because entitlement there is the
licence plus `entitlement.userTier` rather than the client identity. The seat can
run a model it cannot enumerate.

So the listing is populated from `[settings] models_file` under an `"aicode"`
key, and nothing is fetched — an earlier version made the doomed call on every
listing and paid a 403 round-trip for it. This is cosmetic either way: routing is
prefix-based, so an experience absent from the listing still works when named,
and the seat's actual set is whatever your Gemini Enterprise admin configured.

### Config

```toml
[aicode]
account_email = "<seat-holder>"     # which stored gemini cred; the only field nothing else supplies
# project     = "…"                 # override / disambiguate an account holding several licences
# region      = "us"                # override; else from :fetchLicenses
# user_tier   = "gcp-ge-plus-tier"  # override; else from :fetchLicenses
```

The `GET /v1beta/models` fallback is `[settings] models_file` with an `"aicode"`
key, exactly like the other providers — there is no aicode-specific list.
`[aicode]` also makes the provider "available" for that listing, since it has no
credential of its own to discover.

No `[aicode]` → the provider is off and `aicode/*` 404s; nothing else changes.
Errors distinguish what the upstream's own 403 never does — it returns the same
*"The selected license is not valid"* string for a wrong region, a wrong tier
**and** a wrong account — so failures log the whole resolved set, including the
credential email, which is the one value the operator did not type.

## Pipeline placement

Gemini routing sits **right after Map Local** and before OAuth/heat-up/dedup in
both request paths, so a `[[map_local]]` rule on a `/v1beta` URL still wins, but
otherwise Gemini requests bypass the OAuth-token cache and dedup machinery
(which are specific to the `claude` CLI's traffic). See
[architecture.md](architecture.md).

## Anthropic Messages API (`/v1/messages`)

The proxy also serves the **Anthropic Messages API** over the *same* upstreams,
so any Anthropic-API client (Claude Code via `ANTHROPIC_BASE_URL`, the Anthropic
SDK) can drive Gemini/antigravity/aicode models — including antigravity's
`claude-*` models.

**Endpoints:** `POST /v1/messages` (honors `"stream": true`) and
`POST /v1/messages/count_tokens` (→ `{"input_tokens": N}`).

**Routing:** the **body's `model`** carries the provider prefix
(`gemini-cli/<model>`, `antigravity/<model>`, `aicode/<experience>`) — same
`split_model` router as `/v1beta` — **or** is an exact match in the optional
`[anthropic_model_map]`
config, resolved by `anthropic::resolve_provider_model`. This applies uniformly
to both transports below. A model that's neither prefixed nor mapped returns a
`not_found_error` envelope in origin mode.

**Model map (`[anthropic_model_map]`, opt-in):** lets a real, unprefixed
Anthropic model name (e.g. `claude-sonnet-5` — exactly what the real `claude`
CLI sends) be redirected to a provider-prefixed target, for cost/quota reasons
— this is the feature's whole point: redirecting the real `claude` CLI's MITM'd
traffic for specific models without it ever knowing. Empty by default, so it
changes nothing unless configured:

```toml
[anthropic_model_map]
"claude-sonnet-5" = "gemini-cli/gemini-3.5-flash"
```

Key = exact `model` string as sent by the client; value = a normal
provider-prefixed model, same shape used everywhere else. A redirected request
logs `Anthropic model map: <from> -> <provider>/<model>` so it's visible which
traffic is being silently rerouted. The response's `model` field shows the real
upstream model (whatever Gemini reports back), same as ordinary provider-
prefixed routing — it does not echo the client's originally-requested string.

**Duplicate collapsing:** byte-identical concurrent `/v1/messages` requests
result in **one** provider generation, on both transports — the routed path
registers in the shared request-dedup map itself, since its early return in
`handle_intercepted_request` jumps over the generic dedup block. This matters
because Claude Code fires its session-title request twice, ~0.2–2 ms apart, on
the first message of every session; before this, enabling the model map doubled
the cost of that request (passthrough traffic had always been deduped, which is
what hid the double-fire). See
[request-dedup.md § Routed-path dedup](request-dedup.md#routed-path-dedup).

**Transports:**
- **Origin** — plain HTTP at `127.0.0.1:7777` (`ANTHROPIC_BASE_URL=http://127.0.0.1:7777`); no CA needed.
- **MITM** — intercept `api.anthropic.com`, **gated on the model being routable**
  (`anthropic::routed_provider`: a provider prefix, or a `[anthropic_model_map]`
  match). Everything else falls through to the real Anthropic API, so
  the normal `claude` CLI keeps working. This gate is the reason MITM of
  `api.anthropic.com` is safe (unlike Gemini, the `claude` CLI's real traffic
  uses this host) — the model map is a deliberate, narrow, exact-match exception
  to it, off by default, and this is the transport it's designed for.

  Two rewrites apply on the fall-through, both called in
  `proxy::handle_intercepted_request` and both healing session transcripts that
  the gemini→claude translation poisoned. Claude Code stores whatever content
  blocks it received in the transcript and resends them on every later turn, so
  once the session switches back to a real Claude model the real API rejects
  the whole request — permanently, for that session — until the poison is
  stripped.

  - `anthropic::scrub_empty_text_blocks`: empty `{"type":"text","text":""}`
    blocks are stripped from **assistant** messages. An earlier `ClaudeStream`
    bug turned Gemini's empty-`text` parts (emitted just before a
    `functionCall`, carrying only a `thoughtSignature`) into empty text content
    blocks; the real API rejects those with `messages: text content blocks must
    be non-empty`.
  - `anthropic::scrub_unsigned_thinking_blocks`: `thinking` blocks with an
    empty or missing `signature` are stripped from **assistant** messages. We
    emit Gemini's `thought` parts as Anthropic `thinking` blocks but have no
    signature to attach (Gemini's thought signatures aren't Anthropic's, and
    only Anthropic can mint one), so Claude Code stores `"signature": ""` and
    the real API rejects it with ``messages.N.content.M: Invalid `signature` in
    `thinking` block``. There is nothing to salvage — no value would validate —
    so dropping the block is the only shape the API accepts. Genuine Anthropic
    thinking blocks carry a non-empty signature and are kept, even when their
    `thinking` text is empty. Caveat: if the poisoned turn holds the *lastmost*
    `tool_use`, the API separately demands that turn start with a thinking
    block, so that one request fails either way — dropping just changes which
    400; continuing the session heals it.

  Both are deliberately inert for healthy traffic: a cheap substring pre-check
  (`has_string_value`, whitespace tolerant — `"text":""` for the text scrubber,
  `"type":"thinking"` for the thinking one, which has to catch the
  *missing*-signature shape we actually emit and so can't key on
  `"signature":""`) skips the JSON parse entirely, only assistant blocks are touched
  (never user turns), a message whose content is *only* such a block is left
  alone (an empty `content` array is a different 400), and the body is
  re-serialized only when a block was actually removed — otherwise the original
  bytes are forwarded byte-identical.

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
  requires), `image`/`document`(base64)
  →`inline_data`; `tools[].input_schema`→`parametersJsonSchema`; `tool_choice`
  →`toolConfig.functionCallingConfig`;
  `thinking`/`temperature`/`top_p`/`top_k`/`max_tokens` →`generationConfig`
  (`max_tokens`→`maxOutputTokens` — required so the `-thinking` models satisfy the
  backend's `max_tokens > thinking.budget_tokens` rule; antigravity non-claude
  models drop it again). Tool names are sanitized to Gemini's charset; the
  existing `build_envelope` then runs role-normalization + `fix_cli_tool_response`
  grouping + thought-signature injection + default safety.
- **Media inside `tool_result`:** a `tool_result` whose `content` is an array of
  blocks splits two ways — text collapses into `functionResponse.response.result`
  (it stays the tool's bound output), and any block with a base64 `source`
  (`image`, `document`) becomes an `inlineData` entry in
  `functionResponse.parts`, preserving relative order. `base64_source_to_inline_data`
  is purely mime-driven, so it needs no per-type knowledge; `application/pdf` is
  live-probed as working in both positions (as a `functionResponse.parts` entry and
  as a plain content part), billed as `IMAGE`-modality prompt tokens, i.e. genuinely
  ingested rather than ignored.
- **Untranslatable blocks are kept, minus the payload.** A block that can't become
  an `inlineData` part — a `url`-source image, an unrecognized type — is stringified
  into `result` rather than dropped, so the tool's output never silently vanishes.
  `fallback_block_text` first replaces any base64 `source.data` with a
  `<N chars of base64 elided>` marker: as escaped text the payload is unreadable to
  the model anyway, while a multi-megabyte document would inflate the upstream
  request roughly 1:1 (base64 needs no JSON escaping). Everything usable — `type`,
  `media_type`, `url`, sibling fields — survives. A block with no base64 payload
  (the common case) is stringified unchanged.
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
error event. For `aicode`: multi-licence support (one `[aicode]` table, one
seat), a persisted discovery cache (it is per-process, one GET per start), and
region probing — unlike the tier's two values the region space is unbounded, so
guessing would not be discovery. (JSON-schema sanitization for antigravity **is** implemented — see
the transform section.)
