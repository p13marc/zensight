//! Shared telemetry-subscription plumbing for the exporters (#763).
//!
//! # What is shared, and what deliberately is not
//!
//! The two exporters' subscribers began as near-copies, and the case for
//! folding them into one was that every fix had to be applied twice. Working
//! through the correctness backlog changed that picture: the parts that
//! diverged did so for *real* reasons, and forcing them back into one shape
//! would be a false abstraction.
//!
//! - The **Prometheus** exporter holds alert STATE — a gauge that must vanish
//!   when an alert resolves. That is why it needs a liveliness watch and a
//!   startup seed GET (#758), neither of which means anything to an
//!   append-only consumer.
//! - The **OTel** exporter emits append-only log records, which is why it
//!   subscribes to the `events` class (#762) — records the Prometheus exporter
//!   must never take, because an append-only ULID-keyed stream on `/metrics` is
//!   a cardinality explosion (#104).
//!
//! What is genuinely identical is the *telemetry* path: the class-selector
//! guard and the payload decode, which were byte-for-byte the same in both.
//! That lives here, along with the subscriber construction, so the delivery
//! semantics cannot drift between the two again.

use zenoh::Session;
use zenoh::sample::Sample;
use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};

use crate::telemetry::TelemetryPoint;

/// The default telemetry selector: the v1 telemetry class, fleet-wide.
///
/// Base-**relative**. The session applies the deployment base as its Zenoh
/// namespace (#466), so a selector that spells the base matches nothing.
pub const DEFAULT_TELEMETRY_KEY_EXPR: &str = "v1/*/telemetry/**";

/// Declare the telemetry subscriber, with history and recovery.
///
/// # Why an advanced subscriber
///
/// Sensors publish telemetry through a zenoh-ext `AdvancedPublisher`, and the
/// GUI has always subscribed with `history` + `recovery`. Both exporters used a
/// plain `declare_subscriber`, so an exporter started after the sensors got
/// **no backfill**, and a sample dropped in flight was simply lost.
///
/// For a metrics pipeline that is the wrong trade: a gap in a dashboard is a
/// claim about the world, and "we never asked for the data" is not a reason to
/// make it. `detect_late_publishers` covers the sensor that comes up after us;
/// `recovery` covers the sample that went missing between us.
pub async fn declare_telemetry_subscriber(
    session: &Session,
    key_expr: &str,
) -> zenoh::Result<zenoh_ext::AdvancedSubscriber<zenoh::handlers::FifoChannelHandler<Sample>>> {
    session
        .declare_subscriber(key_expr)
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .subscriber_detection()
        .await
}

/// Decode a telemetry sample, rejecting anything outside the telemetry class.
///
/// The selector already scopes the subscription, but a key expression is a
/// pattern and the guard is structural: `is_telemetry_key` parses through the
/// registry rather than matching strings, so an operator-supplied
/// `filters.key_expr` that widens the scope cannot smuggle state or `@media`
/// keys into the metric path.
///
/// Returns `None` for a non-telemetry key or an undecodable payload; the caller
/// counts the two differently, which is why the reason is distinguishable.
pub fn decode_telemetry(sample: &Sample) -> Result<TelemetryPoint, DecodeReject> {
    if !crate::keyexpr::is_telemetry_key(sample.key_expr().as_str()) {
        return Err(DecodeReject::NotTelemetry);
    }
    let payload = sample.payload().to_bytes();
    // JSON first, then CBOR: `Format::default()` is CBOR, but a deployment may
    // still carry JSON publishers, and sniffing beats configuring.
    serde_json::from_slice(&payload)
        .ok()
        .or_else(|| ciborium::from_reader(&payload[..]).ok())
        .ok_or(DecodeReject::Undecodable)
}

/// The fleet events selector, base-relative like [`DEFAULT_TELEMETRY_KEY_EXPR`].
pub const DEFAULT_EVENTS_KEY_EXPR: &str = "v1/*/events/**";

/// Declare the events subscriber, with history and recovery.
///
/// # Why an advanced subscriber here too
///
/// An `events`-class record is a rare, deliberate statement that something
/// happened — a trap, a unit failing. Missing one is not a gap in a chart, it
/// is an event that never existed as far as every later reader is concerned,
/// and unlike a telemetry sample nothing will restate it a second later.
///
/// `detect_late_publishers` covers the sensor that comes up after the
/// subscriber; `recovery` covers the record that went missing between them.
/// Both matter more for this class than for telemetry, not less.
///
/// The events plane is append-only with immutable ULID keys
/// (`docs/KEYSPACE.md`), so a replay is idempotent for any consumer that keys
/// by the record's own id — which is what makes history safe to ask for.
pub async fn declare_events_subscriber(
    session: &Session,
    key_expr: &str,
) -> zenoh::Result<zenoh_ext::AdvancedSubscriber<zenoh::handlers::FifoChannelHandler<Sample>>> {
    session
        .declare_subscriber(key_expr)
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .subscriber_detection()
        .await
}

/// Decode an events-class sample, rejecting anything outside the class.
///
/// The structural guard is the same one [`decode_telemetry`] uses and for the
/// same reason: the selector is a pattern, an operator may widen it, and a
/// widened one must not smuggle state or `@media` keys into the event store.
pub fn decode_event(sample: &Sample) -> Result<crate::EventRecord, DecodeReject> {
    if !crate::keyexpr::is_events_key(sample.key_expr().as_str()) {
        return Err(DecodeReject::NotTelemetry);
    }
    let payload = sample.payload().to_bytes();
    serde_json::from_slice(&payload)
        .ok()
        .or_else(|| ciborium::from_reader(&payload[..]).ok())
        .ok_or(DecodeReject::Undecodable)
}

/// Why a telemetry sample was not turned into a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeReject {
    /// The key is not in the telemetry class.
    NotTelemetry,
    /// The payload parsed as neither JSON nor CBOR.
    Undecodable,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default selector is base-relative. A selector spelling the
    /// deployment base matches nothing since #466 — with a healthy session and
    /// an empty dashboard, which is the failure this constant exists to avoid
    /// being restated wrongly in two crates.
    #[test]
    fn the_default_selector_is_base_relative() {
        assert_eq!(DEFAULT_TELEMETRY_KEY_EXPR, "v1/*/telemetry/**");
        assert!(
            crate::keyexpr::validate_relative_selector(DEFAULT_TELEMETRY_KEY_EXPR).is_ok(),
            "the shared default must pass the validator both exporters now run"
        );
    }

    /// The telemetry selector structurally cannot reach state, events or the
    /// `@media` plane — which is why each needs its own subscription.
    #[test]
    fn the_telemetry_selector_reaches_only_telemetry() {
        use zenoh::key_expr::KeyExpr;

        let telemetry = KeyExpr::new(DEFAULT_TELEMETRY_KEY_EXPR).unwrap();
        for foreign in [
            "v1/h-0123456789ab/state/netlink/alert/9f2c81ab04d7e3f1",
            "v1/h-0123456789ab/events/snmp/trap/01hqzz000000000000000000ab",
            "v1/h-0123456789ab/@media/parallax/cam0/video/h264/high",
        ] {
            let k = KeyExpr::new(foreign).unwrap();
            assert!(
                !telemetry.intersects(&k),
                "the telemetry selector must not reach {foreign}"
            );
        }
    }
}
