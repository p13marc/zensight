//! Out-of-band hardware health, read from the BMC (#953, epic #952).
//!
//! # What was missing
//!
//! Nothing in ZenSight read a power supply. `sysinfo`'s `collect.power` is
//! RAPL energy, hwmon fan RPM and battery capacity — a CPU-and-laptop
//! surface. A grep for `ipmi`, `redfish` or `power supply` matched **zero**
//! files in the tree and zero of the 567 tracker issues.
//!
//! That is SYS-SUP-001 (power-supply state) entirely, and it is also the
//! blind spot behind SYS-SUP-010: on a server whose sensors sit behind a BMC
//! and are never exported to hwmon — most rack hardware — the platform
//! reported nothing about temperature or fans either, so "temperature and fan
//! speed" was met only on hosts that happened to have hwmon.
//!
//! # What this sensor deliberately is not
//!
//! **It cannot act.** No `Actions/Chassis.Reset`, no IPMI `chassis power`, no
//! write procedure of any kind — and `tests/registry_conformance.rs` fails the
//! build if one appears in the slice. A monitor that can power-cycle a server
//! is a different threat model from one that reads its fan speed, and crossing
//! that line is a decision to make deliberately, in its own issue, with the
//! #283 gate pattern. #956 is that decision being taken for PDU outlets; this
//! is not it.
//!
//! **It invents no thresholds.** Every assertion comes from the BMC's own
//! `Health` / `State` / `Redundancy` enums. The BMC knows the rating of the
//! hardware it is soldered to; we do not, and a number we made up about
//! someone else's silicon is worse information than none. Numeric thresholds
//! are `ThresholdsConfig` rules (#931), from an operator who decided. The
//! BMC's own thresholds are published *beside* each reading, so a consumer can
//! make the comparison the hardware vendor intended.
//!
//! **"Not measured" is never a zero.** A BMC that does not answer yields a
//! `bmc-unreachable` assertion, a `reachable` gauge of 0, and **no** other
//! gauges — not a chassis of zeroes. A supply bay reported `Absent` publishes
//! `present: false` and no watts.
//!
//! # Attribution (#883)
//!
//! Everything is published under the **reporting host's** origin, with the
//! chassis in the key path and in the labels. A managed chassis is a facet of
//! the vantage point that polls it, exactly as a guest is a facet of its
//! hypervisor and a probe target of its prober: it does not publish for
//! itself, and filing its series under its own name would put them on no
//! host's card at all.

/// The `{chassis}` chunk: which chassis, of which endpoint, a key and an
/// alert label name.
///
/// **Both halves are load-bearing.** The Redfish chassis id is `1`, `2`,
/// `Self` or `Enclosure` — a name that is unique inside one service and
/// nowhere else, so it cannot stand alone on a fleet. The endpoint name is
/// unique on the fleet and says nothing about *which* chassis, so it cannot
/// stand alone on a blade enclosure or a four-node twin, where one Redfish
/// service fronts several (#1130). One chunk rather than two levels keeps the
/// registry families as they are: `{chassis}` is a wildcard chunk and does not
/// care what is in it.
///
/// It is deliberately NOT conditional on how many chassis this sweep found. A
/// disambiguator that appears when a second chassis shows up and vanishes when
/// it drops out moves every series of the survivor, and its old state document
/// becomes an LWW ghost nothing ever overwrites.
///
pub fn chassis_chunk(endpoint: &str, chassis_id: &str) -> String {
    zensight_sensor_core::key::device_chunk(chassis_value(endpoint, chassis_id))
        .as_str()
        .to_string()
}

/// The `{chassis}` value **before** it is a chunk — what the generated
/// telemetry builders take (#1274). The builder slugs it exactly as
/// [`chassis_chunk`] does, once; handing it the chunk would escape it a
/// second time (the slug is injective), so a key site uses this and a state
/// key or a label uses [`chassis_chunk`], and the two name the same chassis.
pub fn chassis_value(endpoint: &str, chassis_id: &str) -> String {
    format!("{endpoint}-{chassis_id}")
}

/// The chunk for a fact about the **endpoint** rather than about any one
/// chassis: `reachable`, and the `bmc-unreachable` assertion. A BMC that did
/// not answer produced no chassis list, so there is nothing else to name it
/// with.
pub fn endpoint_chunk(endpoint: &str) -> String {
    zensight_sensor_core::key::device_chunk(endpoint)
        .as_str()
        .to_string()
}

pub mod alerts;
pub mod config;
pub mod ipmi;
pub mod poller;
pub mod redfish;

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::registry::bmc::Subject;
    use zensight_common::subject::TelemetrySubject;

    /// Every telemetry family renders the tail it rendered when the chunks
    /// were spelled by hand (#1274): the builder is handed the raw
    /// `endpoint-chassis` pair and the raw component id.
    #[test]
    fn typed_subjects_render_the_hand_spelled_tails() {
        let raw = chassis_value("rack-a", "1");
        let cases = [
            (Subject::psu_present(&raw, "0"), "rack-a-1/psu/0/present"),
            (
                Subject::psu_input_watts(&raw, "0"),
                "rack-a-1/psu/0/input_watts",
            ),
            (
                Subject::psu_output_watts(&raw, "0"),
                "rack-a-1/psu/0/output_watts",
            ),
            (
                Subject::psu_capacity_watts(&raw, "0"),
                "rack-a-1/psu/0/capacity_watts",
            ),
            (Subject::fan_rpm(&raw, "3"), "rack-a-1/fan/3/rpm"),
            (
                Subject::thermal_celsius(&raw, "cpu1"),
                "rack-a-1/thermal/cpu1/celsius",
            ),
            (
                Subject::thermal_upper_critical_c(&raw, "cpu1"),
                "rack-a-1/thermal/cpu1/upper_critical_c",
            ),
            (
                Subject::thermal_upper_warning_c(&raw, "cpu1"),
                "rack-a-1/thermal/cpu1/upper_warning_c",
            ),
            (
                Subject::drive_life_left_percent(&raw, "0"),
                "rack-a-1/drive/0/life_left_percent",
            ),
            (Subject::reachable("rack-a"), "rack-a/reachable"),
        ];
        for (subject, tail) in cases {
            assert_eq!(subject.tail(), tail, "{subject:?}");
        }
    }

    /// The builder's slug is `device_chunk`'s, applied once: the `{chassis}`
    /// chunk a telemetry key carries is the chunk the state key and the
    /// `chassis` label carry, for a legal id and a foreign one alike.
    #[test]
    fn the_builder_slugs_as_the_chunk_helpers_do() {
        for (endpoint, id) in [
            ("rack-a", "1"),
            ("Rack A", "Self"),
            ("bmc.example", "Enclosure 2"),
        ] {
            let subject = Subject::psu_present(chassis_value(endpoint, id), "Bay 1");
            let vars = subject.vars();
            assert_eq!(vars[0], ("chassis", chassis_chunk(endpoint, id)));
            assert_eq!(
                vars[1].1,
                zensight_sensor_core::key::device_chunk("Bay 1")
                    .as_str()
                    .to_string()
            );
            let reachable = Subject::reachable(endpoint);
            assert_eq!(reachable.vars()[0].1, endpoint_chunk(endpoint));
        }
    }
}
