//! Acknowledging a firing alert, as a document on the bus (#922, epic #900).
//!
//! # Why this is not a `HashSet`
//!
//! It was. `AlertsState::acknowledged_external: HashSet<String>` lived in one
//! GUI process, keyed `<source>/<alert_key>`; closing the window lost it, a
//! second GUI never saw it, and the Prometheus and OTel exporters — which
//! mirror every alert — could not tell an acknowledged one from a new one.
//! "Someone is on this" is exactly the fact a second operator and an on-call
//! tool most need, and it was the one fact ZenSight kept only for itself.
//!
//! # The projection rule
//!
//! **An ack applies only while a firing alert with `timestamp <= fired_at`
//! exists.** Every consumer applies it; [`AlertAck::applies_to`] is the one
//! implementation.
//!
//! Two things fall out of it, and both are the point:
//!
//! - An **orphan is inert.** If the catalog dies holding acks and an alert
//!   resolves, the ack outlives the alert — and reads as nothing, rather than
//!   as a silent suppression of the next occurrence. A stale document must
//!   never be able to hide a live problem.
//! - A **re-fire is not acknowledged.** `fired_at` pins the ack to the
//!   occurrence someone actually looked at. When the condition clears and
//!   returns, the new alert's `timestamp` is later, the ack stops applying,
//!   and it pages again — which is what an operator means by "acknowledged",
//!   as opposed to "silenced" (see [`crate::silence`], which is the other
//!   thing and says so).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::alert::{Alert, AlertRef};

/// One operator's acknowledgement of one firing alert.
///
/// Named `AlertAck` rather than `Ack` because the registry already has an
/// `Ack`: the generic "the write landed" reply that twenty-odd write
/// procedures across every producer's slice declare. Two different things
/// under one type name is exactly the drift RFC 08 §5's table exists to
/// prevent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AlertAck {
    /// Which alert. Also the key chunk: `@catalog/state/ack/{alert_ref}`.
    pub alert_ref: AlertRef,
    /// The `timestamp` of the alert occurrence being acknowledged.
    ///
    /// Not decoration: it is what makes a re-fire page again. See the module
    /// doc's projection rule.
    pub fired_at: i64,
    /// Who acknowledged it — the `?actor=` of the `@rpc/@catalog/ack` call.
    ///
    /// Recorded rather than derived, and never guessed: an ack whose author is
    /// unknown is worth less than no ack, because the next operator's first
    /// question is who to ask.
    pub by: String,
    /// Free text. Empty is normal — the useful ones say "restarting it" or
    /// name a ticket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// When the acknowledgement was made (epoch millis).
    pub at: i64,
}

impl AlertAck {
    /// Whether this ack applies to `alert` **right now** (the projection rule).
    ///
    /// `alert` must be the currently-firing document for `alert_ref`; the
    /// caller has already looked it up, and passing it in is what keeps this
    /// pure. A resolved alert is not firing, so the caller passes `None`.
    #[must_use]
    pub fn applies_to(&self, alert: Option<&Alert>) -> bool {
        alert.is_some_and(|a| a.timestamp <= self.fired_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AlertKind, AlertSeverity, Protocol};

    fn ack(fired_at: i64) -> AlertAck {
        AlertAck {
            alert_ref: AlertRef::new("h-3fa9c2d41b7e", "netlink", "a1b2c3d4"),
            fired_at,
            by: "marc".into(),
            note: String::new(),
            at: fired_at + 1_000,
        }
    }

    fn alert(timestamp: i64) -> Alert {
        let mut a = Alert::new(
            "web01",
            Protocol::Netlink,
            AlertKind::Expectation,
            "socket:sshd",
            AlertSeverity::Critical,
            "sshd is not listening",
        );
        a.timestamp = timestamp;
        a
    }

    /// The occurrence someone looked at is acknowledged.
    #[test]
    fn an_ack_applies_to_the_occurrence_it_names() {
        assert!(ack(1_000).applies_to(Some(&alert(1_000))));
        // An alert that has been firing since *before* the ack is the same
        // occurrence — the timestamp is the last transition, not a heartbeat.
        assert!(ack(1_000).applies_to(Some(&alert(900))));
    }

    /// **A content refresh does not un-acknowledge** (#1081).
    ///
    /// This is the mechanism that decides `timestamp`'s meaning. An ack applies
    /// while `timestamp <= fired_at`, so if a still-firing alert took a fresh
    /// timestamp every time its summary was corrected, every acked alert on the
    /// fleet would un-acknowledge itself every refresh interval, forever. A
    /// refresh therefore carries the transition timestamp over and puts the new
    /// reading in `observed_at_ms` — and an escalation, which is a real
    /// transition, still un-acks, as the test below requires.
    #[test]
    fn a_content_refresh_does_not_un_acknowledge() {
        let mut refreshed = alert(1_000);
        refreshed.summary = "sshd is not listening (checked again)".into();
        refreshed.observed_at_ms = Some(9_999);
        assert!(
            ack(1_000).applies_to(Some(&refreshed)),
            "a corrected summary is the same occurrence, not a new one"
        );
    }

    /// **A re-fire pages again.** This is the difference between an ack and a
    /// silence, and the reason `fired_at` exists at all.
    #[test]
    fn a_later_occurrence_is_not_acknowledged() {
        assert!(
            !ack(1_000).applies_to(Some(&alert(1_001))),
            "an alert that cleared and came back is a new problem"
        );
    }

    /// **An orphan is inert.** A stale ack from a dead catalog must not be
    /// able to hide the next firing alert; with nothing firing it reads as
    /// nothing.
    #[test]
    fn an_ack_without_a_firing_alert_applies_to_nothing() {
        assert!(!ack(1_000).applies_to(None));
    }
}
