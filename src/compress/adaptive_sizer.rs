//! Adaptive compression sizing via information saturation detection.
//!
//! Decides *how many* items to keep statistically by detecting the "knee point" of an information
//! saturation curve.
//!
//! # Algorithm overview
//!
//! Three-tier decision:
//! 1. **Fast path**: trivial cases (`n <= 8` → keep all) and near-total
//!    redundancy (≤3 unique-by-simhash → keep that count).
//! 2. **Standard**: Kneedle on cumulative unique-bigram coverage curve.
//!    Coverage stops growing → that's the knee → return that count.
//! 3. **Validation**: zlib-ratio sanity check. If keeping `k` items
//!    produces a much-more-redundant subset than the full set, bump
//!    `k` by 20%.
//!
//! # Implementation details
//!
//! - `simhash` hashes character 4-grams via MD5, then aggregates bits
//!   via weighted voting. Takes the first 64 bits of the MD5 digest as a
//!   big-endian `u64`.
//! - `compute_unique_bigram_curve` operates on whitespace-split words.
//!   Single-word items emit `(word, "")`. Empty-string items emit
//!   `("", "")`.
//! - `find_knee` requires `> 0.05` deviation from the diagonal in
//!   normalized space; threshold is strict (`<` returns None).
//! - `_validate_with_zlib` uses `zlib.compress(..., level=1)`. We use
//!   `flate2` with the default miniz_oxide backend.

use flate2::write::ZlibEncoder;
use flate2::Compression;
use md5::{Digest, Md5};
use std::collections::HashSet;
use std::io::Write;

/// Compute the optimal number of items to keep via information saturation.
///
/// # Arguments
///
/// - `items`: string representations of items in importance order.
/// - `bias`: multiplier on the knee point (>1 = keep more, <1 = compress
///   harder).
/// - `min_k`: lower bound on the return value.
/// - `max_k`: upper bound; `None` means "no cap" (i.e. up to `items.len()`).
pub fn compute_optimal_k(items: &[&str], bias: f64, min_k: usize, max_k: Option<usize>) -> usize {
    let n = items.len();
    let effective_max = max_k.unwrap_or(n);

    // Tier 1: fast path.
    if n <= 8 {
        return n.clamp(min_k.min(effective_max), effective_max);
    }

    // Near-total redundancy: at most 3 unique groups → keep that many.
    let unique_count = count_unique_simhash(items, 3);
    if unique_count <= 3 {
        let k = min_k.max(unique_count);
        return k.min(effective_max);
    }

    // Tier 2: Kneedle on bigram-coverage curve.
    let curve = compute_unique_bigram_curve(items);
    let mut knee = find_knee(&curve);

    // Diversity ratio: fraction of items that are genuinely unique.
    let diversity_ratio = unique_count as f64 / n as f64;

    knee = match knee {
        None => {
            // No saturation found — scale keep-fraction with diversity.
            // diversity ~1.0 → keep 100%; ~0.0 → keep 30%.
            let keep_fraction = 0.3 + 0.7 * diversity_ratio;
            Some(min_k.max((n as f64 * keep_fraction) as usize))
        }
        Some(k) if diversity_ratio > 0.7 => {
            // Knee found, but high diversity — apply diversity floor so
            // we don't drop below `n * (0.3 + 0.7 * diversity)`.
            let floor = min_k.max((n as f64 * (0.3 + 0.7 * diversity_ratio)) as usize);
            Some(k.max(floor))
        }
        some => some,
    };

    let knee = knee.unwrap_or(min_k); // defensive — knee path always sets Some above

    // Apply bias multiplier.
    let mut k = min_k.max((knee as f64 * bias) as usize);
    k = k.min(effective_max);

    // Tier 3: zlib-ratio validation.
    k = validate_with_zlib(items, k, effective_max, 0.15);

    // Final clamp.
    k.clamp(min_k.min(effective_max), effective_max)
}

/// Find the knee in a monotonically-increasing curve (Kneedle).
///
/// Returns the 1-indexed count `knee_idx + 1` so the caller can use it
/// directly as a "keep this many" value.
pub fn find_knee(curve: &[usize]) -> Option<usize> {
    let n = curve.len();
    if n < 3 {
        return None;
    }

    let x_min: usize = 0;
    let x_max: usize = n - 1;
    let y_min = curve[0] as f64;
    let y_max = curve[n - 1] as f64;

    if (y_max - y_min).abs() < f64::EPSILON {
        // Flat curve — all items are identical.
        // Returns the literal `1`.
        return Some(1);
    }

    let x_range = (x_max - x_min) as f64;
    let y_range = y_max - y_min;

    let mut max_diff: f64 = -1.0;
    let mut knee_idx: Option<usize> = None;

    for (i, &y) in curve.iter().enumerate() {
        let x_norm = (i - x_min) as f64 / x_range;
        let y_norm = (y as f64 - y_min) / y_range;
        let diff = y_norm - x_norm;
        if diff > max_diff {
            max_diff = diff;
            knee_idx = Some(i);
        }
    }

    if max_diff < 0.05 {
        return None;
    }

    knee_idx.map(|i| i + 1)
}

/// Cumulative unique-bigram coverage curve.
///
/// Each item contributes its word-level bigrams; single-word items 
/// contribute `(word, "")`. The curve at index `k` is the running count 
/// of unique bigrams after seeing `items[0..=k]`.
pub fn compute_unique_bigram_curve(items: &[&str]) -> Vec<usize> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut curve: Vec<usize> = Vec::with_capacity(items.len());

    for item in items {
        let lower = item.to_lowercase();
        let words: Vec<&str> = lower.split_whitespace().collect();
        if words.len() < 2 {
            // Single word or empty: synthesize a unigram-bigram.
            let first = words.first().copied().unwrap_or("");
            seen.insert((first.to_string(), String::new()));
        } else {
            for j in 0..words.len() - 1 {
                seen.insert((words[j].to_string(), words[j + 1].to_string()));
            }
        }
        curve.push(seen.len());
    }

    curve
}

/// 64-bit SimHash fingerprint of a text string.
///
/// Algorithm:
/// 1. Iterate character 4-grams (sliding window). For input shorter
///    than 4 chars, the loop runs once with the entire string as the
///    only "gram". Empty input still iterates once with `""`.
/// 2. Hash each gram with MD5; take the first 64 bits as a big-endian `u64`.
/// 3. For each bit position 0..64, increment a vote counter when the
///    bit is set, decrement when clear.
/// 4. Final fingerprint: bit `j` is set iff `votes[j] > 0` (strict).
pub fn simhash(text: &str) -> u64 {
    let lower = text.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let n = chars.len();

    // Sliding window of character 4-grams.
    let iter_count = if n <= 3 { 1 } else { n - 3 };

    let mut votes: [i32; 64] = [0; 64];

    for i in 0..iter_count {
        // 4-character window starting at char index i. For short input,
        // this is just the whole string padded by being shorter than 4.
        let gram: String = chars[i..(i + 4).min(n)].iter().collect();

        let digest = Md5::digest(gram.as_bytes());
        // First 8 bytes of the 16-byte digest, big-endian → u64.
        let h = u64::from_be_bytes([
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
        ]);

        for (j, vote) in votes.iter_mut().enumerate() {
            if (h >> j) & 1 == 1 {
                *vote += 1;
            } else {
                *vote -= 1;
            }
        }
    }

    let mut fingerprint: u64 = 0;
    for (j, &v) in votes.iter().enumerate() {
        if v > 0 {
            fingerprint |= 1 << j;
        }
    }
    fingerprint
}

/// Hamming distance between two 64-bit SimHash fingerprints.
#[inline]
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Count items with distinct content via SimHash + greedy clustering.
///
/// Two items cluster together when their fingerprints are within
/// `threshold` Hamming distance.
pub fn count_unique_simhash(items: &[&str], threshold: u32) -> usize {
    if items.is_empty() {
        return 0;
    }

    let fingerprints: Vec<u64> = items.iter().map(|s| simhash(s)).collect();
    let mut clusters: Vec<u64> = Vec::new();

    for &fp in &fingerprints {
        let mut matched = false;
        for &rep in &clusters {
            if hamming_distance(fp, rep) <= threshold {
                matched = true;
                break;
            }
        }
        if !matched {
            clusters.push(fp);
        }
    }

    clusters.len()
}

/// zlib-based compression-ratio validation of the chosen `k`.
///
/// If the subset `items[..k]` compresses *much* better than the full
/// set, the subset is missing diversity → bump `k` by 20%.
///
/// `tolerance` is the maximum allowed ratio difference (default 0.15 = 15%).
pub fn validate_with_zlib(items: &[&str], k: usize, max_k: usize, tolerance: f64) -> usize {
    if k >= items.len() || k >= max_k {
        return k;
    }

    let full_text = items.join("\n");
    let subset_text = items[..k].join("\n");

    // Skip validation for very small content (zlib overhead dominates).
    if full_text.len() < 200 {
        return k;
    }

    let full_compressed = zlib_compressed_len(full_text.as_bytes());
    let subset_compressed = zlib_compressed_len(subset_text.as_bytes());

    let full_ratio = if !full_text.is_empty() {
        full_compressed as f64 / full_text.len() as f64
    } else {
        1.0
    };
    let subset_ratio = if !subset_text.is_empty() {
        subset_compressed as f64 / subset_text.len() as f64
    } else {
        1.0
    };

    let ratio_diff = full_ratio - subset_ratio;

    if ratio_diff > tolerance {
        // Subset compresses much better than full → bump k by 20%.
        let adjusted = ((k as f64) * 1.2) as usize;
        return adjusted.min(max_k);
    }

    k
}

/// Compress `bytes` with zlib level=1 and return the output length.
///
/// Wraps `flate2::ZlibEncoder` at `Compression::fast()` (level 1).
fn zlib_compressed_len(bytes: &[u8]) -> usize {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    // Writes are infallible for an in-memory Vec.
    encoder.write_all(bytes).expect("in-memory write");
    let compressed = encoder.finish().expect("flush");
    compressed.len()
}

