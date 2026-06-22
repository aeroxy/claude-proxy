//! Index-set orchestration helpers used by every planning method.
//!
//! Includes helpers to deduplicate indices by content, fill remaining slots
//! to meet targeted counts, and prioritize indices by critical criteria.

use md5::{Digest, Md5};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

use super::config::SmartCrusherConfig;
use super::outliers::{detect_error_items_for_preservation, detect_structural_outliers};
use super::types::{ArrayAnalysis, FieldStats};
use crate::compress::anchor_selector::compute_item_hash;

/// Collapse content-duplicate indices to their lowest representative.
///
/// Iterates `keep_indices`
/// in ascending order and records the FIRST index that hashes to a
/// given content fingerprint. Subsequent matches drop. Out-of-bounds
/// indices skip.
///
/// `compute_item_hash` returns a 16-hex-char MD5 hash of the content.
pub fn deduplicate_indices_by_content(
    keep_indices: &BTreeSet<usize>,
    items: &[Value],
) -> BTreeSet<usize> {
    if keep_indices.is_empty() {
        return BTreeSet::new();
    }

    // hash -> lowest-seen index. BTreeSet iteration is ascending, so
    // the first insertion for each hash IS the lowest index.
    let mut seen: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for &idx in keep_indices {
        if idx >= items.len() {
            continue;
        }
        let h = item_content_hash(&items[idx], idx);
        seen.entry(h).or_insert(idx);
    }
    seen.values().copied().collect()
}

/// Fill `keep_indices` back up to `effective_max` with diverse,
/// content-unique items.
///
/// Strategy:
/// 1. Compute hashes of currently-kept items.
/// 2. Walk candidates (indices NOT in keep_indices) with stride-based
///    sampling for spatial diversity.
/// 3. Add a candidate if its content hash is fresh.
///
/// Uses two nested loops with `start_offset` to interleave
/// stride scans.
pub fn fill_remaining_slots(
    keep_indices: &BTreeSet<usize>,
    items: &[Value],
    n: usize,
    effective_max: usize,
) -> BTreeSet<usize> {
    let remaining = effective_max.saturating_sub(keep_indices.len());
    if remaining == 0 {
        return keep_indices.clone();
    }

    // Hashes of items we're already keeping — bound the working set
    // we won't re-add.
    let mut seen: HashSet<String> = HashSet::new();
    for &idx in keep_indices {
        if idx < n {
            seen.insert(item_content_hash(&items[idx], idx));
        }
    }

    // Candidate pool: every index not already kept.
    let candidates: Vec<usize> = (0..n).filter(|i| !keep_indices.contains(i)).collect();
    if candidates.is_empty() {
        return keep_indices.clone();
    }

    let mut result = keep_indices.clone();
    let step = (candidates.len() / (remaining + 1)).max(1);
    let mut added = 0;

    // Interleaved stride: outer loop offsets [0, step),
    // inner loop walks `start_offset, +step, +step, ...`. The result
    // visits every candidate exactly once across the outer iterations.
    'outer: for start_offset in 0..step {
        if added >= remaining {
            break;
        }
        let mut i = start_offset;
        while i < candidates.len() {
            if added >= remaining {
                break 'outer;
            }
            let idx = candidates[i];
            let h = item_content_hash(&items[idx], idx);
            if !seen.contains(&h) {
                result.insert(idx);
                seen.insert(h);
                added += 1;
            }
            i += step;
        }
    }

    result
}

/// Top-level prioritizer.
///
/// Pipeline:
/// 1. **Dedup**: collapse content-duplicate indices.
/// 2. **Fill**: top up to `effective_max` with diverse uniques.
/// 3. **Already under budget?** Return as-is.
/// 4. **Otherwise**: keep ALL critical items (errors + structural
///    outliers + numeric anomalies). Then add first-3 + last-2 if room. Then
///    fill remaining with non-critical kept indices in ascending order.
///
/// May return MORE than `effective_max` items when critical items
/// alone exceed the budget.
pub fn prioritize_indices(
    config: &SmartCrusherConfig,
    keep_indices: &BTreeSet<usize>,
    items: &[Value],
    n: usize,
    analysis: Option<&ArrayAnalysis>,
    effective_max: usize,
) -> BTreeSet<usize> {
    // Dedup pass.
    let mut current = if config.dedup_identical_items {
        deduplicate_indices_by_content(keep_indices, items)
    } else {
        keep_indices.clone()
    };

    // Fill pass.
    if current.len() < effective_max && current.len() < n {
        current = fill_remaining_slots(&current, items, n, effective_max);
    }

    // Errors (keyword-detected — preservation guarantee).
    let error_indices: BTreeSet<usize> = detect_error_items_for_preservation(items, None)
        .into_iter()
        .collect();

    // Structural outliers (statistical — rare fields, rare statuses).
    let outlier_indices: BTreeSet<usize> = detect_structural_outliers(items).into_iter().collect();

    // Numeric anomalies (>variance_threshold σ from per-field mean).
    let anomaly_indices = numeric_anomaly_indices(config, items, analysis);

    if current.len() <= effective_max {
        // Under budget — still guarantee preservation of critical items
        // (errors, structural outliers, numeric anomalies) as the
        // doc comment promises: "May return MORE than effective_max
        // items when critical items alone exceed the budget."
        current.extend(&error_indices);
        current.extend(&outlier_indices);
        current.extend(&anomaly_indices);
        return current;
    }

    // Over budget — apply critical-items-first prioritization.

    // TOIN learned-important indices: empty until TOIN is ported.
    let learned_indices: BTreeSet<usize> = BTreeSet::new();

    let mut prioritized: BTreeSet<usize> = BTreeSet::new();
    prioritized.extend(&error_indices);
    prioritized.extend(&outlier_indices);
    prioritized.extend(&anomaly_indices);
    prioritized.extend(&learned_indices);

    // First 3 / last 2 anchors if we have room.
    let mut remaining = effective_max.saturating_sub(prioritized.len());
    if remaining > 0 {
        for i in 0..3.min(n) {
            if !prioritized.contains(&i) && remaining > 0 {
                prioritized.insert(i);
                remaining -= 1;
            }
        }
        let last_start = n.saturating_sub(2);
        for i in last_start..n {
            if !prioritized.contains(&i) && remaining > 0 {
                prioritized.insert(i);
                remaining -= 1;
            }
        }
    }

    // Fill with other-important indices (ascending order).
    if remaining > 0 {
        let mut others: Vec<usize> = current.difference(&prioritized).copied().collect();
        others.sort();
        for i in others {
            if remaining == 0 {
                break;
            }
            prioritized.insert(i);
            remaining -= 1;
        }
    }

    prioritized
}

/// Compute numeric-anomaly indices from `analysis.field_stats`.
fn numeric_anomaly_indices(
    config: &SmartCrusherConfig,
    items: &[Value],
    analysis: Option<&ArrayAnalysis>,
) -> BTreeSet<usize> {
    let mut anomalies: BTreeSet<usize> = BTreeSet::new();
    let Some(analysis) = analysis else {
        return anomalies;
    };
    if analysis.field_stats.is_empty() {
        return anomalies;
    }

    for (field_name, stats) in &analysis.field_stats {
        if !is_numeric_field_with_variance(stats) {
            continue;
        }
        let (Some(mean_val), Some(var)) = (stats.mean_val, stats.variance) else {
            continue;
        };
        if var <= 0.0 {
            continue;
        }
        let std = var.sqrt();
        if std <= 0.0 {
            continue;
        }
        let threshold = config.variance_threshold * std;
        for (i, item) in items.iter().enumerate() {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let Some(v) = obj.get(field_name) else {
                continue;
            };
            if let Some(num) = v.as_f64() {
                if !num.is_nan() && (num - mean_val).abs() > threshold {
                    anomalies.insert(i);
                }
            }
        }
    }

    anomalies
}

fn is_numeric_field_with_variance(stats: &FieldStats) -> bool {
    stats.field_type == "numeric" && stats.mean_val.is_some() && stats.variance.unwrap_or(0.0) > 0.0
}

/// Hash function used by all three orchestration helpers.
///
/// Wraps `compute_item_hash` to provide stable JSON serialization
/// for all Value types while preserving type information.
fn item_content_hash(item: &Value, _idx: usize) -> String {
    compute_item_hash(item)
}

