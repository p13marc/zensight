//! The seam a telemetry point passes on its way to the bus (#930, epic #901).
//!
//! # Why this exists, and why it is here rather than in `sensor-core`
//!
//! Epic #901 says `Publisher::publish_to_key` is "the single choke point every
//! telemetry point in all ten publishing sensors passes through". **It is
//! not.** There are three paths:
//!
//! 1. [`crate::PublisherRegistry`], reached through
//!    `zensight_sensor_core::Publisher`;
//! 2. `zensight_sensor_core::AdvancedPublisherRegistry`, an independent type
//!    with its own publisher cache and its own encode — which is what netlink,
//!    netring, snmp and logs actually use for the bulk of their telemetry;
//! 3. [`crate::PublisherRegistry`] reached **directly**, with a payload the
//!    caller encoded itself — sysinfo, gnmi, modbus and netflow.
//!
//! `Publisher::publish` appears zero times in sysinfo, netlink, netring, snmp
//! and logs *combined*. A hook on it would have covered the smallest share of
//! the fleet's telemetry while looking like it covered all of it.
//!
//! So the observer is a trait here, in `zensight-common`, where every one of
//! those paths can hold one; and the thing that implements it —
//! `zensight_sensor_core::threshold::ThresholdEvaluator` — lives where the
//! `AlertReporter` does.
//!
//! # It must be cheap and it must not block
//!
//! This runs on the publish path of every telemetry point a sensor emits, and
//! sysinfo alone emits hundreds a tick. So:
//!
//! - the call is **synchronous** — anything that needs to `.await` (publishing
//!   an alert) is the implementor's problem, on its own task;
//! - the point is passed **by reference, unencoded**, so nothing allocates and
//!   nothing is decoded;
//! - a registry with no observer installed costs one `Option` check.

use crate::TelemetryPoint;

/// Something that watches every telemetry point on its way out.
///
/// Implementors **must not block**. The publish path is hot and, in the case
/// of `@rpc` handlers sharing a runtime with it, serial.
pub trait PointObserver: Send + Sync + std::fmt::Debug {
    /// Called once per point, before it is encoded.
    ///
    /// `key` is the base-relative full key
    /// (`v1/<origin>/telemetry/<producer>/<metric...>`); `point.metric` is the
    /// same name without the prefix, which is what a rule matches on.
    fn observe_point(&self, key: &str, point: &TelemetryPoint);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl PointObserver for Recorder {
        fn observe_point(&self, _key: &str, point: &TelemetryPoint) {
            self.0.lock().unwrap().push(point.metric.clone());
        }
    }

    /// The trait is object-safe and shareable — it is installed once as an
    /// `Arc<dyn PointObserver>` and read from every publish.
    #[test]
    fn the_observer_is_object_safe_and_shareable() {
        let recorder = std::sync::Arc::new(Recorder::default());
        let as_dyn: std::sync::Arc<dyn PointObserver> = recorder.clone();
        as_dyn.observe_point(
            "v1/h-a/telemetry/sysinfo/cpu/usage",
            &TelemetryPoint::new(
                "host1",
                crate::Protocol::Sysinfo,
                "cpu/usage",
                crate::TelemetryValue::Gauge(1.0),
            ),
        );
        assert_eq!(recorder.0.lock().unwrap().as_slice(), ["cpu/usage"]);
    }
}
