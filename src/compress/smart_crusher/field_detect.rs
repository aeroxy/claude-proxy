//! Statistical detectors for ID-like and score-like fields.
//!
//! These run *after* per-field statistics are computed and consume a
//! `FieldStats` plus the raw values. They're called by the analyzer's
//! crushability logic to decide whether a field carries a meaningful
//! ranking signal (score) or is just a unique identifier (ID) that
//! shouldn't drive compression decisions.

use serde_json::Value;

use super::statistics::{calculate_string_entropy, detect_sequential_pattern, is_uuid_format};
use super::types::FieldStats;

/// Detect whether a field is an "ID field" — high-uniqueness column
/// that doesn't carry semantic information.
///
/// Returns `(is_id, confidence)` where confidence ∈ [0.0, 1.0].
///
/// # Detection rules
///
/// 1. Hard gate: `unique_ratio < 0.9` → not an ID field.
/// 2. String fields:
///    - >80% of first-20 sample values look like UUIDs → confidence 0.95.
///    - Average entropy >0.7 AND `unique_ratio > 0.95` → confidence 0.8.
/// 3. Numeric fields:
///    - Sequential pattern (via `detect_sequential_pattern`) AND
///      `unique_ratio > 0.95` → confidence 0.9.
///    - Has a value range AND `unique_ratio > 0.95` → confidence 0.85.
/// 4. Catch-all: very high uniqueness (`> 0.98`) → confidence 0.7.
pub fn detect_id_field_statistically(stats: &FieldStats, values: &[Value]) -> (bool, f64) {
    // Hard gate.
    if stats.unique_ratio < 0.9 {
        return (false, 0.0);
    }

    // String-field branches.
    if stats.field_type == "string" {
        // First 20 string-typed values for sampling.
        let sample_values: Vec<&str> = values.iter().take(20).filter_map(|v| v.as_str()).collect();

        if !sample_values.is_empty() {
            let uuid_count = sample_values.iter().filter(|s| is_uuid_format(s)).count();
            if (uuid_count as f64 / sample_values.len() as f64) > 0.8 {
                return (true, 0.95);
            }

            // Average entropy across the sample.
            let avg_entropy = sample_values
                .iter()
                .map(|s| calculate_string_entropy(s))
                .sum::<f64>()
                / sample_values.len() as f64;
            if avg_entropy > 0.7 && stats.unique_ratio > 0.95 {
                return (true, 0.8);
            }
        }
    }

    // Numeric-field branches.
    if stats.field_type == "numeric" {
        // Sequential check with default `check_order=True`.
        if detect_sequential_pattern(values, true) && stats.unique_ratio > 0.95 {
            return (true, 0.9);
        }

        // High-uniqueness numeric with non-trivial range — likely an ID
        // even without sequential structure (e.g., random ints in a wide
        // band).
        if let (Some(min_v), Some(max_v)) = (stats.min_val, stats.max_val) {
            let value_range = max_v - min_v;
            if value_range > 0.0 && stats.unique_ratio > 0.95 {
                return (true, 0.85);
            }
        }
    }

    // Catch-all: very high uniqueness alone is a signal.
    if stats.unique_ratio > 0.98 {
        return (true, 0.7);
    }

    (false, 0.0)
}

/// Detect whether a field is a "score field" — bounded-range numeric
/// where higher values mean "more relevant".
///
/// Detect whether a field is a "score field". Returns `(is_score, confidence)`.
///
/// # Detection rules
///
/// 1. Field must be numeric AND have both `min_val` and `max_val`.
/// 2. Range must match a "common score range":
///    - `[0, 1]` (most common ML score range) → +0.4
///    - `[0, 10]` → +0.3
///    - `[0, 100]` → +0.25
///    - `[-1, 1]` (signed similarity) → +0.35
/// 3. Must NOT be a sequential pattern (IDs are sequential; scores aren't).
/// 4. If first-50 values appear sorted descending (>70% of pairs) → +0.3.
/// 5. If >30% of first-20 are non-integer floats → +0.1.
/// 6. Returns `(confidence >= 0.4, min(confidence, 0.95))`.
///
/// `items` is the list of original-array dict items so we can pull the
/// field's values in array order for the descending-sort check.
pub fn detect_score_field_statistically(stats: &FieldStats, items: &[Value]) -> (bool, f64) {
    if stats.field_type != "numeric" {
        return (false, 0.0);
    }

    let (min_val, max_val) = match (stats.min_val, stats.max_val) {
        (Some(min_v), Some(max_v)) => (min_v, max_v),
        _ => return (false, 0.0),
    };

    let mut confidence: f64 = 0.0;

    // Range check. The conditions are arranged in an `if/elif` chain.
    let is_bounded = if (0.0..=1.0).contains(&min_val) && (0.0..=1.0).contains(&max_val) {
        confidence += 0.4;
        true
    } else if (0.0..=10.0).contains(&min_val) && (0.0..=10.0).contains(&max_val) {
        confidence += 0.3;
        true
    } else if (0.0..=100.0).contains(&min_val) && (0.0..=100.0).contains(&max_val) {
        confidence += 0.25;
        true
    } else if min_val >= -1.0 && max_val <= 1.0 {
        confidence += 0.35;
        true
    } else {
        false
    };

    if !is_bounded {
        return (false, 0.0);
    }

    // Pull this field's values from the FIRST 50 items.
    let sample_values: Vec<&Value> = items
        .iter()
        .take(50)
        .filter_map(|item| item.as_object().and_then(|m| m.get(&stats.name)))
        .collect();

    // Sequential check — IDs are sequential, scores aren't.
    // Check both ascending and descending: descending IDs (e.g. 100, 99, 98)
    // must also be rejected to prevent misclassification as score fields.
    let sample_owned: Vec<Value> = sample_values.iter().map(|v| (*v).clone()).collect();
    if detect_sequential_pattern(&sample_owned, true) {
        return (false, 0.0);
    }
    let mut reversed = sample_owned;
    reversed.reverse();
    if detect_sequential_pattern(&reversed, true) {
        return (false, 0.0);
    }

    // Descending-sort check on the first 50 items.
    // Filter to finite-numeric values, preserving array order.
    let values_in_order: Vec<f64> = items
        .iter()
        .take(50)
        .filter_map(|item| item.as_object().and_then(|m| m.get(&stats.name)))
        .filter_map(|v| v.as_f64())
        .filter(|f| f.is_finite())
        .collect();

    if values_in_order.len() >= 5 {
        let num_pairs = values_in_order.len() - 1;
        let descending_count = values_in_order.windows(2).filter(|w| w[0] >= w[1]).count();
        if num_pairs > 0 && (descending_count as f64 / num_pairs as f64) > 0.7 {
            confidence += 0.3;
        }
    }

    // Float-fraction check on first 20 ordered values.
    // "Not equal to its int-truncation" ≈ has a fractional part.
    let first_20: &[f64] = if values_in_order.len() > 20 {
        &values_in_order[..20]
    } else {
        &values_in_order[..]
    };
    let float_count = first_20
        .iter()
        .filter(|&&v| v.is_finite() && v != v.trunc())
        .count();
    if !first_20.is_empty() && (float_count as f64) > (first_20.len() as f64 * 0.3) {
        confidence += 0.1;
    }

    let is_score = confidence >= 0.4;
    let bounded_confidence = confidence.min(0.95);
    (is_score, bounded_confidence)
}

