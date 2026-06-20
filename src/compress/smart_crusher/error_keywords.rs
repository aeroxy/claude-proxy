//! Canonical error keyword set for item preservation.
//!
//! These are the preservation signals. Intentionally broad — better to over-preserve than to
//! drop a real error item.
//!
//! Used by `detect_error_items_for_preservation`. The list is small
//! enough to keep as a `&[&str]`; if we ever cross ~50 keywords, switch
//! to a `phf::Set` or pre-built FST for sub-linear lookup.

/// 12 error/failure keywords. Lowercase by construction; callers must lowercase the
/// haystack before substring-matching.
pub const ERROR_KEYWORDS: &[&str] = &[
    "error",
    "exception",
    "failed",
    "failure",
    "critical",
    "fatal",
    "crash",
    "panic",
    "abort",
    "timeout",
    "denied",
    "rejected",
];

