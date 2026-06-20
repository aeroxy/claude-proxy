//! Smart statistical tool output compression.
//!
//! Evaluates the structure and patterns of massive tool results to determine
//! whether they are safe to crush, identifies high-value rows, and compresses 
//! them using positions, scores, and keyword constraints.

mod analyzer;
mod anchors;
mod builder;
mod classifier;
pub mod compaction;
mod config;
mod constraints;
mod crusher;
mod crushers;
mod error_keywords;
mod field_detect;
mod hashing;
mod observer;
mod orchestration;
mod outliers;
mod planning;
mod statistics;
mod stats_math;
mod traits;
mod types;

pub use analyzer::SmartAnalyzer;
pub use anchors::{extract_query_anchors, item_matches_anchors};
pub use builder::SmartCrusherBuilder;
pub use classifier::{classify_array, ArrayType};
pub use config::SmartCrusherConfig;
pub use constraints::{
    default_oss_constraints, KeepErrorsConstraint, KeepStructuralOutliersConstraint,
};
pub use crusher::{CrushArrayResult, SmartCrusher};
pub use crushers::{compute_k_split, crush_number_array, crush_object, crush_string_array};
pub use error_keywords::ERROR_KEYWORDS;
pub use field_detect::{detect_id_field_statistically, detect_score_field_statistically};
pub use hashing::hash_field_name;
pub use observer::TracingObserver;
pub use orchestration::{deduplicate_indices_by_content, fill_remaining_slots, prioritize_indices};
pub use outliers::{
    detect_error_items_for_preservation, detect_rare_status_values, detect_structural_outliers,
};
pub use planning::{item_has_preserve_field_match, map_to_anchor_pattern, SmartCrusherPlanner};
pub use statistics::{calculate_string_entropy, detect_sequential_pattern, is_uuid_format};
pub use stats_math::{format_g, mean, median, sample_stdev, sample_variance};
pub use traits::{Constraint, CrushEvent, Observer, Scorer};
pub use types::{
    ArrayAnalysis, CompressionPlan, CompressionStrategy, CrushResult, CrushabilityAnalysis,
    FieldStats,
};
