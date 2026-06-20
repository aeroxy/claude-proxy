#![allow(unused_imports, dead_code)]

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

// Per-thread SmartCrusher instances avoid synchronization overhead (e.g. Mutex)
// in the single-threaded hyper service_fn context, at the trade-off of
// increased memory usage with many worker threads.
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

/// Apply compression to a request body based on the provider's config.
/// Returns the original body unchanged if the provider has no compression
/// config or if parsing/compression fails.
pub fn apply(
    body: Bytes,
    provider_name: &str,
    config: &CompressConfig,
) -> Bytes {
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

    if provider_cfg.json_array.unwrap_or(false) {
        modified |= compress_tool_results(&mut parsed, provider_cfg);
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
pub fn resolve_provider(body: &[u8]) -> Option<String> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    resolve_provider_from_value(&parsed)
}

/// Walk messages looking for JSON array tool results and run SmartCrusher on them.
fn compress_tool_results(parsed: &mut Value, _provider_cfg: &CompressProviderConfig) -> bool {
    let mut modified = false;

    let messages = match parsed.get_mut("messages").and_then(|m| m.as_array_mut()) {
        Some(m) => m,
        None => {
            // Gemini format uses "contents" instead of "messages"
            match parsed.get_mut("contents").and_then(|c| c.as_array_mut()) {
                Some(c) => {
                    modified |= compress_gemini_contents(c);
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
                if let Some(compressed) = try_compress_json_array_str(content) {
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
                    if let Some(compressed) = try_compress_json_array_str(content_str) {
                        block["content"] = Value::String(compressed);
                        modified = true;
                    }
                } else if let Some(sub_blocks) =
                    block.get_mut("content").and_then(|c| c.as_array_mut())
                {
                    for sub in sub_blocks.iter_mut() {
                        if sub.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = sub.get_mut("text").and_then(|t| t.as_str()) {
                                if let Some(compressed) = try_compress_json_array_str(text) {
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

fn compress_gemini_contents(contents: &mut Vec<Value>) -> bool {
    let mut modified = false;
    for content in contents.iter_mut() {
        if let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) {
            for part in parts.iter_mut() {
                if let Some(resp) = part.get_mut("functionResponse") {
                    if let Some(response) = resp.get_mut("response") {
                        if let Some(response_str) = response.as_str() {
                            if let Some(compressed) = try_compress_json_array_str(response_str) {
                                *response = Value::String(compressed);
                                modified = true;
                            }
                        } else if response.is_object() {
                            // Gemini wraps tool results in an object; check values
                            if let Some(obj) = response.as_object_mut() {
                                for (_k, v) in obj.iter_mut() {
                                    if let Some(s) = v.as_str() {
                                        if let Some(compressed) = try_compress_json_array_str(s) {
                                            *v = Value::String(compressed);
                                            modified = true;
                                        }
                                    }
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

/// Try to parse a string as a JSON array of dicts and compress it with
/// SmartCrusher. Returns the compressed JSON string, or None if the
/// content isn't a crushable JSON array.
fn try_compress_json_array_str(s: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(s).ok()?;
    let arr = parsed.as_array()?;

    // Only crush arrays of objects with >= 5 items
    if arr.len() < 5 {
        return None;
    }
    if !arr.iter().any(|v| v.is_object()) {
        return None;
    }

    let result = SHARED_CRUSHER.with(|c| c.borrow().crush_array(arr, "", 1.0));

    if let Some(rendered) = result.compacted {
        return Some(rendered);
    }

    if result.items.len() >= arr.len() {
        // No compression achieved
        return None;
    }

    Some(serde_json::to_string(&result.items).ok()?)
}

/// Truncate tool result content that exceeds max_chars.
/// Uses head 70% + tail 10% extraction with an elision marker.
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
                if content.len() > max_chars {
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
                    if content_str.len() > max_chars {
                        let truncated = head_tail_truncate(content_str, max_chars);
                        block["content"] = Value::String(truncated);
                        modified = true;
                    }
                } else if let Some(sub_blocks) = block.get_mut("content").and_then(|c| c.as_array_mut()) {
                    for sub in sub_blocks.iter_mut() {
                        if sub.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = sub.get_mut("text").and_then(|t| t.as_str()) {
                                if text.len() > max_chars {
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

fn truncate_gemini_contents(contents: &mut Vec<Value>, max_chars: usize) -> bool {
    let mut modified = false;
    for content in contents.iter_mut() {
        if let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) {
            for part in parts.iter_mut() {
                if let Some(resp) = part.get_mut("functionResponse") {
                    if let Some(response) = resp.get_mut("response") {
                        if let Some(s) = response.as_str() {
                            if s.len() > max_chars {
                                *response = Value::String(head_tail_truncate(s, max_chars));
                                modified = true;
                            }
                        } else if let Some(obj) = response.as_object_mut() {
                            for (_k, v) in obj.iter_mut() {
                                if let Some(s) = v.as_str() {
                                    if s.len() > max_chars {
                                        *v = Value::String(head_tail_truncate(s, max_chars));
                                        modified = true;
                                    }
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

fn head_tail_truncate(text: &str, max_chars: usize) -> String {
    let head_approx = (max_chars as f64 * 0.7) as usize;
    let tail_approx = (max_chars as f64 * 0.1) as usize;

    let head_end = floor_char_boundary(text, head_approx);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_approx));

    let head = &text[..head_end];
    let tail = &text[tail_start..];

    let elided = text.len().saturating_sub(head_end + (text.len() - tail_start));
    format!(
        "{head}\n\n[... {elided} chars truncated ...]\n\n{tail}"
    )
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn provider_config(max_tool_chars: Option<usize>, json_array: Option<bool>) -> CompressProviderConfig {
        CompressProviderConfig { max_tool_chars, json_array }
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
        let result = head_tail_truncate(&text, 50); // 50 tokens = 200 chars budget
        assert!(result.contains("truncated"));
        // Verify it's valid UTF-8
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }

    #[test]
    fn head_tail_truncate_emoji_does_not_panic() {
        // 4-byte UTF-8 characters (emoji): each is 4 bytes
        let text = "🎉".repeat(100); // 100 chars, 400 bytes
        let result = head_tail_truncate(&text, 10); // very tight budget
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

    // ── char boundary helpers ────────────────────────────────

    #[test]
    fn floor_char_boundary_ascii() {
        assert_eq!(floor_char_boundary("hello", 3), 3);
        assert_eq!(floor_char_boundary("hello", 10), 5);
    }

    #[test]
    fn floor_char_boundary_multibyte() {
        // "你好" = 6 bytes, each char is 3 bytes
        let s = "你好";
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 0); // mid-char → floor to 0
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 3); // char boundary
        assert_eq!(floor_char_boundary(s, 4), 3);
        assert_eq!(floor_char_boundary(s, 6), 6);
    }

    #[test]
    fn ceil_char_boundary_multibyte() {
        let s = "你好";
        assert_eq!(ceil_char_boundary(s, 0), 0);
        assert_eq!(ceil_char_boundary(s, 1), 3); // mid-char → ceil to 3
        assert_eq!(ceil_char_boundary(s, 2), 3);
        assert_eq!(ceil_char_boundary(s, 3), 3);
        assert_eq!(ceil_char_boundary(s, 4), 6);
        assert_eq!(ceil_char_boundary(s, 6), 6);
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
        let modified = compress_tool_results(&mut parsed, &cfg);
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
        let modified = compress_tool_results(&mut parsed, &cfg);
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
        let modified = compress_tool_results(&mut parsed, &cfg);
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
        let modified = compress_tool_results(&mut parsed, &cfg);
        assert!(modified, "SmartCrusher should compress a 50-item array");
        let result = parsed["messages"][0]["content"][0]["content"].as_str().unwrap();
        if let Ok(result_items) = serde_json::from_str::<Vec<Value>>(result) {
            assert!(result_items.len() < 50);
        } else {
            // Lossless compaction win
            assert!(result.contains("]{"), "Lossless result should contain type/schema header");
        }
    }
}
