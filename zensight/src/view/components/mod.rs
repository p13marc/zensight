//! Reusable UI components for specialized views.
//!
//! These components provide domain-specific visualizations that can be
//! composed to build protocol-specific views.

pub mod data_table;
pub mod gauge;
pub mod kit;
pub mod progress_bar;
pub mod sparkline;
pub mod status_led;
pub mod tabs;

pub use data_table::{Column, DataTable, SortKey, TableState};
pub use gauge::{Gauge, GaugeStyle};
pub use kit::{badge, card, empty_state, rgb, rgba, section_header};
pub use progress_bar::{ProgressBar, ProgressBarStyle};
pub use sparkline::Sparkline;
pub use status_led::{StatusLed, StatusLedState};
pub use tabs::{TabItem, tabbed_view};
