//! Redfish / BMC wire types (#953).
//!
//! Nothing in ZenSight read a power supply. `sysinfo`'s `collect.power` is
//! RAPL energy, hwmon fan RPM and battery capacity — a CPU-and-laptop
//! surface. On rack hardware whose sensors sit behind a baseboard management
//! controller and are never exported to hwmon (which is most of it), the
//! platform reported nothing about temperature or fans either, so
//! SYS-SUP-010 was only met on hosts that happened to have hwmon.
//!
//! These are **state-class payloads**, so the #815 gate wants real schemas
//! rather than summaries, and that is why they live here rather than in the
//! sensor crate (the hostspec precedent, #816).
//!
//! # Absent is not zero, and health is not a threshold
//!
//! Two rules run through every type in this file.
//!
//! A slot the BMC reports as `Absent` publishes `present: false` **and no
//! watts** — not `0 W`, which reads as a supply drawing nothing rather than a
//! bay with nothing in it. Every reading is an `Option` for the same reason.
//!
//! And the verdicts are the BMC's own [`Health`] / [`State`] /
//! [`Redundancy`] enums, never a numeric threshold this sensor invents. The
//! BMC knows the rating of the hardware it is soldered to; we do not. Numeric
//! thresholds arrive with #931's `ThresholdsConfig`, from an operator who
//! decided.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Redfish `Status.Health`. The BMC's own verdict on a component.
///
/// `Unknown` is the honest answer for a BMC that answered without a health
/// field, and is deliberately **not** a fault: absent evidence is not evidence
/// of a fault, and a sensor that treats it as one pages on missing data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "PascalCase")]
pub enum Health {
    OK,
    Warning,
    Critical,
    #[serde(other)]
    #[default]
    Unknown,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::OK => "ok",
            Health::Warning => "warning",
            Health::Critical => "critical",
            Health::Unknown => "unknown",
        }
    }

    /// Whether this is a verdict of *failure*. `Unknown` is not.
    pub fn is_faulted(self) -> bool {
        matches!(self, Health::Warning | Health::Critical)
    }
}

/// Redfish `Status.State`. Whether the component is there and working, as
/// distinct from [`Health`], which is how well.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "PascalCase")]
pub enum State {
    Enabled,
    Disabled,
    StandbyOffline,
    StandbySpare,
    InTest,
    Starting,
    /// The slot is empty. Nothing about it is a measurement.
    Absent,
    UnavailableOffline,
    Deferring,
    Quiesced,
    Updating,
    Qualified,
    #[serde(other)]
    #[default]
    Unknown,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Enabled => "enabled",
            State::Disabled => "disabled",
            State::StandbyOffline => "standby-offline",
            State::StandbySpare => "standby-spare",
            State::InTest => "in-test",
            State::Starting => "starting",
            State::Absent => "absent",
            State::UnavailableOffline => "unavailable-offline",
            State::Deferring => "deferring",
            State::Quiesced => "quiesced",
            State::Updating => "updating",
            State::Qualified => "qualified",
            State::Unknown => "unknown",
        }
    }

    /// Whether the slot holds anything at all.
    pub fn is_present(self) -> bool {
        !matches!(self, State::Absent)
    }
}

/// Redfish `Redundancy.Status` for a redundancy group, reduced to what an
/// operator acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Redundancy {
    /// The group is at or above its minimum count and healthy.
    Full,
    /// Still serving, but one more failure is an outage.
    Degraded,
    /// The group is not providing redundancy.
    Failed,
}

/// One power supply bay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PowerSupply {
    /// The BMC's own id for the bay, as it appears in the key.
    pub id: String,
    /// The BMC's label, when it gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the bay holds a supply at all. **A false here means every
    /// reading below is absent**, not zero.
    pub present: bool,
    pub health: Health,
    pub state: State,
    /// Watts drawn from the mains. Absent on a BMC that does not meter, which
    /// is not the same as a supply drawing nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_watts: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_watts: Option<f64>,
    /// Nameplate capacity, for reading the two above against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_watts: Option<f64>,
    /// Which redundancy group this supply belongs to, when the BMC says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<Redundancy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

/// One fan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Fan {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub present: bool,
    pub health: Health,
    pub state: State,
    /// Absent on a BMC that reports fan speed as a percentage of maximum and
    /// nothing else — a different quantity, and publishing it as `rpm` would
    /// be a wrong number rather than a missing one (the #954 lesson).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redundancy: Option<Redundancy>,
}

/// One thermal sensor, with the BMC's own thresholds beside the reading.
///
/// The thresholds ride along because they are the only ones that mean
/// anything: they are the hardware's, set by the vendor against the board it
/// is measuring. A number this sensor invented would be a guess about someone
/// else's silicon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ThermalSensor {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub health: Health,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub celsius: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_critical_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upper_warning_c: Option<f64>,
}

impl ThermalSensor {
    /// Whether the reading is at or above the BMC's **own** upper-critical
    /// threshold. `None` when either half is missing — a comparison with a
    /// threshold we made up is not a comparison.
    pub fn over_critical(&self) -> Option<bool> {
        Some(self.celsius? >= self.upper_critical_c?)
    }
}

/// The chassis rollup: what the BMC says about the machine as a whole.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Chassis {
    /// The id in the Redfish tree, and the chunk in the key.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_tag: Option<String>,
    /// `On` / `Off` as the BMC spells it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_state: Option<String>,
    /// Physical intrusion, where the chassis has a switch for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intrusion: Option<String>,
    pub health: Health,
    pub state: State,
    /// BMC firmware version, for the fleet-wide "what is out of date" question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    /// Which Redfish surface this chassis actually served — the modern
    /// `PowerSubsystem`/`ThermalSubsystem` or the legacy `Power`/`Thermal`.
    ///
    /// Recorded because the two are not interchangeable and a field that is
    /// absent on one is a genuinely different fact from one that is absent on
    /// the other. An operator reading a thin document needs to know which.
    pub surface: RedfishSurface,
}

/// Which pair of Redfish resources answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RedfishSurface {
    /// `Chassis/{id}/PowerSubsystem` + `ThermalSubsystem` (Redfish 2020.4+).
    Subsystem,
    /// `Chassis/{id}/Power` + `Thermal`. Deprecated upstream, and the only
    /// thing a great deal of shipped firmware serves.
    Legacy,
    /// Neither answered.
    None,
}

impl RedfishSurface {
    pub fn as_str(self) -> &'static str {
        match self {
            RedfishSurface::Subsystem => "subsystem",
            RedfishSurface::Legacy => "legacy",
            RedfishSurface::None => "none",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Unknown` is not a fault. A BMC that answered without a health field
    /// has told us nothing, and a sensor that reads nothing as "broken" pages
    /// on missing data — the failure this whole file is arranged against.
    #[test]
    fn an_unknown_health_is_not_a_fault() {
        assert!(!Health::Unknown.is_faulted());
        assert!(!Health::OK.is_faulted());
        assert!(Health::Warning.is_faulted());
        assert!(Health::Critical.is_faulted());
    }

    /// Redfish adds states between releases, and a BMC may send one this
    /// build has never heard of. That must deserialize, not fail the whole
    /// document — one unknown enum should not cost the other eleven fields.
    #[test]
    fn an_unrecognised_state_falls_back_rather_than_failing_the_document() {
        let s: State = serde_json::from_str("\"SomethingRedfish2031Added\"").unwrap();
        assert_eq!(s, State::Unknown);
        let h: Health = serde_json::from_str("\"Excellent\"").unwrap();
        assert_eq!(h, Health::Unknown);
    }

    /// An empty bay is present:false, and that is the only thing it is.
    #[test]
    fn an_absent_slot_is_not_a_present_one() {
        assert!(!State::Absent.is_present());
        assert!(State::Enabled.is_present());
        assert!(
            State::UnavailableOffline.is_present(),
            "offline is still installed"
        );
    }

    /// A threshold comparison needs both halves. Half of one is not a verdict.
    #[test]
    fn a_thermal_verdict_needs_both_the_reading_and_the_threshold() {
        let mut t = ThermalSensor {
            id: "0".into(),
            name: None,
            health: Health::OK,
            state: State::Enabled,
            celsius: Some(91.0),
            upper_critical_c: Some(90.0),
            upper_warning_c: None,
        };
        assert_eq!(t.over_critical(), Some(true));
        t.celsius = Some(20.0);
        assert_eq!(t.over_critical(), Some(false));
        t.upper_critical_c = None;
        assert_eq!(t.over_critical(), None, "no threshold, no verdict");
        t.upper_critical_c = Some(90.0);
        t.celsius = None;
        assert_eq!(t.over_critical(), None, "no reading, no verdict");
    }
}
