//! Default `Observer` implementation. The live SmartCrusher path
//! doesn't subscribe to events; the public observer surface is staged.

//! Default `Observer` implementation.
//!
//! Ships [`TracingObserver`] which writes each `CrushEvent` to the
//! `tracing` crate at `debug` level. Subscribers that filter `debug`
//! out (typical production config) pay nothing — `tracing` stops
//! evaluation at the level check before constructing the event
//! fields. Subscribers that retain `debug` get a structured per-crush
//! event suitable for log analytics.

use super::traits::{CrushEvent, Observer};

/// Writes each `CrushEvent` to the `tracing` crate at `debug` level.
/// Zero-cost when the subscriber filters `debug` out.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingObserver;

impl Observer for TracingObserver {
    fn name(&self) -> &str {
        "tracing"
    }

    fn on_event(&self, event: &CrushEvent) {
        // `tracing::debug!` is a macro; the level check happens before
        // the fields are evaluated, so this is essentially free at
        // higher log levels.
        tracing::debug!(
            target: "compress::smart_crusher",
            strategy = %event.strategy,
            input_bytes = event.input_bytes,
            output_bytes = event.output_bytes,
            elapsed_ns = event.elapsed_ns,
            was_modified = event.was_modified,
            "smart_crusher.crush emitted",
        );
    }
}
