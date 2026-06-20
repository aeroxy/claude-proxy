//! OSS default `Constraint` implementations.
//!
//! Two constraints ship in the default OSS composition:
//!
//! - [`KeepErrorsConstraint`] — items containing error/failure
//!   keywords (`error`, `exception`, `failed`, `fatal`, etc.). Wraps
//!   the existing [`detect_error_items_for_preservation`] function.
//! - [`KeepStructuralOutliersConstraint`] — items with rare fields or
//!   rare categorical values. Wraps the existing
//!   [`detect_structural_outliers`] function.
//!
//! Both are byte-equivalent to the pre-PR1 hardcoded behavior — the
//! detection logic is unchanged; the constraints are thin trait
//! adapters so that custom Enterprise constraints can be stacked
//! alongside or in place of these defaults.
//!
//! # The default factory
//!
//! [`default_oss_constraints`] returns the OSS default stack as a
//! `Vec<Box<dyn Constraint>>`. `SmartCrusher::new(config)` uses this;
//! `SmartCrusherBuilder::new(config)` does NOT (the builder gives you
//! exactly what you ask for, no surprises). To get OSS defaults plus
//! your own constraints, use `add_default_oss_constraints()` on the
//! builder.

use serde_json::Value;

use super::outliers::{detect_error_items_for_preservation, detect_structural_outliers};
use super::traits::Constraint;

// ── KeepErrorsConstraint ──────────────────────────────────────────────────

/// OSS default: keep items that contain error keywords.
///
/// "Error keyword" is matched case-insensitively against the JSON
/// serialization of each item against the list in [`super::error_keywords::ERROR_KEYWORDS`]
/// (`error`, `exception`, `failed`, `fatal`, `critical`, `crash`, `panic`,
/// `abort`, `timeout`, `denied`, `rejected`).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeepErrorsConstraint;

impl Constraint for KeepErrorsConstraint {
    fn name(&self) -> &str {
        "keep_errors"
    }

    fn must_keep(&self, items: &[Value], item_strings: Option<&[String]>) -> Vec<usize> {
        detect_error_items_for_preservation(items, item_strings)
    }
}

// ── KeepStructuralOutliersConstraint ─────────────────────────────────────

/// OSS default: keep items that are *structurally* unusual within the
/// array. Two flavors of unusual:
///
/// - **Rare fields**: items that have a key present in fewer than the
///   uniqueness threshold of items.
/// - **Rare values for common fields**: items whose value for a
///   high-cardinality field appears infrequently (the "rare-status"
///   path that fires on enums like `level: "ERROR"` among many `INFO`).
///
/// Implementation is unchanged from pre-PR1; this is a thin wrapper.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeepStructuralOutliersConstraint;

impl Constraint for KeepStructuralOutliersConstraint {
    fn name(&self) -> &str {
        "keep_structural_outliers"
    }

    fn must_keep(&self, items: &[Value], _item_strings: Option<&[String]>) -> Vec<usize> {
        detect_structural_outliers(items)
    }
}

// ── Default OSS stack ─────────────────────────────────────────────────────

/// Returns the default OSS constraint stack used by
/// `SmartCrusher::new(config)`.
///
/// Stack contents (in order):
/// 1. [`KeepErrorsConstraint`]
/// 2. [`KeepStructuralOutliersConstraint`]
///
/// The order does not affect output (the must-keep set is a union),
/// but is fixed for determinism in observer-emitted strategy strings
/// and audit logs.
pub fn default_oss_constraints() -> Vec<Box<dyn Constraint>> {
    vec![
        Box::new(KeepErrorsConstraint),
        Box::new(KeepStructuralOutliersConstraint),
    ]
}

