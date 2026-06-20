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
use super::classifier::{classify_array, ArrayType};
use super::compaction::{
    try_parse_json_container, CompactConfig, Compaction, CompactionStage,
};
use super::config::SmartCrusherConfig;
use super::crushers::{compute_k_split, crush_number_array, crush_object, crush_string_array};
use super::planning::SmartCrusherPlanner;
use super::traits::{Constraint, CrushEvent, Observer};
use super::types::{CompressionPlan, CompressionStrategy, CrushResult};
use crate::compress::relevance::RelevanceScorer;
use crate::compress::adaptive_sizer::compute_optimal_k;
use crate::compress::anchor_selector::AnchorSelector;

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

    pub fn without_compaction(config: SmartCrusherConfig) -> Self {
        SmartCrusherBuilder::new(config)
            .with_default_oss_setup()
            .build()
    }

    pub fn with_compaction_format(config: SmartCrusherConfig, format_name: &str) -> Option<Self> {
        let stage = CompactionStage::from_format_name(format_name)?;
        Some(
            SmartCrusherBuilder::new(config)
                .with_default_oss_setup()
                .with_compaction(stage)
                .build(),
        )
    }

    /// Begin a builder chain for custom composition. The Enterprise
    /// entry point: swap the scorer, add business-rule constraints,
    /// attach an audit observer.
    pub fn builder(config: SmartCrusherConfig) -> SmartCrusherBuilder {
        SmartCrusherBuilder::new(config)
    }

    /// Construct with a custom scorer (legacy convenience). Equivalent
    /// to `SmartCrusher::builder(config).with_scorer(scorer).with_default_oss_setup().build()`
    /// minus the default scorer override; preserved for backward
    /// compatibility with pre-PR1 callers.
    pub fn with_scorer(
        config: SmartCrusherConfig,
        scorer: Box<dyn RelevanceScorer + Send + Sync>,
    ) -> Self {
        SmartCrusherBuilder::new(config)
            .with_scorer(scorer)
            .add_default_oss_constraints()
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
    pub fn execute_plan(&self, plan: &CompressionPlan, items: &[Value]) -> Vec<Value> {
        let mut indices = plan.keep_indices.clone();
        indices.sort_unstable();
        let mut kept: Vec<Value> = indices
            .into_iter()
            .filter(|&idx| idx < items.len())
            .map(|idx| items[idx].clone())
            .collect();

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
            }
        }

        kept
    }

    /// Top-level entry point.
    ///
    /// Parses `content` as JSON, recursively processes it (compressing
    /// arrays at every depth via the appropriate per-type crusher),
    /// then re-serializes with custom formatting.
    ///
    /// Returns a `CrushResult` with:
    /// - `compressed`: the re-serialized JSON.
    /// - `original`: the input string (unmodified).
    /// - `was_modified`: whether `compressed` differs from `content`'s
    ///   trimmed form.
    /// - `strategy`: combined strategy info from all crushed arrays
    ///   (or `"passthrough"`).
    pub fn crush(&self, content: &str, query: &str, bias: f64) -> CrushResult {
        let start = std::time::Instant::now();
        let (compressed, was_modified, info) = self.smart_crush_content(content, query, bias);
        let strategy = if info.is_empty() {
            "passthrough".to_string()
        } else {
            info
        };

        // Fire one event per top-level crush. Cheap when no observers
        // are configured; cheap when only `TracingObserver` is configured.
        if !self.observers.is_empty() {
            let event = CrushEvent {
                strategy: strategy.clone(),
                input_bytes: content.len(),
                output_bytes: compressed.len(),
                elapsed_ns: start.elapsed().as_nanos() as u64,
                was_modified,
            };
            for observer in &self.observers {
                observer.on_event(&event);
            }
        }

        CrushResult {
            compressed,
            original: content.to_string(),
            was_modified,
            strategy,
        }
    }

    /// JSON-parse, recursively process, re-serialize.
    ///
    /// Returns `(crushed_content, was_modified, info)`.
    pub fn smart_crush_content(
        &self,
        content: &str,
        query_context: &str,
        bias: f64,
    ) -> (String, bool, String) {
        // Parse — non-JSON content passes through unchanged.
        let Ok(parsed) = serde_json::from_str::<Value>(content) else {
            return (content.to_string(), false, String::new());
        };

        let (crushed, info) = self.process_value(&parsed, 0, query_context, bias);

        // Re-serialize with safe formatting:
        // compact `(",", ":")` separators + `ensure_ascii=False`,
        // preserving object-key insertion order.
        let result = crate::compress::anchor_selector::json_safe_dumps(&crushed);
        let was_modified = result != content.trim();
        (result, was_modified, info)
    }

    /// Maximum recursion depth for nested JSON. Beyond this, values are returned as-is.
    const MAX_PROCESS_DEPTH: usize = 50;

    /// Recursively process a value, crushing arrays where appropriate.
    ///
    /// Returns `(processed_value, info_string)`.
    pub fn process_value(
        &self,
        value: &Value,
        depth: usize,
        query_context: &str,
        bias: f64,
    ) -> (Value, String) {
        if depth >= Self::MAX_PROCESS_DEPTH {
            return (value.clone(), String::new());
        }

        let mut info_parts: Vec<String> = Vec::new();

        match value {
            Value::Array(arr) => {
                let n = arr.len();
                if n >= self.config.min_items_to_analyze {
                    let arr_type = classify_array(arr);
                    match arr_type {
                        ArrayType::DictArray => {
                            let result = self.crush_array(arr, query_context, bias);
                            // Lossless path won → substitute the array
                            // with the compacted string in place. This
                            // makes the lossless win visible to the
                            // public `crush()` API: the output JSON
                            // has a string where the array used to be.
                            // The wrapping JSON structure is preserved.
                            if let Some(rendered) = result.compacted {
                                info_parts.push(format!(
                                    "{}({}->len={})",
                                    result.strategy_info,
                                    n,
                                    rendered.len()
                                ));
                                return (Value::String(rendered), info_parts.join(","));
                            }
                            info_parts.push(format!(
                                "{}({}->{})",
                                result.strategy_info,
                                n,
                                result.items.len()
                            ));
                            return (Value::Array(result.items), info_parts.join(","));
                        }
                        ArrayType::StringArray => {
                            let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                            let (crushed, strategy) = crush_string_array(&strs, &self.config, bias);
                            info_parts.push(format!("{}({}->{})", strategy, n, crushed.len()));
                            let crushed_values: Vec<Value> =
                                crushed.into_iter().map(Value::String).collect();
                            return (Value::Array(crushed_values), info_parts.join(","));
                        }
                        ArrayType::NumberArray => {
                            let (crushed, strategy) = crush_number_array(arr, &self.config, bias);
                            info_parts.push(format!("{}({}->{})", strategy, n, crushed.len()));
                            return (Value::Array(crushed), info_parts.join(","));
                        }
                        ArrayType::MixedArray => {
                            let (crushed, strategy) =
                                self.crush_mixed_array(arr, query_context, bias);
                            info_parts.push(format!("{}({}->{})", strategy, n, crushed.len()));
                            return (Value::Array(crushed), info_parts.join(","));
                        }
                        // NestedArray, BoolArray, Empty → fall through
                        // to recursive descent.
                        _ => {}
                    }
                }

                // Below threshold or not crushable → recurse into items.
                let mut processed: Vec<Value> = Vec::with_capacity(n);
                for item in arr {
                    let (p_item, p_info) = self.process_value(item, depth + 1, query_context, bias);
                    processed.push(p_item);
                    if !p_info.is_empty() {
                        info_parts.push(p_info);
                    }
                }
                (Value::Array(processed), info_parts.join(","))
            }
            Value::Object(map) => {
                // First pass: recurse into values to compress nested arrays.
                let mut processed = serde_json::Map::new();
                for (k, v) in map {
                    let (p_val, p_info) = self.process_value(v, depth + 1, query_context, bias);
                    processed.insert(k.clone(), p_val);
                    if !p_info.is_empty() {
                        info_parts.push(p_info);
                    }
                }

                // Second pass: if the object itself has many keys,
                // compress at the key level.
                if processed.len() >= self.config.min_items_to_analyze {
                    let (crushed_dict, strategy) = crush_object(&processed, &self.config, bias);
                    if strategy != "object:passthrough" {
                        info_parts.push(strategy);
                        return (Value::Object(crushed_dict), info_parts.join(","));
                    }
                }

                (Value::Object(processed), info_parts.join(","))
            }
            // Strings: walker-equivalent handling. Delegates to
            // `process_string` which parses stringified-JSON containers
            // (recursing through `process_value`) and CCR-substitutes
            // opaque blobs (with store-write so retrieval works).
            Value::String(s) => self.process_string(s, depth, query_context, bias),
            // Other scalars — passthrough.
            _ => (value.clone(), String::new()),
        }
    }

    /// Walker-equivalent string handling. Mirrors `walker::walk_string`
    /// in `compaction/walker.rs` but lives on `SmartCrusher` so the
    /// public `crush()` path picks it up.
    ///
    /// Two cases:
    /// 1. **Stringified-JSON.** Strings that parse to a JSON object or
    ///    array → recurse via `process_value`, then re-emit the result
    ///    as a compact JSON string. The wrapping string is preserved
    ///    (so the parent JSON shape stays a string-typed field), but
    ///    its contents are processed end-to-end.
    /// 2. **Opaque blobs.** Strings classified as
    ///    [`CellClass::Opaque`] (long base64 / HTML / long-text) →
    ///    substitute with a `<<ccr:HASH,KIND,SIZE>>` marker. Same
    ///    format as `compaction::walker::format_ccr_marker` so
    ///    downstream consumers can pattern-match markers regardless
    ///    of which path emitted them.
    fn process_string(
        &self,
        s: &str,
        depth: usize,
        query_context: &str,
        bias: f64,
    ) -> (Value, String) {
        // 1. Stringified-JSON: parse, recurse, re-render.
        if let Some(parsed) = try_parse_json_container(s) {
            let (processed, sub_info) = self.process_value(&parsed, depth + 1, query_context, bias);
            // If recursion produced something different, re-emit.
            // Special case: if the recursion returned a `Value::String`
            // (lossless compaction substituted the array with a
            // rendered CSV+schema string), use that string directly.
            // Re-encoding it as JSON would produce a quoted string
            // literal — double-encoded — which is not what callers
            // expect in the wrapping field.
            if processed != parsed {
                let rendered = match &processed {
                    Value::String(rendered_str) => rendered_str.clone(),
                    _ => serde_json::to_string(&processed).unwrap_or_else(|_| s.to_string()),
                };
                let info = if sub_info.is_empty() {
                    "string_json".to_string()
                } else {
                    format!("string_json[{sub_info}]")
                };
                return (Value::String(rendered), info);
            }
        }

        // 2. Plain string — passthrough.
        (Value::String(s.to_string()), String::new())
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
        let result = self.execute_plan(&plan, items);

        CrushArrayResult {
            items: result,
            strategy_info: analysis.recommended_strategy.as_str().to_string(),
            compacted: None,
            compaction_kind: None,
        }
    }

    /// Compress a mixed-type array by grouping items by type and
    /// compressing each group with the appropriate handler.
    ///
    /// Strategy:
    /// 1. Group by type (dict / str / number / list / null / bool / other).
    /// 2. For groups with >= `min_items_to_analyze` items: apply the
    ///    type-specific compressor.
    /// 3. For small groups: keep all items.
    /// 4. Reassemble in original order.
    ///
    /// Returns `(crushed_items, strategy_string)`.
    pub fn crush_mixed_array(
        &self,
        items: &[Value],
        query_context: &str,
        bias: f64,
    ) -> (Vec<Value>, String) {
        let n = items.len();
        if n <= 8 {
            return (items.to_vec(), "mixed:passthrough".to_string());
        }

        // Group by type, tracking original indices.
        let mut groups: GroupBuckets = GroupBuckets::default();
        for (i, item) in items.iter().enumerate() {
            groups.push(group_key(item), i, item.clone());
        }

        let mut keep_indices: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut strategy_parts: Vec<String> = Vec::new();

        for (type_key, indices, values) in groups.into_iter() {
            // Small groups: keep all items.
            if values.len() < self.config.min_items_to_analyze {
                keep_indices.extend(&indices);
                continue;
            }

            match type_key {
                "dict" => {
                    let CrushArrayResult { items: crushed, .. } =
                        self.crush_array(&values, query_context, bias);
                    // Find which original indices survived by matching
                    // canonical-JSON serialization, preserving multiplicity counts.
                    let mut crushed_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
                    for item in &crushed {
                        *crushed_counts.entry(canonical_json_for_match(item)).or_insert(0) += 1;
                    }
                    for (i, idx) in indices.iter().enumerate() {
                        let repr = canonical_json_for_match(&values[i]);
                        if let Some(count) = crushed_counts.get_mut(&repr) {
                            if *count > 0 {
                                keep_indices.insert(*idx);
                                *count -= 1;
                            }
                        }
                    }
                    strategy_parts.push(format!("dict:{}->{}", values.len(), crushed.len()));
                }
                "str" => {
                    let strs: Vec<&str> = values.iter().filter_map(|v| v.as_str()).collect();
                    let (crushed, _) = crush_string_array(&strs, &self.config, bias);
                    // Find which original indices survived, preserving multiplicity counts.
                    let mut crushed_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
                    for s in &crushed {
                        *crushed_counts.entry(s.as_str()).or_insert(0) += 1;
                    }
                    for (i, idx) in indices.iter().enumerate() {
                        if let Some(s) = values[i].as_str() {
                            if let Some(count) = crushed_counts.get_mut(s) {
                                if *count > 0 {
                                    keep_indices.insert(*idx);
                                    *count -= 1;
                                }
                            }
                        }
                    }
                    strategy_parts.push(format!("str:{}->{}", values.len(), crushed.len()));
                }
                "number" => {
                    // Adaptive sampling + outlier detection. Keeps first/last by index
                    // and items >variance_threshold standard deviations from mean.
                    let item_strings: Vec<String> = values.iter().map(|v| v.to_string()).collect();
                    let item_refs: Vec<&str> = item_strings.iter().map(|s| s.as_str()).collect();
                    let (_kt, kf, kl, _) = compute_k_split(&item_refs, &self.config, bias);

                    let kf = kf.min(values.len());
                    let kl = kl.min(values.len().saturating_sub(kf));
                    let first_idx: Vec<usize> = indices.iter().take(kf).copied().collect();
                    let last_idx: Vec<usize> =
                        indices.iter().rev().take(kl).copied().collect::<Vec<_>>();
                    keep_indices.extend(&first_idx);
                    keep_indices.extend(&last_idx);

                    // Outliers via finite-only stats.
                    let finite: Vec<f64> = values
                        .iter()
                        .filter_map(|v| v.as_f64().filter(|f| f.is_finite()))
                        .collect();
                    if finite.len() > 1 {
                        if let Some(mean_v) = super::stats_math::mean(&finite) {
                            if let Some(std_v) = super::stats_math::sample_stdev(&finite) {
                                if std_v > 0.0 {
                                    let threshold = self.config.variance_threshold * std_v;
                                    for (i, val) in values.iter().enumerate() {
                                        if let Some(num) = val.as_f64().filter(|f| f.is_finite()) {
                                            if (num - mean_v).abs() > threshold {
                                                keep_indices.insert(indices[i]);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    strategy_parts.push(format!("num:{}", values.len()));
                }
                _ => {
                    // list / bool / none / other → keep all items.
                    keep_indices.extend(&indices);
                }
            }
        }

        // Reassemble in original order.
        let result: Vec<Value> = keep_indices.iter().map(|&i| items[i].clone()).collect();
        let strategy = format!(
            "mixed:adaptive({}->{},{})",
            n,
            result.len(),
            strategy_parts.join(",")
        );
        (result, strategy)
    }
}

// ---------- helpers ----------

/// Group key that defines classification.
fn group_key(item: &Value) -> &'static str {
    match item {
        Value::Object(_) => "dict",
        Value::String(_) => "str",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::Array(_) => "list",
        Value::Null => "none",
    }
}

/// Group buckets keyed by the type-string. Preserves first-occurrence
/// order across keys so dict/str/number/list/none/bool always come out
/// in the same order.
#[derive(Default)]
struct GroupBuckets {
    entries: Vec<(&'static str, Vec<usize>, Vec<Value>)>,
    index_of: std::collections::HashMap<&'static str, usize>,
}

impl GroupBuckets {
    fn push(&mut self, key: &'static str, idx: usize, value: Value) {
        match self.index_of.get(key).copied() {
            Some(i) => {
                self.entries[i].1.push(idx);
                self.entries[i].2.push(value);
            }
            None => {
                self.index_of.insert(key, self.entries.len());
                self.entries.push((key, vec![idx], vec![value]));
            }
        }
    }
}

impl IntoIterator for GroupBuckets {
    type Item = (&'static str, Vec<usize>, Vec<Value>);
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

/// Serialize a `Value` for membership comparison.
fn canonical_json_for_match(value: &Value) -> String {
    crate::compress::anchor_selector::json_dumps_sort_keys(value)
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


