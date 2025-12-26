//! Reusable UI components for specialized views.
//!
//! These components provide domain-specific visualizations that can be
//! composed to build protocol-specific views.

pub mod gauge;
pub mod progress_bar;
pub mod sparkline;
pub mod status_led;

pub use gauge::{Gauge, GaugeStyle};
pub use progress_bar::{ProgressBar, ProgressBarStyle};
pub use sparkline::Sparkline;
pub use status_led::{StatusLed, StatusLedState};
