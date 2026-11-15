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

/// Give a `model`-role `functionCall` part that carries no thought signature
/// the validator-skip sentinel, so Code Assist accepts the replayed call, and
/// leave a signature the client sent alone.
///
/// The signature is how the model gets its own reasoning back on the next turn,
/// whether that follows a tool call or a plain reply. A native-Gemini client
/// that echoes one has to reach the upstream with it intact, or the model works
/// everything out again from the visible history, every turn. Clients that
/// cannot carry a signature still get the sentinel: the OpenAI translator emits
/// bare `functionCall`s, and the Anthropic translator sets the sentinel itself,
/// which is kept like any other value. An empty or non-string signature counts
/// as none and is replaced, as every signature used to be. A signature the
/// upstream refuses is retried as the sentinel, see [`skip_thought_signatures`].
fn inject_thought_signatures(env: &mut Value) {
    for part in model_parts_mut(&mut env["request"]) {
        let signed = part
            .get("thoughtSignature")
            .and_then(|s| s.as_str())
            .is_some_and(|s| !s.is_empty());
        let wants_one = part.get("functionCall").is_some() || part.get("thoughtSignature").is_some();
        if wants_one && !signed {
            part["thoughtSignature"] = json!(SKIP_SIG);
        }
    }
}

/// Whether the body of an upstream 400 refuses a thought signature. Code Assist
/// answers a value that is not base64 with a decoding error naming the
/// `thought_signature` field, and one that decodes but does not verify
/// (corrupted, truncated, or minted by another model or provider) with
/// "Invalid thought signature." or "Corrupted thought signature.".
pub fn is_signature_refusal(body: &[u8]) -> bool {
    let body = String::from_utf8_lossy(body).to_ascii_lowercase();
    body.contains("thought signature") || body.contains("thought_signature")
}

/// Put the sentinel in place of every signature a `model` part carries, for the
/// retry after [`is_signature_refusal`]. The result is the payload every request
/// carried before client signatures were kept. Takes the built payload (an
/// envelope, or the flat aicode body) and returns whether anything changed,
/// since a payload with no client signature would only be refused again.
pub fn skip_thought_signatures(payload: &mut Value) -> bool {
    let request = if payload.get("request").is_some() {
        &mut payload["request"]
    } else {
        payload
    };
    let mut changed = false;
    for part in model_parts_mut(request) {
        if part.get("thoughtSignature").is_some_and(|s| s != SKIP_SIG) {
            part["thoughtSignature"] = json!(SKIP_SIG);
            changed = true;
        }
    }
    changed
}

/// Every part of every `model`-role content under `request` (the envelope's
/// `request`, or the flat aicode body itself).
fn model_parts_mut(request: &mut Value) -> impl Iterator<Item = &mut Value> {
    request
        .get_mut("contents")
        .and_then(|c| c.as_array_mut())
        .into_iter()
        .flatten()
        .filter(|c| c.get("role").and_then(|r| r.as_str()) == Some("model"))
        .filter_map(|c| c.get_mut("parts").and_then(|p| p.as_array_mut()))
        .flatten()
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

#[cfg(test)]
mod signature_tests {
    use super::*;

    const REAL: &str = "CiQBjz1rX2Rlc2lnbmVkLWJ5LXRoZS1tb2RlbA==";

    /// A native body whose one `model` turn holds `parts`, then the answer to
    /// its function call.
    fn history(parts: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "contents": [
                {"role": "user", "parts": [{"text": "list it"}]},
                {"role": "model", "parts": parts},
                {"role": "user", "parts": [{"functionResponse": {"name": "ls", "response": {"ok": true}}}]}
            ]
        }))
        .unwrap()
    }

    fn call(sig: Option<&str>) -> Value {
        let mut part = json!({"functionCall": {"name": "ls", "args": {}}});
        if let Some(s) = sig {
            part["thoughtSignature"] = json!(s);
        }
        part
    }

    /// Every `model` part, from an envelope's `request.contents` or the flat
    /// aicode body's `contents`.
    fn model_parts(contents: &Value) -> Vec<Value> {
        contents
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["role"] == "model")
            .flat_map(|c| c["parts"].as_array().unwrap().clone())
            .collect()
    }

    /// The model parts each provider's payload carries for `body`.
    fn per_provider(body: &[u8]) -> Vec<Vec<Value>> {
        let cli = gemini_to_gemini_cli(body, "m", "p", "generateContent");
        let ag = gemini_to_antigravity(body, "m", "p", "generateContent");
        let (aicode, _) = gemini_to_aicode(body, "e", "t");
        vec![
            model_parts(&cli["request"]["contents"]),
            model_parts(&ag["request"]["contents"]),
            model_parts(&aicode["contents"]),
        ]
    }

    #[test]
    fn a_client_signature_reaches_every_upstream_unchanged() {
        for parts in per_provider(&history(json!([call(Some(REAL))]))) {
            assert_eq!(parts[0]["thoughtSignature"], REAL);
        }
    }

    #[test]
    fn an_unsigned_function_call_gets_the_sentinel() {
        for parts in per_provider(&history(json!([call(None)]))) {
            assert_eq!(parts[0]["thoughtSignature"], SKIP_SIG);
        }
    }

    #[test]
    fn a_client_sent_sentinel_stays_the_sentinel() {
        for parts in per_provider(&history(json!([call(Some(SKIP_SIG))]))) {
            assert_eq!(parts[0]["thoughtSignature"], SKIP_SIG);
        }
    }

    /// An empty value is not a signature the model minted, so it keeps the
    /// sentinel it always got.
    #[test]
    fn an_empty_signature_counts_as_none() {
        for parts in per_provider(&history(json!([call(Some(""))]))) {
            assert_eq!(parts[0]["thoughtSignature"], SKIP_SIG);
        }
    }

    /// A plain answer comes back as its text plus an empty text part carrying
    /// only the signature. Both survive, the signature untouched.
    #[test]
    fn a_text_part_keeps_its_signature_and_its_empty_carrier_survives() {
        let body = history(json!([{"text": "ready"}, {"text": "", "thoughtSignature": REAL}]));
        for parts in per_provider(&body) {
            assert_eq!(parts.len(), 2);
            assert!(parts[0].get("thoughtSignature").is_none());
            assert_eq!(parts[1], json!({"text": "", "thoughtSignature": REAL}));
        }
    }

    /// Claude's format has no slot for a Gemini signature, so the Anthropic
    /// translator sends the sentinel on every replayed call, as before.
    #[test]
    fn the_anthropic_surface_still_sends_the_sentinel() {
        let req = json!({"model": "m", "max_tokens": 64, "messages": [
            {"role": "user", "content": "list it"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "toolu_1", "name": "ls", "input": {}}
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}]}
        ]});
        let body = serde_json::to_vec(&super::super::anthropic_translate::claude_to_gemini(&req)).unwrap();
        let env = gemini_to_gemini_cli(&body, "m", "p", "generateContent");
        let parts = model_parts(&env["request"]["contents"]);
        assert_eq!(parts[0], json!({"text": "checking"}));
        assert_eq!(parts[1]["thoughtSignature"], SKIP_SIG);
    }

    /// OpenAI `tool_calls` carry no signature at all, so each call gets the
    /// sentinel, as before.
    #[test]
    fn the_openai_surface_still_sends_the_sentinel() {
        let req = json!({"model": "m", "messages": [
            {"role": "user", "content": "list it"},
            {"role": "assistant", "content": "checking", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "ls", "arguments": "{}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
        ]});
        let body = serde_json::to_vec(&super::super::openai_translate::openai_to_gemini(&req)).unwrap();
        let env = gemini_to_gemini_cli(&body, "m", "p", "generateContent");
        let parts = model_parts(&env["request"]["contents"]);
        assert_eq!(parts[0], json!({"text": "checking"}));
        assert_eq!(parts[1]["thoughtSignature"], SKIP_SIG);
    }

    /// The retry turns every client signature, on calls and text parts alike,
    /// into the sentinel, in the envelope and in the flat aicode body.
    #[test]
    fn the_retry_replaces_client_signatures_in_both_payload_shapes() {
        let body = history(json!([
            {"text": "", "thoughtSignature": REAL},
            call(Some(REAL)),
            call(None)
        ]));
        let mut env = gemini_to_gemini_cli(&body, "m", "p", "generateContent");
        let (mut flat, _) = gemini_to_aicode(&body, "e", "t");
        assert!(skip_thought_signatures(&mut env));
        assert!(skip_thought_signatures(&mut flat));
        for parts in [model_parts(&env["request"]["contents"]), model_parts(&flat["contents"])] {
            for part in parts {
                assert_eq!(part["thoughtSignature"], SKIP_SIG, "{part}");
            }
        }
        assert!(!skip_thought_signatures(&mut env), "nothing left to replace");
    }

    /// Without a client signature the retry would resend the same payload, so
    /// it reports that there is nothing to retry.
    #[test]
    fn the_retry_has_nothing_to_do_without_a_client_signature() {
        let mut env = gemini_to_gemini_cli(&history(json!([call(None)])), "m", "p", "generateContent");
        assert!(!skip_thought_signatures(&mut env));
    }

    /// The three refusals the upstreams return, and a 400 that is not one.
    #[test]
    fn refusals_are_recognised_by_their_message() {
        for refusal in [
            r#"{"error":{"code":400,"message":"Invalid thought signature.","status":"INVALID_ARGUMENT"}}"#,
            r#"{"error":{"code":400,"message":"Corrupted thought signature.","status":"INVALID_ARGUMENT"}}"#,
            r#"[{"error":{"code":400,"message":"Invalid value at 'request.contents[1].parts[0].thought_signature' (TYPE_BYTES), Base64 decoding failed for \"bogus\"","status":"INVALID_ARGUMENT"}}]"#,
        ] {
            assert!(is_signature_refusal(refusal.as_bytes()), "{refusal}");
        }
        let unrelated = r#"{"error":{"code":400,"message":"Invalid value at 'request.generation_config.thinking_config.thinking_budget' (TYPE_INT32), \"lots\"","status":"INVALID_ARGUMENT"}}"#;
        assert!(!is_signature_refusal(unrelated.as_bytes()));
    }
}
