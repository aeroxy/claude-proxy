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

// `pub use` re-exports — only the items on the live `SmartCrusher` path
// are re-exported. Items reserved for staged sub-features (planning,
// anchors, builder, constraints, observers, etc.) remain private
// to their submodules until they are wired into the live path.
pub use config::SmartCrusherConfig;
pub use crusher::{CrushArrayResult, SmartCrusher};
