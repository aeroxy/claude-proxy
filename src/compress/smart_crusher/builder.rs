//! `SmartCrusherBuilder` is the staged composition entry point. The
//! live path constructs `SmartCrusher` directly; the builder is
//! reserved for the public-API phase, so its items are intentionally
//! unread for now.

//! # Defaults vs explicit
//!
//! `SmartCrusherBuilder::new()` starts EMPTY — no scorer, no
//! constraints, no observers. You get exactly what you ask for. Use
//! [`with_default_oss_setup`](SmartCrusherBuilder::with_default_oss_setup)
//! to start from the OSS default and customize from there. This is
//! the "no silent fallback" rule applied to composition: the builder
//! makes your intent explicit; the `new()` factory shorthand for the
//! OSS preset.

use crate::compress::anchor_selector::{AnchorConfig, AnchorSelector};
use crate::compress::relevance::{BM25Scorer, RelevanceScorer};

use super::analyzer::SmartAnalyzer;
use super::compaction::CompactionStage;
use super::config::SmartCrusherConfig;
use super::constraints::default_oss_constraints;
use super::crusher::SmartCrusher;
use super::observer::TracingObserver;
use super::traits::{Constraint, Observer};

/// Builder for `SmartCrusher`. See module docs.
pub struct SmartCrusherBuilder {
    config: SmartCrusherConfig,
    anchor_config: Option<AnchorConfig>,
    scorer: Option<Box<dyn RelevanceScorer + Send + Sync>>,
    constraints: Vec<Box<dyn Constraint>>,
    observers: Vec<Box<dyn Observer>>,
    compaction: Option<CompactionStage>,
}

impl SmartCrusherBuilder {
    pub fn new(config: SmartCrusherConfig) -> Self {
        SmartCrusherBuilder {
            config,
            anchor_config: None,
            scorer: None,
            constraints: Vec::new(),
            observers: Vec::new(),
            compaction: None,
        }
    }

    /// Override the default `AnchorConfig` (rare — most callers leave
    /// this as the default).
    pub fn anchor_config(mut self, cfg: AnchorConfig) -> Self {
        self.anchor_config = Some(cfg);
        self
    }

    /// Set the relevance scorer.
    pub fn with_scorer(mut self, scorer: Box<dyn RelevanceScorer + Send + Sync>) -> Self {
        self.scorer = Some(scorer);
        self
    }

    /// Append a constraint. Constraints stack — the must-keep set is
    /// the union of every constraint's output. Order does not affect
    /// correctness but is preserved in observer event strategy strings
    /// for determinism.
    pub fn add_constraint(mut self, c: Box<dyn Constraint>) -> Self {
        self.constraints.push(c);
        self
    }

    /// Append the OSS default constraint stack (`KeepErrorsConstraint`
    /// plus `KeepStructuralOutliersConstraint`) to the current builder.
    /// Composes naturally with `add_constraint`:
    ///
    /// ```ignore
    /// SmartCrusherBuilder::new(cfg)
    ///     .add_default_oss_constraints()
    ///     .add_constraint(Box::new(MyBusinessRule))
    /// ```
    pub fn add_default_oss_constraints(mut self) -> Self {
        self.constraints.extend(default_oss_constraints());
        self
    }

    /// Append an observer. Observers stack — every event fires every
    /// observer in registration order.
    pub fn add_observer(mut self, o: Box<dyn Observer>) -> Self {
        self.observers.push(o);
        self
    }

    /// Apply the OSS default setup: `BM25Scorer`,
    /// default-OSS-constraints, `TracingObserver`. Does **not** enable
    /// the lossless compaction stage — call [`Self::with_compaction`]
    /// separately if needed (as [`SmartCrusher::new`] does).
    pub fn with_default_oss_setup(self) -> Self {
        self.with_scorer(Box::<BM25Scorer>::default())
            .add_default_oss_constraints()
            .add_observer(Box::new(TracingObserver))
    }

    /// Plug in a compaction stage. When set, `crush_array` runs the
    /// stage before the lossy pipeline; if it produces a non-`Untouched`
    /// compaction the rendered bytes are returned via
    /// [`CrushArrayResult::compacted`]. The lossy result still fills
    /// `items` so callers can choose either output.
    ///
    /// [`CrushArrayResult::compacted`]: super::crusher::CrushArrayResult::compacted
    pub fn with_compaction(mut self, stage: CompactionStage) -> Self {
        self.compaction = Some(stage);
        self
    }

    /// Convenience: enable the OSS compaction preset (CSV+schema
    /// formatter, default `CompactConfig`). Equivalent to
    /// `with_compaction(CompactionStage::default_csv_schema())`.
    pub fn with_default_compaction(self) -> Self {
        self.with_compaction(CompactionStage::default_csv_schema())
    }

    /// Construct the `SmartCrusher`. If `with_scorer` was not called,
    /// falls back to `BM25Scorer::default()` so a builder with no
    /// other customization still produces a working crusher.
    pub fn build(self) -> SmartCrusher {
        let analyzer = SmartAnalyzer::new(self.config.clone());
        let anchor_selector = AnchorSelector::new(self.anchor_config.unwrap_or_default());
        let scorer = self.scorer.unwrap_or_else(|| Box::<BM25Scorer>::default());
        SmartCrusher::from_parts(
            self.config,
            anchor_selector,
            scorer,
            analyzer,
            self.constraints,
            self.observers,
            self.compaction,
        )
    }
}
