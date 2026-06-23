//! Index-set orchestration helpers used by every planning method.
//!
//! Includes helpers to deduplicate indices by content, fill remaining slots
//! to meet targeted counts, and prioritize indices by critical criteria.

use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

use super::config::SmartCrusherConfig;
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
        let h = compute_item_hash(&items[idx]);
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
            seen.insert(compute_item_hash(&items[idx]));
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
            let h = compute_item_hash(&items[idx]);
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
/// 3. **Already under budget?** Add critical items (capped) and return.
/// 4. **Otherwise**: keep critical items first. Then add first-3 + last-2
///    if room. Then fill remaining with non-critical kept indices in
///    ascending order.
///
/// `critical_indices` is the pre-computed union of constraint results
/// (errors + structural outliers) and numeric anomaly indices from
/// the planning phase. Passing it here avoids redundant re-detection.
///
/// May return MORE than `effective_max` items when critical items
/// alone exceed the budget, but capped at `2 × effective_max` to
/// prevent error-heavy arrays from defeating compression entirely.
pub fn prioritize_indices(
    config: &SmartCrusherConfig,
    keep_indices: &BTreeSet<usize>,
    items: &[Value],
    n: usize,
    critical_indices: &BTreeSet<usize>,
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

    if current.len() <= effective_max {
        // Under budget — guarantee preservation of critical items,
        // capped at 2 * effective_max to prevent error-heavy arrays from
        // defeating compression entirely.
        let capped: BTreeSet<usize> = critical_indices
            .iter()
            .copied()
            .take(effective_max * 2)
            .collect();
        current.extend(&capped);
        return current;
    }

    // Over budget — critical-items-first prioritization, capped at
    // 2 * effective_max to prevent error-heavy arrays from defeating
    // compression entirely.
    let mut prioritized: BTreeSet<usize> = critical_indices
        .iter()
        .copied()
        .take(effective_max * 2)
        .collect();

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
        let others: Vec<usize> = current.difference(&prioritized).copied().collect();
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



