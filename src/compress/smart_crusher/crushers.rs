//! Three universal crushers for non-dict-array JSON shapes.
//!
//! - `crush_string_array`  ← `_crush_string_array`  (line 2727)
//! - `crush_number_array`  ← `_crush_number_array`  (line 2810) — has BUG #1
//! - `crush_object`        ← `_crush_object`        (line 3015)
//!
//! Each takes a `&SmartCrusherConfig`, a `bias` multiplier, and returns
//! `(crushed_items, strategy_string)`. Schema-preserving: the output
//! contains only items/values from the original; no generated text or
//! summary objects sneak in.
//!
//! `_crush_array` (the dict-array orchestrator) and `_crush_mixed_array`
//! (the type-grouped fallback) live in a later commit because they pull
//! in the planning + execution + TOIN/CCR scaffolding.

use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashSet};

use super::config::SmartCrusherConfig;
use super::error_keywords::ERROR_KEYWORDS;
use super::stats_math::{format_g, mean, median, sample_stdev};
use crate::compress::adaptive_sizer::compute_optimal_k;

pub fn compute_k_split(
    items: &[&str],
    config: &SmartCrusherConfig,
    bias: f64,
) -> (usize, usize, usize, usize) {
    let max_k = if config.max_items_after_crush > 0 {
        Some(config.max_items_after_crush)
    } else {
        None
    };
    let k_total = compute_optimal_k(items, bias, 3, max_k);
    // round-half-to-even. Rust's f64::round_ties_even() mirrors that exactly.
    let k_first_raw = 1_usize.max(round_ties_even(k_total as f64 * config.first_fraction) as usize);
    let k_last_raw = 1_usize.max(round_ties_even(k_total as f64 * config.last_fraction) as usize);
    // Clamp so `k_first + k_last <= k_total`.
    let k_first = k_first_raw.min(k_total);
    let k_last = k_last_raw.min(k_total.saturating_sub(k_first));
    let k_importance = k_total.saturating_sub(k_first + k_last);
    (k_total, k_first, k_last, k_importance)
}

/// Crush an array of strings.
///
/// Strategy:
/// 1. Adaptive K via Kneedle (passthrough on `n <= 8`).
/// 2. **Always keep**: error-keyword strings + length-anomaly strings.
/// 3. **Boundary keep**: first K_first + last K_last.
/// 4. **Stride-fill**: stride-based diverse sampling, dedup by content.
/// 5. Output preserves original array order.
///
/// `bias` is the compression-aggressiveness multiplier used by
/// `compute_optimal_k`.
pub fn crush_string_array(
    items: &[&str],
    config: &SmartCrusherConfig,
    bias: f64,
) -> (Vec<String>, String) {
    let n = items.len();
    if n <= 8 {
        return (
            items.iter().map(|s| (*s).to_string()).collect(),
            "string:passthrough".to_string(),
        );
    }

    // K split. We feed the raw &str refs since adaptive_sizer's input 
    // is string representation in importance order.
    let (k_total, k_first, k_last, _k_importance) = compute_k_split(items, config, bias);

    // 1. Error-keyword indices.
    let mut error_indices: BTreeSet<usize> = BTreeSet::new();
    for (i, s) in items.iter().enumerate() {
        let lower = s.to_lowercase();
        if ERROR_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
            error_indices.insert(i);
        }
    }

    // 2. Length anomaly indices.
    let lengths: Vec<f64> = items.iter().map(|s| s.chars().count() as f64).collect();
    let mut anomaly_indices: BTreeSet<usize> = BTreeSet::new();
    if lengths.len() > 1 {
        let mean_len = mean(&lengths).unwrap_or(0.0);
        // Use sample standard deviation.
        let std_len = sample_stdev(&lengths).unwrap_or(0.0);
        if std_len > 0.0 {
            let threshold = config.variance_threshold * std_len;
            for (i, &length) in lengths.iter().enumerate() {
                if (length - mean_len).abs() > threshold {
                    anomaly_indices.insert(i);
                }
            }
        }
    }

    // 3. Boundary indices.
    let first_indices: BTreeSet<usize> = (0..k_first.min(n)).collect();
    let last_start = n.saturating_sub(k_last);
    let last_indices: BTreeSet<usize> = (last_start..n).collect();

    // 4. Combine.
    let mut keep_indices: BTreeSet<usize> = BTreeSet::new();
    keep_indices.extend(error_indices.iter().copied());
    keep_indices.extend(anomaly_indices.iter().copied());
    keep_indices.extend(first_indices.iter().copied());
    keep_indices.extend(last_indices.iter().copied());

    // Pre-populate seen_strings from current keeps.
    let mut seen: HashSet<&str> = HashSet::new();
    for &i in &keep_indices {
        seen.insert(items[i]);
    }

    // 5. Stride-fill remaining budget.
    let mut dedup_count: usize = 0;
    let remaining_budget = k_total.saturating_sub(keep_indices.len());
    if remaining_budget > 0 {
        let stride = ((n.saturating_sub(1)) / (remaining_budget + 1)).max(1);
        // Cap value calculation:
        let cap = k_total + error_indices.len() + anomaly_indices.len();
        let mut i: usize = 0;
        while i < n {
            if keep_indices.len() >= cap {
                break;
            }
            if !keep_indices.contains(&i) {
                if !seen.contains(items[i]) {
                    keep_indices.insert(i);
                    seen.insert(items[i]);
                } else {
                    dedup_count += 1;
                }
            }
            i += stride;
        }
    }

    // 6. Build output preserving original order.
    let result: Vec<String> = keep_indices.iter().map(|&i| items[i].to_string()).collect();

    let mut strategy = format!("string:adaptive({}->{}", n, result.len());
    if dedup_count > 0 {
        strategy.push_str(&format!(",dedup={}", dedup_count));
    }
    if !error_indices.is_empty() {
        strategy.push_str(&format!(",errors={}", error_indices.len()));
    }
    strategy.push(')');

    (result, strategy)
}

/// Crush an array of numbers.
pub fn crush_number_array(
    items: &[Value],
    config: &SmartCrusherConfig,
    bias: f64,
) -> (Vec<Value>, String) {
    let n = items.len();
    if n <= 8 {
        return (items.to_vec(), "number:passthrough".to_string());
    }

    // Filter to finite f64 only.
    let finite: Vec<f64> = items
        .iter()
        .filter_map(|v| v.as_f64().filter(|f| f.is_finite()))
        .collect();
    if finite.is_empty() {
        return (items.to_vec(), "number:no_finite".to_string());
    }

    // K split.
    let item_strings: Vec<String> = items.iter().map(|v| v.to_string()).collect();
    let item_str_refs: Vec<&str> = item_strings.iter().map(|s| s.as_str()).collect();
    let (k_total, k_first, k_last, _) = compute_k_split(&item_str_refs, config, bias);

    // Statistics.
    let mean_val = mean(&finite).unwrap_or(0.0);
    let median_val = median(&finite).unwrap_or(0.0);
    let std_val = if finite.len() > 1 {
        sample_stdev(&finite).unwrap_or(0.0)
    } else {
        0.0
    };

    // Sorted for percentiles.
    let mut sorted_finite: Vec<f64> = finite.clone();
    sorted_finite.sort_by(f64::total_cmp);

    // Percentile calculations via linear interpolation:
    // Matches numpy's "linear" method exactly:
    //   index = q * (n - 1)
    //   if integer: sorted[index]
    //   else: linear interpolate between floor and ceil
    let p25 = percentile_linear(&sorted_finite, 0.25);
    let p75 = percentile_linear(&sorted_finite, 0.75);

    // Outliers (>variance_threshold standard deviations from mean).
    let mut outlier_indices: BTreeSet<usize> = BTreeSet::new();
    if std_val > 0.0 {
        let threshold = config.variance_threshold * std_val;
        for (i, val) in items.iter().enumerate() {
            if let Some(num) = val.as_f64().filter(|f| f.is_finite()) {
                if (num - mean_val).abs() > threshold {
                    outlier_indices.insert(i);
                }
            }
        }
    }

    // Change points via window-mean comparison. Guards on `n > 10`.
    let mut change_indices: BTreeSet<usize> = BTreeSet::new();
    if config.preserve_change_points && n > 10 {
        let window: usize = 5;
        for i in window..n.saturating_sub(window) {
            // Collects only finite items in each window; it's possible
            // for windows to be empty if all items in a slice are non-finite.
            let left: Vec<f64> = items[i - window..i]
                .iter()
                .filter_map(|v| v.as_f64().filter(|f| f.is_finite()))
                .collect();
            let right: Vec<f64> = items[i..i + window]
                .iter()
                .filter_map(|v| v.as_f64().filter(|f| f.is_finite()))
                .collect();
            if !left.is_empty() && !right.is_empty() {
                let left_mean = mean(&left).unwrap_or(0.0);
                let right_mean = mean(&right).unwrap_or(0.0);
                if std_val > 0.0
                    && (right_mean - left_mean).abs() > config.variance_threshold * std_val
                {
                    change_indices.insert(i);
                }
            }
        }
    }

    // Boundary.
    let first_indices: BTreeSet<usize> = (0..k_first.min(n)).collect();
    let last_start = n.saturating_sub(k_last);
    let last_indices: BTreeSet<usize> = (last_start..n).collect();

    // Combine.
    let mut keep_indices: BTreeSet<usize> = BTreeSet::new();
    keep_indices.extend(outlier_indices.iter().copied());
    keep_indices.extend(change_indices.iter().copied());
    keep_indices.extend(first_indices.iter().copied());
    keep_indices.extend(last_indices.iter().copied());

    // Stride-fill. Cap = k_total + len(outlier_indices).
    let remaining_budget = k_total.saturating_sub(keep_indices.len());
    if remaining_budget > 0 {
        let stride = ((n.saturating_sub(1)) / (remaining_budget + 1)).max(1);
        let cap = k_total + outlier_indices.len();
        let mut i: usize = 0;
        while i < n {
            if keep_indices.len() >= cap {
                break;
            }
            if !keep_indices.contains(&i) {
                keep_indices.insert(i);
            }
            i += stride;
        }
    }

    // Build output: kept values only (schema-preserving — no summary prefix).
    let kept_values: Vec<Value> = keep_indices.iter().map(|&i| items[i].clone()).collect();

    let mn = finite_min(&finite);
    let mx = finite_max(&finite);
    let mut strategy = format!(
        "number:adaptive({}->{},min={},max={},mean={},median={},stddev={},p25={},p75={}",
        n,
        kept_values.len(),
        format_number_repr(mn),
        format_number_repr(mx),
        format_g(mean_val),
        format_g(median_val),
        format_g(std_val),
        format_g(p25),
        format_g(p75),
    );
    if !outlier_indices.is_empty() {
        strategy.push_str(&format!(",outliers={}", outlier_indices.len()));
    }
    if !change_indices.is_empty() {
        strategy.push_str(&format!(",change_points={}", change_indices.len()));
    }
    strategy.push(')');

    (kept_values, strategy)
}

/// Crush a JSON object by selecting the most informative keys.
///
/// Treats key-value pairs as items and applies
/// `compute_optimal_k` directly on `"key: value"` strings.
/// Always-kept rules:
/// - keys whose value contains an error keyword.
/// - keys with small total token estimate (<=12 tokens).
/// - first K_first and last K_last keys (insertion order).
pub fn crush_object(
    obj: &Map<String, Value>,
    config: &SmartCrusherConfig,
    bias: f64,
) -> (Map<String, Value>, String) {
    let n = obj.len();
    if n <= 8 {
        return (obj.clone(), "object:passthrough".to_string());
    }

    // Estimate tokens per key-value pair.
    let mut kv_tokens: Vec<(String, usize)> = Vec::with_capacity(n);
    let mut total_tokens: usize = 0;
    for (key, val) in obj {
        let val_str = serde_json::to_string(val).unwrap_or_default();
        let tokens = val_str.len() / 4 + key.len() / 4 + 2;
        kv_tokens.push((key.clone(), tokens));
        total_tokens += tokens;
    }

    if total_tokens < config.min_tokens_to_crush {
        return (obj.clone(), "object:passthrough".to_string());
    }

    // Compute adaptive K on key-value string representations.
    let keys: Vec<&String> = obj.keys().collect();
    let kv_strings: Vec<String> = keys
        .iter()
        .map(|k| {
            format!(
                "{}: {}",
                k,
                serde_json::to_string(&obj[k.as_str()]).unwrap_or_default()
            )
        })
        .collect();
    let kv_refs: Vec<&str> = kv_strings.iter().map(|s| s.as_str()).collect();

    let max_k = if config.max_items_after_crush > 0 {
        Some(config.max_items_after_crush)
    } else {
        None
    };
    let k_total = compute_optimal_k(&kv_refs, bias, 3, max_k);

    if k_total >= n {
        return (obj.clone(), "object:passthrough".to_string());
    }

    // Always keep: error-keyword values.
    let mut keep_keys: HashSet<String> = HashSet::new();
    for (key, val) in obj {
        let val_str = serde_json::to_string(val)
            .unwrap_or_default()
            .to_lowercase();
        if ERROR_KEYWORDS.iter().any(|kw| val_str.contains(kw)) {
            keep_keys.insert(key.clone());
        }
    }

    // Always keep: small values (cheap to keep).
    // tokens <= 12.
    let small_threshold_tokens = 50_usize / 4;
    for (key, tokens) in &kv_tokens {
        if *tokens <= small_threshold_tokens {
            keep_keys.insert(key.clone());
        }
    }

    // Boundary: first K_first and last K_last (over the key insertion order).
    let k_first = 1_usize.max(round_ties_even(k_total as f64 * config.first_fraction) as usize);
    let k_last = 1_usize.max(round_ties_even(k_total as f64 * config.last_fraction) as usize);
    for k in keys.iter().take(k_first) {
        keep_keys.insert((*k).clone());
    }
    for k in keys.iter().rev().take(k_last) {
        keep_keys.insert((*k).clone());
    }

    // Stride fill.
    let remaining = k_total.saturating_sub(keep_keys.len());
    if remaining > 0 {
        let stride = ((n.saturating_sub(1)) / (remaining + 1)).max(1);
        let mut i: usize = 0;
        while i < n {
            let error_kept_count = keep_keys
                .iter()
                .filter(|k| {
                    let s = serde_json::to_string(&obj[k.as_str()])
                        .unwrap_or_default()
                        .to_lowercase();
                    ERROR_KEYWORDS.iter().any(|kw| s.contains(kw))
                })
                .count();
            if keep_keys.len() >= k_total + error_kept_count {
                break;
            }
            keep_keys.insert(keys[i].clone());
            i += stride;
        }
    }

    // Build output preserving original key insertion order.
    let mut result: Map<String, Value> = Map::new();
    for k in &keys {
        if keep_keys.contains(k.as_str()) {
            result.insert((*k).clone(), obj[k.as_str()].clone());
        }
    }

    let strategy = format!("object:adaptive({}->{} keys)", n, result.len());
    (result, strategy)
}

// ---------- helpers ----------

/// Linear-interpolation percentile (numpy "linear" method).
fn percentile_linear(sorted_values: &[f64], q: f64) -> f64 {
    let n = sorted_values.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return sorted_values[0];
    }
    let pos = q * (n - 1) as f64;
    let lo = pos as usize;
    let hi = if lo + 1 < n { lo + 1 } else { lo };
    let frac = pos - lo as f64;
    sorted_values[lo] * (1.0 - frac) + sorted_values[hi] * frac
}

fn finite_min(values: &[f64]) -> f64 {
    values.iter().cloned().reduce(f64::min).unwrap_or(0.0)
}

fn finite_max(values: &[f64]) -> f64 {
    values.iter().cloned().reduce(f64::max).unwrap_or(0.0)
}

/// Banker's rounding (round-half-to-even).
fn round_ties_even(x: f64) -> f64 {
    x.round_ties_even()
}

/// Format a number for default representation. 
/// Integers print without a decimal; floats print
/// with their natural decimal form. We approximate:
/// values exactly representable as `i64` get integer formatting.
fn format_number_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    if x.fract() == 0.0 && x.abs() < 1e16 {
        return format!("{}", x as i64);
    }
    // Display output representation is shortest round-trip.
    format!("{}", x)
}

