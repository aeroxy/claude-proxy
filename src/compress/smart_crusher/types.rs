//! Core data types for SmartCrusher. `FieldStats` is used by the
//! live analyzer; the planner/analyzer-result types are staged.

//! Core data types for SmartCrusher.

use serde_json::Value;
use std::collections::BTreeMap;

/// Compression strategies based on data patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompressionStrategy {
    /// No compression needed.
    None,
    /// Explicitly skip — not safe to crush.
    Skip,
    /// Time-series: keep change points, summarize stable runs.
    TimeSeries,
    /// Cluster-sample: dedupe similar items.
    ClusterSample,
    /// Top-N: keep highest-scored items.
    TopN,
    /// Smart-sample: statistical sampling with anchor-preservation.
    SmartSample,
}

impl CompressionStrategy {
    /// Lowercase string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            CompressionStrategy::None => "none",
            CompressionStrategy::Skip => "skip",
            CompressionStrategy::TimeSeries => "time_series",
            CompressionStrategy::ClusterSample => "cluster",
            CompressionStrategy::TopN => "top_n",
            CompressionStrategy::SmartSample => "smart_sample",
        }
    }
}

/// Statistics for a single field across array items.
#[derive(Debug, Clone)]
pub struct FieldStats {
    pub name: String,
    /// One of: `"numeric"`, `"string"`, `"boolean"`, `"object"`, `"array"`,
    /// `"null"`.
    pub field_type: String,
    pub count: usize,
    pub unique_count: usize,
    pub unique_ratio: f64,
    pub is_constant: bool,
    pub constant_value: Option<Value>,

    // Numeric-specific
    pub min_val: Option<f64>,
    pub max_val: Option<f64>,
    pub mean_val: Option<f64>,
    pub variance: Option<f64>,
    pub change_points: Vec<usize>,

    // String-specific
    pub avg_length: Option<f64>,
    /// Top values by frequency, descending. Bounded list so this stays
    /// cheap to build and serialize.
    pub top_values: Vec<(String, usize)>,
}

/// Analysis of whether an array is safe to crush.
///
/// The key invariant: **if we don't have a reliable signal to determine which
/// items are important, we don't crush at all**. Signals include score
/// fields, error keywords, numeric anomalies, and low uniqueness.
#[derive(Debug, Clone)]
pub struct CrushabilityAnalysis {
    pub crushable: bool,
    pub confidence: f64,
    pub reason: String,
    pub signals_present: Vec<String>,
    pub signals_absent: Vec<String>,

    // Detailed metrics
    pub has_id_field: bool,
    pub id_uniqueness: f64,
    pub avg_string_uniqueness: f64,
    pub has_score_field: bool,
    pub error_item_count: usize,
    pub anomaly_count: usize,
}

impl CrushabilityAnalysis {
    /// Helper to build a "not crushable" verdict.
    pub fn skip(reason: impl Into<String>, confidence: f64) -> Self {
        CrushabilityAnalysis {
            crushable: false,
            confidence,
            reason: reason.into(),
            signals_present: Vec::new(),
            signals_absent: Vec::new(),
            has_id_field: false,
            id_uniqueness: 0.0,
            avg_string_uniqueness: 0.0,
            has_score_field: false,
            error_item_count: 0,
            anomaly_count: 0,
        }
    }
}

/// Complete analysis of an array.
///
/// `field_stats` and `constant_fields` use `BTreeMap` for sorted-by-key iteration.
#[derive(Debug, Clone)]
pub struct ArrayAnalysis {
    pub item_count: usize,
    pub field_stats: BTreeMap<String, FieldStats>,
    /// One of: `"time_series"`, `"logs"`, `"search_results"`, `"generic"`.
    pub detected_pattern: String,
    pub recommended_strategy: CompressionStrategy,
    pub constant_fields: BTreeMap<String, Value>,
    pub estimated_reduction: f64,
    pub crushability: Option<CrushabilityAnalysis>,
}

/// Plan for how to compress an array.
///
/// `keep_indices` is the list of original-array indices that survive compression;
/// `summary_ranges` carries `(start, end, summary_dict)` for runs we
/// summarized rather than dropped.
#[derive(Debug, Clone)]
pub struct CompressionPlan {
    pub strategy: CompressionStrategy,
    pub keep_indices: Vec<usize>,
    pub constant_fields: BTreeMap<String, Value>,
    /// `(start, end, summary)` triples for summarized runs. We use `Value` for the summary so
    /// any JSON shape is representable.
    pub summary_ranges: Vec<(usize, usize, Value)>,
    pub cluster_field: Option<String>,
    pub sort_field: Option<String>,
    pub keep_count: usize,
}

impl Default for CompressionPlan {
    fn default() -> Self {
        CompressionPlan {
            strategy: CompressionStrategy::None,
            keep_indices: Vec::new(),
            constant_fields: BTreeMap::new(),
            summary_ranges: Vec::new(),
            cluster_field: None,
            sort_field: None,
            keep_count: 10,
        }
    }
}

