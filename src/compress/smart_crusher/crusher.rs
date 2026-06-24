//! `SmartCrusher` struct — top-level entry point for compression.
//!
//! Owns the `config`, `anchor_selector`, `scorer`, and `analyzer`
//! singletons that every per-message call needs. Constructed once
//! per process; the struct is `Send + Sync` so it can sit behind an
//! `Arc` in a multi-threaded proxy.
//!
//! Performs lossy and lossless array compression.

use serde_json::Value;

use super::analyzer::SmartAnalyzer;
use super::builder::SmartCrusherBuilder;
use super::compaction::{CompactConfig, Compaction, CompactionStage};
use super::config::SmartCrusherConfig;
use super::planning::SmartCrusherPlanner;
use super::traits::{Constraint, Observer};
use super::types::{CompressionPlan, CompressionStrategy};
use crate::compress::adaptive_sizer::compute_optimal_k;
use crate::compress::anchor_selector::AnchorSelector;
use crate::compress::relevance::RelevanceScorer;

/// Return type for `crush_array`.
///
/// Two operating paths feed the same result type:
///
/// - **Lossless path** — input compacted to a smaller inline form
///   (e.g. CSV+schema). Nothing dropped; `compacted` is populated.
/// - **Lossy path** — input compressed by row-dropping. `items` holds
///   the kept subset.
pub struct CrushArrayResult {
    pub items: Vec<Value>,
    pub strategy_info: String,
    pub compacted: Option<String>,
    pub compaction_kind: Option<&'static str>,
    /// Original-array indices that `items` corresponds to. Populated on
    /// the lossy path (from the plan's `keep_indices`); `None` on the
    /// lossless, passthrough, and skip paths where items == input.
    pub keep_indices: Option<Vec<usize>>,
}

/// Top-level SmartCrusher.
///
/// Three pluggable extensions (Stage 3c.2 PR1):
/// - `scorer` — relevance scoring (`BM25Scorer` by default).
/// - `constraints` — must-keep predicates (`KeepErrorsConstraint` +
///   `KeepStructuralOutliersConstraint` by default).
/// - `observers` — decision-stream telemetry (`TracingObserver` by
///   default).
///
/// Compose via [`SmartCrusherBuilder`]; or call `SmartCrusher::new()`
/// for the OSS default composition.
pub struct SmartCrusher {
    pub config: SmartCrusherConfig,
    pub anchor_selector: AnchorSelector,
    pub scorer: Box<dyn RelevanceScorer + Send + Sync>,
    pub analyzer: SmartAnalyzer,
    pub constraints: Vec<Box<dyn Constraint>>,
    pub observers: Vec<Box<dyn Observer>>,
    pub compaction: Option<CompactionStage>,
}

impl SmartCrusher {
    /// Construct with default composition: scorer + constraints +
    /// observer + **lossless-first compaction stage**. Calling
    /// `crush_array` runs the dispatch:
    ///
    /// 1. Try the lossless compactor.
    /// 2. If savings ratio ≥ `config.lossless_min_savings_ratio`, ship lossless.
    /// 3. Otherwise fall through to the lossy path — drop rows.
    pub fn new(config: SmartCrusherConfig) -> Self {
        // Carry the compaction heuristics from the crusher config into
        // the compaction stage; everything not exposed on
        // SmartCrusherConfig keeps its CompactConfig default.
        let compact_cfg = CompactConfig {
            core_field_fraction: config.compaction_core_field_fraction,
            heterogeneous_core_ratio: config.compaction_heterogeneous_core_ratio,
            max_flatten_inner_keys: config.compaction_max_flatten_inner_keys,
            min_buckets: config.compaction_min_buckets,
            max_buckets: config.compaction_max_buckets,
            ..CompactConfig::default()
        };
        SmartCrusherBuilder::new(config)
            .with_default_oss_setup()
            .with_compaction(CompactionStage::csv_schema(compact_cfg))
            .build()
    }

    /// Construct directly from owned parts. Used by
    /// [`SmartCrusherBuilder::build`] — not part of the public stable
    /// API. Prefer the builder.
    #[doc(hidden)]
    pub fn from_parts(
        config: SmartCrusherConfig,
        anchor_selector: AnchorSelector,
        scorer: Box<dyn RelevanceScorer + Send + Sync>,
        analyzer: SmartAnalyzer,
        constraints: Vec<Box<dyn Constraint>>,
        observers: Vec<Box<dyn Observer>>,
        compaction: Option<CompactionStage>,
    ) -> Self {
        SmartCrusher {
            config,
            anchor_selector,
            scorer,
            analyzer,
            constraints,
            observers,
            compaction,
        }
    }

    fn planner(&self) -> SmartCrusherPlanner<'_> {
        SmartCrusherPlanner::new(
            &self.config,
            &self.anchor_selector,
            &*self.scorer,
            &self.analyzer,
            &self.constraints,
        )
    }

    /// Execute a `CompressionPlan` against `items`, returning the
    /// kept-items list in original-array order.
    ///
    /// Schema-preserving by default: each kept item is cloned unchanged.
    /// No summary objects, generated fields, or wrapper metadata.
    ///
    /// When `factor_out_constants` is enabled (default off), fields the
    /// analyzer found constant across ALL items are stripped from each
    /// kept object and emitted once in a leading
    /// `{"_constant_fields": {...}}` sentinel. Stripping is
    /// defensive: a key is only removed from an item when its value
    /// equals the recorded constant, so a drifted item keeps its own
    /// value.
    pub fn execute_plan(&self, plan: &CompressionPlan, items: &[Value]) -> (Vec<Value>, Vec<usize>) {
        let mut indices = plan.keep_indices.clone();
        indices.sort_unstable();
        let mut kept: Vec<Value> = indices
            .iter()
            .filter(|&&idx| idx < items.len())
            .map(|&idx| items[idx].clone())
            .collect();

        let mut final_indices = indices;

        if self.config.factor_out_constants && !plan.constant_fields.is_empty() && kept.len() >= 2 {
            let mut any_stripped = false;
            for item in kept.iter_mut() {
                if let Value::Object(map) = item {
                    for (key, constant) in &plan.constant_fields {
                        if map.get(key) == Some(constant) {
                            map.remove(key);
                            any_stripped = true;
                        }
                    }
                }
            }
            if any_stripped {
                let mut sentinel = serde_json::Map::new();
                sentinel.insert(
                    "_constant_fields".to_string(),
                    Value::Object(plan.constant_fields.clone().into_iter().collect()),
                );
                kept.insert(0, Value::Object(sentinel));
                final_indices.insert(0, 0);
            }
        }

        (kept, final_indices)
    }

    /// Compress an array of dict items.
    ///
    /// # Pipeline
    ///
    /// 1. Compute `item_strings` once (used as input to adaptive
    ///    sizing and downstream relevance scoring).
    /// 2. `compute_optimal_k` → `adaptive_k`.
    /// 3. If `n <= adaptive_k`, return passthrough.
    /// 4. `analyzer.analyze_array(items)` → `analysis`.
    /// 5. If `analysis.recommended_strategy == Skip`, return passthrough
    ///    with a `skip:<reason>` strategy string.
    /// 6. `planner.create_plan(analysis, items, query_context, ...)`.
    /// 7. `execute_plan(plan, items)` → result.
    /// 8. Strategy info = `analysis.recommended_strategy.as_str()`.
    pub fn crush_array(&self, items: &[Value], query_context: &str, bias: f64) -> CrushArrayResult {
        let item_strings: Vec<String> = items
            .iter()
            .map(|i| serde_json::to_string(i).unwrap_or_default())
            .collect();
        let item_str_refs: Vec<&str> = item_strings.iter().map(|s| s.as_str()).collect();

        let max_k = if self.config.max_items_after_crush > 0 {
            Some(self.config.max_items_after_crush)
        } else {
            None
        };
        let adaptive_k = compute_optimal_k(&item_str_refs, bias, 3, max_k);

        // Tier-1 boundary: array already small enough — passthrough,
        // nothing to compact, nothing to drop.
        if items.len() <= adaptive_k {
            return CrushArrayResult {
                items: items.to_vec(),
                strategy_info: "none:adaptive_at_limit".to_string(),
                compacted: None,
                compaction_kind: None,
                keep_indices: None,
            };
        }

        // ── Lossless-first attempt ──
        //
        // Run the compaction stage if present, then check the savings
        // ratio against `config.lossless_min_savings_ratio`. If the
        // lossless rendering shrinks the input by at least that much,
        // ship it — nothing dropped, no CCR retrieval needed.
        // Otherwise fall through to the lossy path.
        if let Some(stage) = &self.compaction {
            let (c, rendered) = stage.run(items);
            if c.was_compacted() {
                let input_bytes = estimate_array_bytes(&item_strings);
                let savings_ratio = if input_bytes > 0 {
                    1.0 - (rendered.len() as f64 / input_bytes as f64)
                } else {
                    0.0
                };
                if savings_ratio >= self.config.lossless_min_savings_ratio {
                    let kind = compaction_kind_str(&c);
                    return CrushArrayResult {
                        items: items.to_vec(),
                        strategy_info: format!("lossless:{kind}"),
                        compacted: Some(rendered),
                        compaction_kind: Some(kind),
                        keep_indices: None,
                    };
                }
            }
        }

        // ── Lossy path: compress inline + cache full original via CCR ──
        //
        // The runtime caller (PyO3 bridge / proxy server) is expected
        // to stash the full input keyed by `ccr_hash` so a retrieval
        // tool can serve dropped rows back to the LLM on demand.
        // **No data is lost** — "lossy" here means "compressed view
        // inline; full payload retrievable via CCR cache."

        let effective_max_items = adaptive_k;
        let analysis = self.analyzer.analyze_array(items);

        // Crushability gate: not safe to crush → passthrough, no CCR.
        if analysis.recommended_strategy == CompressionStrategy::Skip {
            let reason = match &analysis.crushability {
                Some(c) => format!("skip:{}", c.reason),
                None => String::new(),
            };
            return CrushArrayResult {
                items: items.to_vec(),
                strategy_info: reason,
                compacted: None,
                compaction_kind: None,
                keep_indices: None,
            };
        }

        let plan = self.planner().create_plan(
            &analysis,
            items,
            query_context,
            None,
            Some(effective_max_items),
            Some(&item_strings),
        );
        let (result, final_indices) = self.execute_plan(&plan, items);

        CrushArrayResult {
            items: result,
            strategy_info: analysis.recommended_strategy.as_str().to_string(),
            compacted: None,
            compaction_kind: None,
            keep_indices: Some(final_indices),
        }
    }

}

/// Maps a `Compaction` to a stable kind tag exposed via `CrushArrayResult`.
fn compaction_kind_str(c: &Compaction) -> &'static str {
    match c {
        Compaction::Table { .. } => "table",
        Compaction::Buckets { .. } => "buckets",
        Compaction::OpaqueRef { .. } => "opaque",
        Compaction::Untouched(_) => "untouched",
    }
}

/// Approximate byte size of `[v0, v1, ...]` JSON serialization, given
/// each item's already-serialized form. Adds 2 for outer brackets and
/// 1 per inter-item comma. Used by the lossless savings-ratio check.
fn estimate_array_bytes(item_strings: &[String]) -> usize {
    let payload: usize = item_strings.iter().map(|s| s.len()).sum();
    let separators = item_strings.len().saturating_sub(1);
    payload + separators + 2
}


