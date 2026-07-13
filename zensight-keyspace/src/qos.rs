//! The five named QoS profiles (RFC 04 §3). The profile vocabulary is closed;
//! registry entries reference these by name and publishers set QoS only
//! through them.

use zenoh::qos::{CongestionControl, Priority, Reliability};

/// A named QoS profile: reliability × congestion control × priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QosProfile {
    /// `telemetry` default — superseded samples; a drop is replaced.
    Sampled,
    /// `state` that self-heals by refresh cadence.
    Refreshed,
    /// `state` written on rare transitions consumers cannot learn late; `events`.
    Transition,
    /// `state/*/alert/*` — a transition that must arrive promptly.
    Alert,
    /// `@media` — a stale frame is worthless; the encoder must never block.
    Frame,
}

impl QosProfile {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "sampled" => Some(Self::Sampled),
            "refreshed" => Some(Self::Refreshed),
            "transition" => Some(Self::Transition),
            "alert" => Some(Self::Alert),
            "frame" => Some(Self::Frame),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Sampled => "sampled",
            Self::Refreshed => "refreshed",
            Self::Transition => "transition",
            Self::Alert => "alert",
            Self::Frame => "frame",
        }
    }

    pub fn reliability(self) -> Reliability {
        match self {
            Self::Sampled | Self::Refreshed | Self::Frame => Reliability::BestEffort,
            Self::Transition | Self::Alert => Reliability::Reliable,
        }
    }

    pub fn congestion_control(self) -> CongestionControl {
        match self {
            Self::Sampled | Self::Refreshed | Self::Frame => CongestionControl::Drop,
            Self::Transition | Self::Alert => CongestionControl::Block,
        }
    }

    pub fn priority(self) -> Priority {
        match self {
            Self::Sampled => Priority::DataLow,
            Self::Refreshed | Self::Transition => Priority::Data,
            Self::Alert | Self::Frame => Priority::InteractiveHigh,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for p in [
            QosProfile::Sampled,
            QosProfile::Refreshed,
            QosProfile::Transition,
            QosProfile::Alert,
            QosProfile::Frame,
        ] {
            assert_eq!(QosProfile::from_name(p.name()), Some(p));
        }
        assert_eq!(QosProfile::from_name("telemetry"), None); // old vocabulary
    }

    /// Pins the RFC 04 §3 table.
    #[test]
    fn profile_table() {
        assert_eq!(QosProfile::Sampled.priority(), Priority::DataLow);
        assert_eq!(
            QosProfile::Alert.congestion_control(),
            CongestionControl::Block
        );
        assert_eq!(
            QosProfile::Frame.congestion_control(),
            CongestionControl::Drop
        );
        assert_eq!(QosProfile::Frame.priority(), Priority::InteractiveHigh);
        assert_eq!(QosProfile::Transition.reliability(), Reliability::Reliable);
    }
}
