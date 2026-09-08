//! Sensor-emitted alerts.
//!
//! Unlike the frontend's local threshold-rule alerts (which only evaluate while
//! the desktop app is open), an [`Alert`] is a *durable*, fully-formed decision
//! made by a sensor or the sentinel: the sensor already determined something is
//! wrong (a port scan, a missing listener, a downed interface) and publishes the
//! alert on the bus. This is the wire type for that channel.
//!
//! Alerts are LWW state, keyed by [`Alert::alert_key`], at
//! `<base>/v1/<origin>/state/<producer>/alert/<alert_key>` (RFC 04 §1.2):
//! - a `Put` with [`AlertState::Firing`] raises or updates an alert,
//! - a `Put` with [`AlertState::Resolved`] (then a Zenoh `Delete` tombstone)
//!   clears it.
//!
//! High-cardinality detail (offending IP, domain, JA4, expected/actual values)
//! belongs in [`Alert::labels`] / [`Alert::summary`], never in a metric series
//! name — keep the `alert_key` bucketed so a 1000-port scan is one alert.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::Protocol;
use crate::telemetry::current_timestamp_millis;

/// Severity of a sensor-emitted alert.
///
/// Plain (no `iced` dependency) so it lives in `zensight-common`; the frontend
/// maps it 1:1 onto its display `Severity`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    Hash,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AlertSeverity {
    Info,
    #[default]
    Warning,
    Critical,
}

impl AlertSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertSeverity::Info => "info",
            AlertSeverity::Warning => "warning",
            AlertSeverity::Critical => "critical",
        }
    }
}

impl std::fmt::Display for AlertSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// What produced the alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// Pillar A — a netring detector (port scan, beacon, DGA, ...).
    Anomaly,
    /// Pillar B — an expectation about machine state was violated.
    Expectation,
    /// The sensor's own health (e.g. capture drop-rate) crossed a threshold.
    SensorHealth,
}

impl AlertKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AlertKind::Anomaly => "anomaly",
            AlertKind::Expectation => "expectation",
            AlertKind::SensorHealth => "sensor_health",
        }
    }
}

/// Firing vs resolved. Drives auto-clear in the UI.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Hash, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AlertState {
    #[default]
    Firing,
    Resolved,
}

/// A fully-formed, sensor-decided alert. The wire type published as LWW
/// state on `zensight/v1/<origin>/state/<producer>/alert/<alert_key>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, schemars::JsonSchema)]
pub struct Alert {
    /// Unix epoch millis of the latest **state transition** — raised,
    /// escalated, resolved.
    ///
    /// It does **not** move while an alert simply keeps firing, and a content
    /// refresh (#1081) does not move it either. Three in-tree mechanisms read
    /// it that way and break if it drifts: an acknowledgement applies while
    /// `timestamp <= fired_at` ([`crate::ack::AlertAck::applies_to`]), so a
    /// moving timestamp would un-acknowledge every acked alert on every
    /// refresh; the historian derives a timeline row's uid from it, so a
    /// refresh corrects the existing row in place instead of appending an
    /// "alert fired" event every interval; and the correlator reads it as both
    /// an incident's start and its TTL clock.
    ///
    /// An **escalation** does move it, deliberately — a Warning that became
    /// Critical is a new transition, and it *should* un-acknowledge.
    pub timestamp: i64,
    /// When a still-firing alert's content was last re-observed (#1081).
    ///
    /// Present **only** on a content refresh: a republication of an alert that
    /// never left `Firing`, whose summary or host-scoped labels moved. Absent
    /// on a raise, an escalation and a resolve, where the transition and the
    /// observation are the same instant — so when it is present it is always
    /// `>= timestamp`.
    ///
    /// Absent reads as "not refreshed", never as "not observed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_ms: Option<i64>,
    /// Host / sensor identifier (the same value used as `source` in telemetry).
    pub source: String,
    /// Namespace the alert lives under (`netlink` for expectations, `netring`
    /// for anomalies). Also the `<protocol>` key segment.
    pub protocol: Protocol,
    pub kind: AlertKind,
    /// Stable rule identifier, e.g. "ssh-listening" or "PortScanDetector".
    pub rule: String,
    pub severity: AlertSeverity,
    #[serde(default)]
    pub state: AlertState,
    /// Human-readable one-liner for the alert row / toast.
    pub summary: String,
    /// Structured context (ip, port, peer, sni, expected, actual, ...).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
}

impl Alert {
    /// Create a new firing alert with the current timestamp.
    pub fn new(
        source: impl Into<String>,
        protocol: Protocol,
        kind: AlertKind,
        rule: impl Into<String>,
        severity: AlertSeverity,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: current_timestamp_millis(),
            observed_at_ms: None,
            source: source.into(),
            protocol,
            kind,
            rule: rule.into(),
            severity,
            state: AlertState::Firing,
            summary: summary.into(),
            labels: HashMap::new(),
        }
    }

    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    pub fn with_labels(mut self, labels: HashMap<String, String>) -> Self {
        self.labels.extend(labels);
        self
    }

    /// Mark this alert resolved (state transition; timestamp refreshed).
    pub fn resolved(mut self) -> Self {
        self.state = AlertState::Resolved;
        self.timestamp = current_timestamp_millis();
        // A resolve IS the observation, so there is no second clock to carry —
        // and an inherited `observed_at_ms` would be *older* than the timestamp
        // beside it, breaking the field's one invariant (#1081).
        self.observed_at_ms = None;
        self
    }

    pub fn is_firing(&self) -> bool {
        self.state == AlertState::Firing
    }

    /// Stable identity for this alert: 16 lowercase hex, the **normative**
    /// RFC 11 §3.1 derivation, computed by [`zenkey::alert::alert_key`].
    ///
    /// ```text
    /// input     = rule ++ ( "\n" ++ name ++ "=" ++ value )*   ascending by
    ///                                                         name, byte order
    /// alert_key = lowercase_hex(fnv1a_64(utf8(input)))        16 chars
    /// ```
    ///
    /// The origin never enters the input — origin and producer are already in
    /// the key (`…/state/<producer>/alert/<alert_key>`), which is exactly what
    /// makes the same alert on two hosts the same key under two origins. Two
    /// alerts describing the same condition on the same host share a key, so a
    /// `Put` replaces the prior state in place and a later `Resolved`/`Delete`
    /// clears precisely that alert. The key is stable under label reordering
    /// (labels sort by name before hashing) and carries no rule-name prefix
    /// (rule names are CamelCase; chunks are lowercase-only, RFC 03 §2).
    ///
    /// **`host.*` is ZenSight's host-scoped label vocabulary** and is excluded
    /// before hashing, alongside the RFC's own `host`. That is not a deviation:
    /// RFC 11 §3.1 excludes "the label named `host`, *and any label the
    /// producer documents as host-scoped*", precisely because only the producer
    /// knows its vocabulary. `host.id`, `host.boot_id` and friends are identity
    /// *annotations* stamped onto an alert for correlation, not part of what
    /// the alert is about — and keying on them would orphan a firing alert
    /// every time the identity envelope refreshed: the `Firing` would sit on
    /// the old key forever while the `Resolved` landed on a new one. See
    /// [`HOST_SCOPED_PREFIX`] and the round-trip test in
    /// `zensight-sensor-core/tests/alert_reporter.rs` (#738).
    pub fn alert_key(&self) -> String {
        let discriminating: Vec<(&str, &str)> = self
            .labels
            .iter()
            .filter(|(k, _)| !is_host_scoped(k))
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        match zenkey::alert::alert_key(&self.rule, &discriminating) {
            Ok(key) => key,
            Err(e) => {
                // RFC 11 §3.1 refuses these inputs because the framing would
                // stop being injective: a `\n` in a rule forges a label, an
                // `=` in a name forges a value. `alert_key` is infallible at
                // all ~35 of its call sites, so returning a `Result` here
                // would churn the tree for an input no ZenSight rule produces.
                //
                // Instead the offending bytes become `_` and the normative
                // derivation runs on *that* — deterministic, so a `Firing` and
                // its `Resolved` still agree, which is the one property a key
                // must never lose. The WARN is how an operator learns the rule
                // name needs fixing.
                tracing::warn!(
                    rule = %self.rule,
                    error = %e,
                    "alert rule/labels violate RFC 11 §3.1 framing; keying on a sanitized \
                     rendering — fix the rule name"
                );
                let rule = sanitize_value(&self.rule);
                let rule = if rule.is_empty() {
                    "unnamed".to_string()
                } else {
                    rule
                };
                let pairs: Vec<(String, String)> = discriminating
                    .iter()
                    .map(|(k, v)| {
                        let name = sanitize_name(k);
                        (
                            if name.is_empty() {
                                "_".to_string()
                            } else {
                                name
                            },
                            sanitize_value(v),
                        )
                    })
                    .collect();
                let borrowed: Vec<(&str, &str)> = pairs
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                zenkey::alert::alert_key(&rule, &borrowed)
                    .expect("a sanitized rule and labels satisfy RFC 11 §3.1's framing rules")
            }
        }
    }
}

/// The prefix of ZenSight's **host-scoped label vocabulary** — the identity
/// annotations (`host.id`, `host.boot_id`, …) a producer stamps onto an alert
/// for correlation.
///
/// RFC 11 §3.1 excludes the label named `host` itself and leaves the rest of
/// the host-scoped vocabulary to the producer to declare, because only the
/// producer knows it. This constant is ZenSight's declaration. Excluding these
/// is what keeps a firing alert's key stable across an identity refresh (#738).
pub const HOST_SCOPED_PREFIX: &str = "host.";

/// Whether `name` is a host-scoped label (RFC 11 §3.1) under ZenSight's
/// vocabulary: the RFC's own `host`, or anything in the [`HOST_SCOPED_PREFIX`]
/// annotation namespace.
///
/// `zenkey::alert::alert_key` drops the bare `host` itself; naming it here too
/// keeps the whole rule readable in one place, and the double exclusion is
/// harmless.
#[must_use]
pub fn is_host_scoped(name: &str) -> bool {
    name == "host" || name.starts_with(HOST_SCOPED_PREFIX)
}

/// RFC 11 §3.1 forbids `\n` in a label value and in a rule name.
fn sanitize_value(s: &str) -> String {
    s.replace('\n', "_")
}

/// RFC 11 §3.1 forbids `\n` and `=` in a label name.
fn sanitize_name(s: &str) -> String {
    s.replace(['\n', '='], "_")
}

/// A firing alert's identity, as **one key chunk** (#922).
///
/// `"<origin>.<producer>.<alert_key>"` — `h-3fa9c2d41b7e.netlink.a1b2c3d4e5f60718`.
/// It names the document at
/// `v1/<origin>/state/<producer>/alert/<alert_key>` without being that key,
/// which is the point: it has to fit in the **last chunk** of
/// `@catalog/state/ack/{alert_ref}`, and a key cannot nest inside a key.
///
/// # Why a readable triple rather than a hash
///
/// A hash would be shorter and equally unique. It would also be opaque in
/// `zenctl topic` output, in a storage listing, and in whatever an on-call
/// tool renders — and the operator reading it is exactly the person who needs
/// to know *which host's netlink sensor* is being acknowledged. There is no
/// collision benefit either: the triple is already the alert's full identity.
///
/// # Why `.`
///
/// It is the one separator that is legal inside a chunk and already appears
/// there (`if/eth0/in_errors.rate`), so no component needs escaping and the
/// key grammar is untouched. `/` would make three chunks, `:` and `@` are
/// reserved elsewhere in the grammar.
///
/// The `alert_key` component may itself contain dots, so parsing splits on the
/// **first two** separators and keeps the rest — `splitn(3, '.')`. Origin and
/// producer cannot contain a dot (both are grammar chunks with a narrower
/// alphabet), so that is unambiguous rather than merely conventional.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AlertRef {
    /// The origin that publishes the alert (`h-<12hex>`).
    pub origin: String,
    /// The producer whose slice the alert belongs to (`netlink`, `sysinfo`).
    pub producer: String,
    /// The RFC 11 §3.1 alert key.
    pub alert_key: String,
}

impl AlertRef {
    /// Build a ref. The components are **not** validated here — see
    /// [`AlertRef::parse`], which is where a ref coming off the wire is
    /// checked.
    pub fn new(
        origin: impl Into<String>,
        producer: impl Into<String>,
        alert_key: impl Into<String>,
    ) -> Self {
        AlertRef {
            origin: origin.into(),
            producer: producer.into(),
            alert_key: alert_key.into(),
        }
    }

    /// Parse `"<origin>.<producer>.<alert_key>"`.
    ///
    /// Refuses anything that would not round-trip or would not be legal as a
    /// key chunk: fewer than three components, an empty component, or a
    /// character outside the chunk alphabet. A malformed ref must not become
    /// an `ack/` key — the write would either fail at the router or, worse,
    /// succeed on a key nothing can address back.
    pub fn parse(s: &str) -> Result<Self, AlertRefError> {
        let mut parts = s.splitn(3, '.');
        let (Some(origin), Some(producer), Some(alert_key)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(AlertRefError::Shape);
        };
        if origin.is_empty() || producer.is_empty() || alert_key.is_empty() {
            return Err(AlertRefError::Empty);
        }
        // The chunk alphabet, minus `.` which is our separator and already
        // handled by the split. Anything else would need escaping to survive
        // a key, and a ref that needs escaping is a ref that will be wrong
        // somewhere.
        let legal = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        for (name, part) in [("origin", origin), ("producer", producer)] {
            if part.contains('.') || !part.chars().all(legal) {
                return Err(AlertRefError::Illegal {
                    field: name,
                    value: part.to_string(),
                });
            }
        }
        if !alert_key.chars().all(legal) {
            return Err(AlertRefError::Illegal {
                field: "alert_key",
                value: alert_key.to_string(),
            });
        }
        Ok(AlertRef::new(origin, producer, alert_key))
    }
}

/// Why an [`AlertRef`] was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertRefError {
    /// Fewer than three `.`-separated components.
    Shape,
    /// A component was empty.
    Empty,
    /// A component carried a character that cannot appear in a key chunk.
    Illegal { field: &'static str, value: String },
}

impl std::fmt::Display for AlertRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlertRefError::Shape => write!(
                f,
                "an alert ref is <origin>.<producer>.<alert_key> — three dot-separated parts"
            ),
            AlertRefError::Empty => write!(f, "an alert ref has no empty component"),
            AlertRefError::Illegal { field, value } => write!(
                f,
                "{field} {value:?} is not legal in a key chunk (letters, digits, - and _)"
            ),
        }
    }
}

impl std::error::Error for AlertRefError {}

impl std::fmt::Display for AlertRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.origin, self.producer, self.alert_key)
    }
}

impl std::str::FromStr for AlertRef {
    type Err = AlertRefError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        AlertRef::parse(s)
    }
}

impl TryFrom<String> for AlertRef {
    type Error = AlertRefError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        AlertRef::parse(&s)
    }
}

impl From<AlertRef> for String {
    fn from(r: AlertRef) -> String {
        r.to_string()
    }
}

// A `@catalog` document is state-class, so #815 wants a real schema for it.
// The wire form is the string, not the struct, which is what `schema_for`
// must be told.
impl schemars::JsonSchema for AlertRef {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AlertRef".into()
    }
    fn json_schema(g: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = String::json_schema(g);
        schema.insert(
            "description".into(),
            "<origin>.<producer>.<alert_key> — one slug-safe key chunk naming a firing alert"
                .into(),
        );
        schema.insert(
            "pattern".into(),
            r"^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\..+$".into(),
        );
        schema
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AlertRef` round-trips through its string form — which is the form on
    /// the wire, in the key chunk, and in the JSON.
    #[test]
    fn an_alert_ref_round_trips() {
        let r = AlertRef::new("h-3fa9c2d41b7e", "netlink", "a1b2c3d4e5f60718");
        assert_eq!(r.to_string(), "h-3fa9c2d41b7e.netlink.a1b2c3d4e5f60718");
        assert_eq!(AlertRef::parse(&r.to_string()).unwrap(), r);
        // Serde uses the string, not the struct: the document field and the
        // key chunk are then the same bytes, which is what makes an ack
        // findable from the alert and vice versa.
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, "\"h-3fa9c2d41b7e.netlink.a1b2c3d4e5f60718\"");
        assert_eq!(serde_json::from_str::<AlertRef>(&json).unwrap(), r);
    }

    /// An `alert_key` may itself contain dots (`if/eth0/in_errors.rate` is a
    /// legal metric, and a rule slug can carry one), so the split takes the
    /// FIRST two separators and keeps the rest. Origin and producer cannot
    /// contain a dot, which is what makes that unambiguous.
    #[test]
    fn the_alert_key_may_contain_dots() {
        let r = AlertRef::parse("h-3fa9c2d41b7e.netlink.threshold.rx.errors").unwrap();
        assert_eq!(r.origin, "h-3fa9c2d41b7e");
        assert_eq!(r.producer, "netlink");
        assert_eq!(r.alert_key, "threshold.rx.errors");
        assert_eq!(r.to_string(), "h-3fa9c2d41b7e.netlink.threshold.rx.errors");
    }

    /// A malformed ref must not become an `ack/` key: the write would either
    /// fail at the router or, worse, land on a key nothing can address back.
    #[test]
    fn a_malformed_ref_is_refused() {
        for bad in [
            "h-3fa9c2d41b7e",             // one component
            "h-3fa9c2d41b7e.netlink",     // two
            ".netlink.abc",               // empty origin
            "h-3fa9.netlink.",            // empty key
            "h-3fa9/x.netlink.abc",       // a chunk separator
            "h-3fa9c2d41b7e.net*ink.abc", // a wildcard
            "h-3fa9c2d41b7e.netlink.a b", // a space
        ] {
            assert!(
                AlertRef::parse(bad).is_err(),
                "{bad:?} should not parse into an alert ref"
            );
        }
    }

    /// The type is the one that goes in a key chunk, so its `Display` must
    /// never produce something a key cannot hold.
    #[test]
    fn a_parsed_ref_is_always_a_legal_chunk() {
        let r = AlertRef::parse("h-3fa9c2d41b7e.netlink.a1b2c3d4").unwrap();
        let s = r.to_string();
        assert!(!s.contains('/'), "{s}");
        assert!(!s.contains('*'), "{s}");
        assert!(!s.contains('?'), "{s}");
        assert!(!s.contains('#'), "{s}");
    }

    /// **The RFC 11 §3.1 test vector**, which every implementation MUST
    /// reproduce: rule `link_down`, labels `{peer: r2, port: eth0, host: …}`.
    /// `host` is host-scoped and drops out, `peer` sorts before `port`, and
    /// the hashed input is the 27 bytes `link_down\npeer=r2\nport=eth0`.
    ///
    /// This is the pin that says ZenSight is on the normative derivation
    /// rather than merely on *a* stable one. Before #736 this same alert
    /// keyed as `c25da085d5c5b7e7`: same hash function, same 16-hex width,
    /// two differences — the framing put `\0` *after* the rule and after each
    /// pair rather than `\n` *before* each pair, and the exclusion matched
    /// only the `host.` prefix, so the RFC's own bare `host` label was hashed
    /// in. That is why adopting this re-keys every firing alert (#737).
    #[test]
    fn the_rfc_11_3_1_test_vector() {
        let a = Alert::new(
            "h",
            Protocol::Netlink,
            AlertKind::Expectation,
            "link_down",
            AlertSeverity::Critical,
            "link down",
        )
        .with_label("port", "eth0")
        .with_label("host", "h-3fa9c2d41b7e")
        .with_label("peer", "r2");
        assert_eq!(a.alert_key(), "a659f813308ad1da");
        // And it is the zenkey function's own answer, not a copy of it.
        assert_eq!(
            a.alert_key(),
            zenkey::alert::alert_key("link_down", &[("peer", "r2"), ("port", "eth0")]).unwrap()
        );
    }

    /// A rule with no discriminating labels hashes the bare rule name — the
    /// degenerate case of the framing, and the one an off-by-one separator
    /// would get wrong.
    #[test]
    fn a_rule_with_no_labels_hashes_the_bare_rule_name() {
        let a = Alert::new(
            "h",
            Protocol::Netlink,
            AlertKind::Expectation,
            "link_down",
            AlertSeverity::Critical,
            "link down",
        );
        assert_eq!(
            a.alert_key(),
            zenkey::alert::alert_key("link_down", &[]).unwrap()
        );
    }

    /// The `host.*` annotation namespace is ZenSight's declared host-scoped
    /// vocabulary under RFC 11 §3.1 ("any label the producer documents as
    /// host-scoped"), so a stamped alert hashes exactly like an unstamped one.
    #[test]
    fn host_scoped_labels_are_excluded_from_the_normative_input() {
        assert!(is_host_scoped("host"));
        assert!(is_host_scoped("host.id"));
        assert!(is_host_scoped("host.boot_id"));
        assert!(!is_host_scoped("hostname"));
        assert!(!is_host_scoped("port"));

        let a = Alert::new(
            "h",
            Protocol::Netlink,
            AlertKind::Expectation,
            "link_down",
            AlertSeverity::Critical,
            "link down",
        )
        .with_label("port", "eth0")
        .with_label("host.id", "h-3fa9c2d41b7e")
        .with_label("host.boot_id", "bbbb");
        assert_eq!(
            a.alert_key(),
            zenkey::alert::alert_key("link_down", &[("port", "eth0")]).unwrap()
        );
    }

    /// A rule name that RFC 11 §3.1 refuses (a `\n` forges a label) still
    /// yields a key, deterministically — the wrapper is infallible because
    /// ~35 call sites are, and a `Firing` and its `Resolved` must agree even
    /// for a malformed rule.
    #[test]
    fn a_framing_violating_rule_still_keys_deterministically() {
        let bad = |rule: &str| {
            Alert::new(
                "h",
                Protocol::Netlink,
                AlertKind::Expectation,
                rule,
                AlertSeverity::Warning,
                "s",
            )
            .with_label("port", "22")
        };
        let a = bad("link\ndown");
        assert_eq!(
            a.alert_key(),
            bad("link\ndown").alert_key(),
            "deterministic"
        );
        assert_eq!(a.alert_key().len(), 16);
        assert!(a.alert_key().chars().all(|c| c.is_ascii_hexdigit()));
        // The sanitized rendering is what was hashed.
        assert_eq!(
            a.alert_key(),
            zenkey::alert::alert_key("link_down", &[("port", "22")]).unwrap()
        );
        // An empty rule is the other refusal, and it also survives.
        let empty = Alert::new(
            "h",
            Protocol::Netlink,
            AlertKind::Expectation,
            "",
            AlertSeverity::Warning,
            "s",
        );
        assert_eq!(empty.alert_key().len(), 16);
    }

    #[test]
    fn alert_key_stable_under_label_reordering() {
        let a = Alert::new(
            "host1",
            Protocol::Netlink,
            AlertKind::Expectation,
            "ssh-listening",
            AlertSeverity::Critical,
            "sshd not listening",
        )
        .with_label("port", "22")
        .with_label("expected", "listen");
        let b = Alert::new(
            "host1",
            Protocol::Netlink,
            AlertKind::Expectation,
            "ssh-listening",
            AlertSeverity::Critical,
            "sshd not listening",
        )
        .with_label("expected", "listen")
        .with_label("port", "22");
        assert_eq!(a.alert_key(), b.alert_key());
    }

    #[test]
    fn alert_key_differs_by_rule_and_labels() {
        let base = Alert::new(
            "h",
            Protocol::Netring,
            AlertKind::Anomaly,
            "port_scan",
            AlertSeverity::Warning,
            "scan",
        );
        let with_src = base.clone().with_label("src", "10.0.0.5");
        let with_other = base.clone().with_label("src", "10.0.0.6");
        assert_ne!(base.alert_key(), with_src.alert_key());
        assert_ne!(with_src.alert_key(), with_other.alert_key());
    }

    #[test]
    fn alert_key_ignores_host_annotation_labels() {
        let base = Alert::new(
            "h",
            Protocol::Netring,
            AlertKind::Anomaly,
            "port_scan",
            AlertSeverity::Warning,
            "scan",
        )
        .with_label("src", "10.0.0.5");
        let stamped = base
            .clone()
            .with_label("host.id", "ab".repeat(32))
            .with_label("host.boot_id", "bbbb");
        // Annotation labels never change alert identity: a pre-stamp Firing and
        // a post-stamp Resolved must land on the same key.
        assert_eq!(base.alert_key(), stamped.alert_key());
        // ...but a non-annotation label still does.
        let other = base.clone().with_label("dst", "10.0.0.9");
        assert_ne!(base.alert_key(), other.alert_key());
    }

    #[test]
    fn resolved_transition() {
        let a = Alert::new(
            "h",
            Protocol::Netlink,
            AlertKind::Expectation,
            "r",
            AlertSeverity::Info,
            "s",
        );
        assert!(a.is_firing());
        let r = a.resolved();
        assert_eq!(r.state, AlertState::Resolved);
        assert!(!r.is_firing());
    }

    #[test]
    fn serde_roundtrip_json_and_cbor() {
        let a = Alert::new(
            "host1",
            Protocol::Netring,
            AlertKind::Anomaly,
            "PortScanDetector",
            AlertSeverity::Critical,
            "Port scan from 10.0.0.5 (37 ports)",
        )
        .with_label("src", "10.0.0.5");
        let json = crate::encode(&a, crate::Format::Json).unwrap();
        let back: Alert = crate::decode(&json, crate::Format::Json).unwrap();
        assert_eq!(a, back);
        let cbor = crate::encode(&a, crate::Format::Cbor).unwrap();
        let back2: Alert = crate::decode(&cbor, crate::Format::Cbor).unwrap();
        assert_eq!(a, back2);
    }

    #[test]
    fn alert_key_is_key_expr_safe() {
        let a = Alert::new(
            "h",
            Protocol::Netlink,
            AlertKind::Expectation,
            "socket:sshd/22",
            AlertSeverity::Warning,
            "s",
        );
        let key = a.alert_key();
        assert!(!key.contains('/'));
        assert!(!key.contains('*'));
    }
}
