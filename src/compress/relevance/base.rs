//! Base trait and types for relevance scoring.

/// Relevance score with explainability fields.
#[derive(Debug, Clone)]
pub struct RelevanceScore {
    pub score: f64,
    pub reason: String,
    pub matched_terms: Vec<String>,
}

impl RelevanceScore {
    /// Build a score, clamping to `[0.0, 1.0]`.
    pub fn new(score: f64, reason: impl Into<String>, matched_terms: Vec<String>) -> Self {
        let score = if score.is_finite() { score } else { 0.0 };
        RelevanceScore {
            score: score.clamp(0.0, 1.0),
            reason: reason.into(),
            matched_terms,
        }
    }

    /// Convenience for "no match" scores.
    pub fn empty(reason: impl Into<String>) -> Self {
        RelevanceScore::new(0.0, reason, Vec::new())
    }
}

impl Default for RelevanceScore {
    fn default() -> Self {
        RelevanceScore::new(0.0, "", Vec::new())
    }
}

/// Trait that every relevance scorer implements.
///
/// Requires `score` for single items and `score_batch` for collections.
pub trait RelevanceScorer {
    /// Score a single item against the context.
    fn score(&self, item: &str, context: &str) -> RelevanceScore;

    /// Score a batch of items. Default impl delegates to per-item
    /// `score` — override when the scorer can amortize work across
    /// items (BM25 pre-tokenizes context once, embeddings batch the
    /// matrix multiplication, etc.).
    fn score_batch(&self, items: &[&str], context: &str) -> Vec<RelevanceScore> {
        items.iter().map(|item| self.score(item, context)).collect()
    }

    /// Whether this scorer is available in the current environment.
    /// Override for scorers with optional deps (e.g. ONNX embeddings).
    fn is_available(&self) -> bool {
        true
    }
}

/// Default batch implementation as a free function — convenient for
/// tests that want to verify the fall-back behavior without
/// constructing a trait object.
pub fn default_batch_score<S: RelevanceScorer>(
    scorer: &S,
    items: &[&str],
    context: &str,
) -> Vec<RelevanceScore> {
    items
        .iter()
        .map(|item| scorer.score(item, context))
        .collect()
}
