# In-Flight Request Deduplication

## What it does

When two or more client requests arrive at the proxy with the same `(method, url, body)` while the first is still in flight upstream, only the primary request is forwarded. Secondary requests subscribe to the primary's response and replay the buffered bytes once it completes — they never touch the upstream.

Motivated by the observation that the `claude` CLI occasionally fires byte-identical concurrent POSTs to Vertex AI inference endpoints (same session id, same prompt, same auth token, both with `x-stainless-retry-count: 0`). Each duplicate burns upstream tokens for an identical answer; deduping returns one upstream call instead of N.

## Scope

Applies to **every forwarded request** that survives the earlier interceptors:

1. OAuth token requests (`oauth2.googleapis.com/token`) — handled by their own dedup + disk cache before this layer.
2. Vertex AI heat-up requests (`max_tokens: 1`, `"."` body) — short-circuited to a synthetic response before this layer.
3. Everything else — runs through this dedup.

Heat-ups must short-circuit before the dedup map is touched, otherwise they would inflate the in-flight set with synthetic-response candidates.

Plus the **routed Anthropic path**, which registers in the same map from inside its own gate — see [Routed-path dedup](#routed-path-dedup).

## Routed-path dedup

The `/v1/messages` handler for provider-prefixed and `[anthropic_model_map]` models returns early in `handle_intercepted_request`, well above the shared dedup block, so it used to get no dedup at all. That made enabling `[anthropic_model_map]` silently disable this feature for exactly the traffic it was redirecting.

The concrete symptom: **Claude Code fires the session-title request twice**, byte-identically, ~0.2–2 ms apart, on the first message of every session. Passthrough traffic had always collapsed that (it's the same shape as the duplicates it sends for `/api/oauth/profile`, `/api/claude_cli/bootstrap`, and `/api/claude_code_penguin_mode`). Routed traffic paid for two provider generations instead of one.

So the routed Anthropic gate now registers as primary / joins as waiter itself, in both the MITM branch and the plain-HTTP origin branch of [src/proxy.rs](../src/proxy.rs).

**Shared map, namespaced key, no collisions.** Routability is a pure function of the request body, so a given `(method, url, body)` is either always routed or always forwarded — the two uses of `REQUEST_PROMISES` can never disagree about the same key. There is still no second map; the key does carry a `#mode=` namespace (see [Cache key](#cache-key)), which makes that separation mechanical rather than dependent on routing staying body-pure.

**Streaming needs a recorder.** The forward path can snapshot its response because it already buffers (`resp.bytes().await`); the routed path returns a live SSE body, so there are no complete bytes at the time the response is built. `proxy::record_for_dedup` splits on that:

| Response | Handling |
| --- | --- |
| Non-2xx | `resolve(None)` immediately, response returned untouched. Waiters serve themselves — same rule as the forward path. |
| Exact `size_hint` (error envelopes, non-stream `/v1/messages`, `count_tokens`) | Collected inline, `resolve(Some(..))`, re-wrapped with `full_body` so `Content-Length` stays intact. |
| Stream | Wrapped in `RecordingBody`, which forwards frames untouched, accumulates a copy, and resolves at EOF. |

**Recording is opt-in per response, decided once.** `RecordingBody` checks `RequestPrimaryGuard::has_waiters()` on its *first* poll and allocates nothing when the wait queue is empty — the overwhelmingly common case. The decision is never revisited mid-stream: a waiter that joined after the first frame would receive a truncated SSE body, so late joiners get `None` and fall through to their own request. The window is wide in practice — the first poll happens only after the upstream POST returns (~70–140 ms), while duplicates arrive within ~2 ms.

**Waiters get the whole response at once**, after the primary finishes — the same semantics the forward path has always had for SSE. Adds no latency the waiter wouldn't have spent on its own call.

Two ordering rules to preserve:

- The dedup key uses the **pre-compression** body (what the client actually sent), so both duplicates key identically regardless of `[settings]` compression.
- If `try_handle` ever returns `None` under the gate (unreachable today — it only declines paths `is_messages_path` already rejects), the guard **must** be resolved before falling through, or a concurrent duplicate already waiting on that key hangs until `Drop` evicts the entry. (The forward path no longer collides with it — see `mode` under [Cache key](#cache-key) — so only same-mode waiters are at stake.)

## Cache key

```rust
format!("{} {}\n{}", method, url, body_str)
```

Two whitespace-separated fields, then the body. That layout is contractual and is asserted by `dedup_key_keeps_the_canonical_two_field_layout` in [src/proxy.rs](../src/proxy.rs).

The routing namespace rides **inside the URL field** as a `#mode=` fragment rather than adding a third field:

```text
POST https://api.anthropic.com/v1/messages#mode=routed-claude
{"model":"claude-oauth/claude-opus-5",…}
```

`mode` is the handling path — `routed-gemini`, `routed-claude`, or `forward` (a private `DedupMode` enum in [src/proxy.rs](../src/proxy.rs); the key is only ever built by `dedup_key`). A fragment can never appear in a real request target — hyper would have percent-encoded a literal `#` — so the suffix is unambiguous.

The same method + URL + body can be served three different ways, and the three produce different response bytes, so an entry from one must never be replayed to another. This is **defense in depth, not a live fix**: every mode is currently selected by a pure function of the body, and the body is already in the key, so equal keys always implied equal handling. Namespacing means that property stops being load-bearing — a gate that later keys on a header or host would otherwise start cross-replaying silently.

Method + URL prevent unrelated empty-body GETs to different endpoints from false-deduping against each other. Body is the discriminator for actual content — assumes that two non-empty bodies arriving in the same in-flight window represent a bug, not legitimately distinct calls (the body embeds session id, prompt, etc.).

No hashing — matches the existing OAuth dedup, which uses the raw body string as map key. One extra `String` allocation per in-flight request.

## State machine

`handle_dedup_request(key) -> RequestDedupState`:

- **`Primary(RequestPrimaryGuard)`** — no in-flight entry exists; we registered ourselves. Caller proceeds with the upstream call and must call `guard.resolve(...)` afterwards.
- **`Waiting(broadcast::Receiver)`** — an entry already exists; we subscribed. Caller `await rx.recv()` and replays the broadcast value.

The broadcast carries `Option<Arc<BufferedResponse>>` where `BufferedResponse { status: u16, headers: HeaderMap, body: Bytes }` is a clone-able snapshot. `Arc` keeps the multi-megabyte body cheap to fan out.

## Resolution rules

| Outcome | Primary broadcasts | Secondary behavior |
| --- | --- | --- |
| Upstream returns 2xx | `Some(BufferedResponse)` (filtered headers + body) | Replays the snapshot, returns to its client. |
| Upstream returns non-2xx | `None` | Falls through to a fresh native `reqwest::send` — same fallback shape as the OAuth path. |
| Upstream errors (`reqwest::Error`) | `None` | Same as above. |
| Primary task cancelled mid-flight (client disconnect) | `RequestPrimaryGuard::drop` removes the map entry; the `broadcast::Sender` drops, secondaries observe `RecvError::Closed` | Falls through to native fetch. |

The "non-2xx → secondary retries natively" choice is deliberate: it lets transient upstream failures recover for the still-connected secondary instead of locking it to the primary's bad outcome. The trade-off is that on persistent failures both clients still hit upstream — same cost as no dedup, but no worse.

## Header filtering

When the primary snapshots its 2xx response, these headers are stripped before broadcast (per [`STRIPPED_RESPONSE_HEADERS`](../src/interceptors.rs)):

```
connection, transfer-encoding, keep-alive, proxy-authenticate, proxy-authorization,
te, trailers, upgrade, content-length, set-cookie
```

Hop-by-hop headers can't be reused across two unrelated client connections; `content-length` would be wrong if anything else mutated the body; `set-cookie` is per-client identity.

## RAII cancellation safety

`RequestPrimaryGuard` mirrors the OAuth `PrimaryGuard` pattern at [src/interceptors.rs](../src/interceptors.rs). On drop without a prior `resolve()`:

1. `try_lock` the global map and remove our entry synchronously, OR
2. If the lock is contended, spawn a cleanup task that does the same.

This guarantees secondaries observe `RecvError::Closed` and fall through to native fetch instead of hanging forever waiting on a sender that will never fire. Never call `mem::forget` on a `RequestPrimaryGuard`; never replace `Drop` with manual cleanup paths.

On the routed path the guard is owned by `RecordingBody`, so a client that disconnects mid-stream drops the body with the guard unresolved and lands in exactly this path. That is also why `RecordingBody` must not override `is_end_stream`/`size_hint`: the defaults force hyper to poll to `Ready(None)`, and that final poll is the only thing that resolves the promise on the success path.

## Things deliberately not done

- **No retry-on-`Closed` for secondaries.** Stranded secondaries fall through to native fetch and may thunder the upstream if many were waiting on the same primary. KISS for v1; revisit if logs ever show it happening at scale.
- **No body size cap on the snapshot.** Consistent with the existing unbounded `resp.bytes().await` in [src/proxy.rs](../src/proxy.rs). Add a config knob later if memory pressure shows up.
- **No generic `InFlightMap<T>` abstraction.** OAuth dedup and request dedup share shape but differ in cache, key, and value types. Duplicating ~50 lines of RAII guard boilerplate is clearer than a premature abstraction.
- **No chunk-level multiplexing.** On the forward path neither client streams — `resp.bytes().await` collects fully before returning, so SSE clients see the full event log at once. On the routed path the *primary* streams normally (`RecordingBody` is a pass-through) but waiters still get the complete body in one frame after it finishes. Genuinely streaming to a waiter would mean broadcasting chunks rather than bytes; not worth it for a duplicate the client fired by mistake.
- **No dedup on the routed Gemini `/v1beta` surface.** Identical hole, identical one-line fix (`record_for_dedup` drops straight in), but no client has been observed double-firing there. Wire it up if opencode ever shows the pattern.

## Validation

Fire the same request twice, concurrently. Byte-identical is the whole point —
one differing character in the body is a different cache key and both calls go
upstream:

```bash
BODY='{"model":"claude-oauth/claude-sonnet-5","max_tokens":16,
       "messages":[{"role":"user","content":"dedup probe"}]}'

for _ in 1 2; do
  curl -sS -o /dev/null -X POST http://127.0.0.1:7777/v1/messages \
    -H 'content-type: application/json' -H 'x-api-key: unused' \
    -d "$BODY" &
done
wait
```

Any routed or forwarded POST works; this one needs only origin mode, so there is
no CA to trust and no `-k`. Expected proxy log (one upstream send, one secondary
wait). The sample below is from a *forwarded* Vertex request; a routed one logs
the same sequence with `routed request` in the message and the local URL rather
than the upstream one:

```
Intercepted: POST https://aiplatform.googleapis.com/...:streamRawPredict
Registered as the primary fetcher for this request.
We are the primary fetcher for https://...:streamRawPredict.
Intercepted: POST https://aiplatform.googleapis.com/...:streamRawPredict
Request already in flight, joining existing wait queue.
Waiting on primary in-flight request for https://...:streamRawPredict...
Sending upstream request to https://...
Upstream response status for https://...: 200 OK
Resolved request dedup promise waiters=1
Received response from primary in-flight request for https://...:streamRawPredict.
```

Two upstream sends (in Proxyman / `:rawPredict` log lines) means the dedup wasn't hit — check that the body bytes and URL really matched.

### Routed path

Testable in origin mode — no MITM, no CA trust, no Claude Code session. Run the proxy on a spare port so a live instance keeps working:

```bash
RUST_LOG=info,claude_proxy=debug target/debug/claude-proxy --port 7778

cat > /tmp/body.json <<'EOF'
{"model":"gemini-cli/gemini-3.5-flash","messages":[{"role":"user","content":[{"type":"text","text":"Reply with exactly the word: dedup"}]}],"max_tokens":2000,"stream":true}
EOF

for i in 1 2; do
  curl -s -N -X POST http://127.0.0.1:7778/v1/messages \
    -H 'content-type: application/json' --data-binary @/tmp/body.json \
    -o /tmp/resp$i.txt -w "req$i http=%{http_code} bytes=%{size_download}\n" &
done; wait
cmp /tmp/resp1.txt /tmp/resp2.txt && echo IDENTICAL
```

Expected log — one handler entry, one upstream generation, `waiters=1`:

```
Registered as the primary fetcher for this request.
We are the primary fetcher for routed request http://127.0.0.1:7778/v1/messages.
Anthropic API request: POST /v1/messages
Waiting on primary in-flight routed request for http://127.0.0.1:7778/v1/messages...
Anthropic streamGenerateContent -> provider=gemini-cli model=gemini-3.5-flash (...)
Resolved request dedup promise waiters=1
Received response from primary in-flight routed request for http://127.0.0.1:7778/v1/messages.
```

Cases worth re-checking after touching `record_for_dedup` or `RecordingBody`:

| Case | Expected |
| --- | --- |
| Single request (no duplicate) | `Resolved request dedup promise waiters=0`, nothing recorded. |
| `"stream":false` duplicate pair | One upstream `Anthropic generateContent`, `waiters=1`, identical `application/json` bodies with correct `Content-Length`. |
| `count_tokens` | Still returns `{"input_tokens":N}`; `waiters=0`. |
| Unroutable model, duplicate pair | **Origin mode (this test procedure):** the branch is selected by path before the model is parsed, so the primary registers as usual, `try_handle` 404s, and `record_for_dedup` resolves `None` — log shows `Resolved request dedup promise waiters=1`, then the waiter's `Primary returned None (failed/non-2xx/unrecorded). Serving natively.` and its own independent 404. Failures are never replayed. **MITM mode differs:** `routed_provider` returning `None` fails the gate itself, so the request never enters routed dedup — it falls through to the real-Anthropic upstream forward (and that path's generic dedup block) instead. |
| Client aborts mid-stream (`curl --max-time 0.5`) | `RequestPrimaryGuard dropped without resolve — task was cancelled. Removing in-flight entry.`; an identical request afterwards becomes a fresh primary and succeeds. |

Two upstream generations for one duplicate pair means the recorder never resolved — check that `RecordingBody` is still polled to `Ready(None)` (i.e. that nothing re-introduced an `is_end_stream`/`size_hint` override).
