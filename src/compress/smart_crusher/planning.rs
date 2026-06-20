//! Strategy-specific compression planning.
//!
//! Each planner produces a `CompressionPlan` whose `keep_indices` is a sorted list
//! of original-array indices to retain.
//!
//! All four planners share the same skeleton:
//!
//! 1. **Anchor selection** — `AnchorSelector::select_anchors` for
//!    position-based slots.
//! 2. **Strategy-specific signals** — outliers, change points, top-N
//!    by score, message-cluster reps, etc.
//! 3. **Error keywords** — preservation guarantee.
//! 4. **Query anchors** (deterministic exact match) and **relevance
//!    scoring** (probabilistic) — both gated on `query_context`.
//! 5. **Preserve fields** — items where a query token matches a
//!    learned-important field's value.
//! 6. **Prioritize** — dedup + fill + over-budget pruning.

use md5::{Digest, Md5};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use super::analyzer::SmartAnalyzer;
use super::anchors::{extract_query_anchors, item_matches_anchors};
use super::config::SmartCrusherConfig;
use super::field_detect::detect_score_field_statistically;
use super::hashing::hash_field_name;
use super::orchestration::prioritize_indices;
use super::traits::Constraint;
use super::types::{ArrayAnalysis, CompressionPlan, CompressionStrategy, FieldStats};
// Note: `detect_error_items_for_preservation` and `detect_structural_outliers`
// are still imported transitively by `constraints.rs` (via `KeepErrorsConstraint`
// and `KeepStructuralOutliersConstraint`). Planning no longer calls them
// directly; it iterates `self.constraints` via `apply_constraints`.
use crate::compress::relevance::RelevanceScorer;
use crate::compress::anchor_selector::{AnchorSelector, DataPattern};

/// Stateless planner that owns its dependencies.
pub struct SmartCrusherPlanner<'a> {
    pub config: &'a SmartCrusherConfig,
    pub anchor_selector: &'a AnchorSelector,
    pub scorer: &'a (dyn RelevanceScorer + Send + Sync),
    pub analyzer: &'a SmartAnalyzer,
    /// User-configured must-keep predicates. The plan methods union
    /// the output of every constraint into the kept set; OSS default
    /// composition includes `KeepErrorsConstraint` and
    /// `KeepStructuralOutliersConstraint`, reproducing the pre-PR1
    /// hardcoded behavior byte-for-byte.
    pub constraints: &'a [Box<dyn Constraint>],
}

impl<'a> SmartCrusherPlanner<'a> {
    pub fn new(
        config: &'a SmartCrusherConfig,
        anchor_selector: &'a AnchorSelector,
        scorer: &'a (dyn RelevanceScorer + Send + Sync),
        analyzer: &'a SmartAnalyzer,
        constraints: &'a [Box<dyn Constraint>],
    ) -> Self {
        SmartCrusherPlanner {
            config,
            anchor_selector,
            scorer,
            analyzer,
            constraints,
        }
    }

    /// Apply every configured `Constraint::must_keep` and union the
    /// results into `keep`.
    fn apply_constraints(
        &self,
        items: &[Value],
        item_strings: Option<&[String]>,
        keep: &mut BTreeSet<usize>,
    ) {
        for c in self.constraints {
            keep.extend(c.must_keep(items, item_strings));
        }
    }

    /// Top-level dispatcher.
    pub fn create_plan(
        &self,
        analysis: &ArrayAnalysis,
        items: &[Value],
        query_context: &str,
        preserve_fields: Option<&[String]>,
        effective_max_items: Option<usize>,
        item_strings: Option<&[String]>,
    ) -> CompressionPlan {
        let max_items = effective_max_items.unwrap_or(self.config.max_items_after_crush);

        let mut plan = CompressionPlan {
            strategy: analysis.recommended_strategy,
            constant_fields: if self.config.factor_out_constants {
                analysis.constant_fields.clone()
            } else {
                BTreeMap::new()
            },
            ..CompressionPlan::default()
        };

        // None or Skip path: keep all items.
        if analysis.recommended_strategy == CompressionStrategy::None
            || analysis.recommended_strategy == CompressionStrategy::Skip
        {
            plan.keep_indices = (0..items.len()).collect();
            return plan;
        }

        match analysis.recommended_strategy {
            CompressionStrategy::TimeSeries => self.plan_time_series(
                analysis,
                items,
                plan,
                query_context,
                preserve_fields,
                max_items,
                item_strings,
            ),
            CompressionStrategy::ClusterSample => self.plan_cluster_sample(
                analysis,
                items,
                plan,
                query_context,
                preserve_fields,
                max_items,
                item_strings,
            ),
            CompressionStrategy::TopN => self.plan_top_n(
                analysis,
                items,
                plan,
                query_context,
                preserve_fields,
                max_items,
                item_strings,
            ),
            // SmartSample, None, Skip-already-handled, all fall here.
            _ => self.plan_smart_sample(
                analysis,
                items,
                plan,
                query_context,
                preserve_fields,
                max_items,
                item_strings,
            ),
        }
    }

    /// Plan SMART_SAMPLE — the default/fallback strategy.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_smart_sample(
        &self,
        analysis: &ArrayAnalysis,
        items: &[Value],
        mut plan: CompressionPlan,
        query_context: &str,
        preserve_fields: Option<&[String]>,
        max_items: usize,
        item_strings: Option<&[String]>,
    ) -> CompressionPlan {
        let n = items.len();
        let mut keep: BTreeSet<usize> = BTreeSet::new();

        // 1. Dynamic anchors.
        let anchor_pattern = map_to_anchor_pattern(CompressionStrategy::SmartSample);
        keep.extend(self.anchor_selector.select_anchors(
            items,
            max_items,
            anchor_pattern,
            query_or_none(query_context),
        ));

        // 2. Structural outliers + error keywords (configured via Constraint trait).
        self.apply_constraints(items, item_strings, &mut keep);

        // 3. Numeric anomalies (>variance_threshold σ from per-field mean).
        for (name, stats) in &analysis.field_stats {
            for_each_anomaly(
                name,
                stats,
                items,
                self.config.variance_threshold,
                &mut keep,
            );
        }

        // 4. Items around change points (window of ±1).
        if self.config.preserve_change_points {
            for stats in analysis.field_stats.values() {
                for &cp in &stats.change_points {
                    for offset in -1_isize..=1 {
                        let idx = cp as isize + offset;
                        if idx >= 0 && (idx as usize) < n {
                            keep.insert(idx as usize);
                        }
                    }
                }
            }
        }

        // 5/6. Query-anchor matches + relevance scores.
        self.apply_query_signals(items, query_context, item_strings, &mut keep, false);

        // TOIN preserve_fields.
        self.apply_preserve_field_matches(items, query_context, preserve_fields, &mut keep);

        let final_keep =
            prioritize_indices(self.config, &keep, items, n, Some(analysis), max_items);
        plan.keep_indices = final_keep.into_iter().collect();
        plan
    }

    /// Plan TOP_N — for ranked/scored data.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_top_n(
        &self,
        analysis: &ArrayAnalysis,
        items: &[Value],
        mut plan: CompressionPlan,
        query_context: &str,
        preserve_fields: Option<&[String]>,
        max_items: usize,
        item_strings: Option<&[String]>,
    ) -> CompressionPlan {
        // Locate the highest-confidence score field. If none, fall back
        // to plan_smart_sample.
        let mut score_field: Option<&str> = None;
        let mut max_confidence = 0.0_f64;
        for (name, stats) in &analysis.field_stats {
            let (is_score, confidence) = detect_score_field_statistically(stats, items);
            if is_score && confidence > max_confidence {
                score_field = Some(name);
                max_confidence = confidence;
            }
        }

        let Some(score_field) = score_field else {
            return self.plan_smart_sample(
                analysis,
                items,
                plan,
                query_context,
                preserve_fields,
                max_items,
                item_strings,
            );
        };

        plan.sort_field = Some(score_field.to_string());
        let mut keep: BTreeSet<usize> = BTreeSet::new();

        // 1. TOP-N by score (primary signal).
        let mut scored: Vec<(usize, f64)> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let score = item
                    .as_object()
                    .and_then(|o| o.get(score_field))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                (i, score)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let top_count = max_items.saturating_sub(3);
        for (idx, _) in scored.iter().take(top_count) {
            keep.insert(*idx);
        }

        // 2. Structural outliers + error keywords (configured via Constraint trait).
        self.apply_constraints(items, item_strings, &mut keep);

        // 3. Query-anchor matches (additive — preserved regardless of top-N).
        if !query_context.is_empty() {
            let anchors = extract_query_anchors(query_context);
            for (i, item) in items.iter().enumerate() {
                if !keep.contains(&i) && item_matches_anchors(item, &anchors) {
                    keep.insert(i);
                }
            }
        }

        // 4. HIGH-CONFIDENCE relevance matches (additive only).
        if !query_context.is_empty() {
            let owned_strings: Vec<String>;
            let strs: Vec<&str> = match item_strings {
                Some(arr) => arr.iter().map(|s| s.as_str()).collect(),
                None => {
                    owned_strings = items
                        .iter()
                        .map(|i| serde_json::to_string(i).unwrap_or_default())
                        .collect();
                    owned_strings.iter().map(|s| s.as_str()).collect()
                }
            };
            let scores = self.scorer.score_batch(&strs, query_context);
            // Higher threshold and capped count to avoid adding everything.
            let high_threshold = (self.config.relevance_threshold * 2.0).max(0.5);
            let max_relevance_adds = 3_usize;
            let mut added = 0;
            for (i, sc) in scores.iter().enumerate() {
                if !keep.contains(&i) && sc.score >= high_threshold {
                    keep.insert(i);
                    added += 1;
                    if added >= max_relevance_adds {
                        break;
                    }
                }
            }
        }

        self.apply_preserve_field_matches(items, query_context, preserve_fields, &mut keep);

        let final_keep =
            prioritize_indices(self.config, &keep, items, items.len(), Some(analysis), max_items);
        plan.keep_count = final_keep.len();
        plan.keep_indices = final_keep.into_iter().collect();
        plan
    }

    /// Plan CLUSTER_SAMPLE — for log-style data.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_cluster_sample(
        &self,
        analysis: &ArrayAnalysis,
        items: &[Value],
        mut plan: CompressionPlan,
        query_context: &str,
        preserve_fields: Option<&[String]>,
        max_items: usize,
        item_strings: Option<&[String]>,
    ) -> CompressionPlan {
        let n = items.len();
        let mut keep: BTreeSet<usize> = BTreeSet::new();

        // 1. Anchors.
        let anchor_pattern = map_to_anchor_pattern(CompressionStrategy::ClusterSample);
        keep.extend(self.anchor_selector.select_anchors(
            items,
            max_items,
            anchor_pattern,
            query_or_none(query_context),
        ));

        // 2. Structural outliers + error keywords (configured via Constraint trait).
        self.apply_constraints(items, item_strings, &mut keep);

        // 3. Cluster by message-like field (highest unique_ratio > 0.3).
        let mut message_field: Option<&str> = None;
        let mut max_uniqueness = 0.0_f64;
        for (name, stats) in &analysis.field_stats {
            if stats.field_type == "string"
                && stats.unique_ratio > max_uniqueness
                && stats.unique_ratio > 0.3
            {
                message_field = Some(name);
                max_uniqueness = stats.unique_ratio;
            }
        }

        if let Some(field) = message_field {
            plan.cluster_field = Some(field.to_string());
            // Group by md5(first 50 chars of message)[:8].
            let mut clusters: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for (i, item) in items.iter().enumerate() {
                let msg = item
                    .as_object()
                    .and_then(|o| o.get(field))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let truncated: String = msg.chars().take(50).collect();
                let digest = Md5::digest(truncated.as_bytes());
                let hash = format!("{:x}", digest)[..8].to_string();
                clusters.entry(hash).or_default().push(i);
            }
            // Keep up to 2 representatives from each cluster.
            for indices in clusters.values() {
                for &idx in indices.iter().take(2) {
                    keep.insert(idx);
                }
            }
        }

        // 4/5. Query signals.
        self.apply_query_signals(items, query_context, item_strings, &mut keep, false);

        // TOIN preserve_fields.
        self.apply_preserve_field_matches(items, query_context, preserve_fields, &mut keep);

        let final_keep =
            prioritize_indices(self.config, &keep, items, n, Some(analysis), max_items);
        plan.keep_indices = final_keep.into_iter().collect();
        plan
    }

    /// Plan TIME_SERIES.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_time_series(
        &self,
        analysis: &ArrayAnalysis,
        items: &[Value],
        mut plan: CompressionPlan,
        query_context: &str,
        preserve_fields: Option<&[String]>,
        max_items: usize,
        item_strings: Option<&[String]>,
    ) -> CompressionPlan {
        let n = items.len();
        let mut keep: BTreeSet<usize> = BTreeSet::new();

        // 1. Anchors.
        let anchor_pattern = map_to_anchor_pattern(CompressionStrategy::TimeSeries);
        keep.extend(self.anchor_selector.select_anchors(
            items,
            max_items,
            anchor_pattern,
            query_or_none(query_context),
        ));

        // 2. Items around change points (window of ±2 — wider than smart_sample).
        for stats in analysis.field_stats.values() {
            for &cp in &stats.change_points {
                for offset in -2_isize..=2 {
                    let idx = cp as isize + offset;
                    if idx >= 0 && (idx as usize) < n {
                        keep.insert(idx as usize);
                    }
                }
            }
        }

        // 3. Structural outliers + error keywords (configured via Constraint trait).
        self.apply_constraints(items, item_strings, &mut keep);

        // 4/5. Query signals.
        self.apply_query_signals(items, query_context, item_strings, &mut keep, false);

        // TOIN preserve_fields.
        self.apply_preserve_field_matches(items, query_context, preserve_fields, &mut keep);

        let final_keep =
            prioritize_indices(self.config, &keep, items, n, Some(analysis), max_items);
        plan.keep_indices = final_keep.into_iter().collect();
        plan
    }

    // --- Shared helpers ---

    /// Apply query-anchor matches (deterministic) + relevance scoring
    /// (probabilistic). When `keep_existing_only` is true (top_n's
    /// "additive only" mode), only items not already in keep are added.
    /// When false, all matches are added.
    fn apply_query_signals(
        &self,
        items: &[Value],
        query_context: &str,
        item_strings: Option<&[String]>,
        keep: &mut BTreeSet<usize>,
        keep_existing_only: bool,
    ) {
        if query_context.is_empty() {
            return;
        }

        // Deterministic anchor match.
        let anchors = extract_query_anchors(query_context);
        for (i, item) in items.iter().enumerate() {
            if keep_existing_only && keep.contains(&i) {
                continue;
            }
            if item_matches_anchors(item, &anchors) {
                keep.insert(i);
            }
        }

        // Probabilistic relevance scoring.
        let owned_strings: Vec<String>;
        let strs: Vec<&str> = match item_strings {
            Some(arr) => arr.iter().map(|s| s.as_str()).collect(),
            None => {
                owned_strings = items
                    .iter()
                    .map(|i| serde_json::to_string(i).unwrap_or_default())
                    .collect();
                owned_strings.iter().map(|s| s.as_str()).collect()
            }
        };
        let scores = self.scorer.score_batch(&strs, query_context);
        for (i, sc) in scores.iter().enumerate() {
            if keep_existing_only && keep.contains(&i) {
                continue;
            }
            if sc.score >= self.config.relevance_threshold {
                keep.insert(i);
            }
        }
    }

    fn apply_preserve_field_matches(
        &self,
        items: &[Value],
        query_context: &str,
        preserve_fields: Option<&[String]>,
        keep: &mut BTreeSet<usize>,
    ) {
        let Some(fields) = preserve_fields.filter(|f| !f.is_empty()) else {
            return;
        };
        if query_context.is_empty() {
            return;
        }
        for (i, item) in items.iter().enumerate() {
            if item_has_preserve_field_match(item, fields, query_context) {
                keep.insert(i);
            }
        }
    }
}

// --- Free helper functions ---

/// Map a compression strategy to its anchor data pattern.
pub fn map_to_anchor_pattern(strategy: CompressionStrategy) -> DataPattern {
    match strategy {
        CompressionStrategy::TimeSeries => DataPattern::TimeSeries,
        CompressionStrategy::TopN => DataPattern::SearchResults,
        CompressionStrategy::ClusterSample => DataPattern::Logs,
        // SmartSample / None / Skip → Generic.
        _ => DataPattern::Generic,
    }
}

/// Check if any of an item's preserve_field values matches the query.
///
/// `preserve_field_hashes` are SHA256[:8] hashes — match against
/// `hash_field_name(item_field_name)`.
pub fn item_has_preserve_field_match(
    item: &Value,
    preserve_field_hashes: &[String],
    query_context: &str,
) -> bool {
    if query_context.is_empty() {
        return false;
    }
    let Some(obj) = item.as_object() else {
        return false;
    };
    let query_lower = query_context.to_lowercase();

    for (field_name, value) in obj {
        let h = hash_field_name(field_name);
        if !preserve_field_hashes.iter().any(|p| p == &h) {
            continue;
        }
        if value.is_null() {
            continue;
        }
        let value_str = match value {
            Value::String(s) => s.clone(),
            _ => value.to_string(),
        }
        .to_lowercase();
        // Either direction containment.
        if !value_str.is_empty() && (value_str.contains(&query_lower) || query_lower.contains(&value_str)) {
            return true;
        }
    }
    false
}

fn query_or_none(q: &str) -> Option<&str> {
    if q.is_empty() {
        None
    } else {
        Some(q)
    }
}

fn for_each_anomaly(
    field_name: &str,
    stats: &FieldStats,
    items: &[Value],
    variance_threshold: f64,
    keep: &mut BTreeSet<usize>,
) {
    if stats.field_type != "numeric" {
        return;
    }
    let (Some(mean), Some(var)) = (stats.mean_val, stats.variance) else {
        return;
    };
    if var <= 0.0 {
        return;
    }
    let std = var.sqrt();
    if std <= 0.0 {
        return;
    }
    let threshold = variance_threshold * std;
    for (i, item) in items.iter().enumerate() {
        if let Some(num) = item
            .as_object()
            .and_then(|o| o.get(field_name))
            .and_then(|v| v.as_f64())
        {
            if !num.is_nan() && (num - mean).abs() > threshold {
                keep.insert(i);
            }
        }
    }
}

