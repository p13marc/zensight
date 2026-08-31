//! Configuration (#820).
//!
//! The sensor is **bounded by construction**, and the bounds are checked at
//! startup rather than hoped for: an explicit target list, a per-target
//! interval floor, a concurrency cap, and a timeout that must be shorter than
//! the interval. A prober whose timeout can outlive its own tick is not
//! bounded, it is queued.

use serde::{Deserialize, Serialize};
use zensight_common::config::ZenohConfig;
use zensight_common::probe::ProbeKind;
use zensight_sensor_core::{LoggingConfig, SensorConfig};

/// Nothing may be probed more often than this. A monitoring tool that can be
/// configured into a load generator against someone else's service is a
/// footgun with a config file.
pub const MIN_INTERVAL_SECS: u64 = 5;

fn default_interval() -> u64 {
    60
}
fn default_timeout() -> u64 {
    10
}
fn default_max_concurrent() -> usize {
    8
}
fn default_true() -> bool {
    true
}
fn default_for_secs() -> u64 {
    120
}
fn default_expiry_warn_days() -> i64 {
    30
}
fn default_expiry_critical_days() -> i64 {
    7
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSensorConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub serialization: zensight_common::serialization::Format,
    #[serde(default)]
    pub probe: ProbeConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeConfig {
    /// Where this sensor is looking *from*. Half the answer: the same target
    /// checked from the edge, from a guest and from a workstation gives three
    /// different and equally true results. Default: the hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vantage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Checks in flight at once, across all targets.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<Target>,
    #[serde(default)]
    pub alerts: ProbeAlertsConfig,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            vantage: None,
            source: None,
            interval_secs: default_interval(),
            timeout_secs: default_timeout(),
            max_concurrent: default_max_concurrent(),
            targets: Vec::new(),
            alerts: ProbeAlertsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    /// Operator's name for this target — the device slug and the alert key.
    pub name: String,
    pub kind: ProbeKind,
    /// A URL for `http`, `host:port` for `tls`/`tcp`, a name for `dns`, a host
    /// for `icmp`, a path for `certfile`.
    pub target: String,
    /// Per-target override; falls back to `probe.interval_secs`. Floored at
    /// [`MIN_INTERVAL_SECS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,

    // ── HTTP ─────────────────────────────────────────────────────────────
    /// Status codes that count as success. Empty = any 2xx.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_status: Vec<u16>,
    /// A literal substring the body must contain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_body: Option<String>,
    /// Follow redirects at all. Default: yes, up to 10.
    #[serde(default = "default_true")]
    pub follow_redirects: bool,
    /// Permit a redirect that leaves the configured host. Default **false**:
    /// a probe that silently follows a redirect to somewhere else is checking
    /// something other than what it was asked about, and reporting that as
    /// success is how an outside-in check becomes decorative.
    #[serde(default)]
    pub allow_offhost_redirect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Extra request headers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,

    // ── TLS ──────────────────────────────────────────────────────────────
    /// SNI name to send and to match SANs against. Defaults to the target's
    /// host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// Complete the handshake even if the chain does not validate, so an
    /// expired or self-signed certificate can still be *reported* rather than
    /// producing a bare connection error. Reading a certificate is not
    /// trusting it.
    #[serde(default = "default_true")]
    pub inspect_untrusted: bool,

    // ── DNS ──────────────────────────────────────────────────────────────
    /// Resolver to ask, `ip:port`. Default: the system resolver — and whichever
    /// it is, it is named in the result. **That is the check**: the 2026-08-20
    /// hairpin was one host's resolver giving a different answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver: Option<String>,
    /// Addresses the answer must contain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_addrs: Vec<String>,

    /// Skip this target without deleting it.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeAlertsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
    /// The check did not succeed (any reason).
    #[serde(default = "default_true")]
    pub down: bool,
    /// It timed out specifically. A separate rule, because a hang and a
    /// refusal are different diagnoses and the 2026-08-20 investigation spent
    /// eight days rediscovering that.
    #[serde(default = "default_true")]
    pub timeout: bool,
    /// A redirect left the configured host while `allow_offhost_redirect` is
    /// false.
    #[serde(default = "default_true")]
    pub offhost_redirect: bool,
    /// Certificate expiry thresholds, days.
    #[serde(default = "default_expiry_warn_days")]
    pub expiry_warn_days: i64,
    #[serde(default = "default_expiry_critical_days")]
    pub expiry_critical_days: i64,
    /// The presented chain did not validate.
    #[serde(default = "default_true")]
    pub chain_invalid: bool,
    /// The certificate does not cover the name asked for.
    #[serde(default = "default_true")]
    pub san_mismatch: bool,
    /// A DNS answer did not contain the expected address.
    #[serde(default = "default_true")]
    pub dns_unexpected: bool,
}

impl Default for ProbeAlertsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            for_secs: default_for_secs(),
            down: true,
            timeout: true,
            offhost_redirect: true,
            expiry_warn_days: default_expiry_warn_days(),
            expiry_critical_days: default_expiry_critical_days(),
            chain_invalid: true,
            san_mismatch: true,
            dns_unexpected: true,
        }
    }
}

impl Target {
    pub fn interval(&self, default: u64) -> u64 {
        self.interval_secs.unwrap_or(default).max(MIN_INTERVAL_SECS)
    }

    pub fn timeout(&self, default: u64) -> u64 {
        self.timeout_secs.unwrap_or(default)
    }

    /// The host this target is about — for SNI, for SAN matching, and for
    /// deciding whether a redirect left it.
    pub fn host(&self) -> Option<String> {
        if let Some(n) = &self.server_name {
            return Some(n.clone());
        }
        match self.kind {
            ProbeKind::Http => self
                .target
                .split("://")
                .nth(1)
                .unwrap_or(&self.target)
                .split(['/', '?'])
                .next()
                .map(|h| h.rsplit_once(':').map_or(h, |(x, _)| x).to_string()),
            ProbeKind::Tls | ProbeKind::Tcp => Some(
                self.target
                    .rsplit_once(':')
                    .map_or(self.target.as_str(), |(h, _)| h)
                    .trim_matches(['[', ']'])
                    .to_string(),
            ),
            ProbeKind::Dns | ProbeKind::Icmp => Some(self.target.clone()),
            ProbeKind::CertFile => None,
        }
    }
}

impl ProbeConfig {
    pub fn resolved_vantage(&self) -> String {
        self.vantage.clone().unwrap_or_else(|| {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        })
    }

    pub fn resolved_source(&self) -> String {
        self.source
            .clone()
            .unwrap_or_else(|| self.resolved_vantage())
    }
}

impl SensorConfig for ProbeSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &'static str {
        "probe"
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        let p = &self.probe;
        let mut problems = Vec::new();
        if p.interval_secs < MIN_INTERVAL_SECS {
            problems.push(format!(
                "probe.interval_secs must be >= {MIN_INTERVAL_SECS}"
            ));
        }
        if p.max_concurrent == 0 {
            problems.push("probe.max_concurrent must be > 0".to_string());
        }
        let mut names = std::collections::HashSet::new();
        for t in &p.targets {
            if t.name.trim().is_empty() {
                problems.push("a target has an empty name".to_string());
                continue;
            }
            if !names.insert(t.name.clone()) {
                // Names are the device slug and the alert key: two targets
                // sharing one would silently overwrite each other's series.
                problems.push(format!("duplicate target name {:?}", t.name));
            }
            if t.target.trim().is_empty() {
                problems.push(format!("target {:?} has an empty target", t.name));
            }
            let timeout = t.timeout(p.timeout_secs);
            let interval = t.interval(p.interval_secs);
            if timeout >= interval {
                problems.push(format!(
                    "target {:?}: timeout ({timeout}s) must be shorter than its interval \
                     ({interval}s) — a check that can outlive its own tick is queued, \
                     not bounded",
                    t.name
                ));
            }
            match t.kind {
                ProbeKind::Http if !t.target.starts_with("http") => problems.push(format!(
                    "target {:?}: an http probe needs a URL, got {:?}",
                    t.name, t.target
                )),
                ProbeKind::Tls | ProbeKind::Tcp if !t.target.contains(':') => {
                    problems.push(format!(
                        "target {:?}: a {} probe needs host:port, got {:?}",
                        t.name, t.kind, t.target
                    ))
                }
                ProbeKind::CertFile if !t.target.starts_with('/') => problems.push(format!(
                    "target {:?}: a certfile probe needs an absolute path, got {:?}",
                    t.name, t.target
                )),
                ProbeKind::Icmp if !cfg!(feature = "icmp") => problems.push(format!(
                    "target {:?} is an icmp probe, but this build has no `icmp` feature — \
                     it needs a raw socket (CAP_NET_RAW). Build with --features icmp, or \
                     use a tcp probe, which needs nothing",
                    t.name
                )),
                _ => {}
            }
        }
        if p.alerts.expiry_critical_days > p.alerts.expiry_warn_days {
            problems
                .push("probe.alerts.expiry_critical_days must be <= expiry_warn_days".to_string());
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

    fn cfg(json: &str) -> std::result::Result<ProbeSensorConfig, String> {
        let c: ProbeSensorConfig = json5::from_str(json).map_err(|e| e.to_string())?;
        c.validate().map_err(|e| e.to_string()).map(|()| c)
    }

    #[test]
    fn an_empty_target_list_is_valid_and_asserts_nothing() {
        let c = cfg("{}").unwrap();
        assert!(c.probe.targets.is_empty());
        assert_eq!(c.probe.interval_secs, 60);
    }

    /// A check that can outlive its own tick is queued, not bounded.
    #[test]
    fn a_timeout_at_or_above_the_interval_is_refused_per_target() {
        let e = cfg(r#"{ probe: { interval_secs: 10, targets: [
                 { name: "x", kind: "tcp", target: "h:443", timeout_secs: 10 }] } }"#)
        .unwrap_err();
        assert!(e.contains("must be shorter than its interval"), "{e}");
    }

    /// The floor exists so this cannot be configured into a load generator
    /// against someone else's service.
    #[test]
    fn the_interval_floor_cannot_be_undercut_by_a_target() {
        let c = cfg(
            r#"{ probe: { targets: [
                 { name: "x", kind: "tcp", target: "h:443", interval_secs: 1, timeout_secs: 1 }] } }"#,
        )
        .unwrap();
        assert_eq!(c.probe.targets[0].interval(60), MIN_INTERVAL_SECS);
    }

    /// Names are the device slug and the alert key; a duplicate silently
    /// overwrites another target's series.
    #[test]
    fn duplicate_target_names_are_refused() {
        let e = cfg(r#"{ probe: { targets: [
                 { name: "x", kind: "tcp", target: "a:1" },
                 { name: "x", kind: "tcp", target: "b:2" }] } }"#)
        .unwrap_err();
        assert!(e.contains("duplicate target name"), "{e}");
    }

    #[test]
    fn each_kind_is_checked_against_the_shape_it_needs() {
        for (kind, bad, want) in [
            ("http", "example.com", "needs a URL"),
            ("tls", "example.com", "needs host:port"),
            ("certfile", "cert.pem", "needs an absolute path"),
        ] {
            let e = cfg(&format!(
                r#"{{ probe: {{ targets: [{{ name: "x", kind: "{kind}", target: "{bad}" }}] }} }}"#
            ))
            .unwrap_err();
            assert!(e.contains(want), "{kind}: {e}");
        }
    }

    /// An icmp target in a build without the feature is refused by name, with
    /// the alternative spelled out — rather than silently never running.
    #[test]
    fn an_icmp_target_without_the_feature_is_refused_loudly() {
        let r = cfg(r#"{ probe: { targets: [{ name: "p", kind: "icmp", target: "1.1.1.1" }] } }"#);
        if cfg!(feature = "icmp") {
            r.unwrap();
        } else {
            let e = r.unwrap_err();
            assert!(e.contains("--features icmp"), "{e}");
            assert!(e.contains("tcp probe"), "the alternative is named: {e}");
        }
    }

    /// A probe that follows a redirect somewhere else is checking something
    /// other than what it was asked about.
    #[test]
    fn off_host_redirects_are_disallowed_by_default() {
        let c = cfg(r#"{ probe: { targets: [
                 { name: "site", kind: "http", target: "https://example.com/" }] } }"#)
        .unwrap();
        assert!(!c.probe.targets[0].allow_offhost_redirect);
        assert!(c.probe.targets[0].follow_redirects);
    }

    #[test]
    fn a_targets_host_is_extracted_for_every_kind() {
        let c = cfg(r#"{ probe: { targets: [
                 { name: "a", kind: "http", target: "https://git.marcpardo.eu:8443/x?y=1" },
                 { name: "b", kind: "tls",  target: "git.marcpardo.eu:443" },
                 { name: "c", kind: "dns",  target: "git.marcpardo.eu" },
                 { name: "d", kind: "certfile", target: "/etc/tls/cert.pem" }] } }"#)
        .unwrap();
        let hosts: Vec<Option<String>> = c.probe.targets.iter().map(|t| t.host()).collect();
        assert_eq!(hosts[0].as_deref(), Some("git.marcpardo.eu"));
        assert_eq!(hosts[1].as_deref(), Some("git.marcpardo.eu"));
        assert_eq!(hosts[2].as_deref(), Some("git.marcpardo.eu"));
        assert_eq!(hosts[3], None, "a file has no host");
    }

    #[test]
    fn shipped_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/probe.json5");
        let text = std::fs::read_to_string(path).expect("configs/probe.json5 exists");
        let c: ProbeSensorConfig = json5::from_str(&text).expect("shipped config parses");
        c.validate().expect("shipped config validates");
    }
}
