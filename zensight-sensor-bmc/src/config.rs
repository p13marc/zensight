//! Configuration, and the startup refusals that make it honest (#953).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use zensight_common::serialization::Format;
use zensight_sensor_core::{LoggingConfig, SensorConfig, SensorError, ZenohConfig};

/// The smallest interval an endpoint may be polled at.
///
/// A BMC is a small embedded computer with a slow web stack, sharing a CPU
/// with the thing that keeps the server alive. Hammering it is not a
/// monitoring strategy, and a monitoring tool that can be configured into a
/// load generator against someone else's management controller is a footgun
/// with a config file (the `probe` precedent).
pub const MIN_INTERVAL_SECS: u64 = 10;

fn default_true() -> bool {
    true
}

/// How to reach a BMC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// HTTPS + JSON. What every BMC made since roughly 2016 serves.
    #[default]
    Redfish,
    /// `lanplus`, for hardware that predates Redfish. Behind the `ipmi` build
    /// feature, and refused at startup without it.
    Ipmi,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Redfish => "redfish",
            Transport::Ipmi => "ipmi",
        }
    }
}

/// One managed chassis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// The operator's name for this chassis. It is the key chunk and the
    /// alert label, so two endpoints sharing one would silently overwrite each
    /// other's series.
    pub name: String,
    /// `host` or `host:port`. Not a URL: the scheme is the transport's.
    pub address: String,
    #[serde(default)]
    pub transport: Transport,
    pub username: String,
    /// Through the `secret` indirection (`${ENV}` / `file:`), never in the
    /// file in a deployment.
    pub password: String,
    /// Override the poll cadence for this chassis.
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// A PEM bundle for the CA that signed this BMC's certificate. **The right
    /// answer** for a BMC with its own internal CA, and the one that keeps
    /// verification on.
    #[serde(default)]
    pub ca_file: Option<String>,
    /// Turn TLS verification off for this endpoint.
    ///
    /// Spelled out per endpoint, never implied, and warned at every boot. A
    /// BMC ships a self-signed certificate out of the factory, so refusing to
    /// run against one would just push operators to a worse workaround — but
    /// it means the endpoint is **not authenticated**, and something has to
    /// say so on a schedule.
    #[serde(default)]
    pub insecure: bool,
    /// Skip this chassis without deleting its configuration.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Endpoint {
    pub fn interval(&self, default: u64) -> u64 {
        self.interval_secs.unwrap_or(default).max(MIN_INTERVAL_SECS)
    }

    pub fn timeout(&self, default: u64) -> u64 {
        self.timeout_secs.unwrap_or(default)
    }

    /// The base URL a Redfish client polls. Separate from `address` so the
    /// e2e can point a client at a plain-HTTP fake without standing up a TLS
    /// listener — which would test rustls, not this sensor.
    pub fn base_url(&self) -> String {
        format!("https://{}", self.address)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BmcConfig {
    /// The reporting host. Defaults to this machine's hostname — **never** an
    /// endpoint address, which is an endpoint and not an identity (#885).
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// In-flight requests across all endpoints.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Publish a third-party identity claim about each chassis, so the BMC's
    /// view of a machine fuses in the catalog with that machine's own sensors.
    #[serde(default = "default_true")]
    pub evidence: bool,
    #[serde(default)]
    pub alerts: AlertsConfig,
}

fn default_interval() -> u64 {
    60
}
fn default_timeout() -> u64 {
    10
}
fn default_max_concurrent() -> usize {
    4
}

impl Default for BmcConfig {
    fn default() -> Self {
        Self {
            source: None,
            endpoints: Vec::new(),
            interval_secs: default_interval(),
            timeout_secs: default_timeout(),
            max_concurrent: default_max_concurrent(),
            evidence: true,
            alerts: AlertsConfig::default(),
        }
    }
}

impl BmcConfig {
    /// The reporting host.
    ///
    /// Never falls back to an endpoint address. `configs/` and the systemd
    /// units both recommend `127.0.0.1` for a locally-managed BMC, which is
    /// the one address guaranteed to be ambiguous across machines (#885).
    pub fn resolved_source(&self) -> String {
        self.source.clone().unwrap_or_else(|| {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "unknown".to_string())
        })
    }
}

/// Which assertions this sensor may raise. Every one reads a BMC verdict;
/// none takes a number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// "Violated continuously for N seconds" before publishing.
    #[serde(default)]
    pub for_secs: u64,
    /// Consecutive failed cycles before `bmc-unreachable` fires.
    #[serde(default = "default_unreachable_cycles")]
    pub unreachable_cycles: u32,
    /// Fire when a supply bay the BMC reported present on an earlier cycle
    /// reads `Absent`. Off by default: a chassis shipped with one supply in a
    /// two-bay backplane is normal, and this sensor cannot tell that from a
    /// supply someone pulled.
    #[serde(default)]
    pub psu_absent: bool,
}

fn default_unreachable_cycles() -> u32 {
    3
}

impl Default for AlertsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            for_secs: 0,
            unreachable_cycles: default_unreachable_cycles(),
            psu_absent: false,
        }
    }
}

/// The shipped config file's shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BmcSensorConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub serialization: Format,
    #[serde(default)]
    pub logging: LoggingConfig,

    /// Declared resource envelope (#811/#1091). `resources.budget_rss_mb` is
    /// carried into the health doc's `self_stats.budget_bytes` and graded by
    /// the runner's `sensor-budget` rule at 80 % — declared, not enforced
    /// (#812 is the shed ladder). Absent reads as *undeclared*, never as
    /// unlimited-and-fine.
    #[serde(default)]
    pub resources: zensight_sensor_core::ResourcesConfig,
    #[serde(default)]
    pub bmc: BmcConfig,

    /// `@desired` reconcile settings (#931): the kill switch and refresh
    /// cadence. File config on purpose — the mechanism that could misbehave
    /// must be disarmable from outside itself.
    #[serde(default)]
    pub desired: zensight_common::desired::DesiredConfig,

    /// Operator-authored threshold rules over this sensor's own telemetry
    /// (#931). **Empty by default** — this build ships no threshold that
    /// fires. Also authorable fleet-wide on `@desired` and per-host over
    /// `@rpc/bmc/thresholds/set`; `state/bmc/applied/thresholds`
    /// says which of the three is in force.
    #[serde(default)]
    pub thresholds: zensight_common::threshold::ThresholdsConfig,
}

impl SensorConfig for BmcSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &'static str {
        "bmc"
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

    /// Every problem at once.
    ///
    /// Reporting the first and stopping means an operator fixes one line, runs
    /// it again, and finds the next — which is how a five-minute config change
    /// takes twenty.
    fn validate(&self) -> zensight_sensor_core::Result<()> {
        let b = &self.bmc;
        let mut problems = Vec::new();

        let mut seen = HashSet::new();
        for e in b.endpoints.iter().filter(|e| e.enabled) {
            if e.name.trim().is_empty() {
                problems.push("an endpoint has an empty name".to_string());
            } else if !seen.insert(e.name.as_str()) {
                // Names are the key chunk and the alert label: two endpoints
                // sharing one would silently overwrite each other's series.
                problems.push(format!("two endpoints are both named {:?}", e.name));
            }
            if e.address.trim().is_empty() {
                problems.push(format!("endpoint {:?} has no address", e.name));
            }
            if e.username.trim().is_empty() {
                problems.push(format!("endpoint {:?} has no username", e.name));
            }

            let timeout = e.timeout(b.timeout_secs);
            let interval = e.interval(b.interval_secs);
            if timeout >= interval {
                problems.push(format!(
                    "endpoint {:?}: timeout ({timeout}s) must be shorter than its interval \
                     ({interval}s) — a poll that can outlive its own tick is queued, not \
                     bounded; raise the interval or lower the timeout",
                    e.name
                ));
            }

            // A transport this build cannot speak is refused HERE, not
            // discovered later as a permanently unreachable endpoint. A check
            // that did not run is not evidence about the target.
            if e.transport == Transport::Ipmi {
                problems.push(format!(
                    "endpoint {:?} is an ipmi endpoint, but {}",
                    e.name,
                    crate::ipmi::unavailable_reason()
                ));
            }

            if e.ca_file.is_some() && e.insecure {
                problems.push(format!(
                    "endpoint {:?} sets both ca_file and insecure — insecure turns \
                     verification off entirely, so the CA would never be consulted. Pick one",
                    e.name
                ));
            }
        }

        if b.max_concurrent == 0 {
            problems.push("bmc.max_concurrent must be at least 1".to_string());
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(SensorError::config(problems.join("; ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(name: &str) -> Endpoint {
        Endpoint {
            name: name.to_string(),
            address: "10.0.0.10".to_string(),
            transport: Transport::Redfish,
            username: "monitor".to_string(),
            password: "file:/run/credentials/bmc.password".to_string(),
            interval_secs: None,
            timeout_secs: None,
            ca_file: None,
            insecure: false,
            enabled: true,
        }
    }

    fn cfg(endpoints: Vec<Endpoint>) -> BmcSensorConfig {
        BmcSensorConfig {
            bmc: BmcConfig {
                endpoints,
                ..BmcConfig::default()
            },
            ..BmcSensorConfig::default()
        }
    }

    #[test]
    fn a_plain_redfish_endpoint_validates() {
        cfg(vec![endpoint("rack-a-1")]).validate().unwrap();
    }

    /// An empty endpoint list is a no-op, not an error: it is what a config
    /// shipped to a fleet where only some hosts manage a BMC looks like.
    #[test]
    fn no_endpoints_is_not_an_error() {
        cfg(Vec::new()).validate().unwrap();
    }

    /// The bound has to be real, not decorative: a timeout at or above the
    /// interval means a slow BMC silently turns into a sensor that never
    /// completes a cycle.
    #[test]
    fn a_timeout_that_can_outlive_its_tick_is_refused_and_says_what_to_change() {
        let mut e = endpoint("slow");
        e.interval_secs = Some(10);
        e.timeout_secs = Some(10);
        let err = cfg(vec![e]).validate().unwrap_err().to_string();
        assert!(err.contains("must be shorter"), "{err}");
        assert!(
            err.contains("raise the interval"),
            "the refusal must say what to change: {err}"
        );
    }

    /// Names are the key chunk and the alert label. Two the same is one
    /// chassis silently overwriting the other.
    #[test]
    fn duplicate_names_are_refused() {
        let err = cfg(vec![endpoint("dup"), endpoint("dup")])
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("both named"), "{err}");
    }

    /// A transport this build cannot speak is refused at startup, naming the
    /// flag and the alternative — never discovered later as an endpoint that
    /// is permanently down.
    #[test]
    fn an_ipmi_endpoint_is_refused_loudly_and_names_the_alternative() {
        let mut e = endpoint("old-box");
        e.transport = Transport::Ipmi;
        let err = cfg(vec![e]).validate().unwrap_err().to_string();
        assert!(err.contains("ipmi endpoint"), "{err}");
        assert!(err.contains("redfish"), "the alternative is named: {err}");
    }

    /// Setting both is not "extra safe", it is a contradiction — and one whose
    /// consequence (the CA is never consulted) is invisible at runtime.
    #[test]
    fn a_ca_file_with_insecure_is_a_contradiction() {
        let mut e = endpoint("both");
        e.ca_file = Some("/etc/zensight/bmc-ca.pem".to_string());
        e.insecure = true;
        let err = cfg(vec![e]).validate().unwrap_err().to_string();
        assert!(err.contains("Pick one"), "{err}");
    }

    /// Every problem at once, so one run of the sensor fixes the whole file.
    #[test]
    fn every_problem_is_reported_together() {
        let mut bad = endpoint("");
        bad.address = String::new();
        bad.username = String::new();
        let err = cfg(vec![bad]).validate().unwrap_err().to_string();
        assert!(err.contains("empty name"), "{err}");
        assert!(err.contains("no address"), "{err}");
        assert!(err.contains("no username"), "{err}");
    }

    /// A disabled endpoint is skipped, not validated — that is what makes it
    /// a way to park a chassis rather than delete it.
    #[test]
    fn a_disabled_endpoint_is_not_validated() {
        let mut e = endpoint("parked");
        e.transport = Transport::Ipmi;
        e.enabled = false;
        cfg(vec![e]).validate().unwrap();
    }

    /// The default source is this host, never an endpoint address: `configs/`
    /// recommends `127.0.0.1` for a locally-managed BMC, the one address
    /// guaranteed to be ambiguous across machines (#885).
    #[test]
    fn the_default_source_is_this_host_never_the_bmc_address() {
        let c = BmcConfig {
            endpoints: vec![endpoint("rack-a-1")],
            ..BmcConfig::default()
        };
        let source = c.resolved_source();
        assert_ne!(source, "10.0.0.10");
        assert!(!source.is_empty());
    }

    /// The floor applies even to an endpoint that asks for less.
    #[test]
    fn the_interval_floor_cannot_be_configured_away() {
        let mut e = endpoint("eager");
        e.interval_secs = Some(1);
        assert_eq!(e.interval(60), MIN_INTERVAL_SECS);
    }
}

#[cfg(test)]
mod shipped_config {
    use super::*;

    /// The shipped config must load and validate. Nothing else reads it, so
    /// without this it rots silently — the #845 lesson, six configs over.
    #[test]
    fn shipped_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/bmc.json5");
        let _config = BmcSensorConfig::load(path).expect("configs/bmc.json5 must load");
    }
}
