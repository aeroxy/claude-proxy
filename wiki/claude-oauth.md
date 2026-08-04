# Claude subscription passthrough (`[claude_oauth]`)

Serves `POST /v1/messages` (+ `/v1/messages/count_tokens`) against the **real**
Anthropic API using the Claude Code OAuth credential from the macOS Keychain, so
any Anthropic-API client can drive your Claude subscription without an
`sk-ant-api…` billing key.

Unlike the Gemini surfaces this does **no format translation** — Anthropic in,
Anthropic out, a near-pure pipe like [the OpenAI aggregator](../src/openai/mod.rs).
The work is entirely in the two layers beside it: reading/refreshing the
credential, and shaping the request so Anthropic accepts an OAuth credential for
inference.

| File | Responsibility |
| --- | --- |
| [src/claude_oauth/mod.rs](../src/claude_oauth/mod.rs) | Routing gate (`routes`), upstream POST, 401-retry, buffered + SSE passthrough, Anthropic error envelope |
| [src/claude_oauth/creds.rs](../src/claude_oauth/creds.rs) | Keychain/file read, expiry check, refresh, merged write-back |
| [src/claude_oauth/disguise.rs](../src/claude_oauth/disguise.rs) | All request shaping — pure functions, unit-tested |

## Credentials

Read in this order:

1. **macOS Keychain** — generic password, service `Claude Code-credentials`,
   account `$USER`. Read through the `security` CLI (no new dependency, no
   code-signing entanglement). Because the Keychain ACL is attached to Apple's `security` binary
   rather than to ours, this does **not** raise an authorization prompt in
   practice.
2. **`~/.claude/.credentials.json`** — same JSON shape, for machines without a
   Keychain.

```json
{ "claudeAiOauth": { "accessToken": "sk-ant-oat01-…", "refreshToken": "sk-ant-ort01-…",
                     "expiresAt": 1785855177569, "scopes": ["user:inference", …],
                     "subscriptionType": "team" } }
```

Refresh: `POST https://platform.claude.com/v1/oauth/token` with
`grant_type=refresh_token` and Claude Code's public `client_id`, 60s before the
stored expiry — or immediately, once, when the API answers 401 on a token we
believed was good (the real CLI refreshing can rotate ours out from under us).

Three invariants carried over from the rest of the codebase:

- **The refresh POST uses a `no_proxy()` client.** A configured `upstream_proxy`
  may be us; a proxied token exchange would loop. (The *upstream* `/v1/messages`
  POST deliberately reuses the shared proxy client, so `upstream_proxy` chaining
  through Proxyman keeps working — `api.anthropic.com` isn't us, so it can't loop.)
- **Refreshes are serialized process-wide** and re-read the source after taking
  the lock, so concurrent requests don't each fire a POST and race the write-back.
- **Write-back merges.** The Keychain item also holds `mcpOAuth`,
  `pluginSecrets`, and `organizationUuid`, all owned by the real CLI, and
  unknown keys inside `claudeAiOauth` itself. Only the three token fields are
  replaced. `write_back = false` keeps refreshed tokens in memory, at the cost of
  eventually invalidating the real Claude Code login — Anthropic rotates the
  refresh token, and only one holder can win.

## Routing

| Transport | Gate |
| --- | --- |
| **Origin** (`ANTHROPIC_BASE_URL=http://127.0.0.1:7777`) | `claude-oauth/<model>`, plus plain real model names when `serve_unprefixed = true` (default) — a client pointing at us wants us to serve it |
| **MITM** of `api.anthropic.com` | **Only** `claude-oauth/<model>` |

The MITM restriction is the safety crux, identical in spirit to
[the Gemini-Anthropic gate](gemini-providers.md): an unprefixed model over MITM is
the real `claude` CLI talking to its own API with its own credential, and must
never be rerouted. Verified by `unprefixed_model_never_routes_over_mitm` and by
sending a bogus `x-api-key` through the proxy — it comes back
`invalid x-api-key` from the real API, proving our credential was never
substituted.

`routes` also never claims a model the Gemini-Anthropic surface would serve
(prefixed *or* redirected via `[anthropic_model_map]`), so adding this surface
can't change where existing traffic goes.

**Pipeline placement.** In the origin branch this is checked *before*
`gemini::anthropic::try_handle`, because that handler serves every
`/v1/messages` POST — 404-ing an unroutable model rather than declining — and
would otherwise shadow this surface entirely.

**Dedup.** Both transports register in `REQUEST_PROMISES` themselves, exactly
like the routed Gemini-Anthropic path and for the same reason: the early return
jumps over the shared dedup block, and Claude Code fires byte-identical
concurrent `/v1/messages` requests. See [request-dedup.md](request-dedup.md).

**Compression is deliberately skipped** on this path. It exists to shrink tool
results for weaker providers; here the upstream *is* Anthropic, so the body
should arrive exactly as the client wrote it.

## The disguise

Anthropic only honors an OAuth credential for inference when the request looks
like Claude Code. The shaping is sorted into three tiers, and keeping them apart
is the design rule — see [src/claude_oauth/disguise.rs](../src/claude_oauth/disguise.rs).

### 1. Auth-critical — always injected

- **`system[1]`: the identity block.** `You are Claude Code, Anthropic's official
  CLI for Claude.` (or the `", running within the Claude Agent SDK."` variant
  when `entrypoint != "cli"`, so the two stay coherent).
- **`anthropic-beta: oauth-2025-04-20`.** Re-added by config validation if
  removed — without it the credential is rejected outright.
- **`authorization: Bearer <oauth token>`**, and the client's `x-api-key` is
  dropped: if present it outranks the Bearer and yields a 401.

### 2. Cosmetic — always injected, zero effect on generation

- **`system[0]`: the billing block.**
  `x-anthropic-billing-header: cc_version=<cli_version>; cc_entrypoint=<entrypoint>; cch=<hash>;`
  — an HTTP-header-shaped string the CLI carries *in the prompt*, not in a header
  (it appears nowhere in the captured header list). `cch` looks like a cache
  diagnostic (`cache-diagnosis-2026-04-07` is in the CLI's beta list), so it's
  derived from a hash of the client's own system text rather than pinned to a
  captured constant.
- **Neither injected block carries `cache_control`** — a real CLI puts its
  breakpoints on later blocks, and spending one here would take it from the
  client's budget of four.
- `user-agent: claude-cli/<major.minor.patch> (external, <entrypoint>)`,
  `x-app: cli`, `anthropic-dangerous-direct-browser-access`, the full
  `x-stainless-*` set, `x-claude-code-session-id`, `x-client-request-id`,
  `?beta=true`.
- `metadata.user_id` — a JSON *string* holding `device_id` / `account_uuid` /
  `session_id`, matching the CLI. `account_uuid` is **omitted when unknown**
  (learned from a refresh response) rather than faked: a wrong uuid is a worse
  signal than a missing one.
- `diagnostics: {"previous_message_id": null}`.

Headers are an **allowlist**, not a denylist: exactly the set above is sent, so a
calling SDK can't leak its own fingerprint (`x-stainless-lang: python` from a
Python client, say).

### 3. Semantic — forwarded, never invented

`context_management`, `output_config`, `thinking`, `temperature`,
`stop_sequences`, `top_p`, `tools`, `tool_choice`, `messages`. A proxy that
silently rewrites generation parameters is a debugging nightmare, so these move
only when the client sends them.

`[claude_oauth.inject]` is the deliberate escape hatch — raw JSON merged into the
body, with client-supplied values always winning. Use it to experiment with
CLI-fidelity fields we don't inject:

```toml
[claude_oauth.inject]
context_management = { edits = [{ type = "clear_thinking_20251015", keep = "all" }] }
```

### The client's own system prompt always survives

It is preserved verbatim, just no longer first:

```
client sends:  system: "You are a pirate."
we send:       system: [ {billing}, {identity}, {"type":"text","text":"You are a pirate."} ]
```

Idempotent: a client that already sent either block (Claude Code itself, over
MITM) keeps its own — theirs is accurate, ours is synthesized. The identity check
matches the prefix `You are Claude Code, Anthropic's official CLI for Claude`, so
both variants are recognized. An empty-string `system` never becomes an empty
text block (the API rejects those).

## Two behaviors worth knowing

**`anthropic-beta` is a fixed list, not a union.** Anthropic 400s on any beta it
doesn't recognize (`Unexpected value(s) … for the anthropic-beta header`), so
forwarding a caller's list turns one stray identifier into a total failure — and
there's no way to tell a valid unknown beta from a typo. Only `[claude_oauth]
betas` is sent; dropped client values are named in a warning. This is also what a
real CLI does.

`fallback-credit-2026-06-01` is the one beta from the captured list left **out**
of the default: it appears to authorize spending API credits when the
subscription quota is exhausted, which shouldn't be enabled implicitly for
arbitrary clients. Add it back explicitly if you want it.

**`count_tokens` has a strict schema.** It rejects anything outside
`model` / `messages` / `system` / `tools` / `tool_choice` / `thinking` /
`mcp_servers` with `Extra inputs are not permitted` — including `metadata` and
`max_tokens`, whether we added them or the client did. On that route the body is
trimmed to the allowlist; the system-prompt disguise still applies, because the
OAuth gate applies to every route.

## Config

Every field has a working default, so an empty `[claude_oauth]` table is a
complete config. Absent = surface disabled.

```toml
[claude_oauth]
prefix           = "claude-oauth"  # explicit routing prefix; the only MITM gate
serve_unprefixed = true            # serve plain model names on the origin branch
cli_version      = "2.1.221.9b8"   # cc_version, and the user-agent (suffix trimmed there)
entrypoint       = "cli"           # cc_entrypoint; pairs with the identity variant
write_back       = true            # merge refreshed tokens back into the Keychain
betas            = [ … ]           # fixed anthropic-beta list

[claude_oauth.model_map]           # aliases, applied after the prefix is stripped
"claude-3-5-sonnet-latest" = "claude-sonnet-5"

[claude_oauth.inject]              # raw body merge; client values win
```

## Verification

```bash
# 1. Non-stream. Expect 200 with a real completion.
curl -s localhost:7777/v1/messages -H 'content-type: application/json' \
  -d '{"model":"claude-opus-5","max_tokens":64,"messages":[{"role":"user","content":"Reply with exactly: PROXY OK"}]}'

# 2. Stream. Expect byte-exact SSE (message_start, content_block_delta, …).
curl -sN localhost:7777/v1/messages -H 'content-type: application/json' \
  -d '{"model":"claude-oauth/claude-opus-5","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"Count: 1 2 3"}]}'

# 3. The client's system prompt survived — the reply should be piratical.
curl -s localhost:7777/v1/messages -H 'content-type: application/json' \
  -d '{"model":"claude-opus-5","max_tokens":100,"system":"You are a pirate. Always answer in pirate speak.","messages":[{"role":"user","content":"How is the weather?"}]}'

# 4. count_tokens. Expect {"input_tokens":N}.
curl -s localhost:7777/v1/messages/count_tokens -H 'content-type: application/json' \
  -d '{"model":"claude-opus-5","messages":[{"role":"user","content":"hello there"}]}'

# 5. MITM safety: UNPREFIXED must fall through to the real API.
#    Expect {"type":"error",…,"invalid x-api-key"} — proving our credential
#    was never substituted — and zero `claude-oauth:` lines in the log.
curl -sk -x http://127.0.0.1:7777 https://api.anthropic.com/v1/messages \
  -H 'content-type: application/json' -H 'x-api-key: sk-ant-bogus' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"claude-opus-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}'

# 6. MITM routing: the same request with `claude-oauth/claude-opus-5` is served
#    by us despite the bogus key.

# 7. Dedup: two byte-identical concurrent requests produce one
#    `primary fetcher for routed request` plus one
#    `Received response from primary in-flight routed request`.

# 8. Gemini traffic isn't stolen: `gemini-cli/gemini-3.5-flash` on
#    /v1/messages still answers from the Gemini upstream.
```

## Deliberately not built

- **`login claude`.** The PKCE browser flow (authorize URL → loopback callback →
  code exchange at `platform.claude.com/v1/oauth/token` with Claude Code's public
  `client_id`) would reuse
  [oauth_util](../src/oauth_util.rs), but signing in with the `claude` CLI already
  produces the credential this reads. Only needed on a machine without Claude Code.
- **A `/v1/models` listing.** Routing is prefix-based and never consults a catalog.
- **Any translation layer.** If a non-Anthropic-shaped client needs to reach this,
  it should go through the existing OpenAI or Gemini surfaces instead.
