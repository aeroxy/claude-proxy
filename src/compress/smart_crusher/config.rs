//! SmartCrusher configuration.

/// Configuration for SmartCrusher.
///
/// Several fields are staged knobs for the planner/heuristic surface
/// and not read on the current SmartCrusher path — silence dead_code
/// at the struct level.
///
/// SCHEMA-PRESERVING: Output contains only items from the original array.
/// No wrappers, no generated text, no metadata keys.
#[derive(Debug, Clone)]
pub struct SmartCrusherConfig {
    pub enabled: bool,
    /// Don't analyze arrays smaller than this. Default 5.
    pub min_items_to_analyze: usize,
    /// Only crush content with more than this many tokens. Default 200.
    pub min_tokens_to_crush: usize,
    /// Standard deviations from the mean to count as a change point.
    /// Default 2.0.
    pub variance_threshold: f64,
    /// Below this unique-ratio, a field is treated as nearly constant.
    /// Default 0.1.
    pub uniqueness_threshold: f64,
    /// Similarity score above which strings cluster together. Default 0.8.
    pub similarity_threshold: f64,
    /// Target maximum items in the output. Default 15.
    pub max_items_after_crush: usize,
    /// Whether to preserve detected change points. Default true.
    pub preserve_change_points: bool,
    /// Factor out fields with constant values across all items. Default
    /// false (disabled — preserves original schema).
    pub factor_out_constants: bool,
    /// Include generated text summaries in output. Default false (disabled
    /// — no generated text).
    pub include_summaries: bool,
    /// Use feedback hints to adjust compression aggressiveness. Default true.
    pub use_feedback_hints: bool,
    /// Minimum confidence required to apply TOIN recommendations.
    /// Default 0.5.
    pub toin_confidence_threshold: f64,
    /// Drop content-identical items before sampling. Default true.
    pub dedup_identical_items: bool,
    /// Fraction of K to allocate to the start of the array. Default 0.3.
    pub first_fraction: f64,
    /// Fraction of K to allocate to the end of the array. Default 0.15.
    pub last_fraction: f64,
    /// Items with `RelevanceScore.score >= this` are pinned by the
    /// planning methods.
    /// Default 0.3.
    pub relevance_threshold: f64,
    /// Minimum byte-savings ratio (0.0..1.0) for the lossless compaction
    /// path to be chosen over lossy. Computed as
    /// `1 - len(rendered) / len(input)`. If lossless saves less than
    /// this fraction, `crush_array` falls through to the lossy path. Default `0.15`.
    ///
    /// **Override semantics.** Users can tune this via the config
    /// directly. Set to `0.0` to always prefer
    /// lossless when available; set to `1.0` to effectively disable
    /// the lossless path.
    pub lossless_min_savings_ratio: f64,
    /// Compaction heuristic: a field is "core" if it appears in at
    /// least this fraction of rows. Mirrors
    /// `CompactConfig::core_field_fraction`. Default 0.8.
    pub compaction_core_field_fraction: f64,
    /// Compaction heuristic: when fewer than this fraction of all
    /// observed keys are core, treat the array as heterogeneous and
    /// look for a discriminator. Mirrors
    /// `CompactConfig::heterogeneous_core_ratio`. Default 0.6.
    pub compaction_heterogeneous_core_ratio: f64,
    /// Compaction heuristic: cap on inner-key count for
    /// nested-uniform flattening. Mirrors
    /// `CompactConfig::max_flatten_inner_keys`. Default 6.
    pub compaction_max_flatten_inner_keys: usize,
    /// Compaction heuristic: minimum bucket count before a candidate
    /// discriminator is "useful". Mirrors `CompactConfig::min_buckets`.
    /// Default 2.
    pub compaction_min_buckets: usize,
    /// Compaction heuristic: maximum bucket count — too many buckets
    /// means the discriminator is too granular (e.g. an ID column).
    /// Mirrors `CompactConfig::max_buckets`. Default 8.
    pub compaction_max_buckets: usize,
}

impl Default for SmartCrusherConfig {
    fn default() -> Self {
        SmartCrusherConfig {
            enabled: true,
            min_items_to_analyze: 5,
            min_tokens_to_crush: 200,
            variance_threshold: 2.0,
            uniqueness_threshold: 0.1,
            similarity_threshold: 0.8,
            max_items_after_crush: 15,
            preserve_change_points: true,
            factor_out_constants: false,
            include_summaries: false,
            use_feedback_hints: true,
            toin_confidence_threshold: 0.5,
            dedup_identical_items: true,
            first_fraction: 0.3,
            last_fraction: 0.15,
            relevance_threshold: 0.3,
            lossless_min_savings_ratio: 0.15,
            compaction_core_field_fraction: 0.8,
            compaction_heterogeneous_core_ratio: 0.6,
            compaction_max_flatten_inner_keys: 6,
            compaction_min_buckets: 2,
            compaction_max_buckets: 8,
        }
    }
}
