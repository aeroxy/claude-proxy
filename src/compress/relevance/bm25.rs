//! BM25 keyword relevance scorer.
//!
//! # Score post-processing
//!
//! The raw BM25 score is normalized to `[0, 1]` by dividing by
//! `max_score` (default 10.0). Items with at least one matched token
//! of length 8 or more get a `+0.3` long-token bonus (UUIDs, long IDs are
//! high-signal matches). Final score is clamped to `[0, 1]` via
//! `RelevanceScore::new`.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use super::base::{RelevanceScore, RelevanceScorer};

/// Tokenization regex. Order matters — UUID first so that hex-string
/// IDs aren't broken into pieces by the alphanumeric arm:
/// 1. UUID — 8-4-4-4-12 hex with dashes.
/// 2. Numeric ID — 4+ digits with word boundaries.
/// 3. Alphanumeric (incl. underscore) — fallback.
///
/// Note: The pattern uses word boundaries with `\b`.
static TOKEN_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // Single-line alternation. Order matters: UUID first so hex IDs
    // aren't broken into 8/4/4/4/12 alphanumeric pieces.
    Regex::new(
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}|\b\d{4,}\b|[a-zA-Z0-9_]+",
    )
    .expect("BM25 token regex must compile")
});

pub struct BM25Scorer {
    pub k1: f64,
    pub b: f64,
    pub normalize_score: bool,
    pub max_score: f64,
}

impl Default for BM25Scorer {
    fn default() -> Self {
        BM25Scorer {
            k1: 1.5,
            b: 0.75,
            normalize_score: true,
            max_score: 10.0,
        }
    }
}

impl BM25Scorer {
    #[allow(dead_code)]
    pub fn new(k1: f64, b: f64, normalize_score: bool, max_score: f64) -> Self {
        assert!(max_score.is_finite() && max_score > 0.0, "max_score must be positive and finite");
        BM25Scorer {
            k1,
            b,
            normalize_score,
            max_score,
        }
    }

    /// Tokenize text: lowercase + regex. Returns lowercase tokens in document order.
    fn tokenize(&self, text: &str) -> Vec<String> {
        if text.is_empty() {
            return Vec::new();
        }
        let lower = text.to_lowercase();
        TOKEN_PATTERN
            .find_iter(&lower)
            .map(|m| m.as_str().to_string())
            .collect()
    }

    /// BM25 score for a single (doc, query) pair. Returns
    /// `(raw_score, matched_terms)`.
    ///
    /// The `idf = ln(2)` constant is a simplified single-document IDF 
    /// — same formula whether scoring one item or batch-scoring many.
    fn bm25_score(
        &self,
        doc_tokens: &[String],
        query_freq: &HashMap<String, usize>,
        avg_doc_len: f64,
    ) -> (f64, Vec<String>) {
        if doc_tokens.is_empty() || query_freq.is_empty() {
            return (0.0, Vec::new());
        }

        let doc_len = doc_tokens.len() as f64;
        let avgdl = if avg_doc_len > 0.0 {
            avg_doc_len
        } else if doc_len > 0.0 {
            doc_len
        } else {
            1.0
        };

        // Doc-side term frequency.
        let mut doc_freq: HashMap<&str, usize> = HashMap::new();
        for t in doc_tokens {
            *doc_freq.entry(t.as_str()).or_insert(0) += 1;
        }

        let mut score = 0.0;
        let mut matched: Vec<String> = Vec::new();
        let idf = 2.0_f64.ln();

        // For matched_terms we only care about MEMBERSHIP not ordering 
        // downstream, but we sort tokens alphabetically here for deterministic 
        // output when multiple terms match.
        let mut keys: Vec<&String> = query_freq.keys().collect();
        keys.sort();

        for term in keys {
            let qf = query_freq[term];
            let Some(&f) = doc_freq.get(term.as_str()) else {
                continue;
            };
            matched.push(term.clone());

            let f = f as f64;
            let numerator = f * (self.k1 + 1.0);
            let denominator = f + self.k1 * (1.0 - self.b + self.b * doc_len / avgdl);
            let term_score = idf * numerator / denominator;
            score += term_score * qf as f64;
        }

        (score, matched)
    }

    /// Compute the final normalized score with the long-match bonus and term-match boost.
    fn finalize_score(&self, raw: f64, matched: &[String]) -> f64 {
        let mut normalized = if self.normalize_score {
            (raw / self.max_score).min(1.0)
        } else {
            raw
        };
        // Matched-term boost rules:
        if !matched.is_empty() {
            normalized = normalized.max(0.3);
            if matched.len() >= 2 {
                normalized = (normalized + 0.2).min(1.0);
            }
        }
        if matched.iter().any(|t| t.len() >= 8) {
            normalized = (normalized + 0.3).min(1.0);
        }
        normalized
    }
}

impl RelevanceScorer for BM25Scorer {
    fn score(&self, item: &str, context: &str) -> RelevanceScore {
        let item_tokens = self.tokenize(item);
        let context_tokens = self.tokenize(context);

        // Build query frequency map from context.
        let mut query_freq: HashMap<String, usize> = HashMap::new();
        for t in &context_tokens {
            *query_freq.entry(t.clone()).or_insert(0) += 1;
        }

        let (raw, matched) = self.bm25_score(&item_tokens, &query_freq, 0.0);
        let normalized = self.finalize_score(raw, &matched);

        let reason = match matched.len() {
            0 => "BM25: no term matches".to_string(),
            1 => format!("BM25: matched '{}'", matched[0]),
            n => {
                let preview: Vec<&str> = matched.iter().take(3).map(|s| s.as_str()).collect();
                let suffix = if n > 3 { "..." } else { "" };
                format!(
                    "BM25: matched {} terms ({}{})",
                    n,
                    preview.join(", "),
                    suffix
                )
            }
        };

        // Limit matched_terms field for readability.
        let matched_capped: Vec<String> = matched.iter().take(10).cloned().collect();

        RelevanceScore::new(normalized, reason, matched_capped)
    }

    fn score_batch(&self, items: &[&str], context: &str) -> Vec<RelevanceScore> {
        let context_tokens = self.tokenize(context);

        if context_tokens.is_empty() {
            return items
                .iter()
                .map(|_| RelevanceScore::empty("BM25: empty context"))
                .collect();
        }

        let mut query_freq: HashMap<String, usize> = HashMap::new();
        for t in &context_tokens {
            *query_freq.entry(t.clone()).or_insert(0) += 1;
        }

        // Pre-tokenize all items.
        let all_tokens: Vec<Vec<String>> = items.iter().map(|item| self.tokenize(item)).collect();

        // Average document length:
        //   avg_len = sum(len(t) for t in all_tokens) / max(len(items), 1)
        let total_len: usize = all_tokens.iter().map(|t| t.len()).sum();
        let avg_len = total_len as f64 / items.len().max(1) as f64;

        all_tokens
            .into_iter()
            .map(|item_tokens| {
                let (raw, matched) = self.bm25_score(&item_tokens, &query_freq, avg_len);
                let normalized = self.finalize_score(raw, &matched);

                let reason = match matched.len() {
                    0 => "BM25: no matches".to_string(),
                    n => format!("BM25: {} terms", n),
                };

                // Limit matched_terms for batch path.
                let matched_capped: Vec<String> = matched.iter().take(5).cloned().collect();
                RelevanceScore::new(normalized, reason, matched_capped)
            })
            .collect()
    }
}

