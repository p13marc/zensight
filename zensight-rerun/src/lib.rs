//! EVALUATION-ONLY Rerun visualization adapter for ZenSight (epic #415).
//!
//! Subscribes to the Zenoh bus exactly like the exporters do (telemetry +
//! alerts + health + entities) and feeds a [Rerun](https://rerun.io) recording
//! stream — live to a viewer, to a `.rrd` file, or both. Design and findings
//! live in `docs/plans/rerun/`.
//!
//! Layering rule (grep-gated): [`rerun_sink`] is the only module that may
//! `use rerun`; everything else is Rerun-free and unit-testable without the
//! dependency, via the [`sink::VisualizationSink`] seam.

pub mod config;
pub mod events;
pub mod mapping;
pub mod rerun_sink;
pub mod session;
pub mod sink;
pub mod subscriber;

pub use config::{
    ConfigError, CounterPolicy, FilterConfig, RerunConfig, RerunMode, SamplingConfig,
};
pub use events::{EventKind, EventSeverity, NormalizedEvent};
pub use rerun_sink::RerunSink;
pub use sink::{ControlItem, SinkWorker, TestSink, VisualizationSink, WorkerStats};
