//! Anthropic Messages API ↔ native-Gemini translation.
//!
//! The proxy serves `/v1/messages` by translating the Anthropic body to a
//! native-Gemini body ([`claude_to_gemini`]), feeding it through the **existing**
//! `gemini::translate` envelope + `gemini::provider` upstream, then translating
//! the Gemini reply back to Anthropic ([`gemini_to_claude_nonstream`] /
//! [`ClaudeStream`]). So this file is *only* the Anthropic↔Gemini boundary;
//! everything else (envelope, upstream POST, SSE pump, creds, routing) is reused.
//!
//! Ported faithfully from CLIProxyAPI `internal/translator/gemini/claude/`
//! (`ConvertClaudeRequestToGemini`, `ConvertGeminiResponseToClaude[NonStream]`)
//! and `internal/util/{translator.go,util.go,claude_tool_id.go}`. Schema
//! sanitization (`CleanJSONSchemaForGemini`) and `max_tokens`/`stop_sequences`
//! mapping are intentionally deferred — see [wiki/gemini-providers.md].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

/// Sentinel that tells Code Assist to accept replayed tool calls without a real
/// thought signature (same constant used in [`super::translate`]).
const SKIP_SIG: &str = "skip_thought_signature_validator";

/// Process-wide counter for synthesizing unique `tool_use` ids.
static TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Tool-name utilities (port of internal/util)
// ---------------------------------------------------------------------------

fn is_allowed_fn_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')
}

/// Gemini function names must match `[a-zA-Z0-9_.:-]`, start with a letter or
/// underscore, and be ≤64 chars. (Port of `util.SanitizeFunctionName`.)
pub fn sanitize_function_name(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    // All allowed chars are ASCII and replacements are `_`, so the result is
    // pure ASCII and byte-truncation stays on char boundaries.
    let mut s: String = name
        .chars()
        .map(|c| if is_allowed_fn_char(c) { c } else { '_' })
        .collect();

    match s.as_bytes().first() {
        Some(&b) if b.is_ascii_alphabetic() || b == b'_' => {}
        Some(_) => {
            if s.len() >= 64 {
                s.truncate(63);
            }
            s.insert(0, '_');
        }
        None => s = "_".to_string(),
    }

    if s.len() > 64 {
        s.truncate(64);
    }
    s
}

/// Canonical tool-name key: trimmed, leading underscores stripped, lowercased.
fn canonical_tool_name(name: &str) -> String {
    name.trim().trim_start_matches('_').to_lowercase()
}

fn is_tool_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// Ensure an id matches Claude's `tool_use.id` regex `^[a-zA-Z0-9_-]+$`,
/// generating a fallback when empty. (Port of `util.SanitizeClaudeToolID`.)
fn sanitize_claude_tool_id(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if is_tool_id_char(c) { c } else { '_' })
        .collect();
    if s.is_empty() {
        format!("toolu_{}", TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
    } else {
        s
    }
}

/// A claude `tool_use_id` is shaped `<name>-<n>`; recover `<name>`.
fn tool_name_from_claude_tool_use_id(id: &str) -> String {
    match id.rfind('-') {
        Some(idx) => id[..idx].to_string(),
        None => String::new(),
    }
}

/// Name maps built from the inbound Claude request's `tools[]`, used to restore
/// the exact client-facing tool name on the response (strict clients like
/// Claude Code require byte-identical tool names).
pub struct ToolMaps {
    /// canonical-name → original-name (port of `ToolNameMapFromClaudeRequest`).
    name_map: HashMap<String, String>,
    /// sanitized-name → original-name, only where sanitizing changed the name
    /// (port of `SanitizedToolNameMap`).
    sanitized_map: HashMap<String, String>,
}

impl ToolMaps {
    pub fn from_request(req: &Value) -> ToolMaps {
        let mut name_map = HashMap::new();
        let mut sanitized_map = HashMap::new();
        if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
            for tool in tools {
                let name = tool
                    .get("name")
                    .and_then(|n| n.as_str())
                    .or_else(|| tool.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()))
                    .unwrap_or("")
                    .trim();
                if name.is_empty() {
                    continue;
                }
                let key = canonical_tool_name(name);
                if !key.is_empty() {
                    name_map.entry(key).or_insert_with(|| name.to_string());
                }
                // sanitized_map keyed only on the primary `name` field.
                if let Some(primary) = tool.get("name").and_then(|n| n.as_str()).map(str::trim) {
                    if !primary.is_empty() {
                        let sanitized = sanitize_function_name(primary);
                        if sanitized != primary {
                            sanitized_map.entry(sanitized).or_insert_with(|| primary.to_string());
                        }
                    }
                }
            }
        }
        ToolMaps { name_map, sanitized_map }
    }
}

fn map_tool_name(maps: &ToolMaps, name: &str) -> String {
    if name.is_empty() {
        return name.to_string();
    }
    maps.name_map
        .get(&canonical_tool_name(name))
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

fn restore_sanitized_tool_name(maps: &ToolMaps, sanitized: &str) -> String {
    if sanitized.is_empty() {
        return sanitized.to_string();
    }
    maps.sanitized_map
        .get(sanitized)
        .cloned()
        .unwrap_or_else(|| sanitized.to_string())
}

// ---------------------------------------------------------------------------
// Request: Anthropic Messages -> native Gemini
// ---------------------------------------------------------------------------

/// Translate an Anthropic Messages request body to a native-Gemini request body.
///
/// The result is fed straight into [`super::translate::gemini_to_gemini_cli`] /
/// `gemini_to_antigravity`, whose `build_envelope` already runs role
/// normalization, `fix_cli_tool_response` grouping, thought-signature injection,
/// empty-parts filtering, and default safety — so none of those are repeated
/// here. We deliberately do **not** set a top-level `model`; the caller passes
/// the bare model to the envelope builder.
pub fn claude_to_gemini(req: &Value) -> Value {
    let mut out = json!({ "contents": [] });

    // system instruction (string or array of {type:text,text})
    match req.get("system") {
        Some(Value::Array(arr)) => {
            let parts: Vec<Value> = arr
                .iter()
                .filter(|sp| sp.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|sp| sp.get("text").and_then(|t| t.as_str()))
                .map(|text| json!({ "text": text }))
                .collect();
            if !parts.is_empty() {
                out["system_instruction"] = json!({ "role": "user", "parts": parts });
            }
        }
        Some(Value::String(s)) => {
            out["system_instruction"] = json!({ "parts": [{ "text": s }] });
        }
        _ => {}
    }

    // messages -> contents
    if let Some(messages) = req.get("messages").and_then(|m| m.as_array()) {
        let mut contents: Vec<Value> = Vec::new();
        for msg in messages {
            let role0 = match msg.get("role").and_then(|r| r.as_str()) {
                Some(r) => r,
                None => continue,
            };
            let role = if role0 == "assistant" { "model" } else { role0 };

            match msg.get("content") {
                Some(Value::Array(blocks)) => {
                    let mut parts: Vec<Value> = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                            "text" => {
                                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                if !text.is_empty() {
                                    parts.push(json!({ "text": text }));
                                }
                            }
                            "tool_use" => {
                                let raw_id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                                let mut fname =
                                    block.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                if !raw_id.is_empty() {
                                    let derived = tool_name_from_claude_tool_use_id(raw_id);
                                    if !derived.is_empty() {
                                        fname = derived;
                                    }
                                }
                                let fname = sanitize_function_name(&fname);
                                // Default missing/non-object `input` to `{}` rather than
                                // dropping the call (CLIProxyAPI drops it). A dropped
                                // tool_use orphans the matching tool_result next turn and
                                // desyncs `fix_cli_tool_response`; an empty-args call is valid.
                                let args = block
                                    .get("input")
                                    .filter(|v| v.is_object())
                                    .cloned()
                                    .unwrap_or_else(|| json!({}));
                                // Keep the tool_use id on the functionCall so the
                                // antigravity→Anthropic round-trip (cloudcode-pa) can
                                // rebuild `tool_use.id` — the Vertex backend rejects a
                                // tool_use without one (`tool_use.id: Field required`).
                                let mut fc = json!({ "name": fname, "args": args });
                                if !raw_id.is_empty() {
                                    fc["id"] = json!(raw_id);
                                }
                                parts.push(json!({
                                    "thoughtSignature": SKIP_SIG,
                                    "functionCall": fc,
                                }));
                            }
                            "tool_result" => {
                                let tcid =
                                    block.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("");
                                if tcid.is_empty() {
                                    continue;
                                }
                                let mut fname = tool_name_from_claude_tool_use_id(tcid);
                                if fname.is_empty() {
                                    fname = tcid.to_string();
                                }
                                let fname = sanitize_function_name(&fname);
                                // Anthropic tool_result `content` is usually a plain
                                // string — emit it verbatim. `Value::to_string` would
                                // JSON-encode it (`"text"` -> `"\"text\""`), so the model
                                // sees a double-quoted result. Arrays/objects are still
                                // rendered to their JSON text. (CLIProxyAPI double-encodes
                                // the string case via `.Raw` + `sjson.SetBytes`; we don't.)
                                let result = match block.get("content") {
                                    Some(Value::String(s)) => s.clone(),
                                    Some(other) => other.to_string(),
                                    None => String::new(),
                                };
                                // Carry the id (matching the functionCall) so the round-trip
                                // pairs tool_use ↔ tool_result by the same id.
                                parts.push(json!({
                                    "functionResponse": {
                                        "id": tcid,
                                        "name": fname,
                                        "response": { "result": result },
                                    }
                                }));
                            }
                            "image" => {
                                let source = block.get("source");
                                if source.and_then(|s| s.get("type")).and_then(|t| t.as_str())
                                    != Some("base64")
                                {
                                    continue;
                                }
                                let mime = source
                                    .and_then(|s| s.get("media_type"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");
                                let data = source
                                    .and_then(|s| s.get("data"))
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("");
                                if !mime.is_empty() && !data.is_empty() {
                                    parts.push(json!({ "inlineData": { "mimeType": mime, "data": data } }));
                                }
                            }
                            _ => {}
                        }
                    }
                    contents.push(json!({ "role": role, "parts": parts }));
                }
                Some(Value::String(s)) => {
                    contents.push(json!({ "role": role, "parts": [{ "text": s }] }));
                }
                _ => {}
            }
        }
        out["contents"] = json!(contents);
    }

    // Drop a trailing model turn that is *only* unanswered functionCall parts —
    // Gemini returns an empty response when the last turn is a dangling model
    // functionCall. A mixed turn (text/thinking alongside the call) is kept so
    // we don't discard valid context.
    if let Some(arr) = out.get("contents").and_then(|c| c.as_array()) {
        if let Some(last) = arr.last() {
            let is_dangling_call = last.get("role").and_then(|r| r.as_str()) == Some("model")
                && last
                    .get("parts")
                    .and_then(|p| p.as_array())
                    .map(|ps| !ps.is_empty() && ps.iter().all(|p| p.get("functionCall").is_some()))
                    .unwrap_or(false);
            if is_dangling_call {
                if let Some(a) = out["contents"].as_array_mut() {
                    a.pop();
                }
            }
        }
    }

    // tools -> functionDeclarations
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let mut decls: Vec<Value> = Vec::new();
        for tool in tools {
            let schema = match tool.get("input_schema") {
                Some(s) if s.is_object() => s.clone(),
                _ => continue,
            };
            let mut t = tool.clone();
            if let Some(obj) = t.as_object_mut() {
                obj.remove("input_schema");
                // v1: pass the schema through verbatim (CleanJSONSchemaForGemini deferred).
                obj.insert("parametersJsonSchema".to_string(), schema);
                for k in ["strict", "input_examples", "type", "cache_control", "defer_loading", "eager_input_streaming"] {
                    obj.remove(k);
                }
                let name = obj.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                obj.insert("name".to_string(), json!(sanitize_function_name(&name)));
            }
            decls.push(t);
        }
        if !decls.is_empty() {
            out["tools"] = json!([{ "functionDeclarations": decls }]);
        }
    }

    // tool_choice -> toolConfig.functionCallingConfig
    if let Some(tc) = req.get("tool_choice") {
        let (tc_type, tc_name) = if tc.is_object() {
            (
                tc.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                tc.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            )
        } else if let Some(s) = tc.as_str() {
            (s.to_string(), String::new())
        } else {
            (String::new(), String::new())
        };
        match tc_type.as_str() {
            "auto" => out["toolConfig"]["functionCallingConfig"]["mode"] = json!("AUTO"),
            "none" => out["toolConfig"]["functionCallingConfig"]["mode"] = json!("NONE"),
            "any" => out["toolConfig"]["functionCallingConfig"]["mode"] = json!("ANY"),
            "tool" => {
                out["toolConfig"]["functionCallingConfig"]["mode"] = json!("ANY");
                if !tc_name.is_empty() {
                    out["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"] =
                        json!([sanitize_function_name(&tc_name)]);
                }
            }
            _ => {}
        }
    }

    // thinking + sampling params -> generationConfig
    if let Some(t) = req.get("thinking").filter(|t| t.is_object()) {
        match t.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "enabled" => {
                if let Some(b) = t.get("budget_tokens").and_then(|b| b.as_i64()) {
                    out["generationConfig"]["thinkingConfig"]["thinkingBudget"] = json!(b);
                    out["generationConfig"]["thinkingConfig"]["includeThoughts"] = json!(true);
                }
            }
            "adaptive" | "auto" => {
                // v1: pass an explicit effort through as thinkingLevel, else "high".
                // (Model-aware max-budget lookup is deferred.)
                let effort = req
                    .get("output_config")
                    .and_then(|o| o.get("effort"))
                    .and_then(|e| e.as_str())
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty());
                out["generationConfig"]["thinkingConfig"]["thinkingLevel"] =
                    json!(effort.unwrap_or_else(|| "high".to_string()));
                out["generationConfig"]["thinkingConfig"]["includeThoughts"] = json!(true);
            }
            _ => {}
        }
    }
    if let Some(v) = req.get("temperature").and_then(|x| x.as_f64()) {
        out["generationConfig"]["temperature"] = json!(v);
    }
    if let Some(v) = req.get("top_p").and_then(|x| x.as_f64()) {
        out["generationConfig"]["topP"] = json!(v);
    }
    if let Some(v) = req.get("top_k").and_then(|x| x.as_i64()) {
        out["generationConfig"]["topK"] = json!(v);
    }
    // Map Anthropic `max_tokens` → `maxOutputTokens`. CLIProxyAPI's *gemini*/claude
    // translator omits this, but its *antigravity*/claude translator maps it
    // (antigravity_claude_request.go:560) — and we must too, because for the
    // `-thinking` models the Vertex Anthropic backend requires
    // `max_tokens > thinking.budget_tokens`; without it the upstream default is
    // ≤ the budget and the request 400s. (Antigravity non-claude models drop
    // maxOutputTokens again in `gemini_to_antigravity`; gemini-cli keeps it as a
    // normal output cap.)
    if let Some(v) = req.get("max_tokens").and_then(|x| x.as_i64()) {
        out["generationConfig"]["maxOutputTokens"] = json!(v);
    }

    out
}

// ---------------------------------------------------------------------------
// Response: native Gemini -> Anthropic Messages
// ---------------------------------------------------------------------------

/// Convert a non-streaming Gemini response to a non-streaming Anthropic message.
/// (Port of `ConvertGeminiResponseToClaudeNonStream`.)
pub fn gemini_to_claude_nonstream(gemini_resp: &[u8], maps: &ToolMaps) -> Vec<u8> {
    let root: Value = serde_json::from_slice(gemini_resp).unwrap_or_else(|_| json!({}));

    let usage = root.get("usageMetadata");
    let input_tokens = usage.and_then(|u| u.get("promptTokenCount")).and_then(|v| v.as_i64()).unwrap_or(0);
    let cand_tokens = usage.and_then(|u| u.get("candidatesTokenCount")).and_then(|v| v.as_i64()).unwrap_or(0);
    let thoughts_tokens = usage.and_then(|u| u.get("thoughtsTokenCount")).and_then(|v| v.as_i64()).unwrap_or(0);
    let output_tokens = cand_tokens.saturating_add(thoughts_tokens);

    let mut out = json!({
        "id": root.get("responseId").and_then(|v| v.as_str()).unwrap_or(""),
        "type": "message",
        "role": "assistant",
        "model": root.get("modelVersion").and_then(|v| v.as_str()).unwrap_or(""),
        "content": [],
        "stop_reason": Value::Null,
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
    });

    let mut content: Vec<Value> = Vec::new();
    let mut text_buf = String::new();
    let mut thinking_buf = String::new();
    let mut tool_id_counter: u64 = 0;
    let mut has_tool_call = false;

    if let Some(parts) = root
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    if part.get("thought").and_then(|b| b.as_bool()).unwrap_or(false) {
                        if !text_buf.is_empty() {
                            content.push(json!({ "type": "text", "text": text_buf }));
                            text_buf = String::new();
                        }
                        thinking_buf.push_str(text);
                    } else {
                        if !thinking_buf.is_empty() {
                            content.push(json!({ "type": "thinking", "thinking": thinking_buf }));
                            thinking_buf = String::new();
                        }
                        text_buf.push_str(text);
                    }
                    continue;
                }
            }
            if let Some(fc) = part.get("functionCall") {
                if !thinking_buf.is_empty() {
                    content.push(json!({ "type": "thinking", "thinking": thinking_buf }));
                    thinking_buf = String::new();
                }
                if !text_buf.is_empty() {
                    content.push(json!({ "type": "text", "text": text_buf }));
                    text_buf = String::new();
                }
                has_tool_call = true;
                let upstream = restore_sanitized_tool_name(maps, fc.get("name").and_then(|n| n.as_str()).unwrap_or(""));
                let client = map_tool_name(maps, &upstream);
                tool_id_counter += 1;
                let id = sanitize_claude_tool_id(&format!("{}-{}", upstream, tool_id_counter));
                let input = fc.get("args").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
                content.push(json!({ "type": "tool_use", "id": id, "name": client, "input": input }));
            }
        }
    }
    if !thinking_buf.is_empty() {
        content.push(json!({ "type": "thinking", "thinking": thinking_buf }));
    }
    if !text_buf.is_empty() {
        content.push(json!({ "type": "text", "text": text_buf }));
    }
    out["content"] = json!(content);

    let stop_reason = if has_tool_call {
        "tool_use"
    } else {
        match root
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("finishReason"))
            .and_then(|f| f.as_str())
        {
            Some("MAX_TOKENS") => "max_tokens",
            _ => "end_turn",
        }
    };
    out["stop_reason"] = json!(stop_reason);

    if input_tokens == 0 && output_tokens == 0 && usage.is_none() {
        if let Some(o) = out.as_object_mut() {
            o.remove("usage");
        }
    }

    serde_json::to_vec(&out).unwrap_or_default()
}

/// Render one Anthropic SSE event.
fn sse_event(event: &str, payload: &Value) -> String {
    format!("event: {}\ndata: {}\n\n", event, payload)
}

/// Streaming state machine: native-Gemini chunks in, Anthropic SSE events out.
/// (Port of `ConvertGeminiResponseToClaude`.)
///
/// `response_type`: 0=none, 1=text, 2=thinking, 3=function. Drives open/continue/
/// close of `content_block_*` events across chunks.
pub struct ClaudeStream {
    response_type: u8,
    response_index: i64,
    has_first_response: bool,
    has_content: bool,
    saw_tool_call: bool,
    maps: ToolMaps,
}

impl ClaudeStream {
    pub fn new(maps: ToolMaps) -> Self {
        ClaudeStream {
            response_type: 0,
            response_index: 0,
            has_first_response: false,
            has_content: false,
            saw_tool_call: false,
            maps,
        }
    }

    /// Feed one (already `.response`-unwrapped) native-Gemini SSE chunk; returns
    /// the Anthropic SSE events to forward.
    pub fn push(&mut self, gemini_chunk: &[u8]) -> Vec<String> {
        let chunk: Value = match serde_json::from_slice(gemini_chunk) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let mut events: Vec<String> = Vec::new();

        if !self.has_first_response {
            let mut msg = json!({
                "type": "message_start",
                "message": {
                    "id": "msg_1nZdL29xx5MUA1yADyHTEsnR8uuvGzszyY",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "claude-3-5-sonnet-20241022",
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 },
                }
            });
            if let Some(mv) = chunk.get("modelVersion").and_then(|v| v.as_str()) {
                msg["message"]["model"] = json!(mv);
            }
            if let Some(rid) = chunk.get("responseId").and_then(|v| v.as_str()) {
                msg["message"]["id"] = json!(rid);
            }
            events.push(sse_event("message_start", &msg));
            self.has_first_response = true;
        }

        if let Some(parts) = chunk
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    let is_thought = part.get("thought").and_then(|b| b.as_bool()).unwrap_or(false);
                    if is_thought {
                        self.emit_block(&mut events, 2, "thinking", "thinking_delta", "thinking", text);
                    } else {
                        self.emit_block(&mut events, 1, "text", "text_delta", "text", text);
                    }
                    continue;
                }

                if let Some(fc) = part.get("functionCall") {
                    self.saw_tool_call = true;
                    let upstream = restore_sanitized_tool_name(
                        &self.maps,
                        fc.get("name").and_then(|n| n.as_str()).unwrap_or(""),
                    );

                    // Streaming split: a continuation chunk has empty name while
                    // we're already mid tool-call — emit an args delta only.
                    if self.response_type == 3 && upstream.is_empty() {
                        if let Some(args) = fc.get("args") {
                            events.push(sse_event(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta",
                                    "index": self.response_index,
                                    "delta": { "type": "input_json_delta", "partial_json": args.to_string() },
                                }),
                            ));
                        }
                        continue;
                    }

                    let client = map_tool_name(&self.maps, &upstream);

                    // Close any open block (tool, text, or thinking) — exactly once.
                    if self.response_type == 3 {
                        events.push(self.stop_event());
                        self.response_index += 1;
                        self.response_type = 0;
                    }
                    if self.response_type != 0 {
                        events.push(self.stop_event());
                        self.response_index += 1;
                    }

                    let id = sanitize_claude_tool_id(&format!(
                        "{}-{}",
                        upstream,
                        TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
                    ));
                    events.push(sse_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": self.response_index,
                            "content_block": { "type": "tool_use", "id": id, "name": client, "input": {} },
                        }),
                    ));
                    if let Some(args) = fc.get("args") {
                        events.push(sse_event(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": self.response_index,
                                "delta": { "type": "input_json_delta", "partial_json": args.to_string() },
                            }),
                        ));
                    }
                    self.response_type = 3;
                    self.has_content = true;
                }
            }
        }

        // On the terminal chunk (finishReason + usage), close the block and emit
        // the final message_delta with stop_reason + token usage.
        let has_finish = chunk
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("finishReason"))
            .is_some();
        if let Some(usage) = chunk.get("usageMetadata") {
            if has_finish && self.has_content {
                if let Some(cand) = usage.get("candidatesTokenCount").and_then(|v| v.as_i64()) {
                    events.push(self.stop_event());
                    let thoughts = usage.get("thoughtsTokenCount").and_then(|v| v.as_i64()).unwrap_or(0);
                    let prompt = usage.get("promptTokenCount").and_then(|v| v.as_i64()).unwrap_or(0);
                    let stop_reason = if self.saw_tool_call {
                        "tool_use"
                    } else if chunk
                        .get("candidates")
                        .and_then(|c| c.get(0))
                        .and_then(|c0| c0.get("finishReason"))
                        .and_then(|f| f.as_str())
                        == Some("MAX_TOKENS")
                    {
                        "max_tokens"
                    } else {
                        "end_turn"
                    };
                    events.push(sse_event(
                        "message_delta",
                        &json!({
                            "type": "message_delta",
                            "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                            "usage": { "input_tokens": prompt, "output_tokens": cand.saturating_add(thoughts) },
                        }),
                    ));
                }
            }
        }

        events
    }

    /// Synthesized at upstream EOF (replaces CLIProxyAPI's `[DONE]` sentinel):
    /// emit `message_stop` whenever a message was started (`message_start` was
    /// sent), even if it produced no content. An empty or safety-blocked
    /// completion still needs a terminator, or Anthropic clients report the
    /// stream as truncated. (CLIProxyAPI gates this on content; we don't.)
    pub fn finish(&mut self) -> Vec<String> {
        if self.has_first_response {
            vec![sse_event("message_stop", &json!({ "type": "message_stop" }))]
        } else {
            Vec::new()
        }
    }

    fn stop_event(&self) -> String {
        sse_event(
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": self.response_index }),
        )
    }

    /// Open (or continue) a text/thinking block of the given `response_type`
    /// (`1`=text, `2`=thinking), closing any other open block first.
    fn emit_block(
        &mut self,
        events: &mut Vec<String>,
        kind: u8,
        block_type: &str,
        delta_type: &str,
        delta_key: &str,
        text: &str,
    ) {
        if self.response_type == kind {
            events.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.response_index,
                    "delta": { "type": delta_type, delta_key: text },
                }),
            ));
        } else {
            if self.response_type != 0 {
                events.push(self.stop_event());
                self.response_index += 1;
            }
            events.push(sse_event(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": self.response_index,
                    "content_block": { "type": block_type, delta_key: "" },
                }),
            ));
            events.push(sse_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.response_index,
                    "delta": { "type": delta_type, delta_key: text },
                }),
            ));
            self.response_type = kind;
        }
        self.has_content = true;
    }
}
