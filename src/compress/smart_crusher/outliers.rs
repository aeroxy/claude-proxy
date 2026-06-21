//! Outlier detectors used to mark items as "must preserve" during compression.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::error_keywords::ERROR_KEYWORDS;

/// Detect items that are structural outliers (error-like or
/// uncommonly-shaped).
///
/// Returns deduplicated, ascending-sorted indices.
///
/// # Detection
///
/// 1. **Rare-field outliers**: items containing a field that appears
///    in <20% of the array.
/// 2. **Rare-status outliers**: forwarded to `detect_rare_status_values`,
///    which finds items with statistically rare categorical values.
pub fn detect_structural_outliers(items: &[Value]) -> Vec<usize> {
    if items.len() < 5 {
        return Vec::new();
    }

    // Field counts across the whole array.
    let mut field_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for item in items {
        if let Some(obj) = item.as_object() {
            for key in obj.keys() {
                *field_counts.entry(key.as_str()).or_insert(0) += 1;
            }
        }
    }

    let n = items.len();
    let common_fields: HashSet<String> = field_counts
        .iter()
        .filter(|(_, &c)| c as f64 >= n as f64 * 0.8)
        .map(|(k, _)| (*k).to_string())
        .collect();
    let rare_fields: HashSet<&str> = field_counts
        .iter()
        .filter(|(_, &c)| (c as f64) < n as f64 * 0.2)
        .map(|(k, _)| *k)
        .collect();

    // Use a BTreeSet for stable order.
    let mut outlier_set: BTreeSet<usize> = BTreeSet::new();

    // 1. Rare-field outliers.
    for (i, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let has_rare = obj.keys().any(|k| rare_fields.contains(k.as_str()));
        if has_rare {
            outlier_set.insert(i);
        }
    }

    // 2. Rare-status outliers.
    for idx in detect_rare_status_values(items, &common_fields) {
        outlier_set.insert(idx);
    }

    outlier_set.into_iter().collect()
}

/// Detect items with rare values in status-like categorical fields.
///
/// Algorithm:
/// 1. Cardinality 2..=50.
/// 2. Pareto check: top-K values covering ≥80% with `K ≤ 5`.
/// 3. Items NOT in top-K → outliers.
///
/// Returns indices in the order they were discovered.
pub fn detect_rare_status_values(items: &[Value], common_fields: &HashSet<String>) -> Vec<usize> {
    let mut outlier_indices: Vec<usize> = Vec::new();

    // Iterate fields in sorted order for determinism.
    // Sorting gives us a stable order.
    let mut sorted_fields: Vec<&String> = common_fields.iter().collect();
    sorted_fields.sort();

    for field_name in sorted_fields {
        // Collect this field's values across all items.
        let values: Vec<&Value> = items
            .iter()
            .filter_map(|item| item.as_object())
            .filter_map(|m| m.get(field_name))
            .collect();

        // Stringify values and dedupe to get cardinality.
        // We use stringification: simple
        // scalars use their natural form; nested values use serde_json
        // serialization. This stringification is only used for set-
        // dedup and frequency counting, not surfaced to callers, so the
        // representation is internally consistent.
        let stringify = |v: &Value| -> String {
            match v {
                Value::Null => "__none__".to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                _ => v.to_string(),
            }
        };

        let unique_values: BTreeSet<String> = values
            .iter()
            .map(|v| stringify(v))
            .collect();

        // Cardinality cap.
        if !(2..=50).contains(&unique_values.len()) {
            continue;
        }

        // Frequency count.
        let mut value_counts: BTreeMap<String, usize> = BTreeMap::new();
        for v in &values {
            let key = stringify(v);
            *value_counts.entry(key).or_insert(0) += 1;
        }
        if value_counts.is_empty() {
            continue;
        }

        let total = values.len();

        // Pareto check (BUG #3 FIX): find smallest K such that top-K
        // values cover ≥80% of items.
        let mut sorted_counts: Vec<(&String, &usize)> = value_counts.iter().collect();
        // Sort by count descending; tiebreak by key ascending so the
        // result is deterministic when multiple values have the same
        // frequency.
        sorted_counts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));

        let threshold = (total as f64 * 0.8).ceil() as usize;
        let mut cumulative: usize = 0;
        let mut top_k_values: HashSet<String> = HashSet::new();
        for (value, count) in &sorted_counts {
            cumulative += **count;
            top_k_values.insert((*value).clone());
            if cumulative >= threshold {
                break;
            }
        }

        // Only flag rare values if the top-K is small (≤5). Above this
        // the distribution is too uniform to label any value "rare".
        if top_k_values.len() > 5 {
            continue;
        }

        // Items with values NOT in top_k_values are outliers.
        for (i, item) in items.iter().enumerate() {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(field_value) = obj.get(field_name) else {
                continue;
            };
            let item_value = if matches!(field_value, Value::Null) {
                "__none__".to_string()
            } else {
                stringify(field_value)
            };
            if !top_k_values.contains(&item_value) {
                outlier_indices.push(i);
            }
        }
    }

    outlier_indices
}

/// Detect items containing error keywords for PRESERVATION.
///
/// Ensures error items are NEVER dropped.
///
/// # Args
///
/// - `items`: array items to scan.
/// - `item_strings`: pre-computed JSON serializations to avoid
///   redundant `to_string` work. Pass `None` to serialize on the fly.
///   When provided, must be the same length as `items`.
pub fn detect_error_items_for_preservation(
    items: &[Value],
    item_strings: Option<&[String]>,
) -> Vec<usize> {
    let mut error_indices: Vec<usize> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        if !item.is_object() {
            continue;
        }

        // Reuse cached serialization or serialize fresh.
        let serialized: String = match item_strings {
            Some(arr) if i < arr.len() => arr[i].to_lowercase(),
            _ => match serde_json::to_string(item) {
                Ok(s) => s.to_lowercase(),
                Err(_) => continue,
            },
        };

        // Check if any error keyword exists.
        if ERROR_KEYWORDS.iter().any(|kw| serialized.contains(kw)) {
            error_indices.push(i);
        }
    }

    error_indices
}

