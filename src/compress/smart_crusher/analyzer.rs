//! `SmartAnalyzer` — statistical brain that decides whether and how to crush
//! a JSON array.
//!
//! All eight methods are mirrored here:
//!
//! - `analyze_array` — top-level entry: builds field stats, detects pattern,
//!   runs crushability, picks strategy.
//! - `analyze_field` — per-field statistics (counts, uniqueness, type-specific).
//! - `detect_change_points` — sliding-window mean shift detector for numeric
//!   fields.
//! - `detect_pattern` — classifies the array as `time_series`, `logs`,
//!   `search_results`, or `generic`.
//! - `detect_temporal_field` — structural date/timestamp detection (no
//!   field-name heuristics).
//! - `analyze_crushability` — the main "is it SAFE to crush?" decision.
//! - `select_strategy` — picks `CompressionStrategy` from pattern + crushability.
//! - `estimate_reduction` — coarse compression-ratio estimate for telemetry.
//!
//! # Field iteration order
//!
//! We iterate in sorted order for deterministic behavior.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use super::config::SmartCrusherConfig;
use super::field_detect::{detect_id_field_statistically, detect_score_field_statistically};
use super::stats_math::{mean, sample_stdev, sample_variance};
use super::types::{ArrayAnalysis, CompressionStrategy, CrushabilityAnalysis, FieldStats};

/// Statistical analyzer for compression decisions.
///
/// Stateless aside from `config`. Construct once per request and call
/// `analyze_array` per array.
pub struct SmartAnalyzer {
    pub config: SmartCrusherConfig,
}

impl SmartAnalyzer {
    pub fn new(config: SmartCrusherConfig) -> Self {
        SmartAnalyzer { config }
    }

    /// Top-level analysis.
    pub fn analyze_array(&self, items: &[Value]) -> ArrayAnalysis {
        // Empty / non-dict-first guard: returns NONE strategy with
        // empty stats.
        let first_is_dict = items.first().map(|v| v.is_object()).unwrap_or(false);
        if !first_is_dict {
            return ArrayAnalysis {
                item_count: items.len(),
                field_stats: BTreeMap::new(),
                detected_pattern: "generic".to_string(),
                recommended_strategy: CompressionStrategy::None,
                constant_fields: BTreeMap::new(),
                estimated_reduction: 0.0,
                crushability: None,
            };
        }

        // Union of all keys across dict items. BTreeSet → sorted iteration,
        // matching the BTreeMap we'll build below. Sorted order is the deterministic choice.
        let mut all_keys: BTreeSet<String> = BTreeSet::new();
        for item in items {
            if let Some(obj) = item.as_object() {
                for k in obj.keys() {
                    all_keys.insert(k.clone());
                }
            }
        }

        let mut field_stats: BTreeMap<String, FieldStats> = BTreeMap::new();
        for key in &all_keys {
            field_stats.insert(key.clone(), self.analyze_field(key, items));
        }

        let pattern = self.detect_pattern(&field_stats, items);

        // Constant fields: name → value snapshot. Iteration is BTreeMap
        // sorted, so result map is also key-sorted.
        let constant_fields: BTreeMap<String, Value> = field_stats
            .iter()
            .filter_map(|(k, v)| {
                if v.is_constant {
                    v.constant_value.clone().map(|val| (k.clone(), val))
                } else {
                    None
                }
            })
            .collect();

        let crushability = self.analyze_crushability(items, &field_stats);

        let strategy =
            self.select_strategy(&field_stats, &pattern, items.len(), Some(&crushability));

        let reduction = if strategy == CompressionStrategy::Skip {
            0.0
        } else {
            self.estimate_reduction(&field_stats, strategy, items.len())
        };

        ArrayAnalysis {
            item_count: items.len(),
            field_stats,
            detected_pattern: pattern,
            recommended_strategy: strategy,
            constant_fields,
            estimated_reduction: reduction,
            crushability: Some(crushability),
        }
    }

    /// Per-field statistics.
    pub fn analyze_field(&self, key: &str, items: &[Value]) -> FieldStats {
        // Collect raw values across dict items. `item.get(key)`
        // returns None for missing keys; serde_json returns Value::Null
        // for explicit nulls but no entry for missing. Mirror both as
        // Value::Null in our local `values` vec — downstream
        // non_null_values filter unifies both forms anyway.
        let values: Vec<Value> = items
            .iter()
            .filter_map(|i| i.as_object())
            .map(|obj| obj.get(key).cloned().unwrap_or(Value::Null))
            .collect();
        let non_null: Vec<&Value> = values.iter().filter(|v| !v.is_null()).collect();

        if non_null.is_empty() {
            return FieldStats {
                name: key.to_string(),
                field_type: "null".to_string(),
                count: values.len(),
                unique_count: 0,
                unique_ratio: 0.0,
                is_constant: true,
                constant_value: None,
                min_val: None,
                max_val: None,
                mean_val: None,
                variance: None,
                change_points: Vec::new(),
                avg_length: None,
                top_values: Vec::new(),
            };
        }

        let first = non_null[0];
        // We model JSON's bool/number split directly: serde_json::Value::Bool vs Value::Number.
        let field_type = match first {
            Value::Bool(_) => "boolean",
            Value::Number(_) => "numeric",
            Value::String(_) => "string",
            Value::Object(_) => "object",
            Value::Array(_) => "array",
            _ => "unknown",
        }
        .to_string();

        // Uniqueness: stringify ALL values (including nulls), dedupe, count.
        // `to_repr_string` handles Null as "None", bool as "True"/"False", etc.
        let str_values: Vec<String> = values.iter().map(to_repr_string).collect();
        let unique_set: BTreeSet<&String> = str_values.iter().collect();
        let unique_count = unique_set.len();
        let unique_ratio = if values.is_empty() {
            0.0
        } else {
            unique_count as f64 / values.len() as f64
        };

        let is_constant = unique_count == 1;
        let constant_value = if is_constant {
            Some(non_null[0].clone())
        } else {
            None
        };

        let mut stats = FieldStats {
            name: key.to_string(),
            field_type: field_type.clone(),
            count: values.len(),
            unique_count,
            unique_ratio,
            is_constant,
            constant_value,
            min_val: None,
            max_val: None,
            mean_val: None,
            variance: None,
            change_points: Vec::new(),
            avg_length: None,
            top_values: Vec::new(),
        };

        match field_type.as_str() {
            "numeric" => {
                // Filter to finite f64 only so the same set of
                // values feeds mean/variance/change-points.
                let nums: Vec<f64> = non_null
                    .iter()
                    .filter_map(|v| v.as_f64().filter(|f| f.is_finite()))
                    .collect();
                if !nums.is_empty() {
                    let min_val = nums.iter().cloned().reduce(f64::min);
                    let max_val = nums.iter().cloned().reduce(f64::max);
                    let mean_val = mean(&nums);
                    // `variance = 0` when n < 2.
                    let variance = if nums.len() > 1 {
                        sample_variance(&nums)
                    } else {
                        Some(0.0)
                    };
                    // Drop stats on overflow or math failures: if any computed stat is non-
                    // finite (or computation returned None), drop the
                    // entire numeric stats block and leave change_points
                    // empty.
                    let all_finite = mean_val.map(f64::is_finite).unwrap_or(false)
                        && variance.map(f64::is_finite).unwrap_or(false)
                        && min_val.map(f64::is_finite).unwrap_or(false)
                        && max_val.map(f64::is_finite).unwrap_or(false);
                    if all_finite {
                        stats.min_val = min_val;
                        stats.max_val = max_val;
                        stats.mean_val = mean_val;
                        stats.variance = variance;
                        stats.change_points = self.detect_change_points(&nums, 5);
                    } else {
                        // Reset stats to safe defaults on failure.
                        stats.min_val = None;
                        stats.max_val = None;
                        stats.mean_val = None;
                        stats.variance = Some(0.0);
                        stats.change_points = Vec::new();
                    }
                }
            }
            "string" => {
                let strs: Vec<&str> = non_null.iter().filter_map(|v| v.as_str()).collect();
                if !strs.is_empty() {
                    let lens: Vec<f64> = strs.iter().map(|s| s.chars().count() as f64).collect();
                    stats.avg_length = mean(&lens);
                    stats.top_values = top_n_by_count(&strs, 5);
                }
            }
            _ => {}
        }

        stats
    }

    /// Sliding-window change-point detector. Mirrors `_detect_change_points`
    /// at `smart_crusher.py:1095-1125`.
    pub fn detect_change_points(&self, values: &[f64], window: usize) -> Vec<usize> {
        if values.len() < window * 2 {
            return Vec::new();
        }

        let overall_std = match sample_stdev(values) {
            Some(s) if s > 0.0 => s,
            _ => return Vec::new(),
        };

        let threshold = self.config.variance_threshold * overall_std;

        let mut change_points: Vec<usize> = Vec::new();
        for i in window..values.len().saturating_sub(window) {
            let before = mean(&values[i - window..i]).unwrap_or(0.0);
            let after = mean(&values[i..i + window]).unwrap_or(0.0);
            if (after - before).abs() > threshold {
                change_points.push(i);
            }
        }

        if change_points.is_empty() {
            return Vec::new();
        }

        // Greedy dedup: keep first, then any cp where `cp - last > window`.
        let mut deduped: Vec<usize> = vec![change_points[0]];
        for &cp in &change_points[1..] {
            let last = *deduped.last().unwrap();
            if cp - last > window {
                deduped.push(cp);
            }
        }
        deduped
    }

    /// Pattern classifier. Returns one of `time_series`, `logs`,
    /// `search_results`, `generic`.
    pub fn detect_pattern(
        &self,
        field_stats: &BTreeMap<String, FieldStats>,
        items: &[Value],
    ) -> String {
        let has_timestamp = self.detect_temporal_field(field_stats, items);

        let has_numeric_with_variance = field_stats
            .values()
            .filter(|v| v.field_type == "numeric")
            .any(|v| v.variance.unwrap_or(0.0) > 0.0);

        if has_timestamp && has_numeric_with_variance {
            return "time_series".to_string();
        }

        // logs pattern: high-cardinality string (message) + low-cardinality
        // categorical (level/status).
        let mut has_message_like = false;
        let mut has_level_like = false;
        for stats in field_stats.values() {
            if stats.field_type != "string" {
                continue;
            }
            let avg_len = stats.avg_length.unwrap_or(0.0);
            if stats.unique_ratio > 0.5 && avg_len > 20.0 {
                has_message_like = true;
            } else if stats.unique_ratio < 0.1 && (2..=10).contains(&stats.unique_count) {
                has_level_like = true;
            }
        }
        if has_message_like && has_level_like {
            return "logs".to_string();
        }

        // search_results: any field with score-like signal at confidence >=0.5.
        for stats in field_stats.values() {
            let (is_score, confidence) = detect_score_field_statistically(stats, items);
            if is_score && confidence >= 0.5 {
                return "search_results".to_string();
            }
        }

        "generic".to_string()
    }

    /// Temporal-field detector. Mirrors `_detect_temporal_field` at
    /// `smart_crusher.py:1173-1209`.
    pub fn detect_temporal_field(
        &self,
        field_stats: &BTreeMap<String, FieldStats>,
        items: &[Value],
    ) -> bool {
        for (name, stats) in field_stats {
            match stats.field_type.as_str() {
                "string" => {
                    // First 10 values, str-typed only.
                    let sample: Vec<&str> = items
                        .iter()
                        .take(10)
                        .filter_map(|i| i.as_object())
                        .filter_map(|o| o.get(name))
                        .filter_map(|v| v.as_str())
                        .collect();
                    if sample.is_empty() {
                        continue;
                    }
                    let iso_count = sample
                        .iter()
                        .filter(|s| is_iso_datetime(s) || is_iso_date(s))
                        .count();
                    if (iso_count as f64 / sample.len() as f64) > 0.5 {
                        return true;
                    }
                }
                "numeric" => {
                    if let (Some(mn), Some(_)) = (stats.min_val, stats.max_val) {
                        // Unix epoch range checks. Checking `mn != 0` is used to match behavior.
                        let unix_seconds = (1_000_000_000.0..=2_000_000_000.0).contains(&mn);
                        let unix_millis = (1_000_000_000_000.0..=2_000_000_000_000.0).contains(&mn);
                        if unix_seconds || unix_millis {
                            return true;
                        }
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Crushability decision — the main "is it SAFE?" check.
    ///
    /// Returns a `CrushabilityAnalysis` with the verdict, confidence, and
    /// the signals that drove the decision. Callers consult `crushable`
    /// before invoking any actual compression.
    pub fn analyze_crushability(
        &self,
        items: &[Value],
        field_stats: &BTreeMap<String, FieldStats>,
    ) -> CrushabilityAnalysis {
        use super::outliers::{detect_error_items_for_preservation, detect_structural_outliers};

        let mut signals_present: Vec<String> = Vec::new();
        let mut signals_absent: Vec<String> = Vec::new();

        // 1. ID field detection — keep best (highest confidence) match.
        let mut id_field_name: Option<String> = None;
        let mut id_uniqueness: f64 = 0.0;
        let mut id_confidence: f64 = 0.0;
        for (name, stats) in field_stats {
            let values: Vec<Value> = items
                .iter()
                .filter_map(|i| i.as_object())
                .map(|o| o.get(name).cloned().unwrap_or(Value::Null))
                .collect();
            let (is_id, confidence) = detect_id_field_statistically(stats, &values);
            if is_id && confidence > id_confidence {
                id_field_name = Some(name.clone());
                id_uniqueness = stats.unique_ratio;
                id_confidence = confidence;
            }
        }
        let has_id_field = id_field_name.is_some() && id_confidence >= 0.7;

        // 2. Score field detection — short-circuit on first match.
        let mut has_score_field = false;
        for (name, stats) in field_stats {
            let (is_score, confidence) = detect_score_field_statistically(stats, items);
            if is_score {
                has_score_field = true;
                signals_present.push(format!("score_field:{}(conf={:.2})", name, confidence));
                break;
            }
        }
        if !has_score_field {
            signals_absent.push("score_field".to_string());
        }

        // 3. Structural outliers.
        let outlier_indices = detect_structural_outliers(items);
        let structural_outlier_count = outlier_indices.len();
        if structural_outlier_count > 0 {
            signals_present.push(format!("structural_outliers:{}", structural_outlier_count));
        } else {
            signals_absent.push("structural_outliers".to_string());
        }

        // 3b. Error-keyword fallback when no structural signal.
        let error_keyword_indices = detect_error_items_for_preservation(items, None);
        let keyword_error_count = error_keyword_indices.len();
        if keyword_error_count > 0 && structural_outlier_count == 0 {
            signals_present.push(format!("error_keywords:{}", keyword_error_count));
        }

        let error_count = structural_outlier_count.max(keyword_error_count);

        // 4. Numeric anomalies (>variance_threshold σ from mean).
        let mut anomaly_indices: BTreeSet<usize> = BTreeSet::new();
        for stats in field_stats.values() {
            if stats.field_type != "numeric" {
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
            let threshold = self.config.variance_threshold * std;
            for (i, item) in items.iter().enumerate() {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let Some(v) = obj.get(&stats.name) else {
                    continue;
                };
                if let Some(num) = v.as_f64() {
                    if !num.is_nan() && (num - mean_val).abs() > threshold {
                        anomaly_indices.insert(i);
                    }
                }
            }
        }
        let anomaly_count = anomaly_indices.len();
        if anomaly_count > 0 {
            signals_present.push(format!("anomalies:{}", anomaly_count));
        } else {
            signals_absent.push("anomalies".to_string());
        }

        // 5. Average string uniqueness, EXCLUDING the detected ID field if high confidence.
        let id_name_ref = if has_id_field { id_field_name.as_deref() } else { None };
        let string_ratios: Vec<f64> = field_stats
            .values()
            .filter(|s| s.field_type == "string" && Some(s.name.as_str()) != id_name_ref)
            .map(|s| s.unique_ratio)
            .collect();
        let avg_string_uniqueness = if string_ratios.is_empty() {
            0.0
        } else {
            mean(&string_ratios).unwrap_or(0.0)
        };

        let non_id_numeric_ratios: Vec<f64> = field_stats
            .values()
            .filter(|s| s.field_type == "numeric" && Some(s.name.as_str()) != id_name_ref)
            .map(|s| s.unique_ratio)
            .collect();
        let avg_non_id_numeric_uniqueness = if non_id_numeric_ratios.is_empty() {
            0.0
        } else {
            mean(&non_id_numeric_ratios).unwrap_or(0.0)
        };

        let max_uniqueness = if has_id_field {
            avg_string_uniqueness.max(id_uniqueness).max(0.0)
        } else {
            avg_string_uniqueness.max(0.0)
        };
        let non_id_content_uniqueness = avg_string_uniqueness.max(avg_non_id_numeric_uniqueness);

        // 6. Change points.
        let has_change_points = field_stats
            .values()
            .filter(|s| s.field_type == "numeric")
            .any(|s| !s.change_points.is_empty());
        if has_change_points {
            signals_present.push("change_points".to_string());
        }

        let has_any_signal = !signals_present.is_empty();

        // Decision tree — order matters; case-by-case evaluation.
        let make = |crushable: bool,
                    confidence: f64,
                    reason: &str,
                    signals_present: Vec<String>,
                    signals_absent: Vec<String>|
         -> CrushabilityAnalysis {
            CrushabilityAnalysis {
                crushable,
                confidence,
                reason: reason.to_string(),
                signals_present,
                signals_absent,
                has_id_field,
                id_uniqueness,
                avg_string_uniqueness,
                has_score_field,
                error_item_count: error_count,
                anomaly_count,
            }
        };

        // Case 0: repetitive content with unique IDs.
        if non_id_content_uniqueness < 0.1 && has_id_field {
            let mut sp = signals_present.clone();
            sp.push("repetitive_content".to_string());
            return make(
                true,
                0.85,
                "repetitive_content_with_ids",
                sp,
                signals_absent,
            );
        }

        // Case 1: low uniqueness.
        if max_uniqueness < 0.3 {
            return make(
                true,
                0.9,
                "low_uniqueness_safe_to_sample",
                signals_present,
                signals_absent,
            );
        }

        // Case 2: high uniqueness + ID field + NO signal = DON'T CRUSH.
        if has_id_field && max_uniqueness > 0.8 && !has_any_signal {
            return make(
                false,
                0.85,
                "unique_entities_no_signal",
                signals_present,
                signals_absent,
            );
        }

        // Case 3: high uniqueness + has signal = crush.
        if max_uniqueness > 0.8 && has_any_signal {
            return make(
                true,
                0.7,
                "unique_entities_with_signal",
                signals_present,
                signals_absent,
            );
        }

        // Case 4: medium uniqueness + no signal = don't crush.
        if !has_any_signal {
            return make(
                false,
                0.6,
                "medium_uniqueness_no_signal",
                signals_present,
                signals_absent,
            );
        }

        // Case 5: medium uniqueness + has signal = crush with caution.
        make(
            true,
            0.5,
            "medium_uniqueness_with_signal",
            signals_present,
            signals_absent,
        )
    }

    /// Strategy selector. Mirrors `_select_strategy` at
    /// `smart_crusher.py:1432-1466`.
    pub fn select_strategy(
        &self,
        field_stats: &BTreeMap<String, FieldStats>,
        pattern: &str,
        item_count: usize,
        crushability: Option<&CrushabilityAnalysis>,
    ) -> CompressionStrategy {
        if item_count < self.config.min_items_to_analyze {
            return CompressionStrategy::None;
        }

        if let Some(c) = crushability {
            if !c.crushable {
                return CompressionStrategy::Skip;
            }
        }

        if pattern == "time_series" {
            let has_change_points = field_stats
                .values()
                .filter(|f| f.field_type == "numeric")
                .any(|f| !f.change_points.is_empty());
            if has_change_points {
                return CompressionStrategy::TimeSeries;
            }
        }

        if pattern == "logs" {
            // First sorted iteration order match wins. With
            // sorted iteration, this is deterministic.
            let message_field = field_stats
                .iter()
                .find(|(k, _)| k.to_lowercase().contains("message"))
                .map(|(_, v)| v);
            if let Some(mf) = message_field {
                if mf.unique_ratio < 0.5 {
                    return CompressionStrategy::ClusterSample;
                }
            }
        }

        if pattern == "search_results" {
            return CompressionStrategy::TopN;
        }

        CompressionStrategy::SmartSample
    }

    /// Reduction estimator. Returns ∈ [0, 0.95].
    pub fn estimate_reduction(
        &self,
        field_stats: &BTreeMap<String, FieldStats>,
        strategy: CompressionStrategy,
        _item_count: usize,
    ) -> f64 {
        if strategy == CompressionStrategy::None {
            return 0.0;
        }

        // Guard against division by zero:
        if field_stats.is_empty() {
            return 0.0;
        }

        let constant_count = field_stats.values().filter(|v| v.is_constant).count();
        let constant_ratio = constant_count as f64 / field_stats.len() as f64;

        let base = match strategy {
            CompressionStrategy::TimeSeries => 0.7,
            CompressionStrategy::ClusterSample => 0.8,
            CompressionStrategy::TopN => 0.6,
            CompressionStrategy::SmartSample => 0.5,
            _ => 0.3,
        };

        (base + constant_ratio * 0.2).min(0.95)
    }
}

// ---------- helpers ----------

/// Standard string representation for uniqueness counting.
/// - `Null` → `"None"`
/// - `True`/`False` → `"True"`/`"False"`
/// - numbers → str-form of the number
/// - strings → unquoted body
/// - dict/list → JSON stringified representation.
fn to_repr_string(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        // Nested values aren't typically the unique-count drivers, so we
        // stringify with JSON. Used only for cardinality, not surfaced.
        _ => v.to_string(),
    }
}

/// Counter equivalent. Returns up to `n` (value, count)
/// pairs sorted by count descending; ties broken by FIRST OCCURRENCE order.
fn top_n_by_count(strs: &[&str], n: usize) -> Vec<(String, usize)> {
    use std::collections::HashMap;

    let mut order: Vec<&str> = Vec::new();
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for &s in strs {
        if !counts.contains_key(s) {
            order.push(s);
        }
        *counts.entry(s).or_insert(0) += 1;
    }

    // Stable sort by count desc preserves first-occurrence tie order.
    let mut pairs: Vec<(&&str, usize)> = order.iter().map(|k| (k, counts[k])).collect();
    pairs.sort_by_key(|b| std::cmp::Reverse(b.1));

    pairs
        .into_iter()
        .take(n)
        .map(|(k, c)| ((*k).to_string(), c))
        .collect()
}

// ISO 8601 patterns — matches:
//   `^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}`
//   `^\d{4}-\d{2}-\d{2}$`
// Implemented as direct char-position checks rather than full regex to
// avoid pulling in a regex compilation for every call site. Same
// behavior as standard prefix checks.
fn is_iso_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 19 {
        return false;
    }
    is_digit(b[0])
        && is_digit(b[1])
        && is_digit(b[2])
        && is_digit(b[3])
        && b[4] == b'-'
        && is_digit(b[5])
        && is_digit(b[6])
        && b[7] == b'-'
        && is_digit(b[8])
        && is_digit(b[9])
        && (b[10] == b'T' || b[10] == b' ')
        && is_digit(b[11])
        && is_digit(b[12])
        && b[13] == b':'
        && is_digit(b[14])
        && is_digit(b[15])
        && b[16] == b':'
        && is_digit(b[17])
        && is_digit(b[18])
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 {
        return false;
    }
    is_digit(b[0])
        && is_digit(b[1])
        && is_digit(b[2])
        && is_digit(b[3])
        && b[4] == b'-'
        && is_digit(b[5])
        && is_digit(b[6])
        && b[7] == b'-'
        && is_digit(b[8])
        && is_digit(b[9])
}

#[inline]
fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

