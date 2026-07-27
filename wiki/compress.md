# Content Compression & SmartCrusher

`claude-proxy` includes a highly optimized, lossy content compression engine known as **SmartCrusher**. It is designed to mitigate a common issue in LLM developer loops: tool executions (e.g. database query results, large file reads, or massive logs) returning highly repetitive, over-budget structures that inflate context size and waste billable upstream tokens.

---

## Architecture and Pipeline Placement

Compression is woven into the routing layer in `proxy.rs` at three sites, one per downstream surface. Gemini and Anthropic branches compress inline and return early (they never reach the dedup map, which is specific to the `claude` CLI's traffic); Vertex AI Anthropic traffic is compressed between the Anthropic MITM block and the OAuth/dedup section, so the compressed body feeds both the dedup key and the upstream forward.

```text
                                ┌─ compress (path provider) ─→ gemini::try_handle ──┐
                                │                                                    │
                                ├─ compress (model prefix)  ─→ anthropic::try_handle ┤
 Incoming → Map Local ─┬─ Gemini path?                                                │
                       ├─ Anthropic + prefix?                                          │
                       ├─ Vertex host + Anthropic path? ─→ compress ("vertex") ───┐   │
                       └─ other                                                    │   │
                                                                                  ▼   ▼
                                                                    OAuth → Vertex Heat-Up
                                                                           → Request Dedup
                                                                           → Upstream Forward
```

The compression applies to the *request body* sent by the client, filtering through the `messages` (Anthropic / OpenAI) or `contents` (Gemini) payload to target `tool_result` / `role:tool` (or `functionResponse`) blocks.

### Resolution Strategies

The proxy determines which compression config to use based on the request's downstream destination:

1. **Model Prefix (OpenAI & Gemini Aggregators)**
   - Resolves the provider from the first `/`-delimited segment of the `model` name.
   - e.g. `opengateway/minimax-m3` → provider `opengateway`.

2. **Gemini Path Routing**
   - Resolves the provider from the path parameters of Gemini endpoints.
   - e.g. `/v1beta/models/gemini-cli/...` → provider `gemini-cli`.

3. **Vertex AI Path Routing**
   - Resolves the `"vertex"` provider when intercepting calls to `aiplatform.googleapis.com` containing the Anthropic publisher path `/publishers/anthropic/models/`.

4. **Routed Anthropic Messages (`/v1/messages`)**
   - Resolves the provider via `anthropic::routed_provider`, **not** by the model-prefix strategy above, and passes it to `compress::apply` explicitly.
   - Necessary because strategy 1 needs a `/` in `model`, which a `[anthropic_model_map]` target doesn't have on the way in — a mapped request still says e.g. `claude-sonnet-5`. Sniffing would silently skip the configured `[compress.providers.gemini-cli]` block for exactly the traffic the model map exists to redirect (measured: 2667 prompt tokens uncompressed vs 160 compressed on the same body).
   - Applies on both transports (MITM and plain-HTTP origin). The body's `model` is deliberately left unrewritten so the `Anthropic model map: <from> -> <to>` log still identifies the redirect.

---

## How SmartCrusher Works

The compression pipeline consists of two stages applied to tool results:

```text
[Tool Result String] 
         ↓
   Is it a JSON Array?
     ├── YES → Run SmartCrusher Lossy Compactor (if smart_crusher = true)
     └── NO  → Fall through to simple Truncation (if max_tool_chars > 0)
```

### 1. Simple Truncation
If `max_tool_chars` is configured (non-zero) and the tool result size exceeds this threshold, the string is truncated. 
- It preserves a `head` and `tail` of the text, inserting an elided marker in the middle: `\n[... {N} characters truncated ...]\n`.
- The truncation uses an optimized $O(\text{head_chars} + \text{tail_chars})$ byte offset lookup on the character stream to avoid severe bottlenecks on megabyte-sized logs.

### 2. SmartCrusher Compactor (JSON Array Crushing)
When the tool output is a valid JSON array, SmartCrusher evaluates the array statistically and transforms it into a highly compressed representation (e.g. CSV or Markdown-KV tables) without losing critical diagnostic semantic data.

#### Key Mechanics:
- **Field Stats & Detection**: Analyzes all items in the array to collect field types, uniqueness, ranges, and patterns. It automatically detects "score fields" (e.g. search relevance scores) and sequential patterns (like IDs).
- **Core Field Selection**: Differentiates between uniform and heterogeneous keys. If most records share a common schema, it selects "core fields" based on how frequently they appear.
- **Critical Item Preservation**: Runs algorithms to preserve key items that are statistically anomalous or relevant:
  - Outliers (structural uniqueness).
  - Errors (items containing diagnostic words like "error", "fail", "exception").
  - Numerical anomalies (using standard deviation checks on score-like columns).
- **Format Compaction**: Encodes the compacted output in highly dense representations like CSV tables or custom dotted-notation dotted-column arrays, drastically reducing the JSON syntax footprint (brackets, quotes, repeated keys).

---

## Configuration

To enable compression, configure provider blocks under `[compress.providers]` in `config.toml`:

```toml
[compress.providers.gemini-cli]
max_tool_chars  = 12000  # Hard cap on character length per tool output
smart_crusher   = true   # Compact JSON array structures using SmartCrusher
bias            = 1.0    # Optional bias multiplier on adaptive sizing (>1 = keep more, <1 = compress harder)

[compress.providers.vertex]
max_tool_chars  = 8000
smart_crusher   = true
bias            = 1.0

[compress.providers.opengateway]
max_tool_chars  = 6000
smart_crusher   = false  # Only apply simple truncation
```

*Note: Providers not listed in the `providers` map receive no compression or truncation.*
