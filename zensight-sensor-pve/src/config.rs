//! Configuration (#818).
//!
//! The sensor cannot run without being told an endpoint and a token, so —
//! unlike hostspec, whose empty default is a valid state — the shipped
//! `configs/pve.json5` is an example an operator must edit. That is why the
//! assertion thresholds below carry *real* values rather than being switched
//! off: whoever fills in the token is already reading the file, and a
//! monitoring tool whose assertions all ship disabled asserts nothing.

use serde::{Deserialize, Serialize};
use zensight_sensor_core::{LoggingConfig, SensorConfig, ZenohConfig};

fn default_poll_interval() -> u64 {
    60
}
fn default_config_interval() -> u64 {
    300
}
fn default_backup_interval() -> u64 {
    900
}
fn default_timeout() -> u64 {
    15
}
fn default_true() -> bool {
    true
}
fn default_port() -> u16 {
    8006
}
fn default_for_secs() -> u64 {
    120
}
fn default_pool_used_pct() -> f64 {
    85.0
}
fn default_overcommit_ratio() -> f64 {
    1.0
}
fn default_backup_shrink_pct() -> f64 {
    40.0
}
fn default_max_concurrent() -> usize {
    4
}

/// Top-level sensor config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PveSensorConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub serialization: zensight_common::Format,
    pub pve: PveConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PveConfig {
    /// Hostname or IP of the PVE node's API. No scheme, no path.
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// `PVEAPIToken=<user>@<realm>!<tokenid>=<uuid>`, resolved through the
    /// framework's secret indirection — so the deployed form is
    /// `file:/run/credentials/pve/token`, root-0600, never in git.
    pub token: String,
    /// Restrict polling to these node names. Empty = every node the API lists,
    /// which on a standalone install is the one node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    /// Override this sensor's `source` — the reporting host every series and
    /// alert is filed under. Default: this machine's hostname, which on the
    /// recommended deployment (a native binary **on** the PVE node) is the
    /// node name an operator types into Proxmox's UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Runtime status poll (cheap: one `/cluster/resources` call).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Guest *configuration* poll — `onboot`, NIC firewall flags, disk
    /// `backup=0`. Slower by default because it is one call per guest and the
    /// facts change on a human timescale, not a machine one.
    #[serde(default = "default_config_interval")]
    pub config_interval_secs: u64,
    /// Backup/vzdump poll: task history plus stored-volume listing.
    #[serde(default = "default_backup_interval")]
    pub backup_interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Concurrent API requests. The PVE API is a perl daemon on the machine
    /// whose failure is total; a monitoring sensor has no business saturating
    /// it, so this is bounded by construction rather than by hope.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Accept a self-signed API certificate. A stock Proxmox install has one,
    /// and refusing outright would push operators to something worse — but it
    /// disables authentication of the endpoint, so it is opt-in and logged at
    /// warn on every start.
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// Publish third-party identity claims about guests (name + configured
    /// MACs, `observer: pve`) so the hypervisor's view of a VM fuses with that
    /// VM's own sensors in the catalog.
    #[serde(default = "default_true")]
    pub evidence: bool,
    #[serde(default)]
    pub alerts: PveAlertsConfig,
}

/// The assertions. Each one is a finding the 2026-08-28 audit made by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PveAlertsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Debounce, seconds. Configuration facts do not flap, but a guest
    /// mid-migration briefly looks stopped.
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
    /// Fire when a non-template guest has `onboot=0`. **The finding**: VM 140
    /// would not have come back after a host reboot, and nobody would have
    /// known until they looked.
    #[serde(default = "default_true")]
    pub guest_onboot: bool,
    /// Fire when a guest set to start at boot is not running. Free once
    /// `onboot` is known, and a strictly different fault.
    #[serde(default = "default_true")]
    pub guest_not_running: bool,
    /// Fire when a guest has a NIC without `firewall=1`. **The finding**: the
    /// flag being off made `140.fw` inert and left :8000 open to the whole
    /// service zone for an unknown period.
    #[serde(default = "default_true")]
    pub nic_firewall: bool,
    /// vmids exempt from the guest-level assertions above — a guest that is
    /// *meant* to be off, or on a bridge with no firewall.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exempt_vmids: Vec<u32>,
    /// Pool usage percentage that fires. 0 disables.
    #[serde(default = "default_pool_used_pct")]
    pub pool_used_pct: f64,
    /// `allocated/total` that fires. **The finding**: 990 GB provisioned on a
    /// 937 GB pool — 1.06 — with no configuration change needed to fill it.
    /// 0 disables; 1.0 means "warn as soon as more is promised than exists".
    #[serde(default = "default_overcommit_ratio")]
    pub pool_overcommit_ratio: f64,
    /// Fire when a guest's last vzdump task did not exit OK.
    #[serde(default = "default_true")]
    pub backup_failed: bool,
    /// Fire when the newest stored dump is older than this. 0 disables —
    /// backup cadence is deployment policy and a wrong default is noise. The
    /// shipped config suggests 93600 (26 h) for a nightly job.
    #[serde(default)]
    pub backup_stale_secs: u64,
    /// Fire when the newest dump is more than this percent smaller than the
    /// one before it. **The one a green exit code cannot show**: "the job
    /// exited 0" is what the existing mail notification already says.
    /// 0 disables.
    #[serde(default = "default_backup_shrink_pct")]
    pub backup_shrink_pct: f64,
    /// Fire when the cluster is not quorate. Never fires on a standalone
    /// node, where there is no quorum to lose.
    #[serde(default = "default_true")]
    pub quorum: bool,
    /// Fire when a replication job's last run failed.
    #[serde(default = "default_true")]
    pub replication: bool,
}

impl Default for PveAlertsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            for_secs: default_for_secs(),
            guest_onboot: true,
            guest_not_running: true,
            nic_firewall: true,
            exempt_vmids: Vec::new(),
            pool_used_pct: default_pool_used_pct(),
            pool_overcommit_ratio: default_overcommit_ratio(),
            backup_failed: true,
            backup_stale_secs: 0,
            backup_shrink_pct: default_backup_shrink_pct(),
            quorum: true,
            replication: true,
        }
    }
}

impl PveConfig {
    /// The reporting host: the `source` of every series, alert and evidence
    /// claim this sensor emits.
    ///
    /// It used to fall back to `pve.host` — the API endpoint address — which
    /// on the deployment `configs/pve.json5` and `packaging/systemd/` both
    /// recommend is `127.0.0.1`, the one address guaranteed to be ambiguous
    /// across machines (#885). An address is an endpoint, not an identity.
    ///
    /// The fallback is now the hostname, as in every other host sensor. That
    /// is what makes this sensor's `evidence/self` agree with sysinfo's on the
    /// same box, which is what fuses its series onto the right host entity;
    /// the PVE node name stays where it belongs, as the `node` label on every
    /// series. `host` remains the last resort so this can never return empty.
    pub fn resolved_source(&self) -> String {
        self.source.clone().unwrap_or_else(|| {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| self.host.clone())
        })
    }

    pub fn base_url(&self) -> String {
        format!("https://{}:{}/api2/json", self.host, self.port)
    }
}

impl SensorConfig for PveSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &'static str {
        "pve"
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        let mut problems = Vec::new();
        if self.pve.host.trim().is_empty() {
            problems.push("pve.host is empty".to_string());
        }
        if self.pve.host.contains("://") {
            problems.push(format!(
                "pve.host must be a host, not a URL — got {:?}; the scheme, port and \
                 /api2/json path are added for you",
                self.pve.host
            ));
        }
        if self.pve.token.trim().is_empty() {
            problems.push(
                "pve.token is empty — a read-only PVEAuditor API token is required \
                 (use file:/path so the secret never enters this file)"
                    .to_string(),
            );
        }
        if self.pve.poll_interval_secs == 0 {
            problems.push("pve.poll_interval_secs must be > 0".to_string());
        }
        // A timeout at or above the interval means a slow API silently turns
        // into a sensor that never completes a cycle — the bound has to be
        // real, not decorative.
        if self.pve.timeout_secs >= self.pve.poll_interval_secs {
            problems.push(format!(
                "pve.timeout_secs ({}) must be shorter than pve.poll_interval_secs ({}) — \
                 otherwise a slow API stalls every cycle behind the previous one",
                self.pve.timeout_secs, self.pve.poll_interval_secs
            ));
        }
        if self.pve.max_concurrent == 0 {
            problems.push("pve.max_concurrent must be > 0".to_string());
        }
        if !(0.0..=100.0).contains(&self.pve.alerts.pool_used_pct) {
            problems.push("pve.alerts.pool_used_pct must be 0..=100".to_string());
        }
        if self.pve.alerts.pool_overcommit_ratio < 0.0 {
            problems.push("pve.alerts.pool_overcommit_ratio must be >= 0".to_string());
        }
        if !(0.0..=100.0).contains(&self.pve.alerts.backup_shrink_pct) {
            problems.push("pve.alerts.backup_shrink_pct must be 0..=100".to_string());
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

    fn cfg(json: &str) -> std::result::Result<PveSensorConfig, String> {
        let c: PveSensorConfig = json5::from_str(json).map_err(|e| e.to_string())?;
        c.validate().map_err(|e| e.to_string()).map(|()| c)
    }

    #[test]
    fn a_minimal_config_takes_the_documented_defaults() {
        let c = cfg(r#"{ pve: { host: "pve.example", token: "PVEAPIToken=x!y=z" } }"#).unwrap();
        assert_eq!(c.pve.port, 8006);
        assert_eq!(c.pve.poll_interval_secs, 60);
        assert_eq!(c.pve.timeout_secs, 15);
        assert!(c.pve.evidence);
        assert!(c.pve.alerts.enabled);
        assert_eq!(c.pve.base_url(), "https://pve.example:8006/api2/json");
    }

    /// #885: `source` is the reporting host, and used to fall back to
    /// `pve.host` — the API endpoint address, which on the deployment this
    /// sensor's own packaging recommends is `127.0.0.1`.
    #[test]
    fn the_default_source_is_this_host_never_the_api_address() {
        let c = cfg(r#"{ pve: { host: "127.0.0.1", token: "t" } }"#).unwrap();
        let host = hostname::get().unwrap().into_string().unwrap();
        assert_eq!(c.pve.resolved_source(), host);
        assert_ne!(c.pve.resolved_source(), "127.0.0.1");

        let overridden =
            cfg(r#"{ pve: { host: "127.0.0.1", token: "t", source: "pve01" } }"#).unwrap();
        assert_eq!(overridden.pve.resolved_source(), "pve01");
    }

    /// The assertions ship ON. A monitoring sensor whose checks all default to
    /// disabled asserts nothing, and the operator is already editing this file
    /// to put a token in it.
    #[test]
    fn the_audits_three_findings_are_asserted_by_default() {
        let c = cfg(r#"{ pve: { host: "h", token: "t" } }"#).unwrap();
        assert!(c.pve.alerts.guest_onboot, "VM 140's onboot=0");
        assert!(c.pve.alerts.nic_firewall, "VM 140's inert firewall file");
        assert_eq!(
            c.pve.alerts.pool_overcommit_ratio, 1.0,
            "990 GB promised on a 937 GB pool"
        );
    }

    /// Backup cadence is deployment policy, so the staleness clock is the one
    /// assertion that stays off until someone says how often they back up.
    #[test]
    fn backup_staleness_is_the_one_assertion_that_defaults_off() {
        let c = cfg(r#"{ pve: { host: "h", token: "t" } }"#).unwrap();
        assert_eq!(c.pve.alerts.backup_stale_secs, 0);
        assert_eq!(c.pve.alerts.backup_shrink_pct, 40.0, "but shrinkage is not");
    }

    #[test]
    fn a_url_in_the_host_field_is_refused_by_name() {
        let e = cfg(r#"{ pve: { host: "https://pve.example:8006", token: "t" } }"#).unwrap_err();
        assert!(e.contains("must be a host, not a URL"), "{e}");
    }

    #[test]
    fn an_empty_token_is_refused() {
        let e = cfg(r#"{ pve: { host: "h", token: "" } }"#).unwrap_err();
        assert!(e.contains("pve.token is empty"), "{e}");
    }

    /// A timeout that cannot expire before the next tick is not a bound.
    #[test]
    fn a_timeout_at_or_above_the_interval_is_refused() {
        let e =
            cfg(r#"{ pve: { host: "h", token: "t", poll_interval_secs: 10, timeout_secs: 10 } }"#)
                .unwrap_err();
        assert!(e.contains("must be shorter than"), "{e}");
    }

    #[test]
    fn every_problem_is_named_at_once() {
        let e = cfg(r#"{ pve: { host: "", token: "", max_concurrent: 0 } }"#).unwrap_err();
        assert!(e.contains("pve.host is empty"), "{e}");
        assert!(e.contains("pve.token is empty"), "{e}");
        assert!(e.contains("max_concurrent"), "{e}");
    }

    /// The shipped example must parse and validate — nothing else reads it, so
    /// without this it can rot silently (the #845 lesson, six configs over).
    #[test]
    fn shipped_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/pve.json5");
        let text = std::fs::read_to_string(path).expect("configs/pve.json5 exists");
        let c: PveSensorConfig = json5::from_str(&text).expect("shipped config parses");
        c.validate().expect("shipped config validates");
    }
}
