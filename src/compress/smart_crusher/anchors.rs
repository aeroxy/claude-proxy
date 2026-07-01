//! Regex-based query anchor extraction.
//!
//! Extracts query anchors and matches them against elements. These functions are
//! used by the live SmartCrusher path on every invocation to find anchor items.
//!
//! # Regex behavior
//!
//! These regexes drive which array items survive compression. The patterns below are
//! pinned to lowercase ASCII inputs and use only ASCII-safe constructs to keep behavior
//! identical and predictable.

use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;

// ---------------------------------------------------------------
// Pattern definitions
// ---------------------------------------------------------------

/// `\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b`
static UUID_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b")
        .expect("UUID_PATTERN")
});

/// 4+ digit numbers (likely IDs).
static NUMERIC_ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9]{4,}\b").expect("NUMERIC_ID_PATTERN"));

/// Hostname pattern. Matches `host.tld` with optional `.tld2`.
static HOSTNAME_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[a-zA-Z0-9][-a-zA-Z0-9]*\.[a-zA-Z0-9][-a-zA-Z0-9]*(?:\.[a-zA-Z]{2,})?\b")
        .expect("HOSTNAME_PATTERN")
});

/// Short quoted strings (single OR double quotes), 1-50 chars between
/// matching quotes.
static QUOTED_STRING_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"'([^']{1,50})'|"([^"]{1,50})""#).expect("QUOTED_STRING_PATTERN")
});

/// Email addresses.
static EMAIL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").expect("EMAIL_PATTERN")
});

/// Hostname false-positive blocklist.
const HOSTNAME_FALSE_POSITIVES: &[&str] = &["e.g", "i.e", "etc."];

/// Extract query anchors from user text.
///
/// Output is a set of lowercased anchor strings. Order is not
/// significant.
pub fn extract_query_anchors(text: &str) -> HashSet<String> {
    let mut anchors = HashSet::new();

    if text.is_empty() {
        return anchors;
    }

    // UUIDs — lowercase the match.
    for m in UUID_PATTERN.find_iter(text) {
        anchors.insert(m.as_str().to_lowercase());
    }

    // Numeric IDs — keep original case (digits, no transform needed).
    for m in NUMERIC_ID_PATTERN.find_iter(text) {
        anchors.insert(m.as_str().to_string());
    }

    // Emails — lowercase (processed first to capture byte spans and avoid hostname overlap)
    let mut email_spans = Vec::new();
    for m in EMAIL_PATTERN.find_iter(text) {
        let val = m.as_str().to_lowercase();
        email_spans.push(m.range());
        anchors.insert(val);
    }

    // Hostnames — lowercase, filter false positives and email overlaps.
    for m in HOSTNAME_PATTERN.find_iter(text) {
        let range = m.range();
        if email_spans
            .iter()
            .any(|r| range.start >= r.start && range.end <= r.end)
        {
            continue;
        }
        let lc = m.as_str().to_lowercase();
        if !HOSTNAME_FALSE_POSITIVES.contains(&lc.as_str()) {
            anchors.insert(lc);
        }
    }

    // Quoted strings — capture group 1 (single quotes) or group 2 (double quotes),
    // require trim().len() >= 2.
    for caps in QUOTED_STRING_PATTERN.captures_iter(text) {
        let matched_inner = caps.get(1).or_else(|| caps.get(2));
        if let Some(inner) = matched_inner {
            if inner.as_str().trim().len() >= 2 {
                anchors.insert(inner.as_str().to_lowercase());
            }
        }
    }

    anchors
}

/// Serialize a `serde_json::Value` to a string representation matching
/// the standard output format.
///
/// Used by `item_matches_anchors` because substring matching depends on:
///
/// | Aspect           | Representation format        | Standard `serde_json::to_string` |
/// |------------------|------------------------------|----------------------------------|
/// | String quotes    | single `'`                   | double `"`                       |
/// | Booleans / null  | `True`, `False`, `None`      | `true`, `false`, `null`          |
/// | Spacing          | `key: value`, `a, b`         | `key:value`, `a,b`               |
///
/// All three matter for anchor matching:
/// - An anchor `"name': 'a"` extracted from a user phrase like
///   `find {'name': 'alice'}` would match the representation but
///   never the JSON form.
/// - An anchor `"true"` (lowercased from `"True"`) matches both, but
///   the unlowercased version `"True"` is in output and not
///   JSON. Lowercasing both sides handles this.
/// - An anchor `"name: alice"` (with the space) would match the representation
///   but never JSON.
///
/// Output is then lowercased upstream so True/False/None case is normalized away.
fn to_repr_string(value: &Value) -> String {
    let mut out = String::new();
    write_repr_string(&mut out, value);
    out
}

fn write_repr_string(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) => {
            // Integers and floats are formatted as simple string representation.
            out.push_str(&n.to_string());
        }
        Value::String(s) => {
            // We emit single quotes always — this matches the dominant case
            // (no quotes in the string). We escape any embedded single quotes.
            out.push('\'');
            for c in s.chars() {
                if c == '\'' {
                    out.push_str("\\'");
                } else {
                    out.push(c);
                }
            }
            out.push('\'');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_repr_string(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            // We require the workspace `serde_json` to be built with `preserve_order`
            // so `serde_json::Map` preserves insertion order instead of sorting by key.
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('\'');
                out.push_str(k);
                out.push('\'');
                out.push_str(": ");
                write_repr_string(out, v);
            }
            out.push('}');
        }
    }
}

/// Check if a JSON value matches any query anchors.
///
/// Uses `to_repr_string` representation rather than `serde_json::to_string`
/// so substring matching has the same surface (single quotes, `True`/`False`/`None`, spaced commas/colons).
pub fn item_matches_anchors(item: &Value, anchors: &HashSet<String>) -> bool {
    if anchors.is_empty() {
        return false;
    }

    // Lowercase normalizes the case for stable matching.
    let item_str = to_repr_string(item).to_lowercase();
    anchors.iter().any(|a| item_str.contains(a))
}
