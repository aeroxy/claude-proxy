//! Statistical helpers for field characterization.
//!
//! Used by the analyzer to classify fields (ID-like, score-like, etc.).

use serde_json::Value;
use std::collections::HashMap;

/// Check if a string looks like a UUID.
///
/// Format check only — no version-bit validation. Hex chars are lower
/// or upper case.
pub fn is_uuid_format(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }

    // Expected segment lengths: 8-4-4-4-12.
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected_lens = [8, 4, 4, 4, 12];
    for (part, &expected_len) in parts.iter().zip(expected_lens.iter()) {
        if part.len() != expected_len {
            return false;
        }
        for c in part.chars() {
            if !c.is_ascii_hexdigit() {
                return false;
            }
        }
    }
    true
}

/// Shannon entropy of a string, normalized to `[0, 1]`.
///
/// High entropy (>0.7) suggests random/ID-like content. Low entropy
/// (<0.3) suggests repetitive/predictable content. Used by ID detection.
///
/// # Edge cases
/// - Empty or single-character strings return `0.0`.
/// - All-identical chars: returns `0.0` to avoid division by zero.
pub fn calculate_string_entropy(s: &str) -> f64 {
    let mut freq: HashMap<char, usize> = HashMap::new();
    let mut n = 0usize;
    for c in s.chars() {
        *freq.entry(c).or_insert(0) += 1;
        n += 1;
    }
    if n < 2 {
        return 0.0;
    }

    let length = n as f64;
    let mut entropy = 0.0_f64;
    for &count in freq.values() {
        let p = count as f64 / length;
        if p > 0.0 {
            entropy -= p * p.log2();
        }
    }

    // Normalize by the maximum possible entropy at this length:
    // max_entropy = log2(min(len(freq), length))
    let max_entropy = (freq.len().min(n) as f64).log2();
    if max_entropy > 0.0 {
        entropy / max_entropy
    } else {
        0.0
    }
}

/// Parse a string representing plain integer literals.
///
/// Accepts:
///   - leading/trailing ASCII whitespace (stripped)
///   - leading sign (`+` or `-`)
///   - PEP 515 underscore digit separators (e.g. `"3_000"` → `3000`)
fn parse_int_flexible(s: &str) -> Option<i64> {
    // Strip ASCII whitespace from both ends.
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Drop PEP 515 underscores between digits.
    let cleaned: String = if trimmed.contains('_') {
        // Confirm all underscores are surrounded by digits (PEP 515)
        let chars: Vec<char> = trimmed.chars().collect();
        for i in 0..chars.len() {
            if chars[i] == '_' {
                if i == 0 || i == chars.len() - 1 {
                    return None;
                }
                if !chars[i - 1].is_ascii_digit() || !chars[i + 1].is_ascii_digit() {
                    return None;
                }
            }
        }
        trimmed.replace('_', "")
    } else {
        trimmed.to_string()
    };
    cleaned.parse::<i64>().ok()
}

/// Detect if numeric values form a sequential pattern (like IDs:
/// 1, 2, 3, ...).
///
/// # String-padding handling
/// When a value is a string that parses as a number,
/// we flag the input as "had string-encoded numerics". If ALL parsed values
/// originated as strings, we refuse to classify as a sequential numeric
/// pattern because the padding is categorical. Mixed numeric+string inputs still parse 
/// as sequential.
///
/// # Args
/// - `values`: items to inspect.
/// - `check_order`: when true, also require ascending order in the
///   original array.
pub fn detect_sequential_pattern(values: &[Value], check_order: bool) -> bool {
    if values.len() < 5 {
        return false;
    }

    // Collect numeric values, tracking whether each value originated as
    // a string. This is the BUG #2 fix: we still parse strings into
    // numbers (so legitimate mixed-type fields work), but we'll refuse
    // to flag the field as sequential if EVERY parseable value was a
    // string.
    let mut nums: Vec<f64> = Vec::new();
    let mut had_non_string_numeric = false;

    for v in values {
        match v {
            Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    nums.push(f);
                    had_non_string_numeric = true;
                }
            }
            Value::Bool(_) => {
                // Bools are explicitly excluded.
            }
            Value::String(s) => {
                // Parse strings into numbers.
                if let Some(parsed) = parse_int_flexible(s) {
                    nums.push(parsed as f64);
                    // Do NOT set had_non_string_numeric.
                    // If we later find this is the ONLY source of numeric
                    // values, we refuse to call it sequential.
                }
            }
            _ => {}
        }
    }

    if nums.len() < 5 {
        return false;
    }

    // If every numeric value originated as a string,
    // the field is categorical (e.g. zero-padded codes); not sequential.
    if !had_non_string_numeric {
        return false;
    }

    // Sort and compute pairwise diffs.
    let mut sorted_nums = nums.clone();
    sorted_nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let diffs: Vec<f64> = sorted_nums.windows(2).map(|w| w[1] - w[0]).collect();
    if diffs.is_empty() {
        return false;
    }

    let avg_diff: f64 = diffs.iter().sum::<f64>() / diffs.len() as f64;
    if !(0.5..=2.0).contains(&avg_diff) {
        return false;
    }

    // Most diffs in [0.5, 2.0] => sequential candidate.
    let consistent_count = diffs.iter().filter(|&&d| (0.5..=2.0).contains(&d)).count();
    let is_sequential = consistent_count as f64 / diffs.len() as f64 > 0.8;
    if !is_sequential {
        return false;
    }

    if check_order {
        // Ascending count over original (not-sorted) sequence.
        // IDs ascend in array order; scores typically descend.
        let ascending_count = nums.windows(2).filter(|w| w[0] <= w[1]).count();
        let n_pairs = nums.len() - 1;
        let is_ascending = ascending_count as f64 / n_pairs as f64 > 0.7;
        return is_ascending;
    }

    is_sequential
}

