//! Per-cell classification for the compaction pipeline.
//!
//! Given a JSON value, decide what kind of compaction treatment it needs.
//! The classifier is intentionally conservative — when in doubt, return
//! [`CellClass::Scalar`] so the cell is rendered verbatim.
//!
//! # Detection priorities
//!
//! 1. **Object / array** — pass through to caller, who decides whether to
//!    flatten (uniform-nested) or recurse ([`CellClass::JsonObject`],
//!    [`CellClass::JsonArray`]).
//! 2. **Stringified-JSON** — strings that parse to a JSON object/array.
//!    Common in tool-output payloads where one field is a serialized
//!    sub-structure ([`CellClass::StringifiedJson`]).
//! 3. **Opaque blob** — strings above a length threshold the classifier
//!    couldn't otherwise place. Sub-classified into base64 / HTML /
//!    plain long-string for telemetry ([`CellClass::Opaque`]).
//! 4. **Scalar** — everything else, rendered verbatim.

use serde_json::Value;

use super::ir::OpaqueKind;

/// Per-cell classification result.
#[derive(Debug, Clone, PartialEq)]
pub enum CellClass {
    /// Number, bool, null, short string — render verbatim.
    Scalar,
    /// Cell is a JSON object. Caller decides flatten-vs-recurse based
    /// on schema uniformity across rows.
    JsonObject,
    /// Cell is a JSON array. Caller may recurse with TabularCompactor.
    JsonArray,
    /// String that parses to a JSON object/array. The parsed value is
    /// returned so the caller doesn't re-parse.
    StringifiedJson(Value),
    /// Long string the classifier judged opaque. Sub-classified for
    /// telemetry only — all variants get CCR-substituted.
    Opaque(OpaqueKind),
}

/// Config controlling classification thresholds.
///
/// Defaults are tuned for typical tool-output payloads. Override via
/// builder if a workload has different characteristics (e.g. an API
/// that always emits 500-char status descriptions shouldn't have those
/// CCR-substituted).
#[derive(Debug, Clone)]
pub struct ClassifyConfig {
    /// Strings strictly longer than this become candidates for opaque
    /// classification. Default: 256 bytes.
    pub opaque_min_bytes: usize,
    /// Base64-alphabet ratio threshold. Strings whose chars are at
    /// least this fraction in `[A-Za-z0-9+/=_-]` and longer than 64
    /// bytes are tagged base64. Default: 0.95.
    pub base64_alphabet_ratio: f64,
    /// `<` count above which a long string is considered HTML-ish.
    /// Default: 3.
    pub html_min_open_brackets: usize,
}

impl Default for ClassifyConfig {
    fn default() -> Self {
        Self {
            opaque_min_bytes: 256,
            base64_alphabet_ratio: 0.95,
            html_min_open_brackets: 3,
        }
    }
}

/// Classify a single cell value.
pub fn classify_cell(value: &Value, cfg: &ClassifyConfig) -> CellClass {
    match value {
        Value::Object(_) => CellClass::JsonObject,
        Value::Array(_) => CellClass::JsonArray,
        Value::String(s) => classify_string(s, cfg),
        _ => CellClass::Scalar,
    }
}

fn classify_string(s: &str, cfg: &ClassifyConfig) -> CellClass {
    // Stringified-JSON check first. Cheap fast-path: must start with
    // `{` or `[` (after optional whitespace) — skip strings that
    // can't possibly be JSON containers. Parsing `"123"` would
    // technically succeed as JSON-the-number, but that's a scalar,
    // not a recursion target.
    let trimmed = s.trim_start();
    if matches!(trimmed.as_bytes().first(), Some(&b'{') | Some(&b'[')) {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            if matches!(parsed, Value::Object(_) | Value::Array(_)) {
                return CellClass::StringifiedJson(parsed);
            }
        }
    }

    // Opaque-blob check — only for strings above the byte threshold.
    if s.len() <= cfg.opaque_min_bytes {
        return CellClass::Scalar;
    }

    if looks_like_base64(s, cfg.base64_alphabet_ratio) {
        return CellClass::Opaque(OpaqueKind::Base64Blob);
    }

    if looks_like_html(s, cfg.html_min_open_brackets) {
        return CellClass::Opaque(OpaqueKind::HtmlChunk);
    }

    CellClass::Opaque(OpaqueKind::LongString)
}

fn looks_like_base64(s: &str, ratio_threshold: f64) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 64 {
        return false;
    }

    // Single pass: track alphabet count + unique characters, short-circuit
    // immediately on any disqualifying byte (<, >, whitespace). Real base64
    // is ASCII-only, so working at the byte level is semantically equivalent
    // to the char-based version and avoids three redundant UTF-8 walks.
    // Unique-byte tracking uses a [bool; 256] mask (byte-indexed) rather
    // than a HashSet — zero heap allocation, no hashing overhead.
    let mut alphabet_count = 0usize;
    let mut unique_mask = [false; 256];
    let mut unique_count = 0usize;
    for &b in bytes {
        if b == b'<' || b == b'>' || b.is_ascii_whitespace() {
            return false;
        }
        if b.is_ascii_alphanumeric()
            || b == b'+'
            || b == b'/'
            || b == b'='
            || b == b'_'
            || b == b'-'
        {
            alphabet_count += 1;
        }
        if unique_count < 16 && !unique_mask[b as usize] {
            unique_mask[b as usize] = true;
            unique_count += 1;
        }
    }

    if (alphabet_count as f64) / (bytes.len() as f64) < ratio_threshold {
        return false;
    }

    unique_count >= 16
}

fn looks_like_html(s: &str, min_open_brackets: usize) -> bool {
    // Single pass over bytes with early termination once enough valid
    // tag starts are seen. The original two-pass approach first counted
    // all `<` characters, then counted tag starts — but only the second
    // count is actually used to decide, so the first pass is redundant.
    let bytes = s.as_bytes();
    let mut tag_starts = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'<' {
            if let Some(&next) = bytes.get(i + 1) {
                if next.is_ascii_alphabetic() || next == b'/' || next == b'!' {
                    tag_starts += 1;
                    if tag_starts >= min_open_brackets {
                        return true;
                    }
                }
            }
        }
    }
    false
}

