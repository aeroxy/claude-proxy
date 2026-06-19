//! OpenAI Chat Completions ↔ native-Gemini translation.
//!
//! Mirrors `anthropic_translate.rs` (Claude ↔ Gemini) but for the OpenAI Chat
//! Completions envelope. Feeds the same downstream pipeline —
//! [`super::translate::gemini_to_gemini_cli`] / `gemini_to_antigravity` — so the
//! only surface-specific work is the OpenAI↔native-Gemini boundary.
//!
//! Request:  `openai_to_gemini`         (OpenAI `messages` → Gemini `contents`)
//! Response: `gemini_to_openai_nonstream` (Gemini `candidates` → OpenAI `choices`)
//! Stream:   `OpenAIStream`             (Gemini SSE chunks → OpenAI SSE chunks)

use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A stable-ish chat completion id derived from the Gemini `responseId` if
/// present, else a synthesized `chatcmpl-<ts>`.
fn chat_id(gemini_resp: &Value) -> String {
    gemini_resp
        .get("responseId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("chatcmpl-{}", now_secs()))
}

/// Map a Gemini `finishReason` to an OpenAI `finish_reason`.
fn map_finish_reason(gemini_reason: &str) -> &'static str {
    match gemini_reason {
        "STOP" => "stop",
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" => "content_filter",
        _ => "stop",
    }
}

/// Translate an OpenAI Chat Completions request body into a native-Gemini
/// request body. The result is fed straight into
/// [`super::translate::gemini_to_gemini_cli`] / `gemini_to_antigravity`, whose
/// `build_envelope` already runs role normalization and default safety — so we
/// keep role mapping minimal here (user/model/function) and let the envelope
/// layer finalize.
pub fn openai_to_gemini(req: &Value) -> Value {
    let mut out = json!({ "contents": [] });

    // System prompt: OpenAI encodes it as a message with role=system (or a
    // top-level `system` string, in some Responses-style payloads sent to the
    // chat endpoint). Collect all system messages into one systemInstruction.
    let messages = req.get("messages").and_then(|m| m.as_array());
    let mut system_parts: Vec<Value> = Vec::new();
    if let Some(messages) = messages {
        for msg in messages {
            if msg.get("role").and_then(|r| r.as_str()) != Some("system") {
                continue;
            }
            match msg.get("content") {
                Some(Value::String(s)) => {
                    if !s.is_empty() {
                        system_parts.push(json!({ "text": s }));
                    }
                }
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                system_parts.push(json!({ "text": t }));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(s) = req.get("system").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            system_parts.push(json!({ "text": s }));
        }
    }
    if !system_parts.is_empty() {
        out["system_instruction"] = json!({ "parts": system_parts });
    }

    // Build a tool_call_id → function_name map from prior assistant messages so
    // we can name `role=tool` messages (OpenAI gives only the id, not the name).
    let mut id_to_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(messages) = messages {
        for msg in messages {
            if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
                continue;
            }
            if let Some(calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                for c in calls {
                    let id = c.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                    let name = c
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !id.is_empty() && !name.is_empty() {
                        id_to_name.entry(id).or_insert(name);
                    }
                }
            }
        }
    }

    // messages -> contents (skip system messages, already captured above)
    if let Some(messages) = messages {
        let mut contents: Vec<Value> = Vec::new();
        for msg in messages {
            let role0 = match msg.get("role").and_then(|r| r.as_str()) {
                Some(r) => r,
                None => continue,
            };
            if role0 == "system" {
                continue;
            }
            let role = match role0 {
                "assistant" => "model",
                "tool" => "function",
                "user" => "user",
                other => other, // developer/function — pass through, envelope normalizes
            };

            let mut parts: Vec<Value> = Vec::new();

            // Text / multimodal content
            match msg.get("content") {
                Some(Value::String(s)) => {
                    if !s.is_empty() {
                        parts.push(json!({ "text": s }));
                    }
                }
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        match b.get("type").and_then(|t| t.as_str()).unwrap_or("text") {
                            "text" => {
                                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                    if !t.is_empty() {
                                        parts.push(json!({ "text": t }));
                                    }
                                }
                            }
                            "image_url" => {
                                // Pass through inline data URLs; drop remote URLs
                                // (would need fetching). Format:
                                // data:<mime>;base64,<payload>
                                if let Some(url) = b
                                    .get("image_url")
                                    .and_then(|iu| iu.get("url"))
                                    .and_then(|u| u.as_str())
                                {
                                    if let Some((mime, data)) = parse_data_url(url) {
                                        parts.push(json!({
                                            "inlineData": { "mimeType": mime, "data": data }
                                        }));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }

            // Assistant tool_calls -> functionCall parts
            if role0 == "assistant" {
                if let Some(calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for c in calls {
                        let id = c.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                        let fname = c
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args_str = c
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .unwrap_or("{}");
                        let args: Value = serde_json::from_str(args_str).unwrap_or_else(|_| json!({}));
                        let mut fc = json!({ "name": fname, "args": args });
                        if !id.is_empty() {
                            fc["id"] = json!(id);
                        }
                        parts.push(json!({ "functionCall": fc }));
                    }
                }
            }

            // role=tool result -> functionResponse part
            if role0 == "tool" {
                let tcid = msg.get("tool_call_id").and_then(|i| i.as_str()).unwrap_or("");
                let fname = id_to_name.get(tcid).cloned().unwrap_or_else(|| tcid.to_string());
                let content_val = match msg.get("content") {
                    Some(Value::String(s)) => {
                        match serde_json::from_str::<Value>(s) {
                            Ok(Value::Object(_)) => serde_json::from_str(s).unwrap(),
                            Ok(parsed) => json!({ "result": parsed }),
                            Err(_) => json!({ "result": s }),
                        }
                    }
                    Some(v) if v.is_object() => v.clone(),
                    Some(v) => json!({ "result": v }),
                    None => json!({}),
                };
                parts.push(json!({ "functionResponse": { "name": fname, "response": content_val } }));
            }

            if !parts.is_empty() {
                contents.push(json!({ "role": role, "parts": parts }));
            }
        }
        out["contents"] = json!(contents);
    }

    // tools -> tools.functionDeclarations
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let mut decls: Vec<Value> = Vec::new();
        for t in tools {
            // OpenAI shape: { "type":"function", "function":{ name, description, parameters } }
            let f = t.get("function").unwrap_or(t);
            let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let mut decl = json!({
                "name": name,
            });
            if let Some(desc) = f.get("description").and_then(|d| d.as_str()) {
                decl["description"] = json!(desc);
            }
            if let Some(params) = f.get("parameters") {
                if params.is_object() {
                    decl["parameters"] = params.clone();
                }
            }
            decls.push(decl);
        }
        if !decls.is_empty() {
            out["tools"] = json!({ "functionDeclarations": decls });
        }
    }

    // tool_choice -> toolConfig.functionCallingConfig
    if let Some(tc) = req.get("tool_choice") {
        let (mode, allowed) = match tc {
            Value::String(s) => match s.as_str() {
                "none" => ("NONE", vec![]),
                "required" => ("ANY", vec![]),
                _ => ("AUTO", vec![]),
            },
            Value::Object(o) => {
                // { "type":"function", "function":{ "name":"..." } }
                let name = o
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                ("ANY", if name.is_empty() { vec![] } else { vec![name.to_string()] })
            }
            _ => ("AUTO", vec![]),
        };
        let mut fcc = json!({ "mode": mode });
        if !allowed.is_empty() {
            fcc["allowedFunctionNames"] = json!(allowed);
        }
        out["toolConfig"] = json!({ "functionCallingConfig": fcc });
    }

    // generation params
    if let Some(gc) = build_generation_config(req) {
        out["generationConfig"] = gc;
    }

    out
}

fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (head, data) = rest.split_once(",")?;
    let mime = head.split(';').next().unwrap_or("image/png");
    if mime.is_empty() {
        return None;
    }
    Some((mime, data))
}

fn build_generation_config(req: &Value) -> Option<Value> {
    let mut gc = json!({});
    let mut any = false;
    if let Some(mt) = req.get("max_tokens").and_then(|v| v.as_u64()) {
        gc["maxOutputTokens"] = json!(mt);
        any = true;
    }
    if let Some(t) = req.get("temperature").and_then(|v| v.as_f64()) {
        gc["temperature"] = json!(t);
        any = true;
    }
    if let Some(tp) = req.get("top_p").and_then(|v| v.as_f64()) {
        gc["topP"] = json!(tp);
        any = true;
    }
    if let Some(stop) = req.get("stop") {
        match stop {
            Value::String(s) => {
                gc["stopSequences"] = json!([s]);
                any = true;
            }
            Value::Array(arr) if !arr.is_empty() => {
                let seqs: Vec<String> = arr
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
                if !seqs.is_empty() {
                    gc["stopSequences"] = json!(seqs);
                    any = true;
                }
            }
            _ => {}
        }
    }
    if any {
        Some(gc)
    } else {
        None
    }
}

/// Convert a non-streaming Gemini response to an OpenAI Chat Completions
/// response. `model_echo` is the original requested model string (for the
/// `model` field, which clients expect to see echoed back).
pub fn gemini_to_openai_nonstream(gemini_resp: &[u8], model_echo: &str) -> Vec<u8> {
    let root: Value = serde_json::from_slice(gemini_resp).unwrap_or_else(|_| json!({}));

    let usage = root.get("usageMetadata");
    let prompt_tokens = usage
        .and_then(|u| u.get("promptTokenCount"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("candidatesTokenCount"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        + usage
            .and_then(|u| u.get("thoughtsTokenCount"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

    let mut text_buf = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_idx: u64 = 0;
    let mut finish = "stop";

    if let Some(parts) = root
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                // Drop `thought: true` parts — standard OpenAI Chat Completions
                // has no thinking channel. (Reasoning models expose
                // `reasoning_content`; not emulated here.)
                if part.get("thought").and_then(|b| b.as_bool()).unwrap_or(false) {
                    continue;
                }
                if !text.is_empty() {
                    text_buf.push_str(text);
                }
                continue;
            }
            if let Some(fc) = part.get("functionCall") {
                let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                let id = fc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("call_{}", tool_idx));
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args_str },
                }));
                tool_idx += 1;
            }
        }
    }

    if let Some(reason) = root
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c0| c0.get("finishReason"))
        .and_then(|r| r.as_str())
    {
        finish = map_finish_reason(reason);
    }
    if !tool_calls.is_empty() && finish == "stop" {
        finish = "tool_calls";
    }

    let mut message = json!({ "role": "assistant" });
    if !text_buf.is_empty() {
        message["content"] = json!(text_buf);
    } else {
        message["content"] = Value::Null;
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    let out = json!({
        "id": chat_id(&root),
        "object": "chat.completion",
        "created": now_secs(),
        "model": model_echo,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish,
            "logprobs": Value::Null,
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    });

    serde_json::to_vec(&out).unwrap_or_default()
}

/// Streaming state machine: native-Gemini chunks in, OpenAI SSE chunks out.
/// Mirrors `ClaudeStream` but emits `chat.completion.chunk` objects.
///
/// `response_type`: 0=none, 1=text, 2=tool_call. Tracks whether the first
/// `role:assistant` delta has been sent and whether the upstream has emitted
/// any content, so we always close cleanly at EOF.
pub struct OpenAIStream {
    id: String,
    model: String,
    created: i64,
    sent_role: bool,
    /// OpenAI tool_call index is per-stream-position; we emit at most one tool
    /// call per part, indexed sequentially.
    tool_index: u64,
    finish_emitted: bool,
}

impl OpenAIStream {
    pub fn new(model: String) -> Self {
        Self {
            id: format!("chatcmpl-{}", now_secs()),
            model,
            created: now_secs(),
            sent_role: false,
            tool_index: 0,
            finish_emitted: false,
        }
    }

    fn base(&self) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
        })
    }

    fn first_role_delta(&mut self) -> Option<String> {
        if self.sent_role {
            return None;
        }
        self.sent_role = true;
        let mut v = self.base();
        v["choices"] = json!([{
            "index": 0,
            "delta": { "role": "assistant" },
            "finish_reason": Value::Null,
        }]);
        Some(format!("data: {}\n\n", v))
    }

    fn text_delta(&self, text: &str) -> String {
        let mut v = self.base();
        v["choices"] = json!([{
            "index": 0,
            "delta": { "content": text },
            "finish_reason": Value::Null,
        }]);
        format!("data: {}\n\n", v)
    }

    fn tool_call_delta(&mut self, name: &str, args_str: &str, id: &str) -> String {
        let idx = self.tool_index;
        self.tool_index += 1;
        let call_id = if id.is_empty() {
            format!("call_{}", idx)
        } else {
            id.to_string()
        };
        let mut v = self.base();
        v["choices"] = json!([{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": idx,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": args_str },
                }]
            },
            "finish_reason": Value::Null,
        }]);
        format!("data: {}\n\n", v)
    }

    fn finish_delta(&mut self, reason: &str) -> String {
        if self.finish_emitted {
            return String::new();
        }
        self.finish_emitted = true;
        let mut v = self.base();
        v["choices"] = json!([{
            "index": 0,
            "delta": {},
            "finish_reason": reason,
        }]);
        format!("data: {}\n\n", v)
    }

    /// Feed one (already `.response`-unwrapped) native-Gemini SSE chunk; returns
    /// the OpenAI SSE events to forward.
    pub fn push(&mut self, gemini_chunk: &[u8]) -> Vec<String> {
        let root: Value = match serde_json::from_slice(gemini_chunk) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let mut out: Vec<String> = Vec::new();
        if let Some(role) = self.first_role_delta() {
            out.push(role);
        }

        if let Some(parts) = root
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if part.get("thought").and_then(|b| b.as_bool()).unwrap_or(false) {
                        continue;
                    }
                    if !text.is_empty() {
                        out.push(self.text_delta(text));
                    }
                    continue;
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    let args_str = serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string());
                    let id = fc.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    out.push(self.tool_call_delta(name, &args_str, id));
                }
            }
        }

        // If this chunk carries a finishReason and we have no more content, emit
        // the finish delta now. (Gemini typically sends finishReason on the
        // final chunk, so this lines up with EOF.)
        if let Some(reason) = root
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("finishReason"))
            .and_then(|r| r.as_str())
        {
            let mapped = if self.tool_index > 0 && reason == "STOP" {
                "tool_calls"
            } else {
                map_finish_reason(reason)
            };
            let fin = self.finish_delta(mapped);
            if !fin.is_empty() {
                out.push(fin);
            }
        }

        out
    }

    /// Synthesized at upstream EOF. If no finishReason was seen on the wire,
    // emit a `stop` so the client sees a well-formed termination.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(role) = self.first_role_delta() {
            out.push(role);
        }
        let fin = self.finish_delta("stop");
        if !fin.is_empty() {
            out.push(fin);
        }
        out.push("data: [DONE]\n\n".to_string());
        out
    }
}
