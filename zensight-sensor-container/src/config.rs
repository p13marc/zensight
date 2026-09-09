//! Configuration (#819).

use serde::{Deserialize, Serialize};
use zensight_common::config::ZenohConfig;
use zensight_sensor_core::{LoggingConfig, SensorConfig};

fn default_poll_interval() -> u64 {
    30
}
fn default_timeout() -> u64 {
    10
}
fn default_true() -> bool {
    true
}
fn default_for_secs() -> u64 {
    60
}
fn default_cgroup_root() -> String {
    crate::cgroup::CGROUP_ROOT.to_string()
}
fn default_restart_window_secs() -> u64 {
    600
}
fn default_restart_max() -> u64 {
    3
}
fn default_oom_hold_secs() -> u64 {
    600
}
fn default_upstream_interval() -> u64 {
    21_600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerSensorConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub serialization: zensight_common::serialization::Format,
    #[serde(default)]
    pub container: ContainerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Declared resource envelope (#811/#1091). `resources.budget_rss_mb` is
    /// carried into the health doc's `self_stats.budget_bytes` and graded by
    /// the runner's `sensor-budget` rule at 80 % — declared, not enforced
    /// (#812 is the shed ladder). Absent reads as *undeclared*, never as
    /// unlimited-and-fine.
    #[serde(default)]
    pub resources: zensight_sensor_core::ResourcesConfig,

    /// `@desired` reconcile settings (#931): the kill switch and refresh
    /// cadence. File config on purpose — the mechanism that could misbehave
    /// must be disarmable from outside itself.
    #[serde(default)]
    pub desired: zensight_common::desired::DesiredConfig,

    /// Operator-authored threshold rules over this sensor's own telemetry
    /// (#931). **Empty by default** — this build ships no threshold that
    /// fires. Also authorable fleet-wide on `@desired` and per-host over
    /// `@rpc/container/thresholds/set`; `state/container/applied/thresholds`
    /// says which of the three is in force.
    #[serde(default)]
    pub thresholds: zensight_common::threshold::ThresholdsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerConfig {
    /// Runtime sockets to poll. Empty = the conventional ones, tried in order
    /// (rootful podman, this user's rootless podman, docker).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sockets: Vec<String>,
    /// Override the sensor's `source`. Default: the hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Where the unified cgroup hierarchy is mounted. Overridable so a
    /// containerised sensor can read a host cgroupfs bind-mounted elsewhere.
    #[serde(default = "default_cgroup_root")]
    pub cgroup_root: String,
    /// Container names to ignore (exact match). The sensor's own container is
    /// a reasonable entry; `sysinfo` already reports the sensor host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    /// Publish identity claims about containers (name + IPs), so netlink's
    /// wire-only podman-bridge entities merge into them.
    #[serde(default = "default_true")]
    pub evidence: bool,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub alerts: ContainerAlertsConfig,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            sockets: Vec::new(),
            source: None,
            poll_interval_secs: default_poll_interval(),
            timeout_secs: default_timeout(),
            cgroup_root: default_cgroup_root(),
            ignore: Vec::new(),
            evidence: true,
            upstream: UpstreamConfig::default(),
            alerts: ContainerAlertsConfig::default(),
        }
    }
}

/// The **only** part of this sensor that talks to the internet.
///
/// It resolves each container's configured tag against its registry to answer
/// "is the pinned digest still the newest?", and optionally whether a cosign
/// signature exists. That is a real capability — it replaces
/// `image-update-report.sh` and would have caught cosign signing nothing for
/// eight days — and it is also egress from a monitoring agent to arbitrary
/// third-party hosts. So it is **off by default**, and turning it on is a
/// deliberate choice rather than a side effect of running the sensor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    #[serde(default)]
    pub enabled: bool,
    /// How often to re-resolve. Registries rate-limit; the answer changes on
    /// a release cadence, not a poll cadence.
    #[serde(default = "default_upstream_interval")]
    pub interval_secs: u64,
    /// Also check whether a cosign signature object exists for the running
    /// digest. Requires `enabled`.
    #[serde(default)]
    pub signatures: bool,
    /// Registry hosts this may contact. **Empty means none** — an allowlist
    /// that defaults to "everything" is not an allowlist, and a monitoring
    /// agent should not decide on its own which hosts to reach.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registries: Vec<String>,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_upstream_interval(),
            signatures: false,
            registries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerAlertsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
    /// A configured healthcheck is failing.
    #[serde(default = "default_true")]
    pub unhealthy: bool,
    /// A healthcheck is configured and has **never produced a result** — the
    /// garage case: a `CMD-SHELL` probe in a distroless image cannot run, and
    /// the container reported `unhealthy` for weeks while serving perfectly.
    #[serde(default = "default_true")]
    pub health_never_ran: bool,
    /// More than `restart_max` restarts within `restart_window_secs`.
    #[serde(default = "default_true")]
    pub restart_loop: bool,
    #[serde(default = "default_restart_max")]
    pub restart_max: u64,
    #[serde(default = "default_restart_window_secs")]
    pub restart_window_secs: u64,
    /// The kernel OOM-killed something in this container's cgroup.
    #[serde(default = "default_true")]
    pub oom_killed: bool,
    /// How long a burst of new OOM kills stays alertable, in seconds. The
    /// kill is a one-sweep event against a cumulative counter; the alert
    /// has a `for_secs` debounce that needs to see the condition on more
    /// than one sweep. Holding the OOM baseline still for this long after
    /// the first new kill is what lets the two meet — before it, with the
    /// shipped 30 s poll and 60 s `for_secs`, the condition was true for
    /// exactly one sweep and `container-oom-killed` could never fire.
    #[serde(default = "default_oom_hold_secs")]
    pub oom_hold_secs: u64,
    /// A container exited non-zero and is not running.
    #[serde(default = "default_true")]
    pub exited_nonzero: bool,
    /// The running digest differs from the tag's current upstream digest.
    /// Needs `upstream.enabled`; fires nothing without it.
    #[serde(default = "default_true")]
    pub image_behind: bool,
    /// A registry-hosted image has no signature. Needs `upstream.signatures`.
    #[serde(default = "default_true")]
    pub unsigned: bool,
    /// Container names exempt from every rule above.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt: Vec<String>,
}

impl Default for ContainerAlertsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            for_secs: default_for_secs(),
            unhealthy: true,
            health_never_ran: true,
            restart_loop: true,
            restart_max: default_restart_max(),
            restart_window_secs: default_restart_window_secs(),
            oom_killed: true,
            oom_hold_secs: default_oom_hold_secs(),
            exited_nonzero: true,
            image_behind: true,
            unsigned: true,
            exempt: Vec::new(),
        }
    }
}

impl ContainerConfig {
    pub fn resolved_source(&self) -> String {
        self.source.clone().unwrap_or_else(|| {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        })
    }
}

impl SensorConfig for ContainerSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &'static str {
        "container"
    }

    fn resources(&self) -> &zensight_sensor_core::ResourcesConfig {
        &self.resources
    }

    fn desired(&self) -> zensight_common::desired::DesiredConfig {
        self.desired.clone()
    }

    fn thresholds(&self) -> zensight_common::threshold::ThresholdsConfig {
        self.thresholds.clone()
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        let c = &self.container;
        let mut problems = Vec::new();
        if c.poll_interval_secs == 0 {
            problems.push("container.poll_interval_secs must be > 0".to_string());
        }
        if c.timeout_secs >= c.poll_interval_secs {
            problems.push(format!(
                "container.timeout_secs ({}) must be shorter than \
                 container.poll_interval_secs ({}) — otherwise a wedged runtime socket \
                 stalls every cycle behind the previous one",
                c.timeout_secs, c.poll_interval_secs
            ));
        }
        if c.alerts.restart_loop && c.alerts.restart_window_secs == 0 {
            problems.push(
                "container.alerts.restart_window_secs must be > 0 when restart_loop is on"
                    .to_string(),
            );
        }
        // An allowlist that defaults to "everything" is not an allowlist. If
        // egress is on, the operator has to say where to.
        if c.upstream.enabled && c.upstream.registries.is_empty() {
            problems.push(
                "container.upstream.enabled is set but container.upstream.registries is \
                 empty — this is the only part of the sensor that leaves the host, so \
                 the registries it may contact must be named"
                    .to_string(),
            );
        }
        if c.upstream.signatures && !c.upstream.enabled {
            problems
                .push("container.upstream.signatures needs container.upstream.enabled".to_string());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(zensight_sensor_core::SensorError::config(
                problems.join("; "),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: &str) -> std::result::Result<ContainerSensorConfig, String> {
        let c: ContainerSensorConfig = json5::from_str(json).map_err(|e| e.to_string())?;
        c.validate().map_err(|e| e.to_string()).map(|()| c)
    }

    #[test]
    fn an_empty_config_runs_with_the_documented_defaults() {
        let c = cfg("{}").unwrap();
        assert_eq!(c.container.poll_interval_secs, 30);
        assert!(c.container.sockets.is_empty(), "conventional paths");
        assert!(c.container.alerts.enabled);
        assert_eq!(c.container.cgroup_root, "/sys/fs/cgroup");
    }

    /// Egress is the one thing this sensor does that leaves the host, and it
    /// must never happen because someone left a default alone.
    #[test]
    fn the_egressing_collector_is_off_by_default() {
        let c = cfg("{}").unwrap();
        assert!(!c.container.upstream.enabled);
        assert!(!c.container.upstream.signatures);
    }

    #[test]
    fn egress_without_a_registry_allowlist_is_refused() {
        let e = cfg(r#"{ container: { upstream: { enabled: true } } }"#).unwrap_err();
        assert!(e.contains("registries"), "{e}");
        cfg(r#"{ container: { upstream: { enabled: true, registries: ["docker.io"] } } }"#)
            .unwrap();
    }

    #[test]
    fn signature_checking_needs_the_egress_switch() {
        let e = cfg(r#"{ container: { upstream: { signatures: true } } }"#).unwrap_err();
        assert!(e.contains("needs container.upstream.enabled"), "{e}");
    }

    #[test]
    fn a_timeout_at_or_above_the_interval_is_refused() {
        let e = cfg(r#"{ container: { poll_interval_secs: 5, timeout_secs: 5 } }"#).unwrap_err();
        assert!(e.contains("must be shorter than"), "{e}");
    }

    /// The garage case is asserted by default; so is the OOM that took eleven
    /// days to attribute.
    #[test]
    fn the_audits_findings_are_asserted_by_default() {
        let a = cfg("{}").unwrap().container.alerts;
        assert!(a.health_never_ran, "garage's un-runnable healthcheck");
        assert!(a.oom_killed, "2026-08-17");
        assert!(a.restart_loop);
    }

    #[test]
    fn shipped_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/container.json5");
        let text = std::fs::read_to_string(path).expect("configs/container.json5 exists");
        let c: ContainerSensorConfig = json5::from_str(&text).expect("shipped config parses");
        c.validate().expect("shipped config validates");
    }
}
