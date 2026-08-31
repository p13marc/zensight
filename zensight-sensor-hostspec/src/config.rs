//! Configuration for the hostspec sensor (#821).

use serde::{Deserialize, Serialize};
use zensight_common::config::ZenohConfig;
use zensight_sensor_core::LoggingConfig;

use crate::sentinel::ExpectationsConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostspecSensorConfig {
    /// Zenoh connection settings.
    pub zenoh: ZenohConfig,

    /// Serialization format for telemetry.
    #[serde(default)]
    pub serialization: zensight_common::serialization::Format,

    /// hostspec settings.
    #[serde(default)]
    pub hostspec: HostspecConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// `@desired` reconcile settings (#816): the kill switch and refresh
    /// cadence. File config on purpose — the mechanism that could misbehave
    /// must be disarmable from outside itself.
    #[serde(default)]
    pub desired: zensight_common::desired::DesiredConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostspecConfig {
    /// Override the source id (default: the local hostname).
    #[serde(default)]
    pub source: Option<String>,

    /// The assertion set. Deliberately NOT `Option` — an empty set is a
    /// valid, useful state (every procedure served, `spec` answers "held to
    /// nothing", the gauge publishes 0), so there is no disabled mode and no
    /// `serve_unavailable` arm. The systemd sentinel's `None` means
    /// "feature off"; hostspec IS its sentinel.
    #[serde(default)]
    pub expectations: ExpectationsConfig,
}

impl HostspecSensorConfig {
    pub fn source(&self) -> String {
        self.hostspec
            .source
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| hostname::get().ok().and_then(|h| h.into_string().ok()))
            .unwrap_or_else(|| "unknown".to_string())
    }
}

impl zensight_sensor_core::SensorConfig for HostspecSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &str {
        "hostspec"
    }

    /// The same validation the hot-swap path runs — a config that would be
    /// refused over `expectations/set` refuses to start, with the same
    /// message naming every offending expectation.
    fn validate(&self) -> zensight_sensor_core::Result<()> {
        crate::sentinel::validate(&self.hostspec.expectations)
            .map_err(zensight_sensor_core::SensorError::config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let cfg: HostspecSensorConfig = json5::from_str(r#"{ zenoh: { mode: "peer" } }"#).unwrap();
        assert!(cfg.hostspec.expectations.is_empty());
        assert_eq!(cfg.hostspec.expectations.eval_interval_secs, 60);
        assert_eq!(cfg.hostspec.expectations.default_for_secs, 0);
    }

    /// The shipped example config must load AND ship the empty default set —
    /// that emptiness is what keeps the conformance CI deployment green on a
    /// runner with none of an operator's paths (#821 design).
    #[test]
    fn shipped_config_parses_and_is_empty() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/hostspec.json5");
        let text = std::fs::read_to_string(path).expect("configs/hostspec.json5 exists");
        let cfg: HostspecSensorConfig = json5::from_str(&text).expect("shipped config parses");
        assert!(
            cfg.hostspec.expectations.is_empty(),
            "the shipped default set must be empty (conformance-safe)"
        );
        crate::sentinel::validate(&cfg.hostspec.expectations).expect("shipped set validates");
    }

    /// The `//DEMO ` block in the shipped config is what `gen-configs.sh
    /// --profile demo-max` uncomments (#867). It is inert here — the test
    /// above pins that — so nothing else would notice if it stopped parsing,
    /// or if the deliberately-failing clause quietly became a passing one.
    /// This applies the generator's own transform and checks the result.
    #[test]
    fn demo_profile_assertion_set_parses_and_validates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/hostspec.json5");
        let text = std::fs::read_to_string(path).expect("configs/hostspec.json5 exists");
        // The same substitution as scripts/gen-configs.sh: strip the marker,
        // keep the indentation.
        let demo: String = text
            .lines()
            .map(|l| match l.split_once("//DEMO ") {
                Some((indent, rest)) if indent.trim().is_empty() => format!("{indent}{rest}"),
                _ => l.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !demo.lines().any(|l| l.trim_start().starts_with("//DEMO ")),
            "every marker line must be uncommented by the transform"
        );
        let cfg: HostspecSensorConfig = json5::from_str(&demo).expect("demo config parses");
        let exp = &cfg.hostspec.expectations;
        crate::sentinel::validate(exp).expect("demo set validates");
        assert!(
            !exp.is_empty(),
            "the demo profile must hold this host to something"
        );
        assert_eq!(exp.mounts.len(), 1, "one green mount assertion");
        assert_eq!(exp.files.len(), 1, "one green file assertion");
        // The one that DELIBERATELY fails, the netlink `demo-expected-service`
        // motif: a listener that is not there. If this ever starts passing the
        // demo stops showing the alert pipeline at all.
        assert_eq!(exp.listening.len(), 1);
        let l = &exp.listening[0];
        assert_eq!(l.name, "demo-expected-listener");
        assert!(
            !l.forbid,
            "it must REQUIRE a listener, so its absence fires"
        );
        assert_eq!(l.port, 65001);
    }

    #[test]
    fn expectations_round_trip_json() {
        let cfg: ExpectationsConfig = json5::from_str(
            r#"{
                mounts: [{ name: "vt", path: "/var/tmp", is_bind_of: "/scratch/tmp", severity: "critical" }],
                files: [{ name: "backup", path: "/backup/db.dump", newer_than_secs: 93600, size_within_pct_of_previous: 40 }],
                listening: [{ name: "vpn-only", port: 8443, addr: "10.8.0.1" },
                            { name: "not-public", port: 8443, addr: "0.0.0.0", forbid: true, for_secs: 120 }],
                perms: [{ name: "key", path: "/etc/deploy/key.pem", mode: "0600", owner: "deploy" }],
            }"#,
        )
        .unwrap();
        crate::sentinel::validate(&cfg).unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: ExpectationsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }
}
