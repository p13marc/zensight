//! The assertions (#953).
//!
//! Every rule reads a verdict the **BMC** reached, never a number this sensor
//! invented. That is not squeamishness: the BMC knows the rating of the
//! hardware it is soldered to and we do not, so a threshold we made up would
//! be a guess about someone else's silicon — worse information than none.
//! Where an operator wants a numeric threshold it is a #931 rule in
//! `ThresholdsConfig`, from someone who decided.
//!
//! `grade` is pure — no bus, no HTTP — so the whole rule table is testable
//! against documents.

use std::collections::HashMap;

use zensight_common::bmc::{
    Chassis, Drive, Fan, MemoryModule, PowerSupply, Redundancy, RedundancyGroup, State,
    ThermalSensor,
};
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

use crate::config::AlertsConfig;

pub const RULE_UNREACHABLE: &str = "bmc-unreachable";
pub const RULE_PSU_FAILED: &str = "psu-failed";
pub const RULE_PSU_ABSENT: &str = "psu-absent";
pub const RULE_PSU_REDUNDANCY: &str = "psu-redundancy-lost";
pub const RULE_FAN_FAILED: &str = "fan-failed";
pub const RULE_THERMAL_CRITICAL: &str = "thermal-critical";
pub const RULE_CHASSIS_HEALTH: &str = "chassis-health";
/// A physical drive the BMC has marked faulted, or whose SMART bit predicts
/// failure (#1140).
pub const RULE_DRIVE_FAILED: &str = "drive-failed";
/// A memory module the BMC has marked faulted (#1140).
pub const RULE_MEMORY_FAILED: &str = "memory-failed";
/// A power or thermal redundancy group that is no longer redundant, read from
/// the **group** rather than from a member's copy (#1140).
pub const RULE_REDUNDANCY_LOST: &str = "redundancy-lost";

/// Every rule this build can raise.
///
/// The poller reconciles each one every sweep, so a condition that clears
/// resolves — including one whose whole input disappeared (a supply pulled, a
/// chassis removed from the config). It is also what the reporter adopts on
/// restart (#882), so a rule that stops existing retires its inherited alerts
/// rather than leaving them firing forever.
pub const ALL_RULES: &[&str] = &[
    RULE_UNREACHABLE,
    RULE_PSU_FAILED,
    RULE_PSU_ABSENT,
    RULE_PSU_REDUNDANCY,
    RULE_FAN_FAILED,
    RULE_THERMAL_CRITICAL,
    RULE_CHASSIS_HEALTH,
    RULE_DRIVE_FAILED,
    RULE_MEMORY_FAILED,
    RULE_REDUNDANCY_LOST,
];

/// One chassis's sweep, as the rules see it.
pub struct Observation<'a> {
    /// The **reporting host** — the `source` of every series and alert this
    /// sensor emits (#883). The chassis is a facet of this vantage point, not
    /// a separate machine that publishes for itself; it rides in the labels,
    /// where a rename costs nothing and where `alert_key` cannot see it.
    pub source: &'a str,
    /// The operator's name for the BMC endpoint — one Redfish service, which
    /// on a blade enclosure or a four-node twin fronts SEVERAL chassis. It is
    /// not by itself a name for the thing an alert is about (#1130).
    pub endpoint: &'a str,
    /// `None` when the BMC did not answer this cycle.
    pub chassis: Option<&'a Chassis>,
    pub supplies: &'a [PowerSupply],
    pub fans: &'a [Fan],
    pub thermal: &'a [ThermalSensor],
    /// Physical drives behind the systems this chassis links (#1140).
    pub drives: &'a [Drive],
    /// Memory modules behind the systems this chassis links (#1140).
    pub memory: &'a [MemoryModule],
    /// Power and thermal redundancy groups as the chassis reports them
    /// (#1140), not as a member reports its own group.
    pub redundancy: &'a [RedundancyGroup],
    /// Bays this endpoint has reported present at some point in this process's
    /// life. A bay that was never populated is not a bay someone emptied.
    pub known_present: &'a [String],
    /// Consecutive cycles in which the BMC did not answer.
    pub consecutive_failures: u32,
}

impl Observation<'_> {
    /// The `{chassis}` chunk this observation's alerts are labelled and
    /// reconciled by — the same string the poller puts in the key.
    ///
    /// With no chassis there is nothing but the endpoint to name: a BMC that
    /// did not answer returned no chassis list, and `bmc-unreachable` is a
    /// statement about the endpoint anyway.
    fn chassis_label(&self) -> String {
        match self.chassis {
            Some(c) => crate::chassis_chunk(self.endpoint, &c.id),
            None => crate::endpoint_chunk(self.endpoint),
        }
    }

    /// How a summary names where the fault is, for a human. The chunk is for
    /// machines; `rack-a-1 chassis 2` is for the person reading the page.
    fn site(&self) -> String {
        match self.chassis {
            Some(c) => format!("{} chassis {}", self.endpoint, c.id),
            None => self.endpoint.to_string(),
        }
    }
}

fn alert(
    obs: &Observation<'_>,
    rule: &str,
    severity: AlertSeverity,
    summary: String,
    labels: &[(&str, String)],
) -> Alert {
    let mut a = Alert::new(
        obs.source,
        Protocol::Bmc,
        AlertKind::Expectation,
        rule,
        severity,
        summary,
    );
    let mut map = HashMap::new();
    // The label is the KEY CHUNK, not the endpoint name. `alert_key` hashes
    // the discriminating labels, so with the endpoint here two chassis of one
    // service that both have a PSU `0` produced the SAME alert key — each
    // sweep overwriting the other's alert (#1130). It is also what the poller
    // reconciles under, and a label that disagrees with the key it reconciles
    // by resolves the wrong alert.
    map.insert("chassis".to_string(), obs.chassis_label());
    for (k, v) in labels {
        map.insert((*k).to_string(), v.clone());
    }
    a.labels = map;
    a
}

/// Grade one sweep. Returns every currently-firing alert; the caller
/// reconciles per rule, so anything absent here resolves.
pub fn grade(cfg: &AlertsConfig, obs: &Observation<'_>) -> Vec<Alert> {
    let mut out = Vec::new();
    if !cfg.enabled {
        return out;
    }

    // --- bmc-unreachable ----------------------------------------------------
    if obs.consecutive_failures >= cfg.unreachable_cycles {
        out.push(alert(
            obs,
            RULE_UNREACHABLE,
            AlertSeverity::Critical,
            format!(
                "{}: the BMC has not answered for {} consecutive cycles",
                obs.endpoint, obs.consecutive_failures
            ),
            &[],
        ));
    }

    // A BMC that did not answer produced no components this cycle. Grading the
    // component rules now would resolve every one of them as "recovered" —
    // announcing that a failed power supply is fine because we cannot see it.
    // The rules keep their previous state until the BMC answers again. (The
    // lesson SNMP's `device_answered` guard already paid for.)
    if obs.chassis.is_none() {
        return out;
    }

    // --- psu-failed / psu-absent / psu-redundancy-lost ----------------------
    for psu in obs.supplies {
        let name = psu
            .name
            .clone()
            .unwrap_or_else(|| format!("PSU {}", psu.id));
        let labels = [("psu", psu.id.clone()), ("psu_name", name.clone())];

        if psu.present && psu.health.is_faulted() {
            out.push(alert(
                obs,
                RULE_PSU_FAILED,
                // The BMC's own severity, not a mapping we invented: Warning
                // means the supply is working and unhappy, Critical means it
                // is not working.
                if psu.health == zensight_common::bmc::Health::Critical {
                    AlertSeverity::Critical
                } else {
                    AlertSeverity::Warning
                },
                format!(
                    "{}: {name} health is {} (state {})",
                    obs.site(),
                    psu.health.as_str(),
                    psu.state.as_str()
                ),
                &labels,
            ));
        }

        // An empty bay is only news if it was full. A chassis shipped with one
        // supply in a two-bay backplane is normal and permanent, and firing on
        // it would mean every such machine arrives with a standing alert
        // nobody can clear. Off by default for the same reason.
        if cfg.psu_absent
            && psu.state == State::Absent
            && obs.known_present.iter().any(|k| k == &psu.id)
        {
            out.push(alert(
                obs,
                RULE_PSU_ABSENT,
                AlertSeverity::Warning,
                format!("{}: {name} was present and now reads absent", obs.site()),
                &labels,
            ));
        }

        if matches!(
            psu.redundancy,
            Some(Redundancy::Degraded | Redundancy::Failed)
        ) {
            let failed = psu.redundancy == Some(Redundancy::Failed);
            out.push(alert(
                obs,
                RULE_PSU_REDUNDANCY,
                if failed {
                    AlertSeverity::Critical
                } else {
                    AlertSeverity::Warning
                },
                format!(
                    "{}: power redundancy group {} is {}",
                    obs.site(),
                    psu.redundancy_group.as_deref().unwrap_or("(unnamed)"),
                    if failed { "lost" } else { "degraded" }
                ),
                &labels,
            ));
        }
    }

    // --- fan-failed ---------------------------------------------------------
    for fan in obs.fans {
        if fan.present && fan.health.is_faulted() {
            let name = fan
                .name
                .clone()
                .unwrap_or_else(|| format!("fan {}", fan.id));
            let speed = fan
                .rpm
                .map(|r| format!(" at {r:.0} rpm"))
                .unwrap_or_default();
            out.push(alert(
                obs,
                RULE_FAN_FAILED,
                AlertSeverity::Critical,
                format!(
                    "{}: {name} health is {}{speed}",
                    obs.site(),
                    fan.health.as_str()
                ),
                &[("fan", fan.id.clone()), ("fan_name", name.clone())],
            ));
        }
    }

    // --- thermal-critical ---------------------------------------------------
    //
    // Fires on the BMC's health verdict OR on the reading crossing the BMC's
    // OWN upper-critical threshold. Both, because firmware disagrees about
    // which it updates: some set Health and leave the thresholds decorative,
    // some the reverse. Neither is invented here.
    for sensor in obs.thermal {
        let over = sensor.over_critical() == Some(true);
        if !(over || sensor.health.is_faulted()) {
            continue;
        }
        let name = sensor
            .name
            .clone()
            .unwrap_or_else(|| format!("sensor {}", sensor.id));
        let reading = sensor
            .celsius
            .map(|c| format!(" at {c:.0} C"))
            .unwrap_or_default();
        let threshold = sensor
            .upper_critical_c
            .map(|t| format!(" (BMC critical threshold {t:.0} C)"))
            .unwrap_or_default();
        out.push(alert(
            obs,
            RULE_THERMAL_CRITICAL,
            if over || sensor.health == zensight_common::bmc::Health::Critical {
                AlertSeverity::Critical
            } else {
                AlertSeverity::Warning
            },
            format!("{}: {name}{reading}{threshold}", obs.site()),
            &[("sensor", sensor.id.clone()), ("sensor_name", name.clone())],
        ));
    }

    // --- chassis-health -----------------------------------------------------
    //
    // The rollup, for a fault in something this sensor does not enumerate — a
    // drive backplane, a riser, a battery. Without it, a BMC saying "this
    // machine is Critical" for a reason we do not model would be silently
    // dropped, which is exactly the blind spot the sensor exists to close.
    if let Some(chassis) = obs.chassis
        && chassis.health.is_faulted()
    {
        out.push(alert(
            obs,
            RULE_CHASSIS_HEALTH,
            if chassis.health == zensight_common::bmc::Health::Critical {
                AlertSeverity::Critical
            } else {
                AlertSeverity::Warning
            },
            format!(
                "{}: the BMC reports the chassis as {} — check its own event log for what \
                 this sensor does not enumerate",
                obs.site(),
                chassis.health.as_str()
            ),
            &[],
        ));
    }

    // --- drive-failed -------------------------------------------------------
    //
    // The fault `chassis-health` above used to gesture at. A drive the BMC has
    // already marked Warning — the SMART predictive-failure one — rolled up
    // into `Chassis.Status.Health` and nowhere else, so the operator was told
    // "check its own event log" about a fact this sensor could have named.
    for drive in obs.drives.iter().filter(|d| d.present) {
        let name = drive
            .name
            .clone()
            .unwrap_or_else(|| format!("drive {}", drive.id));
        let predicted = drive.failure_predicted == Some(true);
        if !drive.health.is_faulted() && !predicted {
            continue;
        }
        let labels = [("drive", drive.id.clone()), ("drive_name", name.clone())];
        // The BMC's own severity, never a threshold this sensor invented —
        // except that a predicted failure on an otherwise-OK drive is a
        // warning, because the drive is still serving.
        let severity = if drive.health == zensight_common::bmc::Health::Critical {
            AlertSeverity::Critical
        } else {
            AlertSeverity::Warning
        };
        let why = if drive.health.is_faulted() && predicted {
            format!("{} and predicts its own failure", drive.health.as_str())
        } else if predicted {
            "OK, but predicts its own failure (SMART)".to_string()
        } else {
            drive.health.as_str().to_string()
        };
        out.push(alert(
            obs,
            RULE_DRIVE_FAILED,
            severity,
            format!(
                "{}: {name}{} is {why}",
                obs.site(),
                drive
                    .controller
                    .as_ref()
                    .map(|c| format!(" on {c}"))
                    .unwrap_or_default(),
            ),
            &labels,
        ));
    }

    // --- memory-failed ------------------------------------------------------
    for dimm in obs.memory.iter().filter(|m| m.present) {
        if !dimm.health.is_faulted() {
            continue;
        }
        let name = dimm
            .name
            .clone()
            .unwrap_or_else(|| format!("DIMM {}", dimm.id));
        out.push(alert(
            obs,
            RULE_MEMORY_FAILED,
            if dimm.health == zensight_common::bmc::Health::Critical {
                AlertSeverity::Critical
            } else {
                AlertSeverity::Warning
            },
            format!(
                "{}: {name} is {} — a DIMM the BMC has marked, not a rate this sensor computed",
                obs.site(),
                dimm.health.as_str()
            ),
            &[("dimm", dimm.id.clone()), ("dimm_name", name.clone())],
        ));
    }

    // --- redundancy-lost ----------------------------------------------------
    //
    // The GROUP's verdict. `psu-redundancy-lost` above reads a *member's* copy
    // of its group's status, which a supply that is itself fine reports as
    // Full — so a group below `MinNumNeeded` with every survivor healthy was
    // invisible. This reads the group.
    for group in obs.redundancy {
        let lost = matches!(
            group.redundancy,
            Some(zensight_common::bmc::Redundancy::Degraded)
                | Some(zensight_common::bmc::Redundancy::Failed)
        );
        if !lost {
            continue;
        }
        let name = group
            .name
            .clone()
            .unwrap_or_else(|| format!("{} group {}", group.subsystem, group.id));
        let counts = match (group.members, group.min_needed) {
            (Some(have), Some(need)) => format!(" ({have} member(s), {need} needed)"),
            _ => String::new(),
        };
        out.push(alert(
            obs,
            RULE_REDUNDANCY_LOST,
            if group.redundancy == Some(zensight_common::bmc::Redundancy::Failed) {
                AlertSeverity::Critical
            } else {
                AlertSeverity::Warning
            },
            format!("{}: {name} is no longer redundant{counts}", obs.site()),
            &[
                ("group", group.id.clone()),
                ("subsystem", group.subsystem.clone()),
            ],
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::bmc::{Health, RedfishSurface};

    fn chassis(health: Health) -> Chassis {
        chassis_id("1", health)
    }

    fn chassis_id(id: &str, health: Health) -> Chassis {
        Chassis {
            id: id.into(),
            name: None,
            manufacturer: None,
            model: None,
            serial: None,
            asset_tag: None,
            power_state: Some("On".into()),
            intrusion: None,
            health,
            state: State::Enabled,
            firmware: None,
            surface: RedfishSurface::Legacy,
        }
    }

    fn psu(id: &str, health: Health, state: State) -> PowerSupply {
        PowerSupply {
            id: id.into(),
            name: Some(format!("PSU {id}")),
            present: state.is_present(),
            health,
            state,
            input_watts: None,
            output_watts: None,
            capacity_watts: None,
            redundancy_group: None,
            redundancy: None,
            model: None,
            serial: None,
        }
    }

    fn obs<'a>(
        chassis: Option<&'a Chassis>,
        supplies: &'a [PowerSupply],
        fans: &'a [Fan],
        thermal: &'a [ThermalSensor],
        known: &'a [String],
        failures: u32,
    ) -> Observation<'a> {
        Observation {
            source: "mgmt01",
            endpoint: "rack-a-1",
            chassis,
            supplies,
            fans,
            thermal,
            drives: &[],
            memory: &[],
            redundancy: &[],
            known_present: known,
            consecutive_failures: failures,
        }
    }

    fn rules(alerts: &[Alert]) -> Vec<&str> {
        let mut r: Vec<&str> = alerts.iter().map(|a| a.rule.as_str()).collect();
        r.sort_unstable();
        r
    }

    #[test]
    fn a_healthy_chassis_asserts_nothing() {
        let c = chassis(Health::OK);
        let supplies = [psu("0", Health::OK, State::Enabled)];
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &supplies, &[], &[], &[], 0),
        );
        assert!(out.is_empty(), "{:?}", rules(&out));
    }

    /// **The rule that matters most.** A BMC that did not answer produced no
    /// components; grading them now would resolve every one as "recovered" —
    /// announcing that a failed supply is fine because we cannot see it.
    #[test]
    fn an_unreachable_bmc_fires_once_and_grades_nothing_else() {
        let supplies = [psu("0", Health::Critical, State::Enabled)];
        let out = grade(
            &AlertsConfig::default(),
            // No chassis: the BMC did not answer.
            &obs(None, &supplies, &[], &[], &[], 3),
        );
        assert_eq!(rules(&out), vec![RULE_UNREACHABLE]);
        assert!(
            !out.iter().any(|a| a.rule == RULE_PSU_FAILED),
            "the psu rules must keep their previous state, not resolve"
        );
    }

    /// One failed cycle is not an unreachable BMC.
    #[test]
    fn unreachable_waits_for_the_configured_number_of_cycles() {
        for (failures, expected) in [(0, 0), (2, 0), (3, 1), (4, 1)] {
            let out = grade(
                &AlertsConfig::default(),
                &obs(None, &[], &[], &[], &[], failures),
            );
            assert_eq!(out.len(), expected, "{failures} failures");
        }
    }

    /// The BMC's own severity, not a mapping we invented.
    #[test]
    fn a_failed_supply_keeps_the_severity_the_bmc_gave_it() {
        let c = chassis(Health::OK);
        for (health, want) in [
            (Health::Critical, AlertSeverity::Critical),
            (Health::Warning, AlertSeverity::Warning),
        ] {
            let supplies = [psu("0", health, State::Enabled)];
            let out = grade(
                &AlertsConfig::default(),
                &obs(Some(&c), &supplies, &[], &[], &[], 0),
            );
            let a = out.iter().find(|a| a.rule == RULE_PSU_FAILED).unwrap();
            assert_eq!(a.severity, want);
            assert_eq!(a.labels["chassis"], "rack-a-1-1");
            assert_eq!(a.labels["psu"], "0");
        }
    }

    /// **#1130.** The label is the key chunk — endpoint AND chassis — because
    /// `alert_key` hashes the discriminating labels. With the endpoint alone,
    /// two chassis of one Redfish service that each have a PSU `0` produced
    /// the SAME key, and each sweep overwrote the other's alert.
    #[test]
    fn two_chassis_of_one_endpoint_do_not_share_an_alert_key() {
        let cfg = AlertsConfig::default();
        let supplies = [psu("0", Health::Critical, State::Enabled)];

        let c1 = chassis_id("1", Health::OK);
        let c2 = chassis_id("2", Health::OK);
        let a1 = grade(&cfg, &obs(Some(&c1), &supplies, &[], &[], &[], 0));
        let a2 = grade(&cfg, &obs(Some(&c2), &supplies, &[], &[], &[], 0));

        let k1 = a1.iter().find(|a| a.rule == RULE_PSU_FAILED).unwrap();
        let k2 = a2.iter().find(|a| a.rule == RULE_PSU_FAILED).unwrap();
        assert_eq!(k1.labels["chassis"], "rack-a-1-1");
        assert_eq!(k2.labels["chassis"], "rack-a-1-2");
        assert_ne!(
            k1.alert_key(),
            k2.alert_key(),
            "two chassis, one endpoint, the same bay id — these must be two alerts"
        );
    }

    /// The label and the summary answer different questions: the label is what
    /// the key and the reconcile use, the summary is what a person reads.
    #[test]
    fn the_summary_names_the_chassis_the_label_encodes() {
        let c = chassis_id("2", Health::OK);
        let supplies = [psu("0", Health::Critical, State::Enabled)];
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &supplies, &[], &[], &[], 0),
        );
        let a = out.iter().find(|a| a.rule == RULE_PSU_FAILED).unwrap();
        assert!(
            a.summary.starts_with("rack-a-1 chassis 2:"),
            "summary was {:?}",
            a.summary
        );
    }

    /// `bmc-unreachable` is about the Redfish service, and a BMC that did not
    /// answer returned no chassis list — so its label is the endpoint's chunk,
    /// not a chassis chunk this sensor would have had to invent.
    #[test]
    fn the_unreachable_alert_is_labelled_with_the_endpoint() {
        let out = grade(&AlertsConfig::default(), &obs(None, &[], &[], &[], &[], 3));
        let a = out.iter().find(|a| a.rule == RULE_UNREACHABLE).unwrap();
        assert_eq!(a.labels["chassis"], "rack-a-1");
    }

    /// `Unknown` is the BMC declining to say. Treating it as a fault is paging
    /// on missing data.
    #[test]
    fn an_unknown_health_asserts_nothing() {
        let c = chassis(Health::Unknown);
        let supplies = [psu("0", Health::Unknown, State::Enabled)];
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &supplies, &[], &[], &[], 0),
        );
        assert!(out.is_empty(), "{:?}", rules(&out));
    }

    /// An empty bay is only news if it was full. A chassis shipped with one
    /// supply in a two-bay backplane is normal and permanent — firing on it
    /// means every such machine arrives with a standing alert nobody can
    /// clear, which is why the rule is off by default AND needs prior
    /// evidence.
    #[test]
    fn an_absent_bay_is_only_news_if_it_was_ever_populated() {
        let c = chassis(Health::OK);
        let supplies = [psu("1", Health::Unknown, State::Absent)];
        let cfg = AlertsConfig {
            psu_absent: true,
            ..AlertsConfig::default()
        };

        // Never seen populated: silence.
        let out = grade(&cfg, &obs(Some(&c), &supplies, &[], &[], &[], 0));
        assert!(out.is_empty(), "{:?}", rules(&out));

        // Seen populated earlier in this process's life: news.
        let known = ["1".to_string()];
        let out = grade(&cfg, &obs(Some(&c), &supplies, &[], &[], &known, 0));
        assert_eq!(rules(&out), vec![RULE_PSU_ABSENT]);

        // …and off by default, even so.
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &supplies, &[], &[], &known, 0),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn redundancy_degraded_and_lost_are_different_severities() {
        let c = chassis(Health::OK);
        for (r, want) in [
            (Redundancy::Degraded, AlertSeverity::Warning),
            (Redundancy::Failed, AlertSeverity::Critical),
        ] {
            let mut p = psu("0", Health::OK, State::Enabled);
            p.redundancy = Some(r);
            p.redundancy_group = Some("PSU group".into());
            let supplies = [p];
            let out = grade(
                &AlertsConfig::default(),
                &obs(Some(&c), &supplies, &[], &[], &[], 0),
            );
            let a = out.iter().find(|a| a.rule == RULE_PSU_REDUNDANCY).unwrap();
            assert_eq!(a.severity, want);
        }

        // `Full` is not an alert.
        let mut p = psu("0", Health::OK, State::Enabled);
        p.redundancy = Some(Redundancy::Full);
        let supplies = [p];
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &supplies, &[], &[], &[], 0),
        );
        assert!(out.is_empty());
    }

    /// A stopped fan the BMC calls Critical. The zero rpm rides in the summary
    /// because here it IS the measurement.
    #[test]
    fn a_stopped_fan_fires_with_its_reading() {
        let c = chassis(Health::OK);
        let fans = [Fan {
            id: "3".into(),
            name: Some("Fan 4".into()),
            present: true,
            health: Health::Critical,
            state: State::Enabled,
            rpm: Some(0.0),
            redundancy_group: None,
            redundancy: None,
        }];
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &[], &fans, &[], &[], 0),
        );
        let a = out.iter().find(|a| a.rule == RULE_FAN_FAILED).unwrap();
        assert!(a.summary.contains("0 rpm"), "{}", a.summary);
        assert_eq!(a.labels["fan"], "3");
    }

    /// Fires on the BMC's verdict OR on its own threshold, because firmware
    /// disagrees about which it keeps up to date. Neither number is ours.
    #[test]
    fn thermal_fires_on_the_bmc_verdict_or_the_bmc_threshold() {
        let c = chassis(Health::OK);
        let base = ThermalSensor {
            id: "cpu1".into(),
            name: Some("CPU 1".into()),
            health: Health::OK,
            state: State::Enabled,
            celsius: Some(30.0),
            upper_critical_c: Some(90.0),
            upper_warning_c: None,
        };

        // Neither: silence.
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &[], &[], std::slice::from_ref(&base), &[], 0),
        );
        assert!(out.is_empty());

        // Over the BMC's own threshold, health still OK.
        let hot = ThermalSensor {
            celsius: Some(96.0),
            ..base.clone()
        };
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &[], &[], std::slice::from_ref(&hot), &[], 0),
        );
        let a = out
            .iter()
            .find(|a| a.rule == RULE_THERMAL_CRITICAL)
            .unwrap();
        assert!(a.summary.contains("96 C"), "{}", a.summary);
        assert!(
            a.summary.contains("threshold 90 C"),
            "the BMC's own threshold belongs in the message: {}",
            a.summary
        );

        // Health critical, reading under the threshold.
        let unhappy = ThermalSensor {
            health: Health::Critical,
            ..base.clone()
        };
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &[], &[], std::slice::from_ref(&unhappy), &[], 0),
        );
        assert_eq!(rules(&out), vec![RULE_THERMAL_CRITICAL]);

        // No threshold and no verdict is not a fault.
        let unknown = ThermalSensor {
            celsius: Some(200.0),
            upper_critical_c: None,
            ..base.clone()
        };
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &[], &[], std::slice::from_ref(&unknown), &[], 0),
        );
        assert!(
            out.is_empty(),
            "200 C with no threshold to compare against is not a verdict this sensor may reach"
        );
    }

    /// The rollup catches a fault in something this sensor does not
    /// enumerate — a backplane, a riser, a battery. Without it the BMC saying
    /// "this machine is Critical" would be silently dropped.
    #[test]
    fn the_chassis_rollup_catches_what_the_component_rules_do_not_model() {
        let c = chassis(Health::Critical);
        let out = grade(
            &AlertsConfig::default(),
            &obs(Some(&c), &[], &[], &[], &[], 0),
        );
        assert_eq!(rules(&out), vec![RULE_CHASSIS_HEALTH]);
        assert!(out[0].summary.contains("event log"), "{}", out[0].summary);
    }

    /// Every rule in `ALL_RULES` is reachable, and every rule that fires is in
    /// it — the table the poller reconciles against, so a gap either leaves a
    /// condition unresolvable or an alert unreconciled.
    #[test]
    fn every_rule_in_the_table_can_fire_and_nothing_fires_outside_it() {
        let c = chassis(Health::Critical);
        let mut p = psu("0", Health::Critical, State::Enabled);
        p.redundancy = Some(Redundancy::Failed);
        let absent = psu("1", Health::Unknown, State::Absent);
        let supplies = [p, absent];
        let fans = [Fan {
            id: "0".into(),
            name: None,
            present: true,
            health: Health::Critical,
            state: State::Enabled,
            rpm: Some(0.0),
            redundancy_group: None,
            redundancy: None,
        }];
        let thermal = [ThermalSensor {
            id: "0".into(),
            name: None,
            health: Health::Critical,
            state: State::Enabled,
            celsius: Some(99.0),
            upper_critical_c: Some(90.0),
            upper_warning_c: None,
        }];
        // The #1140 surfaces, each in the state that makes its rule fire.
        let drives = [Drive {
            id: "0".into(),
            name: None,
            controller: Some("ctrl0".into()),
            present: true,
            health: Health::Critical,
            state: State::Enabled,
            model: None,
            serial: None,
            media_type: None,
            protocol: None,
            capacity_bytes: None,
            life_left_percent: None,
            failure_predicted: Some(true),
        }];
        let memory = [MemoryModule {
            id: "DIMM_A1".into(),
            name: None,
            present: true,
            health: Health::Critical,
            state: State::Enabled,
            capacity_mib: Some(32768),
            device_type: None,
            manufacturer: None,
            serial: None,
            speed_mhz: None,
        }];
        let groups = [RedundancyGroup {
            id: "0".into(),
            name: None,
            subsystem: "power".into(),
            health: Health::Critical,
            state: State::Enabled,
            redundancy: Some(Redundancy::Failed),
            min_needed: Some(2),
            max_supported: Some(2),
            members: Some(1),
        }];
        let known = ["1".to_string()];
        let cfg = AlertsConfig {
            psu_absent: true,
            ..AlertsConfig::default()
        };
        let mut o = obs(Some(&c), &supplies, &fans, &thermal, &known, 0);
        o.drives = &drives;
        o.memory = &memory;
        o.redundancy = &groups;
        let out = grade(&cfg, &o);

        let mut fired: Vec<&str> = out.iter().map(|a| a.rule.as_str()).collect();
        fired.sort_unstable();
        fired.dedup();
        for rule in &fired {
            assert!(
                ALL_RULES.contains(rule),
                "{rule} fired but is not in ALL_RULES"
            );
        }
        // Everything except `bmc-unreachable`, which needs the opposite input.
        assert_eq!(fired.len(), ALL_RULES.len() - 1, "fired: {fired:?}");
    }
}
