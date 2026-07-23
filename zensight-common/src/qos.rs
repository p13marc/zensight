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

    /// Express is off everywhere on a low-bandwidth link (batching beats latency).
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

    #[test]
    fn default_is_telemetry() {
        assert_eq!(QosClass::default(), QosClass::Telemetry);
    }
}
