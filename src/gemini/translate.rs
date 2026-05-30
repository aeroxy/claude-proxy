//! Request/response translation between the native Gemini API (what
//! `@ai-sdk/google` speaks) and the Cloud Code Assist `v1internal` envelope used
//! by both the `gemini-cli` and `antigravity` providers.
//!
//! Ported from CLIProxyAPI's `internal/translator/{gemini-cli,antigravity}/gemini`
//! request/response converters. Both providers wrap the body as
//! `{"project","model","request":{…}}`; antigravity adds a few extra envelope
//! fields. Responses arrive wrapped as `{"response":{…}}` and are unwrapped.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::{json, Value};

const SKIP_SIG: &str = "skip_thought_signature_validator";

/// Build the gemini-cli payload for `action` (`generateContent` /
/// `streamGenerateContent` / `countTokens`).
pub fn gemini_to_gemini_cli(body: &[u8], model: &str, project: &str, action: &str) -> Value {
    let mut env = build_envelope(body);
    if action == "countTokens" {
        strip_for_count_tokens(&mut env);
    } else {
        env["project"] = json!(project);
        env["model"] = json!(model);
    }
    env
}

/// Code Assist `countTokens` takes the bare `{request:{…}}` with no
/// project/model and no safetySettings.
fn strip_for_count_tokens(env: &mut Value) {
    if let Value::Object(map) = env {
        map.remove("project");
        map.remove("model");
    }
    if let Some(req) = env.get_mut("request").and_then(|r| r.as_object_mut()) {
        req.remove("safetySettings");
    }
}

/// Build the antigravity payload (same base envelope + antigravity extras).
pub fn gemini_to_antigravity(body: &[u8], model: &str, project: &str, action: &str) -> Value {
    let mut env = build_envelope(body);

    if action == "countTokens" {
        strip_for_count_tokens(&mut env);
        return env;
    }

    env["model"] = json!(model);
    env["userAgent"] = json!("antigravity");

    let is_image = model.contains("image");
    env["requestType"] = json!(if is_image { "image_gen" } else { "agent" });

    if project.is_empty() {
        if let Value::Object(map) = &mut env {
            map.remove("project");
        }
    } else {
        env["project"] = json!(project);
    }

    if is_image {
        env["requestId"] = json!(format!(
            "image_gen/{}/{}/12",
            chrono::Utc::now().timestamp_millis(),
            uuid::Uuid::new_v4()
        ));
    } else {
        env["requestId"] = json!(format!("agent-{}", uuid::Uuid::new_v4()));
        let sid = stable_session_id(&env);
        env["request"]["sessionId"] = json!(sid);
    }

    // antigravity does not take safetySettings.
    if let Some(req) = env.get_mut("request").and_then(|r| r.as_object_mut()) {
        req.remove("safetySettings");
    }

    if model.contains("claude") {
        env["request"]["toolConfig"]["functionCallingConfig"]["mode"] = json!("VALIDATED");
    } else if let Some(gc) = env
        .get_mut("request")
        .and_then(|r| r.get_mut("generationConfig"))
        .and_then(|g| g.as_object_mut())
    {
        gc.remove("maxOutputTokens");
    }

    env
}

/// Wrap the raw native-Gemini body as `{"project":"","model":"","request":{…}}`
/// and apply the shared normalizations.
fn build_envelope(body: &[u8]) -> Value {
    let request: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
    let mut env = json!({ "project": "", "request": request, "model": "" });

    // Lift any inline `request.model` to the top level (native bodies rarely
    // carry one; the real model comes from the URL path).
    if let Some(m) = env["request"].get("model").cloned() {
        env["model"] = m;
        if let Some(req) = env["request"].as_object_mut() {
            req.remove("model");
        }
    }

    fix_cli_tool_response(&mut env);
    rename_system_instruction(&mut env);
    normalize_roles(&mut env);
    inject_thought_signatures(&mut env);
    filter_empty_parts(&mut env);
    attach_default_safety(&mut env);

    env
}

fn rename_system_instruction(env: &mut Value) {
    if let Some(req) = env["request"].as_object_mut() {
        if let Some(si) = req.remove("system_instruction") {
            req.entry("systemInstruction").or_insert(si);
        }
    }
}

/// Normalize content roles to `user`/`model`, alternating when missing/invalid.
fn normalize_roles(env: &mut Value) {
    let contents = match env["request"].get_mut("contents").and_then(|c| c.as_array_mut()) {
        Some(c) => c,
        None => return,
    };
    let mut prev_role = String::new();
    for content in contents.iter_mut() {
        let role = content.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();
        let valid = role == "user" || role == "model";
        let role = if role.is_empty() || !valid {
            let new_role = if prev_role.is_empty() || prev_role == "model" {
                "user"
            } else {
                "model"
            };
            content["role"] = json!(new_role);
            new_role.to_string()
        } else {
            role
        };
        prev_role = role;
    }
}

/// On `model`-role parts that carry (or imply) a thought signature, replace it
/// with the validator-skip sentinel so Code Assist accepts replayed tool calls.
fn inject_thought_signatures(env: &mut Value) {
    let contents = match env["request"].get_mut("contents").and_then(|c| c.as_array_mut()) {
        Some(c) => c,
        None => return,
    };
    for content in contents.iter_mut() {
        if content.get("role").and_then(|r| r.as_str()) != Some("model") {
            continue;
        }
        if let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) {
            for part in parts.iter_mut() {
                let has_fc = part.get("functionCall").is_some();
                let has_sig = part.get("thoughtSignature").is_some();
                if has_fc || has_sig {
                    part["thoughtSignature"] = json!(SKIP_SIG);
                }
            }
        }
    }
}

/// Drop contents with no parts (Code Assist rejects empty `parts`).
fn filter_empty_parts(env: &mut Value) {
    if let Some(contents) = env["request"].get("contents").and_then(|c| c.as_array()) {
        let kept: Vec<Value> = contents
            .iter()
            .filter(|c| {
                c.get("parts")
                    .and_then(|p| p.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if kept.len() != contents.len() {
            env["request"]["contents"] = json!(kept);
        }
    }
}

fn attach_default_safety(env: &mut Value) {
    if env["request"].get("safetySettings").is_some() {
        return;
    }
    env["request"]["safetySettings"] = json!([
        {"category": "HARM_CATEGORY_HARASSMENT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_HATE_SPEECH", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_DANGEROUS_CONTENT", "threshold": "OFF"},
        {"category": "HARM_CATEGORY_CIVIC_INTEGRITY", "threshold": "BLOCK_NONE"},
    ]);
}

/// Group function calls with their responses, mirroring CLIProxyAPI's
/// `fixCLIToolResponse`: model turns that emit N `functionCall`s are followed by
/// a single `role:"function"` content collecting their N `functionResponse`s.
fn fix_cli_tool_response(env: &mut Value) {
    let contents = match env["request"].get("contents").and_then(|c| c.as_array()) {
        Some(c) => c.clone(),
        None => return,
    };

    struct Group {
        responses_needed: usize,
        call_names: Vec<String>,
    }

    let mut out: Vec<Value> = Vec::new();
    let mut pending: Vec<Group> = Vec::new();
    let mut collected: Vec<Value> = Vec::new();

    let flush = |out: &mut Vec<Value>, group: &Group, collected: &mut Vec<Value>| {
        let take: Vec<Value> = collected.drain(0..group.responses_needed).collect();
        let mut parts: Vec<Value> = Vec::new();
        for (ri, resp) in take.into_iter().enumerate() {
            let fallback = group.call_names.get(ri).map(|s| s.as_str()).unwrap_or("");
            parts.push(backfill_response_name(resp, fallback));
        }
        if !parts.is_empty() {
            out.push(json!({"parts": parts, "role": "function"}));
        }
    };

    for content in contents.iter() {
        let role = content.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let parts = content.get("parts").and_then(|p| p.as_array());

        let response_parts: Vec<Value> = parts
            .map(|ps| {
                ps.iter()
                    .filter(|p| p.get("functionResponse").is_some())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        if !response_parts.is_empty() {
            collected.extend(response_parts);
            while !pending.is_empty() && collected.len() >= pending[0].responses_needed {
                let group = pending.remove(0);
                flush(&mut out, &group, &mut collected);
            }
            continue;
        }

        if role == "model" {
            let call_names: Vec<String> = parts
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| {
                            p.get("functionCall")
                                .and_then(|fc| fc.get("name"))
                                .and_then(|n| n.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(content.clone());
            if !call_names.is_empty() {
                pending.push(Group {
                    responses_needed: call_names.len(),
                    call_names,
                });
            }
        } else {
            out.push(content.clone());
        }
    }

    for group in &pending {
        if collected.len() >= group.responses_needed {
            flush(&mut out, group, &mut collected);
        }
    }

    env["request"]["contents"] = json!(out);
}

fn backfill_response_name(mut resp: Value, fallback: &str) -> Value {
    let empty = resp
        .get("functionResponse")
        .and_then(|fr| fr.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    if empty && !fallback.is_empty() {
        resp["functionResponse"]["name"] = json!(fallback);
    }
    resp
}

fn stable_session_id(env: &Value) -> String {
    if let Some(contents) = env["request"].get("contents").and_then(|c| c.as_array()) {
        for content in contents {
            if content.get("role").and_then(|r| r.as_str()) == Some("user") {
                if let Some(text) = content
                    .get("parts")
                    .and_then(|p| p.as_array())
                    .and_then(|a| a.first())
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                {
                    if !text.is_empty() {
                        let mut h = DefaultHasher::new();
                        text.hash(&mut h);
                        let n = (h.finish() & 0x7FFF_FFFF_FFFF_FFFF) as i64;
                        return format!("-{}", n);
                    }
                }
            }
        }
    }
    let n = (rand::random::<u64>() & 0x7FFF_FFFF_FFFF_FFFF) as i64;
    format!("-{}", n)
}

/// Unwrap a non-streaming Code Assist response `{"response":{…}}` to the bare
/// Gemini response. Returns the input unchanged if there's no `response` key.
pub fn unwrap_response_nonstream(bytes: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(v) => match v.get("response") {
            Some(inner) => serde_json::to_vec(inner).unwrap_or_else(|_| bytes.to_vec()),
            None => bytes.to_vec(),
        },
        Err(_) => bytes.to_vec(),
    }
}

/// Given the JSON payload of one SSE `data:` line, return the unwrapped
/// `.response` JSON string to re-emit, or `None` if absent.
pub fn unwrap_sse_payload(data: &str) -> Option<String> {
    let v: Value = serde_json::from_str(data).ok()?;
    v.get("response").map(|r| r.to_string())
}
