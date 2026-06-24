// The `compress` subsystem is a port-in-progress. Many staged
// features are scaffolded but not yet wired into the live `crush_array`
// path (relevance scoring, builder, constraints, observer, planning,
// alternative formatters, document walker, etc.). Until they are
// activated, the `dead_code` lint would surface dozens of false
// positives on items that are intentionally staged. Allow at the
// parent module rather than scattering per-file annotations.
#![allow(dead_code)]

pub mod smart_crusher;
pub mod relevance;
pub mod anchor_selector;
pub mod adaptive_sizer;
pub mod config;

use std::cell::RefCell;

use hyper::body::Bytes;
use serde_json::Value;
use tracing::info;

pub use self::config::{CompressConfig, CompressProviderConfig};
use self::smart_crusher::{SmartCrusher, SmartCrusherConfig};

// Per-thread SmartCrusher instances avoid synchronization overhead (e.g. Mutex).
// The async wrappers (`maybe_apply_async`, `compress_gemini_body_async`)
// offload to `spawn_blocking`, so each blocking-thread that handles
// compression lazily creates its own instance. The number of live instances
// is bounded by the number of distinct blocking threads that do compression
// work, not the total blocking pool size (idle threads never initialize it).
thread_local! {
    static SHARED_CRUSHER: RefCell<SmartCrusher> = RefCell::new(
        SmartCrusher::new(SmartCrusherConfig::default())
    );
}

/// Apply compression if any providers are configured. Short-circuits
/// immediately when the config is empty — avoids JSON parsing overhead
/// on every request when compression isn't in use.
pub fn maybe_apply(
    body: Bytes,
    config: &CompressConfig,
) -> Bytes {
    if config.providers.is_empty() {
        return body;
    }
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    let Some(provider) = resolve_provider_from_value(&parsed) else {
        return body;
    };
    apply_parsed(body, parsed, &provider, config)
}

/// Async wrapper around [`maybe_apply`] that offloads the CPU-bound
/// parse/compress work (JSON parse, BM25, SimHash, zlib) to the blocking
/// thread pool. Use this from async request handlers to avoid head-of-line
/// blocking on large bodies.
pub async fn maybe_apply_async(body: Bytes, config: CompressConfig) -> Bytes {
    let original_body = body.clone();
    match tokio::task::spawn_blocking(move || maybe_apply(body, &config)).await {
        Ok(res) => res,
        Err(_) => original_body,
    }
}

/// Apply compression to a request body based on the provider's config.
/// Returns the original body unchanged if the provider has no compression
/// config or if parsing/compression fails.
pub fn apply(
    body: Bytes,
    provider_name: &str,
    config: &CompressConfig,
) -> Bytes {
    if config.providers.is_empty() {
        return body;
    }
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    apply_parsed(body, parsed, provider_name, config)
}

fn apply_parsed(
    body: Bytes,
    mut parsed: Value,
    provider_name: &str,
    config: &CompressConfig,
) -> Bytes {
    let Some(provider_cfg) = config.providers.get(provider_name) else {
        return body;
    };

    let mut modified = false;

    if provider_cfg.smart_crusher.unwrap_or(false) {
        let query_context = extract_query_context(&parsed);
        modified |= compress_tool_results(&mut parsed, provider_cfg, &query_context);
    }

    if provider_cfg.max_tool_chars.unwrap_or(0) > 0 {
        modified |= truncate_tool_results(&mut parsed, provider_cfg.max_tool_chars.unwrap());
    }

    if modified {
        match serde_json::to_vec(&parsed) {
            Ok(bytes) => {
                info!(
                    provider = provider_name,
                    before = body.len(),
                    after = bytes.len(),
                    "compressed request body"
                );
                Bytes::from(bytes)
            }
            Err(_) => body,
        }
    } else {
        body
    }
}

pub fn resolve_provider_from_value(parsed: &Value) -> Option<String> {
    let model = parsed.get("model")?.as_str()?;
    let (head, rest) = model.split_once('/')?;
    if rest.is_empty() {
        return None;
    }
    Some(head.to_string())
}

/// Resolve the downstream provider name from a request body's `model` field.
/// Returns the first `/`-segment of the model (e.g. `gemini-cli` from
/// `gemini-cli/gemini-2.5-pro`, or `opengateway` from `opengateway/minimax/m3`).
#[allow(dead_code)]
pub fn resolve_provider(body: &[u8]) -> Option<String> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    resolve_provider_from_value(&parsed)
}

/// Extract the model provider from a Gemini path if possible.
/// e.g. `/v1beta/models/gemini-cli/gemini-2.5-pro:generateContent` -> `gemini-cli`
pub fn gemini_provider_from_path(path: &str) -> Option<String> {
    let path_only = path.split('?').next().unwrap_or(path);
    let rest = path_only.strip_prefix("/v1beta/")?;
    let spec = rest.strip_prefix("models/").unwrap_or(rest);
    let model = match spec.split_once(':') {
        Some((m, _)) => m,
        None => spec,
    };
    let (head, rest_model) = model.split_once('/')?;
    if rest_model.is_empty() {
        return None;
    }
    Some(head.to_string())
}

/// Returns `Some("vertex")` for Vertex AI Anthropic paths
/// (e.g. `/v1/projects/.../publishers/anthropic/models/...:streamRawPredict`).
/// The provider name matches the `[compress.providers.vertex]` config key.
pub fn vertex_provider_from_path(path: &str) -> Option<String> {
    let path_only = path.split('?').next().unwrap_or(path);
    if path_only.contains("/publishers/anthropic/models/") {
        Some("vertex".to_string())
    } else {
        None
    }
}

/// Walk messages looking for JSON array tool results and run SmartCrusher on them.
fn compress_tool_results(
    parsed: &mut Value,
    provider_cfg: &CompressProviderConfig,
    query_context: &str,
) -> bool {
    let bias = provider_cfg.bias.unwrap_or(1.0);
    let mut modified = false;

    let messages = match parsed.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => {
            // Gemini format uses "contents" instead of "messages"
            match parsed.get_mut("contents").and_then(|c| c.as_array_mut()) {
                Some(c) => {
                    modified |= compress_gemini_contents(c, query_context, bias);
                    return modified;
                }
                None => return false,
            }
        }
    };

    for message in messages.iter_mut() {
        // OpenAI format: role == "tool", content is a string
        if message.get("role").and_then(|r| r.as_str()) == Some("tool") {
            if let Some(content) = message.get_mut("content").and_then(|c| c.as_str()) {
                if let Some(compressed) = try_compress_json_array_str(content, query_context, bias)
                {
                    message["content"] = Value::String(compressed);
                    modified = true;
                }
            }
        // Anthropic format: content is an array of blocks, look for tool_result
        } else if let Some(content_blocks) = message.get_mut("content").and_then(|c| c.as_array_mut()) {
            for block in content_blocks.iter_mut() {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                    continue;
                }
                // tool_result content can be a string or array of sub-blocks
                if let Some(content_str) = block.get_mut("content").and_then(|c| c.as_str()) {
                    if let Some(compressed) =
                        try_compress_json_array_str(content_str, query_context, bias)
                    {
                        block["content"] = Value::String(compressed);
                        modified = true;
                    }
                } else if let Some(sub_blocks) =
                    block.get_mut("content").and_then(|c| c.as_array_mut())
                {
                    for sub in sub_blocks.iter_mut() {
                        if sub.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = sub.get_mut("text").and_then(|t| t.as_str()) {
                                if let Some(compressed) =
                                    try_compress_json_array_str(text, query_context, bias)
                                {
                                    sub["text"] = Value::String(compressed);
                                    modified = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    modified
}

fn compress_gemini_contents(contents: &mut [Value], query_context: &str, bias: f64) -> bool {
    let mut modified = false;
    for content in contents.iter_mut() {
        if let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) {
            for part in parts.iter_mut() {
                if let Some(resp) = part.get_mut("functionResponse") {
                    if let Some(response) = resp.get_mut("response") {
                        modified |=
                            compress_json_array_value(response, query_context, bias, 0);
                    }
                }
            }
        }
    }
    modified
}

/// Maximum recursion depth for `compress_json_array_value` and
/// `truncate_value_in_place`. Prevents stack overflow on pathologically
/// deeply nested JSON. Generous enough for any realistic tool output
/// (the SmartCrusher's own `process_value` uses 50; this is the
/// orchestration-layer cap that runs on every request body).
const MAX_RECURSION_DEPTH: usize = 64;

/// Recursively walk `val` and run the SmartCrusher on any JSON array
/// encountered at any depth. Descends into arrays and object values so
/// deeply-nested crushable arrays aren't silently skipped. Returns true if
/// any compression was applied.
///
/// `depth` tracks the current nesting level to prevent unbounded
/// recursion on pathologically deep payloads.
fn compress_json_array_value(
    val: &mut Value,
    query_context: &str,
    bias: f64,
    depth: usize,
) -> bool {
    if depth >= MAX_RECURSION_DEPTH {
        return false;
    }
    match val {
        Value::String(s) => {
            if let Some(compressed) = try_compress_json_array_str_inner(s, query_context, bias, depth + 1) {
                *val = Value::String(compressed);
                true
            } else {
                false
            }
        }
        Value::Array(arr) => {
            let mut modified = false;
            // Try to crush this array as a whole first.
            if let Some(compressed) = try_compress_json_array(arr, query_context, bias) {
                *val = compressed;
                // Recurse into kept items so nested arrays within them
                // still get compressed.
                if let Value::Array(ref mut kept_items) = val {
                    for item in kept_items.iter_mut() {
                        modified |= compress_json_array_value(
                            item,
                            query_context,
                            bias,
                            depth + 1,
                        );
                    }
                }
                return true;
            }
            // Not crushable — recurse into each element so nested arrays
            // deeper in the tree still get processed.
            for v in arr.iter_mut() {
                modified |= compress_json_array_value(v, query_context, bias, depth + 1);
            }
            modified
        }
        Value::Object(obj) => {
            let mut modified = false;
            for (_k, v) in obj.iter_mut() {
                modified |= compress_json_array_value(v, query_context, bias, depth + 1);
            }
            modified
        }
        _ => false,
    }
}

fn try_compress_json_array(arr: &[Value], query_context: &str, bias: f64) -> Option<Value> {
    let result = crush_array_value(arr, query_context, bias)?;

    if let Some(rendered) = result.compacted {
        return Some(Value::String(rendered));
    }

    if result.items.len() >= arr.len() {
        return None;
    }

    Some(Value::Array(result.items))
}

fn try_compress_json_array_str(s: &str, query_context: &str, bias: f64) -> Option<String> {
    try_compress_json_array_str_inner(s, query_context, bias, 0)
}

fn try_compress_json_array_str_inner(s: &str, query_context: &str, bias: f64, depth: usize) -> Option<String> {
    if depth >= MAX_RECURSION_DEPTH {
        return None;
    }
    let parsed: Value = serde_json::from_str(s).ok()?;
    let arr = parsed.as_array()?;
    let result = crush_array_value(arr, query_context, bias)?;

    if let Some(rendered) = result.compacted {
        return Some(rendered);
    }

    let mut items = result.items;
    let mut nested_modified = false;
    for item in items.iter_mut() {
        nested_modified |= compress_json_array_value(item, query_context, bias, depth + 1);
    }

    if items.len() >= arr.len() && !nested_modified {
        return None;
    }

    serde_json::to_string(&items).ok()
}

/// Shared crush logic: guards + `crush_array` call. Returns `None` if
/// the array isn't crushable (< 5 items or no objects).
fn crush_array_value(
    arr: &[Value],
    query_context: &str,
    bias: f64,
) -> Option<smart_crusher::CrushArrayResult> {
    if arr.len() < 5 {
        return None;
    }
    if !arr.iter().any(|v| v.is_object()) {
        return None;
    }

    Some(SHARED_CRUSHER.with(|c| c.borrow().crush_array(arr, query_context, bias)))
}

/// Extract query context from the request body's messages/contents.
/// Joins the last 2 user messages into a single string for relevance
/// scoring during compression.
fn extract_query_context(parsed: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(messages) = parsed.get("messages").and_then(|m| m.as_array()) {
        for msg in messages.iter().rev() {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "user" {
                continue;
            }
            if let Some(text) = msg.get("content").and_then(|c| c.as_str()) {
                parts.push(text.to_string());
            } else if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            parts.push(text.to_string());
                        }
                    }
                }
            }
            if parts.len() >= 2 {
                break;
            }
        }
    } else if let Some(contents) = parsed.get("contents").and_then(|c| c.as_array()) {
        for content in contents.iter().rev() {
            let role = content.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role != "user" {
                continue;
            }
            if let Some(parts_arr) = content.get("parts").and_then(|p| p.as_array()) {
                for part in parts_arr {
                    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                        parts.push(text.to_string());
                    }
                }
            }
            if parts.len() >= 2 {
                break;
            }
        }
    }

    parts.reverse();
    parts.join("\n")
}

/// Truncate tool result content that exceeds max_chars.
/// Uses head 80% + tail 20% extraction with an elision marker.
fn truncate_tool_results(parsed: &mut Value, max_chars: usize) -> bool {
    let mut modified = false;

    let messages = match parsed.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => {
            if let Some(contents) = parsed.get_mut("contents").and_then(|c| c.as_array_mut()) {
                modified |= truncate_gemini_contents(contents, max_chars);
            }
            return modified;
        }
    };

    for message in messages.iter_mut() {
        // OpenAI tool messages
        if message.get("role").and_then(|r| r.as_str()) == Some("tool") {
            if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
                if content.chars().count() > max_chars {
                    let truncated = head_tail_truncate(content, max_chars);
                    message["content"] = Value::String(truncated);
                    modified = true;
                }
            }
        }

        // Anthropic tool_result blocks
        if let Some(content_blocks) = message.get_mut("content").and_then(|c| c.as_array_mut()) {
            for block in content_blocks.iter_mut() {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                    continue;
                }
                if let Some(content_str) = block.get_mut("content").and_then(|c| c.as_str()) {
                    if content_str.chars().count() > max_chars {
                        let truncated = head_tail_truncate(content_str, max_chars);
                        block["content"] = Value::String(truncated);
                        modified = true;
                    }
                } else if let Some(sub_blocks) = block.get_mut("content").and_then(|c| c.as_array_mut()) {
                    for sub in sub_blocks.iter_mut() {
                        if sub.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = sub.get_mut("text").and_then(|t| t.as_str()) {
                                if text.chars().count() > max_chars {
                                    sub["text"] = Value::String(head_tail_truncate(text, max_chars));
                                    modified = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    modified
}

fn truncate_gemini_contents(contents: &mut [Value], max_chars: usize) -> bool {
    let mut modified = false;
    for content in contents.iter_mut() {
        if let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) {
            for part in parts.iter_mut() {
                if let Some(resp) = part.get_mut("functionResponse") {
                    if let Some(response) = resp.get_mut("response") {
                        modified |= truncate_value_in_place(response, max_chars, 0);
                    }
                }
            }
        }
    }
    modified
}

/// Recursively walk `val`, truncating every string leaf that exceeds
/// `max_chars`. Descends into arrays and object values so deeply nested
/// strings aren't silently skipped.
///
/// `depth` tracks the current nesting level to prevent unbounded
/// recursion on pathologically deep payloads.
fn truncate_value_in_place(val: &mut Value, max_chars: usize, depth: usize) -> bool {
    if depth >= MAX_RECURSION_DEPTH {
        return false;
    }
    match val {
        Value::String(s) => {
            if s.chars().count() > max_chars {
                *val = Value::String(head_tail_truncate(s, max_chars));
                true
            } else {
                false
            }
        }
        Value::Array(arr) => {
            let mut modified = false;
            for v in arr.iter_mut() {
                modified |= truncate_value_in_place(v, max_chars, depth + 1);
            }
            modified
        }
        Value::Object(obj) => {
            let mut modified = false;
            for (_k, v) in obj.iter_mut() {
                modified |= truncate_value_in_place(v, max_chars, depth + 1);
            }
            modified
        }
        _ => false,
    }
}

fn head_tail_truncate(text: &str, max_chars: usize) -> String {
    let char_len = text.chars().count();
    if char_len <= max_chars {
        return text.to_string();
    }

    // Estimate marker size (e.g., "\n\n[... X chars truncated ...]\n\n" is up to 50 chars)
    let marker_len_approx = 50;
    let budget = max_chars.saturating_sub(marker_len_approx);

    let head_chars = ((budget as f64 * 0.8) as usize).min(char_len);
    // Clamp tail to whatever remains after head so the two slices never
    // overlap. Without this, a budget close to char_len makes tail_chars
    // exceed (char_len - head_chars), char_indices().rev().nth() falls off
    // the front, and the "tail" silently becomes the entire input.
    let tail_chars = budget.saturating_sub(head_chars).min(char_len.saturating_sub(head_chars));

    // Convert character indices to byte indices efficiently
    let head_end_byte = text.char_indices()
        .nth(head_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());

    let tail_start_byte = if tail_chars == 0 {
        text.len()
    } else {
        text.char_indices()
            .rev()
            .nth(tail_chars - 1)
            .map(|(idx, _)| idx)
            .unwrap_or(0)
    };

    let head = &text[..head_end_byte];
    let tail = &text[tail_start_byte..];

    let elided = char_len.saturating_sub(head_chars + tail_chars);
    let result = format!("{head}\n\n[... {elided} chars truncated ...]\n\n{tail}");

    // Final safety clip to ensure it NEVER exceeds max_chars under any circumstance
    let result_chars = result.chars().count();
    if result_chars > max_chars {
        result.chars().take(max_chars).collect()
    } else {
        result
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn provider_config(max_tool_chars: Option<usize>, smart_crusher: Option<bool>) -> CompressProviderConfig {
        CompressProviderConfig { max_tool_chars, smart_crusher, bias: None }
    }

    fn config_with_provider(name: &str, cfg: CompressProviderConfig) -> CompressConfig {
        let mut providers = HashMap::new();
        providers.insert(name.to_string(), cfg);
        CompressConfig { providers }
    }

    // ── resolve_provider ─────────────────────────────────────

    #[test]
    fn resolve_provider_extracts_head_segment() {
        let body = serde_json::to_vec(&json!({"model": "gemini-cli/gemini-2.5-pro"})).unwrap();
        assert_eq!(resolve_provider(&body), Some("gemini-cli".to_string()));
    }

    #[test]
    fn resolve_provider_handles_nested_model() {
        let body = serde_json::to_vec(&json!({"model": "opengateway/minimax/minimax-m3"})).unwrap();
        assert_eq!(resolve_provider(&body), Some("opengateway".to_string()));
    }

    #[test]
    fn resolve_provider_returns_none_without_slash() {
        let body = serde_json::to_vec(&json!({"model": "gpt-4"})).unwrap();
        assert_eq!(resolve_provider(&body), None);
    }

    #[test]
    fn resolve_provider_returns_none_for_empty_rest() {
        let body = serde_json::to_vec(&json!({"model": "gemini-cli/"})).unwrap();
        assert_eq!(resolve_provider(&body), None);
    }

    #[test]
    fn resolve_provider_returns_none_for_invalid_json() {
        assert_eq!(resolve_provider(b"not json"), None);
    }

    #[test]
    fn resolve_provider_returns_none_for_missing_model() {
        let body = serde_json::to_vec(&json!({"messages": []})).unwrap();
        assert_eq!(resolve_provider(&body), None);
    }

    // ── maybe_apply ──────────────────────────────────────────

    #[test]
    fn maybe_apply_passthrough_when_no_providers() {
        let config = CompressConfig { providers: HashMap::new() };
        let body = Bytes::from(r#"{"model":"gemini-cli/test"}"#);
        let result = maybe_apply(body.clone(), &config);
        assert_eq!(result, body);
    }

    #[test]
    fn maybe_apply_passthrough_when_provider_not_configured() {
        let config = config_with_provider("other-provider", provider_config(Some(1000), None));
        let body = Bytes::from(r#"{"model":"gemini-cli/test","messages":[]}"#);
        let result = maybe_apply(body.clone(), &config);
        assert_eq!(result, body);
    }

    // ── apply ────────────────────────────────────────────────

    #[test]
    fn apply_passthrough_on_malformed_json() {
        let config = config_with_provider("test", provider_config(Some(1000), Some(true)));
        let body = Bytes::from(b"not json".to_vec());
        let result = apply(body.clone(), "test", &config);
        assert_eq!(result, body);
    }

    #[test]
    fn apply_passthrough_when_no_features_enabled() {
        let config = config_with_provider("test", provider_config(None, None));
        let body = Bytes::from(r#"{"model":"test/m","messages":[]}"#);
        let result = apply(body.clone(), "test", &config);
        assert_eq!(result, body);
    }

    // ── truncate_tool_results ────────────────────────────────

    #[test]
    fn truncate_openai_tool_message() {
        let mut parsed = json!({
            "model": "test/m",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "tool", "content": "x".repeat(10000)}
            ]
        });
        let modified = truncate_tool_results(&mut parsed, 500);
        assert!(modified);
        let content = parsed["messages"][1]["content"].as_str().unwrap();
        assert!(content.len() < 10000);
        assert!(content.contains("truncated"));
    }

    #[test]
    fn truncate_skips_short_content() {
        let mut parsed = json!({
            "model": "test/m",
            "messages": [
                {"role": "tool", "content": "short"}
            ]
        });
        let modified = truncate_tool_results(&mut parsed, 500);
        assert!(!modified);
    }

    #[test]
    fn truncate_anthropic_tool_result() {
        let long_content = "y".repeat(10000);
        let mut parsed = json!({
            "model": "test/m",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "content": long_content
                        }
                    ]
                }
            ]
        });
        let modified = truncate_tool_results(&mut parsed, 500);
        assert!(modified);
        let content = parsed["messages"][0]["content"][0]["content"].as_str().unwrap();
        assert!(content.len() < 10000);
        assert!(content.contains("truncated"));
    }

    #[test]
    fn truncate_gemini_function_response() {
        let long_content = "z".repeat(10000);
        let mut parsed = json!({
            "model": "test/m",
            "contents": [
                {
                    "parts": [
                        {
                            "functionResponse": {
                                "response": long_content
                            }
                        }
                    ]
                }
            ]
        });
        let modified = truncate_tool_results(&mut parsed, 500);
        assert!(modified);
        let content = parsed["contents"][0]["parts"][0]["functionResponse"]["response"].as_str().unwrap();
        assert!(content.len() < 10000);
    }

    #[test]
    fn truncate_gemini_function_response_deeply_nested() {
        // Regression: the previous implementation only truncated top-level
        // strings and first-level object values. Strings inside nested
        // arrays or objects deeper than one level were silently skipped.
        let long_content = "q".repeat(10000);
        let mut parsed = json!({
            "model": "test/m",
            "contents": [{
                "parts": [{
                    "functionResponse": {
                        "response": {
                            "data": {
                                "nested": [{ "text": long_content.clone() }],
                                "sibling": long_content
                            }
                        }
                    }
                }]
            }]
        });
        let modified = truncate_tool_results(&mut parsed, 500);
        assert!(modified);
        let nested_str = parsed["contents"][0]["parts"][0]["functionResponse"]["response"]["data"]["nested"][0]["text"].as_str().unwrap();
        assert!(nested_str.len() < 10000, "deeply nested string must be truncated");
        let sibling_str = parsed["contents"][0]["parts"][0]["functionResponse"]["response"]["data"]["sibling"].as_str().unwrap();
        assert!(sibling_str.len() < 10000, "deeply nested sibling string must be truncated");
    }

    // ── head_tail_truncate UTF-8 ─────────────────────────────

    #[test]
    fn head_tail_truncate_ascii() {
        let text = "a".repeat(1000);
        let result = head_tail_truncate(&text, 100);
        assert!(result.len() < 1000);
        assert!(result.contains("truncated"));
    }

    #[test]
    fn head_tail_truncate_multibyte_utf8_does_not_panic() {
        // 3-byte UTF-8 characters (CJK): each char is 3 bytes
        let text = "你好世界".repeat(100); // 400 chars, 1200 bytes
        let result = head_tail_truncate(&text, 50); // 50 chars budget
        assert!(result.contains("truncated"));
        // Verify it's valid UTF-8
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn head_tail_truncate_emoji_does_not_panic() {
        // 4-byte UTF-8 characters (emoji): each is 4 bytes
        let text = "🎉".repeat(100); // 100 chars, 400 bytes
        let result = head_tail_truncate(&text, 50); // tight budget
        assert!(result.contains("truncated"));
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn head_tail_truncate_short_text_unchanged() {
        let text = "hello";
        let result = head_tail_truncate(text, 1000);
        // head=700, tail=100, both exceed text length so entire text is head
        assert!(result.starts_with("hello"));
    }

    #[test]
    fn head_tail_truncate_near_boundary_preserves_marker() {
        // Regression: max_chars close to char_len used to make tail_chars
        // overrun the remaining chars, producing a "tail" equal to the full
        // input that the final safety clip brutally truncated (often
        // mid-marker). The result must still contain the marker and a
        // non-empty head/tail slice around it.
        let text = "a".repeat(120);
        let result = head_tail_truncate(&text, 100);
        assert!(result.contains("truncated"));
        assert!(result.starts_with("aaaa"));
        assert!(result.ends_with("aaaa"));
        assert!(result.chars().count() <= 100);
    }


    // ── compress_tool_results (SmartCrusher integration) ────

    #[test]
    fn compress_openai_tool_json_array() {
        // Build a JSON array of 50 objects — well above the min_tokens_to_crush (200) threshold
        let items: Vec<Value> = (0..50)
            .map(|i| json!({
                "id": i,
                "name": format!("item_{i}"),
                "status": if i % 5 == 0 { "error" } else { "ok" },
                "description": format!("A longer description for item number {i} to add token weight")
            }))
            .collect();
        let content_str = serde_json::to_string(&items).unwrap();

        let mut parsed = json!({
            "model": "test/m",
            "messages": [
                {"role": "tool", "content": content_str}
            ]
        });

        let cfg = provider_config(None, Some(true));
        let modified = compress_tool_results(&mut parsed, &cfg, "");
        assert!(modified, "SmartCrusher should compress a 50-item array with mixed statuses");

        let result_content = parsed["messages"][0]["content"].as_str().unwrap();
        if let Ok(result_items) = serde_json::from_str::<Vec<Value>>(result_content) {
            assert!(result_items.len() < 50, "compressed array should be smaller");
        } else {
            // Lossless compaction win
            assert!(result_content.contains("]{"), "Lossless result should contain type/schema header");
        }
    }

    #[test]
    fn compress_skips_small_arrays() {
        let items: Vec<Value> = (0..3)
            .map(|i| json!({"id": i, "name": format!("item_{i}")}))
            .collect();
        let content_str = serde_json::to_string(&items).unwrap();

        let mut parsed = json!({
            "model": "test/m",
            "messages": [
                {"role": "tool", "content": content_str}
            ]
        });

        let cfg = provider_config(None, Some(true));
        let modified = compress_tool_results(&mut parsed, &cfg, "");
        assert!(!modified, "arrays < 5 items should not be crushed");
    }

    #[test]
    fn compress_skips_non_json_content() {
        let mut parsed = json!({
            "model": "test/m",
            "messages": [
                {"role": "tool", "content": "just plain text, not JSON"}
            ]
        });

        let cfg = provider_config(None, Some(true));
        let modified = compress_tool_results(&mut parsed, &cfg, "");
        assert!(!modified);
    }

    #[test]
    fn compress_anthropic_tool_result_json_array() {
        let items: Vec<Value> = (0..50)
            .map(|i| json!({
                "id": i,
                "value": i * 10,
                "label": format!("row_{i}"),
                "category": if i % 7 == 0 { "special" } else { "normal" },
                "notes": format!("Extended notes for row {i} to ensure token threshold is met")
            }))
            .collect();
        let content_str = serde_json::to_string(&items).unwrap();

        let mut parsed = json!({
            "model": "test/m",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "content": content_str}
                    ]
                }
            ]
        });

        let cfg = provider_config(None, Some(true));
        let modified = compress_tool_results(&mut parsed, &cfg, "");
        assert!(modified, "SmartCrusher should compress a 50-item array");
        let result = parsed["messages"][0]["content"][0]["content"].as_str().unwrap();
        if let Ok(result_items) = serde_json::from_str::<Vec<Value>>(result) {
            assert!(result_items.len() < 50);
        } else {
            // Lossless compaction win
            assert!(result.contains("]{"), "Lossless result should contain type/schema header");
        }
    }

    #[test]
    fn compress_gemini_deeply_nested_array() {
        // Regression: the previous `compress_json_array_value` only handled
        // top-level String/Array values and the caller descended one level
        // into object values. Arrays buried deeper (object → object → array)
        // were silently skipped.
        let items: Vec<Value> = (0..50)
            .map(|i| json!({
                "id": i,
                "name": format!("item_{i}"),
                "status": if i % 5 == 0 { "error" } else { "ok" },
                "description": format!("Longer description for item {i} to push the token weight up")
            }))
            .collect();
        let mut parsed = json!({
            "model": "test/m",
            "contents": [{
                "parts": [{
                    "functionResponse": {
                        "response": {
                            "data": {
                                "results": items,
                                "meta": { "count": 50 }
                            }
                        }
                    }
                }]
            }]
        });
        let before_bytes = serde_json::to_vec(&parsed).unwrap().len();
        let cfg = provider_config(None, Some(true));
        let modified = compress_tool_results(&mut parsed, &cfg, "");
        assert!(modified, "SmartCrusher should descend into object→object→array and compress");
        let after_bytes = serde_json::to_vec(&parsed).unwrap().len();
        assert!(after_bytes < before_bytes, "compressed body should be smaller");
    }
}
