//! Per-traffic-class Zenoh QoS for low-bandwidth / unreliable links.
//!
//! Telemetry is loss-tolerant — a dropped sample is superseded by the next — so it
//! drops under congestion at low priority and never back-pressures a sensor. Alerts,
//! commands, evidence and entities *must* arrive, so they are reliable + block at a
//! higher priority. `express` is off everywhere: it disables batching to cut latency
//! at the cost of bandwidth, the wrong trade on a constrained link (priority already
//! orders control ahead of telemetry).
//!
//! Apply with the getters on a Zenoh publisher/put/declare builder, e.g.
//! ```ignore
//! use zenoh::prelude::QoSBuilderTrait;
//! let q = QosClass::Alert;
//! session
//!     .put(key, payload)
//!     .congestion_control(q.congestion_control())
//!     .priority(q.priority())
//!     .express(q.express())
//!     .reliability(q.reliability())
//!     .await?;
//! ```

use zenoh::qos::{CongestionControl, Priority, Reliability};

/// A traffic class, mapped to a fixed Zenoh QoS profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QosClass {
    /// High-rate telemetry samples (superseded by the next sample). Drop-friendly.
    #[default]
    Telemetry,
    /// Health / liveness / error snapshots — periodic, superseded, low value if lost.
    HealthLiveness,
    /// Alert state changes (firing / resolved / tombstone) — must arrive.
    Alert,
    /// Runtime commands + status — must arrive.
    Command,
    /// Identity evidence — must arrive (correlator correctness).
    Evidence,
    /// Materialized host entities — must arrive.
    Entity,
    /// Append-only event records (`events` class, #534) — must arrive, like
    /// alerts: a dropped trap/transition is unrecoverable (nothing supersedes
    /// it). Priority `Data`: ahead of telemetry, behind live alert flips.
    Event,
    /// Queryable replies / on-demand bulk detail — reliable, but low priority (bulk).
    Query,
    /// Live media (`@media` plane, #359): opaque access units, superseded by the
    /// next frame. Drop-friendly but latency-sensitive — a stale frame is
    /// worthless, so it rides ahead of telemetry at `InteractiveHigh` while still
    /// dropping (never blocking the encoder) under congestion.
    LiveVideo,
}

impl QosClass {
    /// Reliability: best-effort for superseded streams, reliable for must-arrive.
    pub fn reliability(self) -> Reliability {
        match self {
            QosClass::Telemetry | QosClass::HealthLiveness | QosClass::LiveVideo => {
                Reliability::BestEffort
            }
            _ => Reliability::Reliable,
        }
    }

    /// Congestion control: drop the superseded, block (back-pressure) the must-arrive.
    pub fn congestion_control(self) -> CongestionControl {
        match self {
            QosClass::Telemetry | QosClass::HealthLiveness | QosClass::LiveVideo => {
                CongestionControl::Drop
            }
            _ => CongestionControl::Block,
        }
    }

    /// Priority: control (alerts/commands) ahead of telemetry; bulk at the bottom.
    pub fn priority(self) -> Priority {
        match self {
            QosClass::Telemetry => Priority::DataLow,
            QosClass::HealthLiveness => Priority::Data,
            QosClass::Alert | QosClass::Command | QosClass::LiveVideo => Priority::InteractiveHigh,
            QosClass::Evidence | QosClass::Entity | QosClass::Event => Priority::Data,
            QosClass::Query => Priority::DataLow,
        }
    }

    /// Express is off for **every** class, `LiveVideo` included (#733).
    ///
    /// Express means "send this message on its own, do not wait to batch it
    /// with the next one". The intuition that a video frame wants it is
    /// wrong twice over. Batching only engages when there is something to
    /// batch *with* — i.e. under back-pressure — so on an unsaturated link
    /// express is a no-op that still costs per-message framing; and when the
    /// link *is* saturated it spends that overhead at exactly the moment a
    /// `drop`-profile publisher should be shedding instead. This is what
    /// zenkey RFC v1.26 M1 concluded when it removed `express` from the
    /// `frame` profile, and this method has always agreed with it.
    ///
    /// **parallax's `ZenohSink::media` disagrees** — it sets `express = true`
    /// on the `frame` profile (`src/elements/network/zenoh.rs:1568`, doc at
    /// `:1538`: "because a stale frame is worthless and the encoder must never
    /// block"). We do not adopt that sink; the parallax sensor publishes
    /// through `zensight-sensor-core`'s `RawMediaPublisher`, which reads its
    /// QoS from here. See `zensight-sensor-parallax/docs/qos-express.md` for
    /// the decision and its reasoning.
    ///
    /// Pinned by `express_is_off_for_every_class` below, so this does not get
    /// "fixed" toward parallax's table.
    pub fn express(self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_is_drop_besteffort_low() {
        let q = QosClass::Telemetry;
        assert_eq!(q.congestion_control(), CongestionControl::Drop);
        assert_eq!(q.reliability(), Reliability::BestEffort);
        assert_eq!(q.priority(), Priority::DataLow);
        assert!(!q.express());
    }

    #[test]
    fn alerts_and_control_are_reliable_block_high() {
        for q in [QosClass::Alert, QosClass::Command] {
            assert_eq!(q.congestion_control(), CongestionControl::Block);
            assert_eq!(q.reliability(), Reliability::Reliable);
            assert_eq!(q.priority(), Priority::InteractiveHigh);
        }
    }

    #[test]
    fn evidence_and_entity_are_reliable_block() {
        for q in [QosClass::Evidence, QosClass::Entity] {
            assert_eq!(q.congestion_control(), CongestionControl::Block);
            assert_eq!(q.reliability(), Reliability::Reliable);
        }
    }

    /// #359: live video is loss-tolerant (drop, best-effort — never block the
    /// encoder) but latency-sensitive (interactive-high, ahead of telemetry).
    /// Express stays off: batching still wins on a constrained link.
    #[test]
    fn live_video_is_drop_besteffort_interactive_high() {
        let q = QosClass::LiveVideo;
        assert_eq!(q.congestion_control(), CongestionControl::Drop);
        assert_eq!(q.reliability(), Reliability::BestEffort);
        assert_eq!(q.priority(), Priority::InteractiveHigh);
        assert!(!q.express());
    }

    /// Express stays off for every class — the whole table, not a line of two
    /// other tests (#733).
    ///
    /// zenkey RFC v1.26 M1 removed `express` from the `frame` profile:
    /// batching engages only under back-pressure, so express is a no-op on an
    /// unsaturated link and spends per-message overhead exactly when a `drop`
    /// profile should be shedding. parallax's `ZenohSink::media` sets it *on*
    /// for the same plane (`src/elements/network/zenoh.rs:1568`), which is the
    /// table a future reader is most likely to copy from. We deliberately do
    /// not adopt that sink — see `zensight-sensor-parallax/docs/qos-express.md`
    /// — so flipping any of these to `true` is a wire-behaviour change that
    /// must go through that document, not through this assertion.
    #[test]
    fn express_is_off_for_every_class() {
        for q in [
            QosClass::Telemetry,
            QosClass::HealthLiveness,
            QosClass::Alert,
            QosClass::Command,
            QosClass::LiveVideo,
            QosClass::Evidence,
            QosClass::Entity,
            QosClass::Event,
            QosClass::Query,
        ] {
            assert!(
                !q.express(),
                "{q:?} turned express on; see docs/qos-express.md before changing this"
            );
        }
    }

    #[test]
    fn default_is_telemetry() {
        assert_eq!(QosClass::default(), QosClass::Telemetry);
    }
}
