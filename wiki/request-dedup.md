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

## Cache key

```
format!("{} {}\n{}", method, url, body_str)
```

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

## Things deliberately not done

- **No retry-on-`Closed` for secondaries.** Stranded secondaries fall through to native fetch and may thunder the upstream if many were waiting on the same primary. KISS for v1; revisit if logs ever show it happening at scale.
- **No body size cap on the snapshot.** Consistent with the existing unbounded `resp.bytes().await` in [src/proxy.rs](../src/proxy.rs). Add a config knob later if memory pressure shows up.
- **No generic `InFlightMap<T>` abstraction.** OAuth dedup and request dedup share shape but differ in cache, key, and value types. Duplicating ~50 lines of RAII guard boilerplate is clearer than a premature abstraction.
- **Streaming pass-through is still not supported.** Both primary and secondary receive the response only after the upstream finishes — `resp.bytes().await` collects fully before returning. SSE clients see the full event log at once. If real streaming pass-through is added later, the dedup multiplexer will need to broadcast chunks, not bytes.

## Validation

Run two identical curls in parallel against a live proxy:

```bash
sed '1 s/^curl /curl -k /' refs/1.sh | bash &
sed '1 s/^curl /curl -k /' refs/2.sh | bash &
wait
```

Expected proxy log (one upstream send, one secondary wait):

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
