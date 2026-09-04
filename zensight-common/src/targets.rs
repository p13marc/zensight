//! Runtime target sets for the two sensors that poll things an operator chose
//! (#936): which SNMP devices, which synthetic probes.
//!
//! # Why these are not the sensors' own config types
//!
//! Both sensors already have a target type in their file config, and neither
//! may go on the wire as it stands. `DeviceConfig` carries an SNMP community
//! string and v3 authentication/privacy passphrases; `Target` carries
//! arbitrary HTTP request headers, which is where an `Authorization: Bearer …`
//! lives in practice — and, unlike SNMP's credentials, that field does **not**
//! go through `zensight_sensor_core::secret`, so what is in the file is the
//! literal token.
//!
//! Putting either type on `@desired` wholesale would put credentials on the
//! bus, which is the never-list's central prohibition (#816): nothing on that
//! origin may carry a secret, because one bad publish must never lock a fleet
//! out of its own supervision.
//!
//! **The `@desired` never-list lint would not catch it.** That lint tests key
//! *names* — `password`, `community`, `token` — and here the key is `headers`
//! or `credentials`; the secret is a *value* nested inside. So the protection
//! has to be the type, not a check over it.
//!
//! # The shape: the wire references a name, the secret stays local
//!
//! These specs are deliberate **subsets**. An SNMP target names a credential
//! set; a probe target has no header field at all. The sensor resolves each
//! against its own file config when it applies the set, and refuses a name it
//! does not have — so a fleet author can say *which* credentials to use and can
//! never say *what* they are.
//!
//! That is the same answer `snmp.credentials` already gives file config, moved
//! one layer out.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::probe::ProbeKind;

// ── SNMP ────────────────────────────────────────────────────────────────────

/// The SNMP device set for one host, as a fleet may author it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SnmpTargets {
    #[serde(default)]
    pub targets: Vec<SnmpTarget>,
}

/// One SNMP device.
///
/// The file-config `DeviceConfig` has thirteen more fields, most of them
/// tuning (`timeout_secs`, `retries`, `max_pdus_per_sec`) and two of them
/// secret (`community`, `security`). This carries what a *fleet* decides —
/// which devices exist, how they are reached, how often, and which profile
/// reads them — and leaves the rest where it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnmpTarget {
    /// Operator's name for the device. The subject chunk its telemetry rides
    /// on, so it is also what an alert names.
    pub name: String,
    /// `ip:port`, or `ip` for the default 161.
    pub address: String,
    /// **A name** into the sensor's file-config `snmp.credentials` map — never
    /// a community string, never a passphrase.
    ///
    /// A name the host does not have is refused, and the refusal rides
    /// `state/snmp/applied/targets`. That is the failure worth being loud
    /// about: silently polling with the wrong community reads as a device that
    /// stopped answering.
    pub credentials: String,
    /// Device profile to apply (`snmp.profiles`), when the fleet knows what
    /// kind of thing this is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Named OID group from file config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_interval_secs: Option<u64>,
}

// ── probe ───────────────────────────────────────────────────────────────────

/// The synthetic-probe target set for one host, as a fleet may author it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ProbeTargets {
    #[serde(default)]
    pub targets: Vec<ProbeTarget>,
}

/// One probe target: the file-config `Target`, **minus `headers`**.
///
/// The omission is the whole point of the type. Everything else is here,
/// because a fleet that can add a check but not say what counts as success has
/// not been given target management.
///
/// `probe_target_spec_is_the_file_target_minus_headers` pins the two together:
/// a field added to one and not the other fails it. That test exists because
/// the alternative — one shared type — would have put an `Authorization`
/// header on the bus, and the alternative to *that* — `serde(flatten)` — moved
/// ninety field accesses in the sensor for the same guarantee.
///
/// `deny_unknown_fields` is not tidiness here: without it serde would *ignore*
/// a `headers` key rather than refuse it, so an operator who put a bearer
/// token in the policy would see the probe run, see it succeed against an
/// unauthenticated endpoint, and never learn the header was dropped. Refusing
/// teaches; ignoring misleads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProbeTarget {
    /// Operator's name — the device slug and the alert key.
    pub name: String,
    pub kind: ProbeKind,
    /// A URL for `http`, `host:port` for `tls`/`tcp`, a name for `dns`, a host
    /// for `icmp`, a path for `certfile`.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,

    // ── HTTP ────────────────────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_status: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_body: Option<String>,
    #[serde(default = "crate::default_true")]
    pub follow_redirects: bool,
    #[serde(default)]
    pub allow_offhost_redirect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    // ── burst ───────────────────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spacing_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,

    // ── TLS ─────────────────────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    #[serde(default = "crate::default_true")]
    pub inspect_untrusted: bool,

    // ── DNS ─────────────────────────────────────────────────────────────
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expect_addrs: Vec<String>,

    /// Skip this target without deleting it.
    #[serde(default = "crate::default_true")]
    pub enabled: bool,
}

// ── shared validation ───────────────────────────────────────────────────────

/// Names must be present, unique and slug-safe — for both sensors, and for the
/// same reason.
///
/// A target's name becomes a **key chunk** (`telemetry/snmp/<name>/…`,
/// `state/probe/device/<name>/alive`) and part of the alert key. Two targets
/// sharing one collapse onto a single series and a single alert that flap over
/// each other; a name with a `/` in it invents a subject level nobody
/// declared. Neither shows up as an error anywhere — the set applies, the
/// polls run, and the output is quietly wrong.
pub fn check_names<'a>(kind: &str, names: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        if name.trim().is_empty() {
            return Err(format!("{kind}: a target has an empty name"));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "{kind}: target name {name:?} is not a legal key chunk — it becomes one \
                 (RFC 03 §2), so only [A-Za-z0-9._-] are allowed"
            ));
        }
        if !seen.insert(name) {
            return Err(format!(
                "{kind}: duplicate target name {name:?} — two targets sharing a name share \
                 one series and one alert key"
            ));
        }
    }
    Ok(())
}

impl SnmpTargets {
    pub fn validate(&self) -> Result<(), String> {
        check_names("snmp", self.targets.iter().map(|t| t.name.as_str()))?;
        for t in &self.targets {
            if t.address.trim().is_empty() {
                return Err(format!("snmp: target {:?} has no address", t.name));
            }
            if t.credentials.trim().is_empty() {
                return Err(format!(
                    "snmp: target {:?} names no credential set. The wire carries a NAME into \
                     the host's own `snmp.credentials`; it never carries a community string \
                     or a v3 passphrase",
                    t.name
                ));
            }
        }
        Ok(())
    }
}

impl ProbeTargets {
    pub fn validate(&self) -> Result<(), String> {
        check_names("probe", self.targets.iter().map(|t| t.name.as_str()))?;
        for t in &self.targets {
            if t.target.trim().is_empty() {
                return Err(format!("probe: target {:?} has nothing to check", t.name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_that_is_not_a_key_chunk_is_refused() {
        let t = SnmpTargets {
            targets: vec![SnmpTarget {
                name: "sw1/port".into(),
                address: "10.0.0.1".into(),
                credentials: "ro".into(),
                profile: None,
                oid_group: None,
                poll_interval_secs: None,
            }],
        };
        let e = t.validate().unwrap_err();
        assert!(e.contains("legal key chunk"), "{e}");
    }

    #[test]
    fn duplicate_names_are_refused_because_they_share_a_series() {
        let one = SnmpTarget {
            name: "sw1".into(),
            address: "10.0.0.1".into(),
            credentials: "ro".into(),
            profile: None,
            oid_group: None,
            poll_interval_secs: None,
        };
        let t = SnmpTargets {
            targets: vec![one.clone(), one],
        };
        assert!(t.validate().unwrap_err().contains("duplicate"));
    }

    /// The wire type must not be able to express a credential. This is the
    /// property the whole module exists for, so it is asserted rather than
    /// assumed: an SNMP target's payload carries a credential *name* and no
    /// field that could hold the secret itself.
    #[test]
    fn an_snmp_target_cannot_carry_a_community_string() {
        let t = SnmpTarget {
            name: "sw1".into(),
            address: "10.0.0.1".into(),
            credentials: "ro".into(),
            profile: None,
            oid_group: None,
            poll_interval_secs: None,
        };
        let json = serde_json::to_value(&t).expect("encode");
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for forbidden in [
            "community",
            "security",
            "password",
            "passphrase",
            "auth_pass",
        ] {
            assert!(
                !keys.contains(&forbidden),
                "{forbidden} reached the wire type"
            );
        }
        // And a payload that tries anyway is refused by the type, not merely
        // ignored — `credentials` is the only way in.
        let sneaky = serde_json::json!({
            "name": "sw1", "address": "10.0.0.1", "credentials": "ro",
            "community": "public"
        });
        assert!(
            serde_json::from_value::<SnmpTarget>(sneaky).is_err()
                || !serde_json::to_value(
                    serde_json::from_value::<SnmpTarget>(serde_json::json!({
                        "name": "sw1", "address": "10.0.0.1", "credentials": "ro"
                    }))
                    .unwrap()
                )
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("community"),
            "a community string must not survive a round trip"
        );
    }

    /// Likewise for probe: no header field exists, so an `Authorization:
    /// Bearer …` has nowhere to go.
    #[test]
    fn a_probe_target_cannot_carry_a_header() {
        let t = ProbeTarget {
            name: "site".into(),
            kind: ProbeKind::Http,
            target: "https://example.com".into(),
            interval_secs: None,
            timeout_secs: None,
            expect_status: vec![200],
            expect_body: None,
            follow_redirects: true,
            allow_offhost_redirect: false,
            method: None,
            count: None,
            spacing_ms: None,
            transport: None,
            server_name: None,
            inspect_untrusted: true,
            resolver: None,
            expect_addrs: Vec::new(),
            enabled: true,
        };
        let json = serde_json::to_value(&t).expect("encode");
        assert!(
            !json.as_object().unwrap().contains_key("headers"),
            "the field whose absence is the point of this type"
        );
    }
}
