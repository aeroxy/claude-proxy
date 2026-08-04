//! Request shaping: everything that makes a forwarded request look like the one
//! a real `claude-cli` sends. All pure functions over the parsed body plus
//! config, so the whole disguise is unit-testable without a transport.
//!
//! Three tiers, deliberately kept apart (see `wiki/claude-oauth.md`):
//!
//! - **Auth-critical** — the identity system block and the `oauth-2025-04-20`
//!   beta. Without these the OAuth credential is rejected.
//! - **Cosmetic** — the billing system block, user-agent, `x-app`, the
//!   `x-stainless-*` set, session/request ids, `metadata.user_id`,
//!   `diagnostics`. Zero effect on generation; injected unconditionally.
//! - **Semantic** — `context_management`, `output_config`, `thinking`,
//!   `temperature`, … Forwarded when the client sends them, **never invented**,
//!   because inventing them would silently change what the caller asked for.
//!   `[claude_oauth.inject]` is the escape hatch for doing it deliberately.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::config::ClaudeOAuthConfig;

/// System block 0 of a real CLI request: HTTP-header-shaped client attribution,
/// carried in the prompt rather than in a header.
pub const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

/// The identity string that unlocks an OAuth credential, `cc_entrypoint=cli` form.
pub const IDENTITY_CLI: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Identity string for non-`cli` entrypoints (VS Code / Agent SDK surfaces).
pub const IDENTITY_SDK: &str =
    "You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.";

/// Common prefix of both identity strings — what the idempotency check matches,
/// so a client that already sent either variant isn't given a second one.
const IDENTITY_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude";

/// Injected when the client omitted `max_tokens`, which the API requires.
const DEFAULT_MAX_TOKENS: u64 = 32_000;

/// Lowercase hex of `seed`'s SHA-256, truncated to `len` chars.
fn hex_digest(seed: &str, len: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(seed.as_bytes());
    let mut out = String::with_capacity(len);
    // Two chars per byte, so only the first `len/2` (rounded up) matter.
    for byte in digest.iter().take(len.div_ceil(2)) {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out.truncate(len);
    out
}

/// A UUID derived deterministically from `seed`, so the same conversation keeps
/// the same id across turns without us having to track sessions.
fn stable_uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// 64-hex device id, stable for this user on this machine. The real CLI persists
/// a random one; deriving it from the account avoids owning another state file
/// while still being stable across restarts.
pub fn device_id() -> String {
    let user = std::env::var("USER").unwrap_or_default();
    let home = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    hex_digest(&format!("claude-proxy-device:{user}:{home}"), 64)
}

/// The identity string matching the configured entrypoint, so the two stay
/// coherent — a `cli` entrypoint claiming the Agent SDK prompt (or the reverse)
/// is a sharper fingerprint than either choice on its own.
fn identity_text(cfg: &ClaudeOAuthConfig) -> &'static str {
    if cfg.entrypoint == "cli" {
        IDENTITY_CLI
    } else {
        IDENTITY_SDK
    }
}

/// `text` of a system block, or `""` for shapes without one.
fn block_text(block: &Value) -> &str {
    block.get("text").and_then(|t| t.as_str()).unwrap_or("")
}

/// Normalize the client's `system` into a block array carrying, in order:
/// billing block, identity block, then the client's own blocks untouched.
///
/// Both injected blocks deliberately omit `cache_control`: a real CLI puts its
/// breakpoints on later blocks, and spending one here would take it from the
/// client's budget of four.
///
/// Idempotent — a client that already sent either block (Claude Code itself,
/// arriving over the MITM path) keeps its own, which is more accurate than ours.
/// The identity check is a **prefix** match (so both wordings count) scanning only
/// the **first three** blocks, which is where a real CLI puts it. A client that
/// buries its identity block deeper than that gets a second copy prepended —
/// harmless (the gate passes either way) but worth knowing before chasing a
/// duplicated prompt.
pub fn normalize_system(req: &mut Value, cfg: &ClaudeOAuthConfig) {
    let mut blocks: Vec<Value> = match req.get("system") {
        Some(Value::String(s)) if !s.trim().is_empty() => {
            vec![json!({"type": "text", "text": s})]
        }
        // An empty string would become an empty text block, which the API
        // rejects outright ("text content blocks must be non-empty").
        Some(Value::String(_)) | None | Some(Value::Null) => vec![],
        Some(Value::Array(a)) => a.clone(),
        // Anything else is malformed; drop it rather than forward a 400.
        Some(_) => vec![],
    };

    // Hash the client's own system text, before injection: this is a cache
    // diagnostic in the real CLI, so it should track the prompt it describes.
    let joined: String = blocks
        .iter()
        .map(block_text)
        .collect::<Vec<_>>()
        .join("\u{1f}");
    let cch = hex_digest(&joined, 5);

    let has_identity = blocks
        .iter()
        .take(3)
        .any(|b| block_text(b).starts_with(IDENTITY_PREFIX));
    let has_billing = blocks
        .first()
        .is_some_and(|b| block_text(b).starts_with(BILLING_PREFIX));

    if !has_identity {
        // Slot the identity *after* a billing block the client already sent —
        // inserting at 0 unconditionally would demote theirs out of position 0,
        // which is the one thing the billing block's placement requires.
        let at = usize::from(has_billing);
        blocks.insert(at, json!({"type": "text", "text": identity_text(cfg)}));
    }
    if !has_billing {
        let billing = format!(
            "{} cc_version={}; cc_entrypoint={}; cch={};",
            BILLING_PREFIX, cfg.cli_version, cfg.entrypoint, cch
        );
        blocks.insert(0, json!({"type": "text", "text": billing}));
    }

    req["system"] = Value::Array(blocks);
}

/// Text of the first user message, used to derive a session id that's stable for
/// the life of a conversation.
fn first_user_text(req: &Value) -> String {
    let Some(messages) = req.get("messages").and_then(|m| m.as_array()) else {
        return String::new();
    };
    let Some(first) = messages.first() else {
        return String::new();
    };
    match first.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(block_text)
            .collect::<Vec<_>>()
            .join("\u{1f}"),
        _ => String::new(),
    }
}

/// Session id for this request: the client's own if it sent one, else derived
/// from the conversation's opening message so it stays put across turns.
pub fn session_id(req: &Value, client_session: Option<&str>) -> String {
    match client_session {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => stable_uuid(&format!("claude-proxy-session:{}", first_user_text(req))),
    }
}

/// Set `metadata.user_id` to the CLI's shape: a JSON *string* holding
/// `device_id` / `account_uuid` / `session_id`. A client-supplied `user_id` is
/// left alone. `account_uuid` is omitted when unknown rather than faked — a
/// wrong uuid is a worse signal than a missing one.
pub fn apply_metadata(req: &mut Value, session_id: &str, account_uuid: Option<&str>) {
    if req
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())
        .is_some_and(|u| !u.is_empty())
    {
        return;
    }
    let mut ids = Map::new();
    ids.insert("device_id".into(), json!(device_id()));
    if let Some(uuid) = account_uuid.filter(|u| !u.is_empty()) {
        ids.insert("account_uuid".into(), json!(uuid));
    }
    ids.insert("session_id".into(), json!(session_id));
    let user_id = Value::Object(ids).to_string();

    match req.get_mut("metadata") {
        Some(Value::Object(m)) => {
            m.insert("user_id".into(), json!(user_id));
        }
        _ => req["metadata"] = json!({"user_id": user_id}),
    }
}

/// Cosmetic body fields a real CLI always sends. Inert — `previous_message_id`
/// is null on a fresh turn anyway — but its absence is a fingerprint.
pub fn apply_cosmetic_fields(req: &mut Value) {
    if req.get("diagnostics").is_none() {
        req["diagnostics"] = json!({"previous_message_id": null});
    }
}

/// Ensure the API's required `max_tokens` is present. Returns true when injected,
/// so the caller can log that it substituted a value the client didn't choose.
pub fn ensure_max_tokens(req: &mut Value) -> bool {
    if req.get("max_tokens").and_then(|m| m.as_u64()).is_some() {
        return false;
    }
    req["max_tokens"] = json!(DEFAULT_MAX_TOKENS);
    true
}

/// Merge `[claude_oauth.inject]` into the body. Client-supplied values win, so
/// the escape hatch can't quietly override what the caller explicitly asked for.
pub fn apply_inject(req: &mut Value, cfg: &ClaudeOAuthConfig) {
    for (key, value) in &cfg.inject {
        if req.get(key).is_none() {
            req[key] = value.clone();
        }
    }
}

/// Strip our routing prefix and apply `[claude_oauth.model_map]`, yielding the
/// real Anthropic model name to send upstream.
pub fn resolve_model(model_full: &str, cfg: &ClaudeOAuthConfig) -> String {
    let bare = model_full
        .strip_prefix(&format!("{}/", cfg.prefix))
        .unwrap_or(model_full);
    cfg.model_map
        .get(bare)
        .cloned()
        .unwrap_or_else(|| bare.to_string())
}

/// The `anthropic-beta` header: exactly the configured list, deduped.
///
/// The client's own values are deliberately **not** merged in. Anthropic rejects
/// any beta it doesn't recognize with a hard 400 (`Unexpected value(s) … for the
/// `anthropic-beta` header`), so forwarding a caller's list turns one stray
/// identifier into a total failure — and we have no way to tell a valid unknown
/// beta from a typo. Sending a fixed list is also what a real `claude-cli` does.
/// A client needing a beta we don't send is a `[claude_oauth] betas` edit away;
/// [`dropped_client_betas`] makes that visible in the log.
pub fn beta_header(cfg: &ClaudeOAuthConfig) -> String {
    let mut out: Vec<&str> = Vec::new();
    for beta in &cfg.betas {
        let beta = beta.trim();
        if !beta.is_empty() && !out.contains(&beta) {
            out.push(beta);
        }
    }
    out.join(",")
}

/// Client-requested betas that [`beta_header`] won't forward, so the caller can
/// say so out loud instead of silently changing the request's capabilities.
pub fn dropped_client_betas(client_betas: Option<&str>, cfg: &ClaudeOAuthConfig) -> Vec<String> {
    let Some(client) = client_betas else {
        return Vec::new();
    };
    client
        .split(',')
        .map(|b| b.trim())
        .filter(|b| !b.is_empty() && !cfg.betas.iter().any(|c| c.trim() == *b))
        .map(|b| b.to_string())
        .collect()
}

/// `user-agent` value: `claude-cli/<major.minor.patch> (external, <entrypoint>)`.
/// The build suffix carried by `cli_version` for `cc_version` isn't part of the
/// user-agent the CLI sends, so it's trimmed to three dotted components.
pub fn user_agent(cfg: &ClaudeOAuthConfig) -> String {
    let short: Vec<&str> = cfg.cli_version.split('.').take(3).collect();
    format!(
        "claude-cli/{} (external, {})",
        short.join("."),
        cfg.entrypoint
    )
}

/// The `x-stainless-*` fingerprint of the Node SDK the CLI ships with. Replaces
/// whatever the calling SDK sent, so a Python/Rust client doesn't leak its own.
pub const STAINLESS_HEADERS: &[(&str, &str)] = &[
    ("x-stainless-arch", "arm64"),
    ("x-stainless-lang", "js"),
    ("x-stainless-os", "MacOS"),
    ("x-stainless-package-version", "0.94.0"),
    ("x-stainless-retry-count", "0"),
    ("x-stainless-runtime", "node"),
    ("x-stainless-runtime-version", "v26.3.0"),
    ("x-stainless-timeout", "600"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ClaudeOAuthConfig {
        ClaudeOAuthConfig::default()
    }

    fn texts(req: &Value) -> Vec<String> {
        req["system"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| block_text(b).to_string())
            .collect()
    }

    #[test]
    fn string_system_becomes_blocks_with_client_prompt_last() {
        let mut req = json!({"system": "You are a pirate."});
        normalize_system(&mut req, &cfg());
        let t = texts(&req);
        assert_eq!(t.len(), 3);
        assert!(t[0].starts_with(BILLING_PREFIX));
        assert_eq!(t[1], IDENTITY_CLI);
        assert_eq!(t[2], "You are a pirate.");
    }

    #[test]
    fn absent_system_gets_exactly_the_two_injected_blocks() {
        let mut req = json!({"messages": []});
        normalize_system(&mut req, &cfg());
        let t = texts(&req);
        assert_eq!(t.len(), 2);
        assert!(t[0].starts_with(BILLING_PREFIX));
        assert_eq!(t[1], IDENTITY_CLI);
    }

    #[test]
    fn empty_string_system_never_yields_an_empty_text_block() {
        let mut req = json!({"system": "   "});
        normalize_system(&mut req, &cfg());
        assert_eq!(texts(&req).len(), 2);
        assert!(texts(&req).iter().all(|t| !t.trim().is_empty()));
    }

    #[test]
    fn real_claude_code_body_is_left_alone() {
        let mut req = json!({"system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=9.9.9; cc_entrypoint=cli; cch=abcde;"},
            {"type": "text", "text": IDENTITY_SDK},
            {"type": "text", "text": "harness", "cache_control": {"type": "ephemeral"}},
        ]});
        let before = req.clone();
        normalize_system(&mut req, &cfg());
        assert_eq!(req, before);
    }

    #[test]
    fn billing_without_identity_keeps_billing_first() {
        let client_billing = "x-anthropic-billing-header: cc_version=9.9.9; cc_entrypoint=cli; cch=abcde;";
        let mut req = json!({"system": [
            {"type": "text", "text": client_billing},
            {"type": "text", "text": "harness"},
        ]});
        normalize_system(&mut req, &cfg());
        let t = texts(&req);
        assert_eq!(t.len(), 3);
        // The client's own billing block must stay at index 0 — inserting the
        // identity at 0 would demote it.
        assert_eq!(t[0], client_billing);
        assert_eq!(t[1], IDENTITY_CLI);
        assert_eq!(t[2], "harness");
    }

    #[test]
    fn identity_without_billing_gains_only_billing() {
        let mut req = json!({"system": [{"type": "text", "text": IDENTITY_CLI}]});
        normalize_system(&mut req, &cfg());
        let t = texts(&req);
        assert_eq!(t.len(), 2);
        assert!(t[0].starts_with(BILLING_PREFIX));
        assert_eq!(t[1], IDENTITY_CLI);
    }

    #[test]
    fn injected_blocks_carry_no_cache_control() {
        let mut req = json!({"system": "hi"});
        normalize_system(&mut req, &cfg());
        let blocks = req["system"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_none());
        assert!(blocks[1].get("cache_control").is_none());
    }

    #[test]
    fn billing_block_reports_configured_version_and_entrypoint() {
        let mut c = cfg();
        c.cli_version = "2.1.221.9b8".into();
        c.entrypoint = "cli".into();
        let mut req = json!({"system": "hi"});
        normalize_system(&mut req, &c);
        let billing = texts(&req)[0].clone();
        assert!(billing.contains("cc_version=2.1.221.9b8;"));
        assert!(billing.contains("cc_entrypoint=cli;"));
        assert!(billing.contains("cch="));
    }

    #[test]
    fn non_cli_entrypoint_pairs_with_the_sdk_identity() {
        let mut c = cfg();
        c.entrypoint = "claude-vscode".into();
        let mut req = json!({"system": "hi"});
        normalize_system(&mut req, &c);
        assert_eq!(texts(&req)[1], IDENTITY_SDK);
    }

    #[test]
    fn cch_tracks_the_client_prompt() {
        let mut a = json!({"system": "one"});
        let mut b = json!({"system": "two"});
        normalize_system(&mut a, &cfg());
        normalize_system(&mut b, &cfg());
        assert_ne!(texts(&a)[0], texts(&b)[0]);
    }

    #[test]
    fn beta_header_carries_the_mandatory_oauth_beta_once() {
        let header = beta_header(&cfg());
        assert!(header.contains("oauth-2025-04-20"));
        assert!(header.contains("claude-code-20250219"));
        assert_eq!(header.matches("oauth-2025-04-20").count(), 1);
    }

    #[test]
    fn unknown_client_betas_are_not_forwarded() {
        // Anthropic 400s on unrecognized values, so a caller's stray beta must
        // never reach it.
        let header = beta_header(&cfg());
        assert!(!header.contains("my-custom-beta"));
        assert_eq!(
            dropped_client_betas(Some("my-custom-beta,oauth-2025-04-20"), &cfg()),
            vec!["my-custom-beta".to_string()]
        );
        assert!(dropped_client_betas(None, &cfg()).is_empty());
    }

    #[test]
    fn fallback_credit_is_not_enabled_by_default() {
        assert!(!beta_header(&cfg()).contains("fallback-credit"));
    }

    #[test]
    fn model_prefix_is_stripped_and_aliases_apply() {
        let mut c = cfg();
        c.model_map
            .insert("claude-3-5-sonnet-latest".into(), "claude-sonnet-5".into());
        assert_eq!(resolve_model("claude-oauth/claude-opus-5", &c), "claude-opus-5");
        assert_eq!(resolve_model("claude-opus-5", &c), "claude-opus-5");
        assert_eq!(
            resolve_model("claude-oauth/claude-3-5-sonnet-latest", &c),
            "claude-sonnet-5"
        );
    }

    #[test]
    fn session_id_is_stable_across_turns_of_a_conversation() {
        let turn1 = json!({"messages": [{"role": "user", "content": "hello"}]});
        let turn2 = json!({"messages": [
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": "more"},
        ]});
        assert_eq!(session_id(&turn1, None), session_id(&turn2, None));
        assert_ne!(
            session_id(&turn1, None),
            session_id(&json!({"messages": [{"role": "user", "content": "other"}]}), None)
        );
    }

    #[test]
    fn client_session_header_wins() {
        let req = json!({"messages": []});
        assert_eq!(session_id(&req, Some("abc")), "abc");
    }

    #[test]
    fn metadata_user_id_is_a_json_string_with_device_and_session() {
        let mut req = json!({"messages": []});
        apply_metadata(&mut req, "sess-1", Some("acct-1"));
        let raw = req["metadata"]["user_id"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed["session_id"], "sess-1");
        assert_eq!(parsed["account_uuid"], "acct-1");
        assert_eq!(parsed["device_id"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn unknown_account_uuid_is_omitted_not_faked() {
        let mut req = json!({"messages": []});
        apply_metadata(&mut req, "sess-1", None);
        let raw = req["metadata"]["user_id"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(raw).unwrap();
        assert!(parsed.get("account_uuid").is_none());
    }

    #[test]
    fn client_metadata_is_preserved() {
        let mut req = json!({"metadata": {"user_id": "mine"}});
        apply_metadata(&mut req, "sess-1", None);
        assert_eq!(req["metadata"]["user_id"], "mine");
    }

    #[test]
    fn semantic_fields_are_never_invented() {
        let mut req = json!({"messages": [], "max_tokens": 10});
        normalize_system(&mut req, &cfg());
        apply_cosmetic_fields(&mut req);
        apply_metadata(&mut req, "s", None);
        assert!(req.get("context_management").is_none());
        assert!(req.get("output_config").is_none());
        assert!(req.get("thinking").is_none());
    }

    #[test]
    fn inject_fills_gaps_but_never_overrides_the_client() {
        let mut c = cfg();
        c.inject.insert(
            "context_management".into(),
            json!({"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]}),
        );
        c.inject.insert("output_config".into(), json!({"effort": "high"}));
        let mut req = json!({"output_config": {"effort": "low"}});
        apply_inject(&mut req, &c);
        assert_eq!(req["context_management"]["edits"][0]["keep"], "all");
        assert_eq!(req["output_config"]["effort"], "low");
    }

    #[test]
    fn max_tokens_injected_only_when_missing() {
        let mut req = json!({"messages": []});
        assert!(ensure_max_tokens(&mut req));
        assert_eq!(req["max_tokens"], DEFAULT_MAX_TOKENS);
        let mut kept = json!({"max_tokens": 7});
        assert!(!ensure_max_tokens(&mut kept));
        assert_eq!(kept["max_tokens"], 7);
    }

    #[test]
    fn user_agent_drops_the_build_suffix() {
        let mut c = cfg();
        c.cli_version = "2.1.221.9b8".into();
        assert_eq!(user_agent(&c), "claude-cli/2.1.221 (external, cli)");
    }
}
