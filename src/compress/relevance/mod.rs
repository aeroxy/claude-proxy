//! Relevance scoring
//!
//! Used by SmartCrusher's planning layer to decide which items in a tool
//! output match the user's query (the user's recent prompts plus the
//! assistant's tool-call argument JSON, joined). Items above a relevance
//! threshold are pinned into `keep_indices`.
//!
//! # Scorer ladder
//!
//! 1. **BM25** (`bm25`): keyword overlap with TF-IDF + length
//!    normalization. No ML deps. Excellent for exact-match cases (UUIDs,
//!    field=value filters). Tool-call arguments are usually literal
//!    keywords that appear verbatim in the response, so BM25 catches
//!    most cases.
//!
//! Each scorer implements the `RelevanceScorer` trait.

mod base;
mod bm25;

pub use base::RelevanceScorer;
pub use bm25::BM25Scorer;

/// Factory to construct a relevance scorer.
///
/// Returns a boxed trait object so callers don't have to know which
/// concrete scorer they got. `tier`:
///
/// - `"bm25"` or `"hybrid"` (default) — `BM25Scorer` (pure keyword + boost fallback).
pub fn create_scorer(tier: &str) -> Result<Box<dyn RelevanceScorer + Send + Sync>, String> {
    match tier.to_lowercase().as_str() {
        "bm25" | "hybrid" => Ok(Box::new(BM25Scorer::default())),
        other => Err(format!(
            "Unknown scorer tier: {}. Valid tiers: bm25, hybrid",
            other
        )),
    }
}
