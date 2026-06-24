//! Field-name hashing for cache keys.
//!
//! Used to look up anonymized `preserve_fields` — anonymized keys are stored
//! as SHA-256[:8] for privacy, so cache lookups will silently miss
//! if the truncation length drifts.
//!
//! The hashed name is used to look up staged `preserve_fields`; the function
//! itself is staged along with the cache layer.
#![allow(dead_code)]

use sha2::{Digest, Sha256};

/// SHA-256 of the UTF-8 bytes, hex-encoded, truncated to **8** chars.
///
/// Lowercase hex.
pub fn hash_field_name(field_name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(field_name.as_bytes());
    let digest = hasher.finalize();
    // Truncate to first 8 hex chars (4 bytes of digest).
    let hex = format!("{:x}", digest);
    let limit = hex.len().min(8);
    hex[..limit].to_string()
}

