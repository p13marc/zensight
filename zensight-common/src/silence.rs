//! Suppressing alerts by matcher, for a window, with an author (#922,
//! epic #900).
//!
//! # Silence is not acknowledgement
//!
//! An [`crate::ack::AlertAck`] says *"I have seen this one occurrence and I am on
//! it"*, and a re-fire pages again. A `Silence` says *"do not tell me about
//! anything matching this until the maintenance window closes"*, and it holds
//! across re-fires — because that is what a maintenance window is for. Keeping
//! them as two documents rather than one flag is what lets an operator answer
//! "why did nobody hear about this?" with one of two different sentences.
//!
//! # Why matchers rather than a source list
//!
//! The GUI had `silenced_sources: HashMap<String, i64>` — whole-source only.
//! That is enough to mute a host being rebuilt and useless for "the disk
//! alerts on every host in rack 3 while the SAN is down", which is the shape a
//! real maintenance window has. A matcher set over
//! origin / producer / source / rule / `labels.*` covers both, and covers them
//! with the same vocabulary a `ThresholdRule` already matches on (#928), so an
//! operator learns it once.
//!
//! # What this deliberately is not
//!
//! Notification routing, escalation, repeat intervals, on-call rotations.
//! zenkey's zenwatch (#387-#390) scoped those out on purpose — *"if a
//! deployment needs those it needs an on-call product, and webhook is how it
//! gets there."* This module makes the document such a tool would read.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::alert::Alert;

/// How a matcher compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchOp {
    /// Exact string equality.
    #[default]
    Eq,
    /// An unanchored regular expression.
    ///
    /// Unanchored on purpose: `web` matching `web01` is what an operator
    /// silencing a rack expects, and anchoring is one `^…$` away for anyone
    /// who wants it. The opposite default surprises people into silencing
    /// nothing.
    Regex,
}

/// One condition an alert must satisfy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Matcher {
    /// What to look at: `origin`, `producer`, `source`, `rule`, or
    /// `labels.<name>` for any label the alert carries.
    pub name: String,
    #[serde(default)]
    pub op: MatchOp,
    pub value: String,
}

/// A suppression window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Silence {
    /// ULID. Also the key chunk: `@catalog/state/silence/{id}`.
    pub id: String,
    /// **All** matchers must match. An empty set matches nothing, not
    /// everything — see [`Silence::matches`].
    pub matchers: Vec<Matcher>,
    /// Epoch millis. A silence that has not started yet suppresses nothing.
    pub starts_at: i64,
    /// Epoch millis. The catalog tombstones the document here; a consumer
    /// that has not seen the tombstone yet still stops applying it, so a
    /// stale silence cannot outlive its window on a partitioned reader.
    pub ends_at: i64,
    /// Who created it — the `?actor=` of the `@rpc/@catalog/silence` call.
    pub by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

impl Silence {
    /// Whether this silence suppresses `alert` at `now` (epoch millis).
    ///
    /// `origin` and `producer` are the alert's **bus coordinates**, which the
    /// document itself does not carry: an `Alert`'s `source` is the polled
    /// device for a proxy sensor, not the host that published it (#883). The
    /// caller read them off the key and passes them in, which is also what
    /// keeps this pure.
    #[must_use]
    pub fn matches(&self, now: i64, origin: &str, producer: &str, alert: &Alert) -> bool {
        // An empty matcher set matches NOTHING. The other reading — vacuous
        // truth, "all zero conditions hold" — is how a fat-fingered silence
        // mutes an entire fleet, and the harm is asymmetric: refusing to
        // suppress costs a page, suppressing everything costs an outage
        // nobody hears about.
        if self.matchers.is_empty() {
            return false;
        }
        if now < self.starts_at || now >= self.ends_at {
            return false;
        }
        self.matchers
            .iter()
            .all(|m| Self::field(m, origin, producer, alert).is_some_and(|v| matches_op(m, v)))
    }

    /// The value a matcher looks at, or `None` when the alert has no such
    /// field — which fails the matcher rather than matching an empty string.
    fn field<'a>(
        m: &Matcher,
        origin: &'a str,
        producer: &'a str,
        alert: &'a Alert,
    ) -> Option<&'a str> {
        match m.name.as_str() {
            "origin" => Some(origin),
            "producer" => Some(producer),
            "source" => Some(alert.source.as_str()),
            "rule" => Some(alert.rule.as_str()),
            other => other
                .strip_prefix("labels.")
                .and_then(|l| alert.labels.get(l))
                .map(String::as_str),
        }
    }

    /// Whether any silence in `silences` suppresses `alert`.
    #[must_use]
    pub fn any_matches(
        silences: &[Silence],
        now: i64,
        origin: &str,
        producer: &str,
        alert: &Alert,
    ) -> bool {
        silences
            .iter()
            .any(|s| s.matches(now, origin, producer, alert))
    }
}

fn matches_op(m: &Matcher, value: &str) -> bool {
    match m.op {
        MatchOp::Eq => m.value == value,
        // A silence whose regex does not compile suppresses NOTHING. The
        // alternative — treating it as a match — turns one typo into a
        // fleet-wide mute, and `@rpc/@catalog/silence` refuses a bad pattern
        // at write time anyway, so reaching here means a document that
        // predates the check or arrived from somewhere else.
        MatchOp::Regex => regex::Regex::new(&m.value).is_ok_and(|r| r.is_match(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AlertKind, AlertSeverity, Protocol};

    fn alert() -> Alert {
        Alert::new(
            "web01",
            Protocol::Netlink,
            AlertKind::Expectation,
            "socket:sshd",
            AlertSeverity::Critical,
            "sshd is not listening",
        )
        .with_label("iface", "eth0")
    }

    fn silence(matchers: Vec<Matcher>) -> Silence {
        Silence {
            id: "01J000000000000000000000".into(),
            matchers,
            starts_at: 1_000,
            ends_at: 2_000,
            by: "marc".into(),
            note: String::new(),
        }
    }

    fn eq(name: &str, value: &str) -> Matcher {
        Matcher {
            name: name.into(),
            op: MatchOp::Eq,
            value: value.into(),
        }
    }

    #[test]
    fn every_field_is_matchable() {
        let a = alert();
        for (name, value) in [
            ("origin", "h-3fa9c2d41b7e"),
            ("producer", "netlink"),
            ("source", "web01"),
            ("rule", "socket:sshd"),
            ("labels.iface", "eth0"),
        ] {
            let s = silence(vec![eq(name, value)]);
            assert!(
                s.matches(1_500, "h-3fa9c2d41b7e", "netlink", &a),
                "{name} should match {value}"
            );
        }
    }

    /// `source` is the polled device for a proxy sensor and the host for a
    /// host sensor (#883), so it is NOT the origin — silencing "everything
    /// this host publishes" and "everything about this device" are different
    /// requests, and both must be expressible.
    #[test]
    fn origin_and_source_are_different_questions() {
        let a = alert();
        let by_origin = silence(vec![eq("origin", "h-3fa9c2d41b7e")]);
        let by_source = silence(vec![eq("source", "web01")]);
        assert!(by_origin.matches(1_500, "h-3fa9c2d41b7e", "netlink", &a));
        assert!(by_source.matches(1_500, "h-3fa9c2d41b7e", "netlink", &a));
        // A proxy: the polling host is not the device, so a silence naming the
        // ORIGIN mutes everything it polls, and one naming that origin as a
        // SOURCE mutes nothing — which is the distinction #883 made real.
        assert!(by_origin.matches(1_500, "h-3fa9c2d41b7e", "snmp", &a));
        assert!(!silence(vec![eq("source", "h-3fa9c2d41b7e")]).matches(
            1_500,
            "h-3fa9c2d41b7e",
            "snmp",
            &a
        ));
    }

    #[test]
    fn all_matchers_must_match() {
        let a = alert();
        let both = silence(vec![eq("producer", "netlink"), eq("source", "web01")]);
        assert!(both.matches(1_500, "h-1", "netlink", &a));
        let one_wrong = silence(vec![eq("producer", "netlink"), eq("source", "db01")]);
        assert!(!one_wrong.matches(1_500, "h-1", "netlink", &a));
    }

    /// The asymmetric-harm case: an empty matcher set is not vacuous truth.
    #[test]
    fn an_empty_matcher_set_suppresses_nothing() {
        assert!(!silence(Vec::new()).matches(1_500, "h-1", "netlink", &alert()));
    }

    #[test]
    fn the_window_bounds_it_at_both_ends() {
        let s = silence(vec![eq("source", "web01")]);
        let a = alert();
        assert!(!s.matches(999, "h-1", "netlink", &a), "before starts_at");
        assert!(
            s.matches(1_000, "h-1", "netlink", &a),
            "starts_at is inside"
        );
        assert!(
            !s.matches(2_000, "h-1", "netlink", &a),
            "ends_at is exclusive — the window is closed at the instant it ends"
        );
    }

    #[test]
    fn a_regex_matcher_is_unanchored() {
        let a = alert();
        let s = silence(vec![Matcher {
            name: "source".into(),
            op: MatchOp::Regex,
            value: "web".into(),
        }]);
        assert!(s.matches(1_500, "h-1", "netlink", &a), "web matches web01");
        let anchored = silence(vec![Matcher {
            name: "source".into(),
            op: MatchOp::Regex,
            value: "^web$".into(),
        }]);
        assert!(!anchored.matches(1_500, "h-1", "netlink", &a));
    }

    /// One typo must not mute a fleet.
    #[test]
    fn a_regex_that_does_not_compile_suppresses_nothing() {
        let s = silence(vec![Matcher {
            name: "source".into(),
            op: MatchOp::Regex,
            value: "web(".into(),
        }]);
        assert!(!s.matches(1_500, "h-1", "netlink", &alert()));
    }

    /// A matcher on a label the alert does not carry fails, rather than
    /// matching an absent value against an empty string.
    #[test]
    fn an_absent_label_does_not_match() {
        let s = silence(vec![eq("labels.unit", "")]);
        assert!(!s.matches(1_500, "h-1", "netlink", &alert()));
    }
}
