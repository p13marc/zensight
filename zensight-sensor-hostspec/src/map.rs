//! Point construction for the one telemetry subject (#821).

use zensight_common::registry::hostspec::Subject;
use zensight_common::{TelemetryPoint, TelemetryValue};

/// The one telemetry subject this sensor publishes (#1274).
pub const FAILING: Subject = Subject::AssertionsFailing;

/// The failing-assertion count, published every sweep — zero included: an
/// empty assertion set reads as 0, never as silence.
pub fn failing_point(source: &str, failing: usize) -> TelemetryPoint {
    TelemetryPoint::for_subject(source, &FAILING, TelemetryValue::Gauge(failing as f64))
}
