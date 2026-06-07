# Map Local

## What it does

For each `[[map_local]]` rule in `config.toml`, the proxy matches incoming requests by URL pattern + optional HTTP method and returns a fixed response (inline literal, local file, or empty body) instead of forwarding upstream. Lets users silence telemetry endpoints, neuter update checks, and replay canned fixtures without burning real API tokens.

Charles/Proxyman ship something equivalent. We add it to claude-proxy because the existing OAuth-cache + heat-up + dedup interceptors only cover a few specific endpoints — Map Local is the general escape hatch when "I want this URL to return that response, full stop."

## Scope

Runs **first** in the pipeline, ahead of OAuth caching, Vertex AI heat-up short-circuit, and request dedup:

```
Map Local → OAuth token cache → Vertex heat-up → request dedup → upstream forward
```

Why first:
- A user mapping the OAuth token endpoint to a fixture wants the fixture, not a disk-cached real token.
- A user mapping a `:rawPredict` URL to a canned response wants the file even when the body matches the heat-up shape.
- Bypassing dedup is correct — there's no upstream call to share, so populating the in-flight map with synthetic-response candidates would be wrong.

This generalizes the existing invariant "heat-ups must short-circuit before dedup": any synthetic-response interceptor must precede dedup. Map Local is now the most-prior of those.

### Plain HTTP

The non-CONNECT branch of [`handle_request`](../src/proxy.rs) historically returned `500 Only CONNECT supported`. To support `http://` URLs in Map Local rules (e.g. internal log endpoints behind plain HTTP), the matcher runs there too, *before* the 500 fallback. Non-mapped plain HTTP still 500s — we deliberately did not add general plain-HTTP forwarding. Plain-HTTP support exists exclusively to make Map Local work on `http://` URLs.

The plain-HTTP URL is reconstructed as either `req.uri().to_string()` (absolute-URI form, what proxy clients normally send) or `format!("http://{host}{path}")` from the `Host` header (origin-form fallback).

## Config schema

```toml
[[map_local]]
url          = "<wildcard pattern>"           # required
method       = "GET" | "POST" | ...           # optional; case-insensitive; omit = match any verb
body         = "<inline literal>"             # optional; mutually exclusive with `file`
file         = "<path; ~ expanded>"           # optional; live-read at request time
status       = 200                            # optional; default 200
content_type = "application/json"             # optional; see defaulting below
[map_local.headers]                           # optional; merged into the response
"x-mock-source" = "fixture"
```

Loaded by [`config::load_config`](../src/config.rs). Tilde expansion is applied to `file` once at config load — not at request time.

Validation (warn, don't fail) at load time:
- `url` empty → log a warning, rule is kept but never matches.
- Unknown `method` → log a warning; the rule will only match that exact verb literally.
- Both `body` and `file` set → `body` wins, `file` ignored, warning logged.
- `file` set but path missing → warning. The file may legitimately appear later; live-read picks it up. (At request time, missing file produces a 502 — see [Error envelope](#error-envelope).)

The proxy logs `Loaded N Map Local rule(s)` at startup when N > 0. An empty `Vec<MapLocalRule>` short-circuits the matcher in O(1) — zero overhead for users who don't configure any rules.

## Matching semantics

Two filters, one specificity score.

### URL pattern

Hand-rolled fnmatch in [`wildcard_match`](../src/interceptors.rs). `*` matches zero or more characters, `?` matches exactly one. The pattern is matched against the *whole* URL string (scheme + host + path + query) so users can be as specific or as broad as they like — `https://api.example.com/v1/foo` is exact, `*example.com*` is broad, `https://api.example.com/v1/*` covers a path prefix.

DP table is `O(|pattern| × |url|)` — both well under a few hundred chars in practice. No regex crate added.

### Method

Optional, case-insensitive. `None` matches any verb; otherwise must equal the request method exactly (after upper-casing both sides).

### Specificity tiebreaker

When more than one rule matches, the most specific one wins:

```text
score = (number of literal, non-wildcard chars in url)
      + (1_000_000 if method is set, else 0)
```

The +1,000,000 bonus guarantees a method-pinned rule beats a same-URL any-method rule for the corresponding verb. This matters for the canonical "GET and POST against the same URL map to different files" case: the request hits the verb-specific rule, and an any-method fallback rule for the same URL still catches the *other* verbs.

There is no precedence by config order — score-only.

## Body resolution

Three sources, checked in priority order at request time:

1. `body` set → use it as inline bytes. `BodyKind::Inline`.
2. `file` set → `tokio::fs::read(path).await`. On success: `BodyKind::File(path)`. On error: 502 envelope (see below).
3. Neither → `BodyKind::Empty`. Response body is empty bytes.

File reads are **live** — done on every matching request, not cached. Editing the file changes the next response. No file-watching, no mtime caching: the cost of one open + read per request is negligible compared to a real upstream round-trip we're replacing, and the alternative ("restart the proxy to pick up edits") is the kind of papercut Charles/Proxyman users complain about.

`tokio::fs::read` keeps file I/O off the synchronous path. We always read the whole file in one shot; not worth streaming for the sizes we expect.

## Content-Type defaulting

Decision matrix (first row that matches wins):

| Condition | Result |
| --- | --- |
| Rule has explicit `content_type` | Use that |
| `BodyKind::File(path)` | Derive from extension via `guess_mime_from_path`; fallback `application/octet-stream` |
| `BodyKind::Inline` and body is non-empty | `application/json` |
| Anything else (including empty body) | No `Content-Type` header at all |

The "empty body → no Content-Type" rule is what makes the update-check case (verification 4 below) work cleanly. A `Content-Type: text/plain` on a zero-byte 200 would be a lie.

`guess_mime_from_path` covers the common extensions we actually expect to serve (json, txt, html, css, js, xml, csv, png, jpg/jpeg, gif, webp, svg, pdf, zip, wasm, woff, woff2). For anything else, the fallback is `application/octet-stream` — sane default for "I don't know what this is, but the bytes are correct."

## Headers

User-supplied `[map_local.headers]` are merged into the response after Content-Type but before the diagnostic headers. **`content-length` is silently dropped** — Hyper writes it from the actual body bytes, and a double-set risks malformed responses.

The proxy unconditionally adds:

- `X-Map-Local: true` — always, on every Map Local response (including the 502 error envelope).
- `X-Map-Local-Source: <path>` — only when the body came from a `file`. Lets you tell file-backed responses from inline ones at a glance in network logs.
- `X-Map-Local-Error: file-unreadable` — only on the 502 envelope.

## Error envelope

If `file` is set and the read fails (missing, permission denied, IO error):

```
HTTP/1.1 502 Bad Gateway
Content-Type: text/plain; charset=utf-8
X-Map-Local: true
X-Map-Local-Error: file-unreadable

Map Local: cannot read /abs/path/to/file: <io error description>
```

Loud failure beats silent passthrough. If we passed through to upstream on a missing fixture, the user would see "the mock isn't working" with no signal *why* — they'd assume the URL didn't match. A 502 with the path in the body answers the question instantly.

No caching of the error state. The next matching request retries the read; if the file was just being rewritten and is now readable, the next request gets the real content.

## Things deliberately not done

- **No body-shape matching.** FRTMProxy's bridge supports a SHA-256-of-canonical-body signature in its rule keys. We don't — URL+method covers every verification case the user gave us, and adding body matching means deciding how to canonicalize JSON / form-urlencoded / arbitrary bytes. Revisit only if a real use case shows up.
- **No directory mapping.** A rule maps one match to one response. We don't translate URL paths into filenames inside a folder — that introduces path-traversal concerns and a UX surface (index files, content negotiation) that's a separate feature.
- **No hot config reload.** Config changes require `claude-proxy restart`. Adding a `notify`-based watch is straightforward but separable from this feature.
- **No streaming bodies.** Whole-file `read()`. SSE-style mocks can't be served byte-by-byte; the client receives them as one chunk after the read finishes.
- **No templating.** The response body is exactly what `body` or the file says it is. No echo of request fields, no Jinja.
- **No general plain-HTTP forwarding.** Plain HTTP is supported only enough for Map Local to match `http://` URLs. Non-mapped plain HTTP still 500s, preserving the existing contract.

## Validation

The four canonical cases double as the README example config so users have something concrete to copy from.

### Case 1 — Datadog logs (HTTPS, custom status, inline body)

```toml
[[map_local]]
url    = "https://http-intake.logs.us5.datadoghq.com/api/v2/logs"
method = "POST"
status = 202
body   = "{}"
```

Test:

```bash
curl -sk -x http://127.0.0.1:7777 -X POST -i \
    -d 'whatever' "https://http-intake.logs.us5.datadoghq.com/api/v2/logs"
```

Expect `HTTP/1.1 202 Accepted` + `Content-Type: application/json` + `Content-Length: 2` + `X-Map-Local: true` + body `{}`.

### Case 2 — internal logs (plain HTTP)

```toml
[[map_local]]
url    = "http://10.102.148.28/v1/logs"
method = "POST"
body   = "{}"
```

Test:

```bash
curl -s -x http://127.0.0.1:7777 -X POST -i \
    -d 'whatever' "http://10.102.148.28/v1/logs"
```

Expect `HTTP/1.1 200 OK` + `Content-Type: application/json` + body `{}` + `X-Map-Local: true`. Confirms the non-CONNECT branch of `handle_request` runs the matcher. Without a matching rule the proxy returns `500 Only CONNECT supported` — that's the preserved fallback.

### Case 3 — Anthropic event-logging batch (HTTPS, inline JSON literal)

```toml
[[map_local]]
url    = "https://api.anthropic.com/api/event_logging/v2/batch"
method = "POST"
body   = '{"accepted_count":100,"rejected_count":0}'
```

Expect `HTTP/1.1 200 OK` + `Content-Type: application/json` + body `{"accepted_count":100,"rejected_count":0}` + `X-Map-Local: true`.

### Case 4 — Claude Code update check (GET, empty body)

```toml
[[map_local]]
url    = "https://downloads.claude.ai/claude-code-releases/latest"
method = "GET"
```

No `body`, no `file`, no `content_type`. Expect `HTTP/1.1 200 OK` + `Content-Length: 0` + **no `Content-Type` header** + empty body + `X-Map-Local: true`. Confirms the "neither body nor file" path produces a clean empty response without spurious Content-Type defaulting.

### Regression checks (existing behavior unchanged)

5. **No-rules backwards compat.** Run with no `[[map_local]]` blocks; walk through the OAuth disk-cache hit, Vertex heat-up short-circuit, and dedup primary/waiter pair from [request-dedup.md](request-dedup.md). All three should behave exactly as before.
6. **Method differentiation.** Two rules, same URL, one for `GET` and one for `POST`, returning different bodies. Confirm GET and POST hit their own rules.
7. **Specificity tiebreaker.** Add a third any-method rule with the same URL. GETs and POSTs continue to hit their method-specific rules; PUT falls through to the any-method rule (proves the +1,000,000 bonus).
8. **Wildcard URL.** Map `https://api.example.com/v1/*` to a fixture; confirm `/v1/foo` and `/v1/bar?x=1` both match.
9. **File-backed live reload.** Map a URL to `~/dev/mocks/x.json`; hit it; edit the file; hit again; confirm the new content with no proxy restart. Confirm `Content-Type: application/json` was auto-derived.
10. **Binary file.** Map a URL to a `.png`; pipe `curl` output to `file -`; confirm valid PNG and `Content-Type: image/png`.
11. **Missing file.** Point a rule at `/tmp/does-not-exist.json`; confirm `502` + `X-Map-Local-Error: file-unreadable` + helpful body. Replace with a valid file; the next request succeeds (no caching of the error state).
12. **OAuth-endpoint shadowing.** Map `https://oauth2.googleapis.com/token` to a token-shaped fixture. Logs should show `Map Local hit` and **not** `Cache hit on disk for token` — proves Map Local sits ahead of the OAuth interceptor.
13. **Dedup bypass.** Fire two byte-identical concurrent POSTs at a mapped URL (using the recipe in [request-dedup.md](request-dedup.md)). Both return the mock; logs show two `Map Local hit` lines and **no** `We are the primary fetcher` / `Waiting on primary in-flight request` — proves Map Local sits ahead of the dedup map.

The unit-test surface in [src/interceptors.rs](../src/interceptors.rs) covers the matcher invariants directly: wildcard correctness (`wildcard_basics`), method-pinned-beats-any-method (`match_method_specific_beats_any`), method filtering (`match_method_filter`), and the empty-rules fast path (`match_no_rules`). They run under plain `cargo test`.
