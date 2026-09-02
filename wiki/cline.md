# Cline as a built-in provider

Serves `POST /v1/chat/completions` against Cline's own API (`api.cline.bot`) using a Cline
account credential, so any OpenAI-compatible client can spend a Cline subscription without
holding a Cline key.

Like [`src/openai/`](../src/openai/mod.rs) and unlike the Gemini surfaces, this is a
**near-pure pipe** — OpenAI in, OpenAI out, no format translation. Three things stand
between the client and a verbatim forward:

1. **The credential.** Cline wants `Authorization: Bearer workos:<jwt>` on a token that
   lives about an hour, so refresh is on the request path.
2. **The identity headers.** Cline's API is addressed by Cline's own clients, so we send
   exactly the header set a real `cline` CLI sends.
3. **The response envelope.** A non-streaming success arrives wrapped; errors arrive with
   `error` as a bare string. Both are reshaped. Streaming frames need neither.

Code: [`src/cline/mod.rs`](../src/cline/mod.rs) (routing, headers, envelope) and
[`src/cline/creds.rs`](../src/cline/creds.rs) (stores, refresh, write-back).

## Configuration

The surface is **always on**, like `gemini-cli/` and `antigravity/`: `cline/<model>` routes
with no config at all, and a prefixed request with no credential on disk is a 401 carrying
the `login cline` hint — the same shape as `gemini-cli/<model>` without a Google login.
The prefix is the consent; nothing reads, refreshes or writes a credential until a request
carries it. That is why no `[cline]` table is needed, where `[claude_oauth]` and `[aicode]`
are opt-in: those spend a credential on *unprefixed* traffic by default.

The one deliberate opt-in is `serve_unprefixed`. It defaults to `false` here (and to `true`
under `[claude_oauth]`, where the table itself is the opt-in) because a bare
`anthropic/claude-haiku-4.5` on the origin branch used to be an aggregator 400, and an
always-on surface must not turn that into a silent spend of the user's Cline account.

```toml
# ~/.config/claude-proxy/config.toml — overrides only; every field has a working default
[cline]
prefix = "cline"                      # model prefix: cline/<upstream-model>
serve_unprefixed = false              # origin branch only — never widens the MITM gate
base_url = "https://api.cline.bot"    # staging: https://core-api.staging.int.cline.bot
client_version = "3.0.60"             # X-CLIENT-VERSION, X-PLATFORM-VERSION, User-Agent
core_version = "0.0.81"               # X-CORE-VERSION
write_back = true                     # persist rotated tokens to the store they came from
# settings_path = "~/.cline/data/settings/providers.json"   # only if the CLI moved it
```

## Signing in

```bash
claude-proxy login cline          # WorkOS device flow; add --no-browser to open the URL yourself
```

There is **no loopback callback server** here — unlike every other `login` flow in this
repo, WorkOS hands back a `user_code` to type into the browser and we poll for the result,
so `oauth_util::accept_oauth_callback` is not involved.

The flow, all of it on a `no_proxy()` client (`login` runs in a shell where `HTTPS_PROXY`
points at this proxy):

1. `POST https://api.workos.com/user_management/authorize/device` with
   `client_id=client_01K3A541FN8TA3EPPHTD2325AR` — the public client id hardcoded in the
   cline repo.
2. Print the `user_code`, open `verification_uri_complete`.
3. Poll `POST /user_management/authenticate` with
   `grant_type=urn:ietf:params:oauth:grant-type:device_code`, honoring
   `authorization_pending` and `slow_down` — pacing comes from the server's own `interval`,
   widened by a second each time it says `slow_down`, not from a fixed sleep.
4. `POST {base_url}/api/v1/auth/register` with **both** WorkOS tokens, which returns the
   Cline credential. The **refresh token is persisted**, not just the access token: it is
   the only thing that can keep the login alive, and it cannot be re-derived.

Two token pairs are in play and only the second is ours to keep — WorkOS mints the
identity, Cline exchanges it for the credential its API accepts.

## Credential stores

Two, in read order:

1. **The real `cline` CLI's** `providers.json`, so a machine that already runs Cline needs
   no `login cline` at all. Resolved the way the CLI resolves it: `settings_path` config >
   `CLINE_PROVIDER_SETTINGS_PATH` > `$CLINE_DATA_DIR/settings/providers.json` >
   `${CLINE_DIR:-~/.cline}/data/settings/providers.json`. The credential lives at
   `providers.cline.settings.auth` (`accessToken` / `refreshToken` / `expiresAt` ms).
2. **Ours**, `cline-<email>.json` in an `auth_dirs` entry, written by `login cline`:
   `{"type":"cline","email":…,"access_token":…,"refresh_token":…,"expires_at":<ms>}`.

The `workos:` prefix is stripped on read and re-added on send, since either form can be
sitting in a store.

### Write-back must merge, never replace

`providers.json` also holds the selected model, the model catalog, custom headers, and
**every other provider the user configured**. `creds::persist` re-reads the file and
replaces only `accessToken` / `refreshToken` / `expiresAt` under
`providers.cline.settings.auth`; a whole-object write would silently destroy an
`openrouter` API key sitting beside it. Same invariant as the Claude Code Keychain item in
[claude-oauth.md](claude-oauth.md).

Write-back is on by default deliberately: Cline rotates the refresh token, so keeping ours
private would eventually invalidate the real `cline` CLI's login.

### The four refresh invariants

Each was paid for once already in `claude_oauth::creds`, and one of them has a scar in
Cline's own client:

- **`no_proxy()` client** for the refresh POST, so it can't loop back through us.
- **A process-wide refresh lock**, so concurrent requests don't each spend the rotating
  refresh token. The credential is re-read *under* the lock, retry path included: a
  refresh that landed while we waited also rotated the token we captured before it.
- **An in-memory last-refresh overlay**, ordered by expiry, so `write_back = false`
  doesn't replay a token Cline already rotated away — and so a refresh the real CLI landed
  after ours correctly wins over our cache.
- **Transient ≠ rejected.** Only an `invalid_grant`-shaped refusal (an explicit
  grant/token error code, or a 400/401/403 whose message reads like a rejection) counts as
  "this credential is dead". A timeout, a 5xx, a 429 keeps the stored credential and
  retries. The cline client carries a scar from exactly this: a blip landing just after
  expiry was read as a rejection and logged out every Cline process on the machine.

A 401 from the chat endpoint triggers one forced refresh and one retry, passing the
*rejected* token so `ensure_fresh` can tell "the store still holds the bad token" apart
from "someone already replaced it, use theirs".

## Routing — the safety crux

`HTTPS_PROXY` points every client on the machine at this proxy, so **the real `cline`
CLI's traffic to `api.cline.bot` already passes through us**. Claiming that host blindly
would hijack the user's own CLI.

| Transport | What routes here |
| --- | --- |
| **Origin** (`OPENAI_BASE_URL=http://127.0.0.1:7777`) | `cline/<model>`; plus bare model names when `serve_unprefixed`, but only ones no `[[openai]]` provider name would claim |
| **MITM** of `api.cline.bot` | **Only** the explicit `cline/` prefix. Everything else falls through to the real API untouched |

`serve_unprefixed` is an origin-branch knob and is passed as `allow_unprefixed = false` on
MITM regardless — never widen it. Unit tests in `src/cline/mod.rs` assert both directions,
including that turning `serve_unprefixed` on does not loosen the MITM gate.

`/v1/chat/completions` is contested by three surfaces. Order in
[`proxy.rs`](../src/proxy.rs), origin branch:

1. Gemini providers (`gemini-cli/`, `antigravity/`, `aicode/`, `vertex/`)
2. **Cline**
3. The `[[openai]]` aggregator

The decision is one pure function, `origin_chat_completions_route`, and the order is pinned
by a unit test there — a silent reorder would move traffic between surfaces. Cline sits
ahead of the aggregator but asks `openai::split_model` before taking an unprefixed model,
so a configured `[[openai]]` name keeps its models and no existing traffic moves. Config
validation warns when a `[[openai]]` `name` equals the Cline
`prefix`, since that entry would then be unreachable.

Cline is **not** registered in `REQUEST_PROMISES`, matching its sibling `crate::openai`:
nothing is known to fire byte-identical concurrent chat completions the way Claude Code
does for `/v1/messages`.

## Client identity

The exact set a real `cline` CLI sends (`resolveProviderRequestHeaders` in the cline SDK,
with the CLI's `extensionContext.client`). It is an allowlist, not a passthrough, so a
calling SDK can't leak its own fingerprint or an `Authorization` that would outrank ours.

```
HTTP-Referer: https://cline.bot
X-Title: Cline
User-Agent: Cline/<client_version>
X-CLIENT-TYPE: cline-cli
X-CLIENT-VERSION: <client_version>
X-PLATFORM: cli
X-PLATFORM-VERSION: <client_version>
X-CORE-VERSION: <core_version>
X-IS-MULTIROOT: false
X-Task-ID: <client's X-Task-ID, else one id per proxy process>
```

`X-Task-ID` groups a task's calls. We have no task boundary to observe, so one value per
process is the honest default; a client that tracks its own sessions can send the header
and we forward it.

## Response handling

**Non-streaming success is wrapped** and a bare passthrough breaks OpenAI SDKs:

```json
{"data":{"id":"gen-…","choices":[…],"usage":{…}},"success":true}
```

`unwrap_envelope` returns `data`. It is defensive both ways: a body that isn't wrapped is
returned untouched, and a wrapped body whose `data` isn't an object is left alone.

**Streaming frames are *not* wrapped.** Measured against `anthropic/claude-haiku-4.5`:
plain `data: {chunk}` lines terminated by `data: [DONE]`, identical to OpenAI's own shape.
So the stream is the same raw byte pump (`proxy::stream_passthrough`) every other surface
here uses — no per-frame rewriting.

**The two contracts disagree on the `stream` default** — OpenAI's is `false`, Cline's API is
`true` — so an absent `stream` is pinned to `false` on the way out (`shape_request`). This
is an OpenAI surface: an SDK that omits the field must get one JSON object, not an event
stream. It is the only field rewritten besides `model`.

The response branch is then taken on the **upstream's** `content-type` rather than on the
value we pinned — belt and braces, because if the two ever disagree the cost is handing a
JSON parser an event stream.

**Errors put a bare string where SDKs expect an object:**

```json
{"error":"empty response content","success":false}     // 500, e.g. z-ai/glm-5.3-flash
{"error":"model not found","success":false}            // 404
{"error":"Unauthorized: …re-authenticate your Cline account."}   // 401, no `success` key
```

`reshape_error` maps these to `{"error":{"message":…,"type":…,"code":null}}`, with `type`
derived from the status (`invalid_request_error` / `authentication_error` /
`rate_limit_error` / `api_error`). An `error` that is already an object passes through
untouched, and a non-JSON body (a CDN's HTML 502) is wrapped rather than mislabeled
`application/json`.

Response headers: unlike `crate::openai`, which drops them all, this surface forwards
`retry-after`, `x-request-id` and `x-ratelimit-*` — it owns a credential it can be
throttled on.

## Verification

With a credential in place (no config needed for cases 1, 2 and 4–7; case 3 needs
`[cline] serve_unprefixed = true`):

```bash
# 1. prefixed, non-streaming — the envelope must be gone (top-level `choices`)
curl -s http://127.0.0.1:7777/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"cline/anthropic/claude-haiku-4.5","stream":false,"max_tokens":16,
       "messages":[{"role":"user","content":"say pong"}]}'

# 2. prefixed, streaming — plain OpenAI chunks, terminated by `data: [DONE]`
curl -sN http://127.0.0.1:7777/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"cline/anthropic/claude-haiku-4.5","stream":true,"max_tokens":48,
       "messages":[{"role":"user","content":"count 1 to 5"}]}'

# 3. unprefixed — only with `[cline] serve_unprefixed = true`; by default this is
#    the aggregator's "Model must be prefixed with a configured [[openai]] provider" 400
curl -s http://127.0.0.1:7777/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"anthropic/claude-haiku-4.5","stream":false,"max_tokens":16,
       "messages":[{"role":"user","content":"say pong"}]}'

# 4. error reshaping — an OpenAI-shaped envelope, not a bare string
curl -s http://127.0.0.1:7777/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"cline/z-ai/glm-5.3-flash","stream":false,"max_tokens":16,
       "messages":[{"role":"user","content":"say pong"}]}'
# -> {"error":{"message":"empty response content","type":"api_error","code":null}}

# 5. MITM gate, unprefixed — MUST fall through to the real API and be refused there
CA=~/Library/Application\ Support/claude-proxy/ca.crt
curl -s -x http://127.0.0.1:7777 --cacert "$CA" \
  https://api.cline.bot/api/v1/chat/completions -H 'content-type: application/json' \
  -H 'Authorization: Bearer workos:bogus' \
  -d '{"model":"anthropic/claude-haiku-4.5","stream":false,
       "messages":[{"role":"user","content":"hi"}]}'
# -> 401 {"error":"Unauthorized: Please make sure you're using the latest version of Cline…"}
#    (the real API's own raw error — proof it never entered our surface)

# 6. MITM gate, prefixed — MUST be served by us despite the bogus key
curl -s -x http://127.0.0.1:7777 --cacert "$CA" \
  https://api.cline.bot/api/v1/chat/completions -H 'content-type: application/json' \
  -H 'Authorization: Bearer workos:bogus' \
  -d '{"model":"cline/anthropic/claude-haiku-4.5","stream":false,"max_tokens":16,
       "messages":[{"role":"user","content":"say pong"}]}'
# -> 200

# 7. the aggregator is not shadowed: with `[[openai]] name = "anthropic"` configured,
#    an unprefixed `anthropic/…` model must reach that backend, not Cline.
```

Set `expiresAt` / `expires_at` to `0` in the store to force a refresh on the next request
— an unknown expiry is deliberately not "fresh", so this exercises `ensure_fresh` and
write-back end to end.

### Checking `login cline` without a browser

A device flow needs a human for exactly one thing: obtaining the *first* WorkOS token pair.
Everything after that — renewing the pair on a refresh grant, registering with Cline,
writing our credential file, reading it back — runs unattended. Given any live WorkOS
refresh token:

```bash
CLINE_TEST_WORKOS_REFRESH=<token> cargo test login_tail -- --ignored --nocapture
```

`#[ignore]`d because it needs a live account and the network, so it never runs in a plain
`cargo test`. WorkOS **rotates** the refresh token on every call, so the test prints the
next one to use; a stale value gives `invalid_grant`, not a code failure.

What is left needing a human is only the device-authorization poll loop reacting to the
approval at `authkit.cline.bot` — nothing on this side can grant that.

## Deferred

- **`GET /v1/models`.** Cline publishes a catalog; nothing here lists it, and routing never
  gated on a listing anyway (it's prefix-based).
- **Serving `/v1/messages` from Cline.** Cline is OpenAI-shaped, so an Anthropic surface
  would need real format translation — a different kind of change from this near-pure pipe.
- **The `retry-empty-response` retry.** Cline's own client retries the
  `{"error":"empty response content"}` case once. Reproduced against
  `z-ai/glm-5.3-flash` at both 16 and 64 `max_tokens`; the error is mapped, but not
  retried. A knob that defaults to off would be dead config until someone needs it.
