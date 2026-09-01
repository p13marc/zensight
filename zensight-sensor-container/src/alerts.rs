//! The container assertions (#819).
//!
//! Pure — no socket, no cgroupfs — so the whole rule table is testable against
//! documents. Each rule is a finding the 2026-08-28 audit made by hand.

use std::collections::HashMap;

use zensight_common::container::{ContainerInfo, HealthState, SignatureState};
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

use crate::config::ContainerAlertsConfig;

pub const RULE_UNHEALTHY: &str = "container-unhealthy";
pub const RULE_HEALTH_NEVER_RAN: &str = "container-healthcheck-never-ran";
pub const RULE_RESTART_LOOP: &str = "container-restart-loop";
pub const RULE_OOM: &str = "container-oom-killed";
pub const RULE_EXITED: &str = "container-exited-nonzero";
pub const RULE_IMAGE_BEHIND: &str = "container-image-behind-upstream";
pub const RULE_UNSIGNED: &str = "container-image-unsigned";

pub const ALL_RULES: &[&str] = &[
    RULE_UNHEALTHY,
    RULE_HEALTH_NEVER_RAN,
    RULE_RESTART_LOOP,
    RULE_OOM,
    RULE_EXITED,
    RULE_IMAGE_BEHIND,
    RULE_UNSIGNED,
];

/// One sweep's input: the containers, plus what the previous sweep saw of
/// their counters (restarts and OOM kills are cumulative, so a rule about
/// *rate* needs the delta, not the total).
pub struct Observation<'a> {
    /// The reporting host — the `source` of every alert, as of #883. A
    /// container name is unique per host, not globally; the name, image and
    /// owning unit ride in the labels.
    pub source: &'a str,
    pub containers: &'a [ContainerInfo],
    /// `name -> (restart_count, oom_kills)` as of `baseline_age_secs` ago.
    pub baseline: &'a HashMap<String, (u64, u64)>,
    /// How long ago the baseline was taken.
    pub baseline_age_secs: u64,
}

fn alert(
    source: &str,
    c: &ContainerInfo,
    rule: &str,
    severity: AlertSeverity,
    summary: String,
    extra: &[(&str, String)],
) -> Alert {
    let mut a = Alert::new(
        source,
        Protocol::Container,
        AlertKind::Expectation,
        rule,
        severity,
        summary,
    );
    let mut labels = HashMap::new();
    labels.insert("container".to_string(), c.name.clone());
    labels.insert("image".to_string(), c.image.reference.clone());
    if let Some(u) = &c.unit {
        // The join with the systemd sensor: an operator paging on this alert
        // gets the unit to restart, not just a container id.
        labels.insert("unit".to_string(), u.clone());
    }
    for (k, v) in extra {
        labels.insert((*k).to_string(), v.clone());
    }
    a.labels = labels;
    a
}

pub fn grade(cfg: &ContainerAlertsConfig, obs: &Observation<'_>) -> Vec<Alert> {
    let mut out = Vec::new();
    if !cfg.enabled {
        return out;
    }
    for c in obs.containers {
        if cfg.exempt.contains(&c.name) {
            continue;
        }

        match c.health {
            HealthState::Unhealthy if cfg.unhealthy => out.push(alert(
                obs.source,
                c,
                RULE_UNHEALTHY,
                AlertSeverity::Warning,
                format!(
                    "{}'s healthcheck is failing{}",
                    c.name,
                    match c.health_failing_streak {
                        Some(n) if n > 0 => format!(" ({n} consecutive)"),
                        _ => String::new(),
                    }
                ),
                &[(
                    "failing_streak",
                    c.health_failing_streak.unwrap_or(0).to_string(),
                )],
            )),
            // The garage case. A separate rule, and deliberately a separate
            // sentence: "the probe cannot run" is a defect in the check, and
            // telling an operator their service is unhealthy sends them to
            // debug the wrong thing — for weeks, as it turned out.
            HealthState::NeverRan if cfg.health_never_ran => out.push(alert(
                obs.source,
                c,
                RULE_HEALTH_NEVER_RAN,
                AlertSeverity::Warning,
                format!(
                    "{}'s healthcheck has never produced a result — the probe itself \
                     cannot run (a CMD-SHELL check in an image with no shell is the \
                     usual cause). The service may be perfectly healthy.",
                    c.name
                ),
                &[],
            )),
            _ => {}
        }

        if cfg.restart_loop
            && let Some((prev_restarts, _)) = obs.baseline.get(&c.name)
            && obs.baseline_age_secs <= cfg.restart_window_secs
        {
            let delta = c.restart_count.saturating_sub(*prev_restarts);
            if delta > cfg.restart_max {
                out.push(alert(
                    obs.source,
                    c,
                    RULE_RESTART_LOOP,
                    AlertSeverity::Critical,
                    format!(
                        "{} restarted {delta} times in the last {}s (limit {})",
                        c.name, obs.baseline_age_secs, cfg.restart_max
                    ),
                    &[
                        ("restarts", delta.to_string()),
                        ("window_secs", obs.baseline_age_secs.to_string()),
                    ],
                ));
            }
        }

        // Cumulative counter, so the rule is about the DELTA. Firing on the
        // total would make a container that was OOM-killed once, a year ago,
        // alert forever.
        if cfg.oom_killed
            && let Some(kills) = c.resources.oom_kills
            && let Some((_, prev_kills)) = obs.baseline.get(&c.name)
            && kills > *prev_kills
        {
            out.push(alert(
                obs.source,
                c,
                RULE_OOM,
                AlertSeverity::Critical,
                format!(
                    "the kernel OOM-killed a process in {}'s cgroup ({} new){}",
                    c.name,
                    kills - prev_kills,
                    match c.resources.memory_max_bytes {
                        Some(m) => format!(" — its limit is {}", human_bytes(m)),
                        None => String::new(),
                    }
                ),
                &[
                    ("oom_kills_total", kills.to_string()),
                    (
                        "memory_max_bytes",
                        c.resources.memory_max_bytes.unwrap_or(0).to_string(),
                    ),
                ],
            ));
        }

        if cfg.exited_nonzero
            && !c.is_running()
            && let Some(code) = c.exit_code
            && code != 0
        {
            out.push(alert(
                obs.source,
                c,
                RULE_EXITED,
                AlertSeverity::Critical,
                format!("{} exited with code {code} and is not running", c.name),
                &[("exit_code", code.to_string())],
            ));
        }

        if cfg.image_behind && c.image.is_behind_upstream() {
            out.push(alert(
                obs.source,
                c,
                RULE_IMAGE_BEHIND,
                AlertSeverity::Info,
                format!(
                    "{} runs {} at a digest that is no longer what its tag resolves to \
                     upstream",
                    c.name, c.image.reference
                ),
                &[
                    ("running_digest", c.image.digest.clone().unwrap_or_default()),
                    (
                        "upstream_digest",
                        c.image.upstream_digest.clone().unwrap_or_default(),
                    ),
                ],
            ));
        }

        // Only `Absent` fires. `NotChecked` is silence about a question nobody
        // asked, and treating it as "unsigned" would make every deployment
        // without the egress collector look like a supply-chain failure.
        if cfg.unsigned && c.image.signature == SignatureState::Absent {
            out.push(alert(
                obs.source,
                c,
                RULE_UNSIGNED,
                AlertSeverity::Warning,
                format!("{} runs an image with no signature in its registry", c.name),
                &[],
            ));
        }
    }
    out
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.0} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::container::{ContainerImage, ContainerResources};

    /// The reporting host: every alert is filed under it, never under the
    /// container being reported on (#883).
    const HOST: &str = "vm-apps-01";

    fn container(name: &str) -> ContainerInfo {
        ContainerInfo {
            id: format!("id-{name}"),
            name: name.into(),
            status: "running".into(),
            image: ContainerImage {
                reference: "docker.io/library/caddy:2.11.4".into(),
                digest: Some("sha256:aaa".into()),
                upstream_digest: None,
                signature: SignatureState::NotChecked,
            },
            created_at: None,
            started_at: None,
            restart_count: 0,
            exit_code: None,
            health: HealthState::None,
            health_failing_streak: None,
            unit: Some(format!("{name}.service")),
            restart_policy: None,
            rootless: false,
            ports: vec![],
            mounts: vec![],
            cgroup_path: None,
            resources: ContainerResources::default(),
            ips: vec![],
            observed_at_ms: 0,
        }
    }

    fn obs<'a>(cs: &'a [ContainerInfo], base: &'a HashMap<String, (u64, u64)>) -> Observation<'a> {
        Observation {
            source: HOST,
            containers: cs,
            baseline: base,
            baseline_age_secs: 60,
        }
    }

    fn rules(a: &[Alert]) -> Vec<&str> {
        let mut r: Vec<&str> = a.iter().map(|x| x.rule.as_str()).collect();
        r.sort_unstable();
        r
    }

    /// garage: `unhealthy` from the day it was deployed, serving traffic
    /// perfectly, for weeks. The alert must say the PROBE is broken, not the
    /// service — the whole cost of that finding was people debugging garage.
    #[test]
    fn a_healthcheck_that_never_ran_gets_its_own_rule_and_its_own_sentence() {
        let mut c = container("garage");
        c.health = HealthState::NeverRan;
        let base = HashMap::new();
        let a = grade(&ContainerAlertsConfig::default(), &obs(&[c], &base));
        assert_eq!(rules(&a), vec![RULE_HEALTH_NEVER_RAN]);
        assert!(a[0].summary.contains("probe itself"), "{}", a[0].summary);
        assert!(
            a[0].summary.contains("may be perfectly healthy"),
            "{}",
            a[0].summary
        );
        assert_eq!(a[0].labels["unit"], "garage.service", "the systemd join");
    }

    #[test]
    fn a_failing_healthcheck_is_a_different_rule() {
        let mut c = container("caddy");
        c.health = HealthState::Unhealthy;
        c.health_failing_streak = Some(5);
        let base = HashMap::new();
        let a = grade(&ContainerAlertsConfig::default(), &obs(&[c], &base));
        assert_eq!(rules(&a), vec![RULE_UNHEALTHY]);
        assert_eq!(a[0].labels["failing_streak"], "5");
    }

    #[test]
    fn a_container_with_no_healthcheck_fires_nothing() {
        let base = HashMap::new();
        assert!(
            grade(
                &ContainerAlertsConfig::default(),
                &obs(&[container("caddy")], &base)
            )
            .is_empty()
        );
    }

    /// OOM kills are cumulative. Firing on the total would alert forever about
    /// a kill that happened a year ago.
    #[test]
    fn oom_fires_on_the_delta_not_the_total() {
        let mut c = container("netring");
        c.resources.oom_kills = Some(3);
        c.resources.memory_max_bytes = Some(64 * 1024 * 1024);
        let mut base = HashMap::new();
        base.insert("netring".to_string(), (0, 3));
        assert!(
            grade(&ContainerAlertsConfig::default(), &obs(&[c.clone()], &base)).is_empty(),
            "an old kill must not keep firing"
        );
        base.insert("netring".to_string(), (0, 2));
        let a = grade(&ContainerAlertsConfig::default(), &obs(&[c], &base));
        assert_eq!(rules(&a), vec![RULE_OOM]);
        assert!(a[0].summary.contains("64 MiB"), "{}", a[0].summary);
    }

    #[test]
    fn a_restart_loop_fires_on_the_delta_within_the_window() {
        let mut c = container("flappy");
        c.restart_count = 9;
        let mut base = HashMap::new();
        base.insert("flappy".to_string(), (4, 0));
        let a = grade(&ContainerAlertsConfig::default(), &obs(&[c.clone()], &base));
        assert_eq!(rules(&a), vec![RULE_RESTART_LOOP]);

        // Same total, but the baseline is older than the window: not a rate.
        let o = Observation {
            source: HOST,
            containers: std::slice::from_ref(&c),
            baseline: &base,
            baseline_age_secs: 100_000,
        };
        assert!(grade(&ContainerAlertsConfig::default(), &o).is_empty());
    }

    #[test]
    fn a_container_that_exited_nonzero_fires_and_a_clean_exit_does_not() {
        let mut bad = container("job");
        bad.status = "exited".into();
        bad.exit_code = Some(137);
        let mut good = container("cron");
        good.status = "exited".into();
        good.exit_code = Some(0);
        let base = HashMap::new();
        let a = grade(&ContainerAlertsConfig::default(), &obs(&[bad, good], &base));
        assert_eq!(rules(&a), vec![RULE_EXITED]);
        assert_eq!(a[0].labels["exit_code"], "137");
    }

    /// "Not checked" is not "unsigned". Without this, every deployment that
    /// leaves the egress collector off would look like a supply-chain failure.
    #[test]
    fn an_unchecked_signature_is_not_an_absent_one() {
        let mut c = container("caddy");
        c.image.signature = SignatureState::NotChecked;
        let base = HashMap::new();
        assert!(grade(&ContainerAlertsConfig::default(), &obs(&[c.clone()], &base)).is_empty());
        c.image.signature = SignatureState::Absent;
        assert_eq!(
            rules(&grade(&ContainerAlertsConfig::default(), &obs(&[c], &base))),
            vec![RULE_UNSIGNED]
        );
    }

    /// Likewise: an image whose upstream was never resolved is not behind.
    #[test]
    fn an_unresolved_upstream_is_not_behind() {
        let mut c = container("caddy");
        let base = HashMap::new();
        assert!(grade(&ContainerAlertsConfig::default(), &obs(&[c.clone()], &base)).is_empty());
        c.image.upstream_digest = Some("sha256:bbb".into());
        assert_eq!(
            rules(&grade(&ContainerAlertsConfig::default(), &obs(&[c], &base))),
            vec![RULE_IMAGE_BEHIND]
        );
    }

    #[test]
    fn exempt_containers_are_skipped() {
        let mut c = container("noisy");
        c.health = HealthState::Unhealthy;
        let cfg = ContainerAlertsConfig {
            exempt: vec!["noisy".into()],
            ..Default::default()
        };
        let base = HashMap::new();
        assert!(grade(&cfg, &obs(&[c], &base)).is_empty());
    }

    /// Every rule the grader can emit must be reconciled, or a condition that
    /// cleared keeps firing until the sensor restarts.
    #[test]
    fn every_emitted_rule_is_reconciled() {
        let mut a1 = container("a");
        a1.health = HealthState::Unhealthy;
        let mut a2 = container("b");
        a2.health = HealthState::NeverRan;
        let mut a3 = container("c");
        a3.restart_count = 99;
        a3.resources.oom_kills = Some(5);
        let mut a4 = container("d");
        a4.status = "exited".into();
        a4.exit_code = Some(1);
        let mut a5 = container("e");
        a5.image.upstream_digest = Some("sha256:zzz".into());
        a5.image.signature = SignatureState::Absent;

        let mut base = HashMap::new();
        base.insert("c".to_string(), (0u64, 0u64));
        let cs = [a1, a2, a3, a4, a5];
        let fired: std::collections::HashSet<&str> =
            grade(&ContainerAlertsConfig::default(), &obs(&cs, &base))
                .iter()
                .map(|a| Box::leak(a.rule.clone().into_boxed_str()) as &str)
                .collect();
        assert_eq!(fired.len(), ALL_RULES.len(), "fired: {fired:?}");
        for r in &fired {
            assert!(ALL_RULES.contains(r), "{r} is not reconciled");
        }
    }
}
