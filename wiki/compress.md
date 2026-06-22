# Content Compression & SmartCrusher

`claude-proxy` includes a highly optimized, lossy content compression engine known as **SmartCrusher**. It is designed to mitigate a common issue in LLM developer loops: tool executions (e.g. database query results, large file reads, or massive logs) returning highly repetitive, over-budget structures that inflate context size and waste billable upstream tokens.

---

## Architecture and Pipeline Placement

Request compression runs inside the HTTP interception layer in `proxy.rs`, just before request deduplication and upstream forwarding.

```
Incoming Request → Map Local → [Content Compression] → OAuth / Heat-Up → Request Dedup → Upstream Forward
```

The compression applies to the *request body* sent by the client, filtering through the `messages` (Anthropic) or `contents` (Gemini) payload to target `tool_result` (or `functionResponse`) blocks.

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

---

## How SmartCrusher Works

The compression pipeline consists of two stages applied to tool results:

```
[Tool Result String] 
         ↓
   Is it a JSON Array?
     ├── YES → Run SmartCrusher Lossy Compactor (if json_array = true)
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
- **Critical Item Preservation**: Runs algorithms to preserve key items that are statistically anomalous or highly relevant:
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
json_array      = true   # Compact JSON array structures using SmartCrusher

[compress.providers.vertex]
max_tool_chars  = 8000
json_array      = true

[compress.providers.opengateway]
max_tool_chars  = 6000
json_array      = false  # Only apply simple truncation
```

*Note: Providers not listed in the `providers` map receive no compression or truncation.*
