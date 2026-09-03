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

use zensight_common::bmc::{Chassis, Fan, PowerSupply, Redundancy, State, ThermalSensor};
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

use crate::config::AlertsConfig;

pub const RULE_UNREACHABLE: &str = "bmc-unreachable";
pub const RULE_PSU_FAILED: &str = "psu-failed";
pub const RULE_PSU_ABSENT: &str = "psu-absent";
pub const RULE_PSU_REDUNDANCY: &str = "psu-redundancy-lost";
pub const RULE_FAN_FAILED: &str = "fan-failed";
pub const RULE_THERMAL_CRITICAL: &str = "thermal-critical";
pub const RULE_CHASSIS_HEALTH: &str = "chassis-health";

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
];

/// One chassis's sweep, as the rules see it.
pub struct Observation<'a> {
    /// The **reporting host** — the `source` of every series and alert this
    /// sensor emits (#883). The chassis is a facet of this vantage point, not
    /// a separate machine that publishes for itself; it rides in the labels,
    /// where a rename costs nothing and where `alert_key` cannot see it.
    pub source: &'a str,
    /// The operator's name for the chassis.
    pub endpoint: &'a str,
    /// `None` when the BMC did not answer this cycle.
    pub chassis: Option<&'a Chassis>,
    pub supplies: &'a [PowerSupply],
    pub fans: &'a [Fan],
    pub thermal: &'a [ThermalSensor],
    /// Bays this endpoint has reported present at some point in this process's
    /// life. A bay that was never populated is not a bay someone emptied.
    pub known_present: &'a [String],
    /// Consecutive cycles in which the BMC did not answer.
    pub consecutive_failures: u32,
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
    map.insert("chassis".to_string(), obs.endpoint.to_string());
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
                    obs.endpoint,
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
                format!("{}: {name} was present and now reads absent", obs.endpoint),
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
                    obs.endpoint,
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
                    obs.endpoint,
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
            format!("{}: {name}{reading}{threshold}", obs.endpoint),
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
                obs.endpoint,
                chassis.health.as_str()
            ),
            &[],
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::bmc::{Health, RedfishSurface};

    fn chassis(health: Health) -> Chassis {
        Chassis {
            id: "1".into(),
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
            assert_eq!(a.labels["chassis"], "rack-a-1");
            assert_eq!(a.labels["psu"], "0");
        }
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
        let known = ["1".to_string()];
        let cfg = AlertsConfig {
            psu_absent: true,
            ..AlertsConfig::default()
        };
        let out = grade(&cfg, &obs(Some(&c), &supplies, &fans, &thermal, &known, 0));

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
