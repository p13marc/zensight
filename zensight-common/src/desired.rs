//! The `@desired` reconcile contract's shared types (#816).
//!
//! A controller publishes per-host runtime POLICY under the `@desired`
//! service origin (`v1/@desired/state/<host>/<producer>/<topic>` — RFC 07
//! §3, RFC 12 §3); the target sensor reconciles it over its file config and
//! publishes an [`AppliedConfig`] marker saying what is actually in force,
//! so drift between desired and effective is visible rather than assumed.
//!
//! **The never-list** (the single most important constraint in #816):
//! nothing under `@desired` may carry secrets or anything a sensor needs to
//! REACH THE BUS — endpoints, TLS material, the namespace. One bad desired
//! publish must never lock the fleet out of its own supervision. The
//! consumer enforces this structurally: the reconciler only deserializes a
//! sentinel's own config type and only writes that sentinel's handle.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}
fn default_refresh_secs() -> u64 {
    300
}

/// Per-sensor file-config block for the `@desired` path: the kill switch and
/// the reconcile cadence. Lives in FILE config on purpose — the mechanism
/// that could misbehave must be disarmable from outside itself.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DesiredConfig {
    /// `false` makes this sensor ignore `@desired` entirely (the marker then
    /// says `source: file`). The kill switch for the case where the
    /// mechanism itself is the problem.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Periodic re-seed GET interval, seconds. The GET against the
    /// deployment's `@desired` storage is the PRIMARY convergence path —
    /// level-triggered, it survives any missed sample, reconnect or router
    /// restart; the live subscription is the accelerator.
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
}

impl Default for DesiredConfig {
    fn default() -> Self {
        DesiredConfig {
            enabled: true,
            refresh_secs: default_refresh_secs(),
        }
    }
}

/// Where the config actually in force came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AppliedSource {
    /// The file/bootstrap baseline (also: kill switch on, or a desired
    /// delete reverted to baseline).
    File,
    /// A `@desired` document.
    Desired,
    /// An operator's `@rpc/<producer>/<topic>/set`. Two writers exist and
    /// the rule is LWW by arrival; this marker is what says who won last.
    Rpc,
}

/// A desired document the sensor REFUSED, kept on the marker until a good
/// one supersedes it — the rejection is on the bus, not only in a log.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RejectedDesired {
    /// Millis since epoch when the rejection happened.
    pub at: i64,
    /// The rejected sample's HLC timestamp, when it carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// The validation/decode error, verbatim.
    pub error: String,
}

/// The effective-config marker (`state/<producer>/applied/<topic>`): what is
/// actually in force for one reconcilable topic, and why. Published on every
/// change of the answer — including at startup, so drift is visible before
/// any desired doc exists.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AppliedConfig {
    pub topic: String,
    pub source: AppliedSource,
    /// Millis since epoch when this config took effect.
    pub applied_at: i64,
    /// The applied desired sample's HLC timestamp (`source: desired` only) —
    /// the drift-correlation handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_timestamp: Option<String>,
    /// The document actually in force, JSON-encoded. A string, deliberately:
    /// its real schema is the TOPIC's own type (named by `topic` and served
    /// by the producer's `describe`), and a `Value` field here would be the
    /// anything-goes stub the #815 schema gate exists to refuse. Consumers
    /// wanting structure parse it with the topic type.
    pub effective_json: String,
    /// The most recent rejected desired doc, until superseded by a good one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rejected: Option<RejectedDesired>,
}

// ── The topic table and the never-list lint (#937) ───────────────────────────

/// One `@desired` topic: which producer reconciles it, and how to tell whether
/// a candidate document is one.
///
/// The table exists because three parties need the same answer and had no
/// shared way to get it: the policy controller (#938) must refuse a document
/// before publishing it, the GUI must refuse one before offering to, and the
/// registry conformance test must be able to say that a *new* topic shipped
/// without a validator. Before this, the only thing that could reject a bad
/// document was the sensor that received it, which is the last possible moment
/// and the one place where the operator is not looking.
pub struct TopicSpec {
    /// The producer chunk — the second subject chunk of
    /// `{host}/<producer>/<topic>`.
    pub producer: &'static str,
    /// The topic chunk.
    pub topic: &'static str,
    /// The registry type name this subject declares, so a failure can name it.
    pub type_name: &'static str,
    /// Deserialize `json` into the registered type. `Ok` means the bytes are
    /// that type; it does **not** mean the document is sensible, which is the
    /// receiving sentinel's own `validate`.
    parse: fn(&serde_json::Value) -> Result<(), String>,
}

impl TopicSpec {
    /// Both halves of "is this a valid document for this topic": it
    /// deserializes into the registered type, **and** it carries nothing on
    /// the never-list.
    ///
    /// The order matters. Type-checking first means a never-list hit is
    /// reported against a document that is otherwise well-formed, rather than
    /// alongside a wall of serde errors.
    pub fn validate(&self, json: &serde_json::Value) -> Result<(), String> {
        (self.parse)(json)?;
        never_list_lint(json)
    }
}

impl std::fmt::Debug for TopicSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TopicSpec")
            .field("producer", &self.producer)
            .field("topic", &self.topic)
            .field("type_name", &self.type_name)
            .finish_non_exhaustive()
    }
}

/// Deserialize-and-discard, as a `fn` pointer the table can hold.
fn parses_as<T: serde::de::DeserializeOwned>(json: &serde_json::Value) -> Result<(), String> {
    serde_json::from_value::<T>(json.clone())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The producers that carry a `thresholds` topic — every one of them, since
/// #931: `ThresholdsConfig` was written in `zensight-common` from the start,
/// so they all joined at once rather than one per sentinel rewrite.
const THRESHOLD_PRODUCERS: &[&str] = &[
    "bmc",
    "container",
    "gnmi",
    "hostspec",
    "logs",
    "modbus",
    "netflow",
    "netlink",
    "netring",
    "parallax",
    "probe",
    "pve",
    "snmp",
    "sysinfo",
    "systemd",
];

/// Every `@desired` topic and its validator.
///
/// Kept in step with `registry/desired.toml` by
/// `every_desired_subject_has_a_validator` below: a new subject with no entry
/// here fails the build's test run, which is the only moment anyone is looking.
pub fn topics() -> Vec<TopicSpec> {
    let mut out = vec![
        TopicSpec {
            producer: "hostspec",
            topic: "expectations",
            type_name: "HostspecExpectations",
            parse: parses_as::<crate::hostspec::ExpectationsConfig>,
        },
        TopicSpec {
            producer: "systemd",
            topic: "expectations",
            type_name: "ExpectationsConfig",
            parse: parses_as::<crate::systemd::ExpectationsConfig>,
        },
        TopicSpec {
            producer: "netlink",
            topic: "expectations",
            type_name: "NetlinkExpectations",
            parse: parses_as::<crate::netlink::NetlinkExpectations>,
        },
        TopicSpec {
            producer: "logs",
            topic: "rules",
            type_name: "LogRulesConfig",
            parse: parses_as::<crate::logs::LogRulesConfig>,
        },
    ];
    out.extend(THRESHOLD_PRODUCERS.iter().map(|p| TopicSpec {
        producer: p,
        topic: "thresholds",
        type_name: "ThresholdsConfig",
        parse: parses_as::<crate::threshold::ThresholdsConfig>,
    }));
    out
}

/// Look one topic up by `(producer, topic)`.
pub fn topic(producer: &str, topic: &str) -> Option<TopicSpec> {
    topics()
        .into_iter()
        .find(|t| t.producer == producer && t.topic == topic)
}

/// Key names that must never appear in a `@desired` document.
///
/// The never-list is the single most important constraint in #816: nothing on
/// this origin may carry a secret or anything a sensor needs to REACH THE BUS,
/// because one bad desired publish would otherwise lock the fleet out of its
/// own supervision — and the fix would have to travel over the bus it just
/// broke.
const NEVER: &[&str] = &[
    "community",
    "connect",
    "endpoint",
    "endpoints",
    "listen",
    "namespace",
    "password",
    "passphrase",
    "secret",
    "tls",
    "token",
];

/// Reject a document that carries a never-list key **with a value that could
/// be one**.
///
/// # Why the value's shape is part of the test
///
/// A blind key ban is wrong here, and two shipped types prove it:
/// `NetlinkExpectation`'s `listen: Option<u16>` is the TCP port a socket
/// expectation checks for a listener, and `HostspecExpectations::listening` is
/// a whole family of assertions about ports. Neither is a bus endpoint;
/// refusing them would make two sentinels unauthorable to protect against a
/// spelling.
///
/// What actually distinguishes them is the value. A secret, an endpoint, a TLS
/// block and a namespace are strings, arrays of strings, or objects. A port is
/// a number. So a never-list key is refused unless its value is a number or a
/// boolean, and the two false positives pass for the reason they should: they
/// are numbers.
///
/// This is defence in depth, not the only defence. The consumer side is
/// structural — the reconciler deserializes only a sentinel's own config type
/// and writes only that sentinel's handle — and no registered `@desired` type
/// has a string field with any of these names. The lint exists so that a type
/// which *grows* one is caught at the moment it is proposed rather than at the
/// moment a fleet stops answering.
pub fn never_list_lint(json: &serde_json::Value) -> Result<(), String> {
    fn walk(v: &serde_json::Value, path: &str) -> Result<(), String> {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    let here = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    let lower = k.to_ascii_lowercase();
                    if NEVER.contains(&lower.as_str())
                        && !matches!(
                            val,
                            serde_json::Value::Number(_) | serde_json::Value::Bool(_)
                        )
                    {
                        return Err(format!(
                            "{here}: `{k}` is on the @desired never-list — nothing on this \
                             origin may carry a secret or anything a sensor needs to reach \
                             the bus (#816). Keep it in the host's own config file."
                        ));
                    }
                    walk(val, &here)?;
                }
                Ok(())
            }
            serde_json::Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}[{i}]"))?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(json, "")
}

#[cfg(test)]
mod topic_table_tests {
    use super::*;

    /// A new `@desired` subject cannot ship without a validator.
    ///
    /// The table and `registry/desired.toml` are two lists of the same thing,
    /// and the failure mode of two lists is that they diverge silently — the
    /// controller publishes a document nothing checked, and the first thing to
    /// notice is the sensor that refuses it, on a host nobody is watching.
    #[test]
    fn every_desired_subject_has_a_validator() {
        let toml = crate::registry::desired::REGISTRY_TOML;
        let table = topics();

        let mut declared = Vec::new();
        for line in toml.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("path = \"") else {
                continue;
            };
            let Some(path) = rest.strip_suffix('"') else {
                continue;
            };
            // `{host}/<producer>/<topic>` — the G1 proxy ordering, enforced by
            // zenkey-build's H4 lint, so the shape is guaranteed.
            let parts: Vec<&str> = path.split('/').collect();
            assert_eq!(
                parts.len(),
                3,
                "unexpected @desired subject shape {path:?} — the table's parser assumes \
                 {{host}}/<producer>/<topic>"
            );
            declared.push((parts[1].to_string(), parts[2].to_string()));
        }
        assert!(
            !declared.is_empty(),
            "parsed no subjects out of desired.toml — the parser broke, not the registry"
        );

        for (producer, topic) in &declared {
            assert!(
                table
                    .iter()
                    .any(|t| t.producer == producer && t.topic == topic),
                "@desired subject {{host}}/{producer}/{topic} has no entry in \
                 `desired::topics()`. A topic without a validator is a document the \
                 controller and the GUI will publish unchecked."
            );
        }

        // And the other way: an entry naming a subject the registry does not
        // declare would validate documents nobody can publish.
        for spec in &table {
            assert!(
                declared
                    .iter()
                    .any(|(p, t)| p == spec.producer && t == spec.topic),
                "`desired::topics()` has {}/{} , which `desired.toml` does not declare",
                spec.producer,
                spec.topic
            );
        }
    }

    #[test]
    fn a_document_of_the_wrong_type_is_refused() {
        let spec = topic("logs", "rules").expect("logs/rules");
        let err = spec
            .validate(&serde_json::json!({ "rules": "not an array" }))
            .expect_err("a string is not a rule list");
        assert!(!err.is_empty());
        assert!(
            spec.validate(&serde_json::json!({})).is_ok(),
            "empty is valid"
        );
    }

    #[test]
    fn the_never_list_refuses_a_secret_and_names_where_it_is() {
        let err = never_list_lint(&serde_json::json!({
            "devices": [{ "name": "sw1", "community": "public" }]
        }))
        .expect_err("a community string must never ride @desired");
        assert!(err.contains("devices[0].community"), "{err}");
        assert!(err.contains("never-list"), "{err}");
    }

    #[test]
    fn the_never_list_refuses_an_endpoint_however_it_is_shaped() {
        for doc in [
            serde_json::json!({ "connect": ["tcp/10.0.0.1:7447"] }),
            serde_json::json!({ "zenoh": { "endpoint": "tcp/10.0.0.1:7447" } }),
            serde_json::json!({ "namespace": "prod" }),
            serde_json::json!({ "tls": { "root_ca": "/etc/pki/ca.pem" } }),
        ] {
            assert!(
                never_list_lint(&doc).is_err(),
                "should have been refused: {doc}"
            );
        }
    }

    /// The two shipped false positives. `SocketExpectation::listen` is the TCP
    /// port a netlink expectation checks for a listener, and
    /// `HostspecExpectations::listening` is a whole family of port assertions.
    /// A blind key ban would make both sentinels unauthorable to protect
    /// against a spelling.
    #[test]
    fn a_port_named_listen_is_not_an_endpoint() {
        let netlink = serde_json::json!({
            "sockets": [{ "name": "sshd", "listen": 22, "min": 1 }]
        });
        assert!(never_list_lint(&netlink).is_ok(), "a port is a number");
        assert!(
            topic("netlink", "expectations")
                .expect("netlink/expectations")
                .validate(&netlink)
                .is_ok(),
            "and the real type accepts it"
        );

        let hostspec = serde_json::json!({
            "listening": [{ "name": "sshd", "port": 22 }]
        });
        assert!(
            never_list_lint(&hostspec).is_ok(),
            "`listening` is not `listen`"
        );

        // But a *string* under the same key is still refused: that is what an
        // endpoint looks like, and it is the case worth catching.
        assert!(never_list_lint(&serde_json::json!({ "listen": "tcp/0.0.0.0:7447" })).is_err());
    }

    /// Every registered type must survive its own `Default` through the
    /// lint — if a shipped shape trips it, the lint is wrong, not the type.
    #[test]
    fn no_registered_type_trips_the_never_list_on_its_own_defaults() {
        for (name, doc) in [
            (
                "HostspecExpectations",
                serde_json::to_value(crate::hostspec::ExpectationsConfig::default()).unwrap(),
            ),
            (
                "ExpectationsConfig",
                serde_json::to_value(crate::systemd::ExpectationsConfig::default()).unwrap(),
            ),
            (
                "NetlinkExpectations",
                serde_json::to_value(crate::netlink::NetlinkExpectations::default()).unwrap(),
            ),
            (
                "LogRulesConfig",
                serde_json::to_value(crate::logs::LogRulesConfig::default()).unwrap(),
            ),
            (
                "ThresholdsConfig",
                serde_json::to_value(crate::threshold::ThresholdsConfig::default()).unwrap(),
            ),
        ] {
            assert!(
                never_list_lint(&doc).is_ok(),
                "{name}'s own default trips the never-list: {:?}",
                never_list_lint(&doc)
            );
        }
    }
}
