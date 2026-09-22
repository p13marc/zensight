//! One built telemetry point beside the subject it publishes under (#1274).
//!
//! The publish loops render the key from the subject, and the metric on the
//! point is the subject's tail by construction — a subject the registry does
//! not declare has no spelling.

use zensight_common::registry::logs::Subject;
use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};

/// A point and the subject it publishes under.
pub type Built = (Subject, TelemetryPoint);

/// A point under `subject` from `source`, paired with it.
pub fn built(source: &str, subject: Subject, value: TelemetryValue) -> Built {
    let point = TelemetryPoint::for_subject(source, &subject, value);
    (subject, point)
}
