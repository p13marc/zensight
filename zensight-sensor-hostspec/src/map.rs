//! Point construction for the one telemetry subject (#821).

use zensight_common::{TelemetryPoint, TelemetryValue};

/// The failing-assertion count, published every sweep — zero included: an
/// empty assertion set reads as 0, never as silence.
pub fn failing_point(source: &str, failing: usize) -> TelemetryPoint {
    crate::telemetry_guard::checked_point(
        source,
        "assertions/failing",
        TelemetryValue::Gauge(failing as f64),
    )
}
