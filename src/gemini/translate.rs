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
use tracing::debug;

const SKIP_SIG: &str = "skip_thought_signature_validator";

/// The only top-level fields `businessaicode` accepts, taken from captured
/// client traffic. This is an allowlist and not a filter-of-known-bad because
/// the API rejects unknown names outright — `project`, which both envelope
/// translators inject, is a 400 there.
const AICODE_ALLOWED: [&str; 6] = [
    "contents",
    "systemInstruction",
    "tools",
    "toolConfig",
    "generationConfig",
    "labels",
];

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

/// Build the `businessaicode` payload: **flat**, not an envelope.
///
/// Everything valuable still comes from [`build_envelope`] — role
/// normalization, `systemInstruction` renaming, tool-response grouping,
/// thought-signature injection, empty-part filtering — and then the envelope is
/// unwrapped and its `request` filtered down to [`AICODE_ALLOWED`]. So
/// `project`, `model` and `safetySettings` fall out by construction rather than
/// by being individually remembered.
///
/// Returns the payload plus a trajectory id: the API groups a conversation by
/// `X-Aicode-Trajectory-Id`, and deriving it from the first user message means
/// a multi-turn session reads as one trajectory instead of N unrelated ones.
pub fn gemini_to_aicode(body: &[u8], experience: &str, user_tier: &str) -> (Value, String) {
    let mut env = build_envelope(body);
    let trajectory = format!("traj{}", stable_session_id(&env));

    // Same tool-schema shape as the antigravity upstream: captured requests
    // carry `functionDeclarations[].parameters`, never `parametersJsonSchema`.
    // Every experience here is a Gemini model, so the Gemini cleaner applies.
    sanitize_antigravity_schemas(&mut env, false);

    let mut out = serde_json::Map::new();
    let mut dropped: Vec<&str> = Vec::new();
    if let Some(req) = env["request"].as_object() {
        for (k, v) in req {
            if AICODE_ALLOWED.contains(&k.as_str()) {
                out.insert(k.clone(), v.clone());
            } else {
                dropped.push(k.as_str());
            }
        }
    }
    if !dropped.is_empty() {
        debug!("aicode: dropped non-allowlisted request fields: {}", dropped.join(", "));
    }

    let mut out = Value::Object(out);
    out["aicode"] = json!({ "experience": experience });
    out["entitlement"] = json!({ "userTier": user_tier });
    (out, trajectory)
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

    // cloudcode-pa's antigravity endpoint reads tool schemas from
    // `functionDeclarations[].parameters` (not `parametersJsonSchema`) and rejects
    // unsupported JSON-Schema keywords — so rename the key and clean each schema.
    // Mirrors CLIProxyAPI's antigravity executor `buildRequest` (the part hidden
    // outside the translator). `claude` / `gemini-3-pro` / `gemini-3.1-pro` use
    // the stricter antigravity cleaner (with VALIDATED placeholders); the rest use
    // the Gemini cleaner.
    let use_antigravity_schema = model.contains("claude")
        || model.contains("gemini-3-pro")
        || model.contains("gemini-3.1-pro");
    sanitize_antigravity_schemas(&mut env, use_antigravity_schema);

    env
}

/// Rename `parametersJsonSchema`→`parameters` and clean every tool schema (and
/// any `responseJsonSchema`/`responseSchema`) for the antigravity upstream.
fn sanitize_antigravity_schemas(env: &mut Value, use_antigravity: bool) {
    let clean = |schema: Value| -> Value {
        if use_antigravity {
            super::schema_clean::clean_for_antigravity(schema)
        } else {
            super::schema_clean::clean_for_gemini(schema)
        }
    };

    if let Some(tools) = env
        .pointer_mut("/request/tools")
        .and_then(|t| t.as_array_mut())
    {
        for tool in tools.iter_mut() {
            if let Some(fds) = tool
                .get_mut("functionDeclarations")
                .and_then(|f| f.as_array_mut())
            {
                for fd in fds.iter_mut() {
                    if let Some(fdo) = fd.as_object_mut() {
                        // Prefer the raw-JSON-schema key, falling back to an
                        // already-`parameters` schema; either way emit `parameters`.
                        let schema = fdo
                            .remove("parametersJsonSchema")
                            .or_else(|| fdo.remove("parameters"));
                        if let Some(schema) = schema {
                            fdo.insert("parameters".into(), clean(schema));
                        }
                    }
                }
            }
        }
    }

    if let Some(gc) = env
        .pointer_mut("/request/generationConfig")
        .and_then(|g| g.as_object_mut())
    {
        for key in ["responseJsonSchema", "responseSchema"] {
            if let Some(schema) = gc.remove(key) {
                gc.insert(key.into(), clean(schema));
            }
        }
    }
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
    let contents = match env["request"]
        .get_mut("contents")
        .and_then(|c| c.as_array_mut())
    {
        Some(c) => c,
        None => return,
    };
    let mut prev_role = String::new();
    for content in contents.iter_mut() {
        let role = content
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
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
    let contents = match env["request"]
        .get_mut("contents")
        .and_then(|c| c.as_array_mut())
    {
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
            // Preserve any non-response parts that shared this turn (e.g. user text
            // or instructions sent alongside the tool result). Anthropic tool_result
            // blocks and native-Gemini parts may legally mix with text; CLIProxyAPI's
            // `fixCLIToolResponse` drops them, but that silently loses caller-supplied
            // context — so re-emit them as their own turn after the grouped responses.
            let other_parts: Vec<Value> = parts
                .map(|ps| {
                    ps.iter()
                        .filter(|p| p.get("functionResponse").is_none())
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if !other_parts.is_empty() {
                let role = if role.is_empty() { "user" } else { role };
                out.push(json!({ "role": role, "parts": other_parts }));
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

/// A conversation id derived from the first user message, so every turn of one
/// conversation hashes alike. `DefaultHasher::new()` is fixed-key (the
/// randomization lives in `RandomState`, not here), so this is stable across
/// process restarts as well as within one — but it is explicitly *not*
/// guaranteed stable across Rust versions, which is fine: grouping only has to
/// hold for the life of a conversation.
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

#[cfg(test)]
mod aicode_tests {
    use super::*;

    fn body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "systemInstruction": {"parts": [{"text": "be brief"}]},
            "tools": [{"functionDeclarations": [{
                "name": "ls",
                "description": "list",
                "parametersJsonSchema": {"type": "object", "properties": {}}
            }]}],
            "toolConfig": {"functionCallingConfig": {}},
            "generationConfig": {"maxOutputTokens": 65536},
            "labels": {"last_step_index": "1"},
            "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "OFF"}],
            "model": "should-not-survive"
        }))
        .unwrap()
    }

    /// The payload is flat and carries exactly the allowlist plus the two
    /// fields we inject. `project` / `model` / `safetySettings` fall out by
    /// construction rather than by being individually removed — which is the
    /// point of filtering rather than deleting, since the API 400s on any
    /// unknown name.
    #[test]
    fn payload_is_flat_and_allowlisted() {
        let (out, _) = gemini_to_aicode(&body(), "gemini-3.7-flash-high", "gcp-ge-plus-tier");
        let obj = out.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "aicode",
                "contents",
                "entitlement",
                "generationConfig",
                "labels",
                "systemInstruction",
                "toolConfig",
                "tools",
            ]
        );
        assert!(out.get("request").is_none(), "must not stay an envelope");
        assert_eq!(out["aicode"]["experience"], "gemini-3.7-flash-high");
        assert_eq!(out["entitlement"]["userTier"], "gcp-ge-plus-tier");
    }

    /// Captured requests carry `functionDeclarations[].parameters`; the native
    /// key our Anthropic translator emits would be rejected.
    #[test]
    fn tool_schemas_are_renamed_to_parameters() {
        let (out, _) = gemini_to_aicode(&body(), "e", "t");
        let fd = &out["tools"][0]["functionDeclarations"][0];
        assert!(fd.get("parameters").is_some());
        assert!(fd.get("parametersJsonSchema").is_none());
    }

    /// The envelope's normalizations still apply — this is the whole reason
    /// the flat body is built by filtering `build_envelope` rather than by
    /// copying the client's JSON.
    #[test]
    fn envelope_normalizations_still_run() {
        let raw = serde_json::to_vec(&json!({
            "contents": [{"parts": [{"text": "hi"}]}],
            "system_instruction": {"parts": [{"text": "sys"}]}
        }))
        .unwrap();
        let (out, _) = gemini_to_aicode(&raw, "e", "t");
        assert_eq!(out["contents"][0]["role"], "user", "role normalized");
        assert!(
            out.get("systemInstruction").is_some(),
            "snake_case key renamed"
        );
    }

    /// One conversation is one trajectory: the id is derived from the first
    /// user message, so a follow-up turn groups with its predecessor instead of
    /// looking like an unrelated session from the same identity.
    #[test]
    fn trajectory_is_stable_across_turns_of_one_conversation() {
        let first = serde_json::to_vec(&json!({
            "contents": [{"role": "user", "parts": [{"text": "hello there"}]}]
        }))
        .unwrap();
        let second = serde_json::to_vec(&json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hello there"}]},
                {"role": "model", "parts": [{"text": "hi"}]},
                {"role": "user", "parts": [{"text": "and now?"}]}
            ]
        }))
        .unwrap();
        let (_, a) = gemini_to_aicode(&first, "e", "t");
        let (_, b) = gemini_to_aicode(&second, "e", "t");
        assert_eq!(a, b);
        assert!(a.starts_with("traj"), "{a}");

        let other = serde_json::to_vec(&json!({
            "contents": [{"role": "user", "parts": [{"text": "different opener"}]}]
        }))
        .unwrap();
        let (_, c) = gemini_to_aicode(&other, "e", "t");
        assert_ne!(a, c);
    }
}
