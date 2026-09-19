//! Traces signal: spans synthesized from alert lifecycles.
//!
//! ZenSight sensors do not propagate trace context — there is no W3C
//! `traceparent` flowing through the bus. Instead, this module *synthesizes*
//! spans from request/response-shaped events the exporter already observes:
//! the alert lifecycle on `state/*/alert/*`. Each firing → resolved transition
//! becomes exactly one span named `alert:<rule>` whose start is the firing
//! timestamp and whose end is the resolved timestamp, i.e. the span duration
//! is *how long the condition was violated*. That makes alert flap patterns,
//! durations, and overlaps first-class citizens in a tracing backend
//! (Tempo/Jaeger) without any sensor-side changes.
//!
//! Trace/span ids are derived **deterministically** from the alert key plus
//! the firing timestamp (FNV-1a with domain separation), so re-processing the
//! same lifecycle — e.g. after an exporter restart replaying history — yields
//! the same ids instead of duplicate spans with fresh identities. This is
//! synthesis, not propagation: the ids correlate replays of the same alert
//! lifecycle; they do not link to any sensor-side trace.
//!
//! Artifact-transfer spans (the `artifact/status` read procedure) are
//! intentionally **not** synthesized: the exporter only subscribes to
//! telemetry and `state/*/alert/*`,
//! and artifact status does not pass either subscription. See
//! [`crate::config::TracesConfig`].
//!
//! [`AlertSpanTracker`] is pure state-in/data-out (no OTel SDK types), so the
//! full synthesis logic is unit-testable; the thin OTLP wiring lives in
//! [`crate::exporter`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tracing::warn;
use zensight_common::alert::{Alert, AlertState};

/// Upper bound on simultaneously-firing alerts tracked for span synthesis.
/// Alerts are low-cardinality by design (bucketed `alert_key`), so hitting
/// this indicates something pathological; new firings are dropped with a
/// warning rather than growing without bound.
const MAX_PENDING: usize = 10_000;

/// A synthesized span describing one completed alert lifecycle
/// (firing → resolved). Plain data, convertible to an OTel span by the
/// exporter.
#[derive(Debug, Clone, PartialEq)]
pub struct AlertSpan {
    /// Span name: `alert:<rule>`.
    pub name: String,
    /// Deterministic 16-byte trace id (from alert key + firing timestamp).
    pub trace_id: [u8; 16],
    /// Deterministic 8-byte span id (from alert key + firing timestamp).
    pub span_id: [u8; 8],
    /// Firing timestamp (Unix epoch millis) — span start.
    pub start_ms: i64,
    /// Resolved timestamp (Unix epoch millis) — span end. Never before start.
    pub end_ms: i64,
    /// `alert.*` attributes (same naming as the alert log-event path).
    pub attributes: Vec<(String, String)>,
}

impl AlertSpan {
    /// Firing duration in milliseconds.
    pub fn duration_ms(&self) -> i64 {
        self.end_ms - self.start_ms
    }
}

/// Tracks firing alerts and yields an [`AlertSpan`] when one resolves.
///
/// Feed every decoded alert transition to [`AlertSpanTracker::on_alert`]:
/// - `Firing` records the *first* firing timestamp for the alert key
///   (refresh Puts of an already-firing alert do not move the span start),
/// - `Resolved` completes the lifecycle and returns the synthesized span,
/// - a `Resolved` for an unseen key (exporter started mid-lifecycle) yields
///   `None` — the start time is unknown, so no span can be synthesized.
#[derive(Debug, Default)]
pub struct AlertSpanTracker {
    /// `(origin, alert_key)` → first firing timestamp (epoch millis).
    ///
    /// Not `alert_key` alone: since epic #453 the hash no longer includes the
    /// source, so **two hosts firing the identical rule share one**. Keyed by
    /// the hash, the second host's firing edge was swallowed by `or_insert`,
    /// the first host's resolve consumed the entry, and the second host's
    /// resolve then found nothing and synthesized **no span at all** — an
    /// incident that never reached the trace backend.
    firing: HashMap<(Option<String>, String), Pending>,
}

/// One open lifecycle: when it started, and when this process last heard about
/// it (#1146).
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// The FIRST firing timestamp. A refresh does not move it — that is the
    /// span's start.
    start_ms: i64,
    /// When this process last saw the alert, by ITS clock. Not the alert's
    /// timestamp: expiry is about how long we have been waiting, and a sensor
    /// whose clock is wrong would otherwise expire instantly or never.
    last_seen: Instant,
}

impl AlertSpanTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of alert lifecycles currently open (firing, not yet resolved).
    pub fn pending(&self) -> usize {
        self.firing.len()
    }

    /// Forget lifecycles nothing has refreshed for `ttl` (#1146). Returns how
    /// many went.
    ///
    /// A `Resolved` was the ONLY thing that drained this map, so a host that
    /// died mid-alert left its entry forever. At `MAX_PENDING` new firings are
    /// dropped with a warning and **no span is ever emitted again for the
    /// process's lifetime** — a slow ratchet on any fleet that reinstalls
    /// hosts, and one that reports itself only as a periodic warning.
    ///
    /// Expiring is the honest answer rather than a loss: a lifecycle whose
    /// resolve never arrives has no end time, so there is no span to
    /// synthesize from it — that is the same reason a `Resolved` for an unseen
    /// key yields `None`. Sensors re-publish a firing alert, so a real
    /// incident refreshes `last_seen` and outlives any TTL set above the
    /// alert-refresh interval.
    pub fn expire(&mut self, ttl: Duration) -> usize {
        let before = self.firing.len();
        self.firing.retain(|_, p| p.last_seen.elapsed() < ttl);
        let dropped = before - self.firing.len();
        if dropped > 0 {
            warn!(
                dropped,
                remaining = self.firing.len(),
                ttl_secs = ttl.as_secs(),
                "alert-span tracker: forgot lifecycles nothing refreshed"
            );
        }
        dropped
    }

    /// Forget every lifecycle of one origin — its sensor's liveliness token
    /// went away (#1146).
    ///
    /// The same signal the Prometheus exporter acts on with
    /// `drop_origin_alerts`: a sensor that is gone will never publish the
    /// `Resolved` these are waiting for. Returns how many went.
    pub fn drop_origin(&mut self, origin: &str) -> usize {
        let before = self.firing.len();
        self.firing.retain(|(o, _), _| o.as_deref() != Some(origin));
        before - self.firing.len()
    }

    /// Apply one alert transition; returns a completed span on resolve.
    ///
    /// `origin` is the chunk of the key the alert arrived on — the caller has
    /// it and the payload does not, and without it two hosts' identical rule
    /// is one lifecycle. `None` (a test, or an unparseable key) keys under
    /// its own bucket rather than colliding with every real host.
    pub fn on_alert(&mut self, origin: Option<&str>, alert: &Alert) -> Option<AlertSpan> {
        let key = (origin.map(str::to_string), alert.alert_key());
        match alert.state {
            AlertState::Firing => {
                if !self.firing.contains_key(&key) && self.firing.len() >= MAX_PENDING {
                    warn!(
                        max = MAX_PENDING,
                        "alert-span tracker full, dropping new firing lifecycle"
                    );
                    return None;
                }
                // Keep the first firing timestamp: refreshes don't restart the
                // span. They DO refresh `last_seen`, which is what keeps a
                // long-running incident from expiring under the TTL.
                let now = Instant::now();
                self.firing
                    .entry(key)
                    .and_modify(|p| p.last_seen = now)
                    .or_insert(Pending {
                        start_ms: alert.timestamp,
                        last_seen: now,
                    });
                None
            }
            AlertState::Resolved => {
                let start_ms = self.firing.remove(&key)?.start_ms;
                // Guard against clock skew between the two transitions.
                let end_ms = alert.timestamp.max(start_ms);
                // The ids are derived from the ORIGIN and the hash together
                // (they are the span's identity), so two hosts' identical rule
                // gets two traces rather than colliding on one.
                let seed = match &key.0 {
                    Some(o) => format!("{o}.{}", key.1),
                    None => key.1.clone(),
                };
                let (trace_id, span_id) = deterministic_ids(&seed, start_ms);

                let mut attributes = vec![
                    ("alert.key".to_string(), key.1.clone()),
                    ("alert.source".to_string(), alert.source.clone()),
                    ("alert.protocol".to_string(), alert.protocol.to_string()),
                    ("alert.rule".to_string(), alert.rule.clone()),
                    ("alert.kind".to_string(), alert.kind.as_str().to_string()),
                    (
                        "alert.severity".to_string(),
                        alert.severity.as_str().to_string(),
                    ),
                    ("alert.summary".to_string(), alert.summary.clone()),
                ];
                for (k, v) in &alert.labels {
                    attributes.push((format!("alert.label.{k}"), v.clone()));
                }

                Some(AlertSpan {
                    name: format!("alert:{}", alert.rule),
                    trace_id,
                    span_id,
                    start_ms,
                    end_ms,
                    attributes,
                })
            }
        }
    }
}

/// Derive deterministic, non-zero trace/span ids from an alert key and its
/// firing timestamp.
///
/// FNV-1a 64-bit with domain-separation prefixes; two hashes are concatenated
/// for the 128-bit trace id. Stable across runs and platforms — the same
/// lifecycle always maps to the same ids. All-zero ids are invalid in OTel, so
/// a set bit is forced in the (astronomically unlikely) zero case.
pub fn deterministic_ids(alert_key: &str, firing_ts_ms: i64) -> ([u8; 16], [u8; 8]) {
    let hash = |domain: &[u8]| -> u64 {
        let mut h = Fnv1a::new();
        h.update(domain);
        h.update(b"\0");
        h.update(alert_key.as_bytes());
        h.update(b"\0");
        h.update(&firing_ts_ms.to_be_bytes());
        h.finish()
    };

    let mut trace_id = [0u8; 16];
    trace_id[..8].copy_from_slice(&hash(b"zensight-trace-hi").to_be_bytes());
    trace_id[8..].copy_from_slice(&hash(b"zensight-trace-lo").to_be_bytes());
    let mut span_id = hash(b"zensight-span").to_be_bytes();

    // An all-zero id is "invalid" in OTel; make sure we never emit one.
    if trace_id == [0u8; 16] {
        trace_id[15] = 1;
    }
    if span_id == [0u8; 8] {
        span_id[7] = 1;
    }
    (trace_id, span_id)
}

/// FNV-1a 64-bit — stable across runs/platforms (same construction as
/// `zensight_common::alert`'s key hash).
struct Fnv1a(u64);

impl Fnv1a {
    fn new() -> Self {
        Fnv1a(0xcbf2_9ce4_8422_2325)
    }
    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::Protocol;
    use zensight_common::alert::{AlertKind, AlertSeverity};

    fn firing_at(ts: i64) -> Alert {
        let mut a = Alert::new(
            "host01",
            Protocol::Netlink,
            AlertKind::Expectation,
            "ssh-listening",
            AlertSeverity::Critical,
            "sshd not listening on :22",
        )
        .with_label("port", "22");
        a.timestamp = ts;
        a
    }

    fn resolved_at(ts: i64) -> Alert {
        let mut a = firing_at(ts).resolved();
        a.timestamp = ts;
        a
    }

    #[test]
    fn lifecycle_produces_one_span_with_alert_timestamps() {
        let mut tracker = AlertSpanTracker::new();
        assert!(
            tracker
                .on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000))
                .is_none()
        );
        assert_eq!(tracker.pending(), 1);

        let span = tracker
            .on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(5_000))
            .expect("span");
        assert_eq!(tracker.pending(), 0);
        assert_eq!(span.name, "alert:ssh-listening");
        assert_eq!(span.start_ms, 1_000);
        assert_eq!(span.end_ms, 5_000);
        assert_eq!(span.duration_ms(), 4_000);
    }

    #[test]
    fn attributes_carry_alert_context() {
        let mut tracker = AlertSpanTracker::new();
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000));
        let span = tracker
            .on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(2_000))
            .unwrap();

        let get = |k: &str| {
            span.attributes
                .iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("alert.source"), Some("host01"));
        assert_eq!(get("alert.protocol"), Some("netlink"));
        assert_eq!(get("alert.rule"), Some("ssh-listening"));
        assert_eq!(get("alert.kind"), Some("expectation"));
        assert_eq!(get("alert.severity"), Some("critical"));
        assert_eq!(get("alert.summary"), Some("sshd not listening on :22"));
        assert_eq!(get("alert.label.port"), Some("22"));
        assert_eq!(get("alert.key"), Some(firing_at(0).alert_key().as_str()));
    }

    #[test]
    fn firing_refresh_keeps_original_start() {
        let mut tracker = AlertSpanTracker::new();
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000));
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(3_000)); // refresh Put — must not restart
        assert_eq!(tracker.pending(), 1);

        let span = tracker
            .on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(5_000))
            .unwrap();
        assert_eq!(span.start_ms, 1_000);
    }

    #[test]
    fn resolved_without_firing_yields_no_span() {
        // Exporter joined mid-lifecycle: start time unknown, nothing to emit.
        let mut tracker = AlertSpanTracker::new();
        assert!(
            tracker
                .on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(5_000))
                .is_none()
        );
        assert_eq!(tracker.pending(), 0);
    }

    #[test]
    fn end_never_precedes_start() {
        // Clock skew: resolved carries an earlier timestamp than firing.
        let mut tracker = AlertSpanTracker::new();
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(5_000));
        let span = tracker
            .on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(4_000))
            .unwrap();
        assert_eq!(span.start_ms, 5_000);
        assert_eq!(span.end_ms, 5_000, "end clamps to start");
    }

    #[test]
    fn ids_are_deterministic_and_distinct() {
        // Same lifecycle -> same ids (restart/replay safe).
        let (t1, s1) = deterministic_ids("ssh-listening-abc", 1_000);
        let (t2, s2) = deterministic_ids("ssh-listening-abc", 1_000);
        assert_eq!(t1, t2);
        assert_eq!(s1, s2);

        // Different key or different firing time -> different identity.
        let (t3, s3) = deterministic_ids("ssh-listening-abc", 2_000);
        let (t4, s4) = deterministic_ids("other-key", 1_000);
        assert_ne!(t1, t3);
        assert_ne!(s1, s3);
        assert_ne!(t1, t4);
        assert_ne!(s1, s4);

        // Never the invalid all-zero ids.
        assert_ne!(t1, [0u8; 16]);
        assert_ne!(s1, [0u8; 8]);
    }

    #[test]
    fn same_lifecycle_yields_same_span_ids_across_trackers() {
        // Two independent trackers (e.g. exporter restart with replay) must
        // synthesize the identical span identity for the identical lifecycle.
        let span_a = {
            let mut t = AlertSpanTracker::new();
            t.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000));
            t.on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(2_000))
                .unwrap()
        };
        let span_b = {
            let mut t = AlertSpanTracker::new();
            t.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000));
            t.on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(2_000))
                .unwrap()
        };
        assert_eq!(span_a.trace_id, span_b.trace_id);
        assert_eq!(span_a.span_id, span_b.span_id);
    }

    #[test]
    fn independent_alerts_track_independently() {
        let mut tracker = AlertSpanTracker::new();
        let mut other = Alert::new(
            "host02",
            Protocol::Netring,
            AlertKind::Anomaly,
            "PortScan",
            AlertSeverity::Warning,
            "scan from 10.0.0.5",
        );
        other.timestamp = 500;

        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000));
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &other);
        assert_eq!(tracker.pending(), 2);

        // Resolving one leaves the other open.
        let span = tracker
            .on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(2_000))
            .unwrap();
        assert_eq!(span.attributes[1].1, "host01");
        assert_eq!(tracker.pending(), 1);
    }

    /// **Two hosts firing one rule are two lifecycles** (epic #453).
    ///
    /// The alert-key hash no longer includes the source, so keyed by the hash
    /// alone: host B's firing edge was swallowed by `or_insert`, host A's
    /// resolve consumed the single entry, and **host B's resolve synthesized
    /// no span at all** — an incident that never reached the trace backend.
    #[test]
    fn two_hosts_firing_one_rule_are_two_lifecycles() {
        let mut tracker = AlertSpanTracker::new();
        let a = firing_at(1_000);
        let b = firing_at(1_000);
        assert_eq!(a.alert_key(), b.alert_key(), "the premise of this test");

        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &a);
        tracker.on_alert(Some("h-bbbbbbbbbbbb"), &b);
        assert_eq!(tracker.pending(), 2, "two hosts, two open lifecycles");

        let span_a = tracker
            .on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(5_000))
            .expect("host A's span");
        let span_b = tracker
            .on_alert(Some("h-bbbbbbbbbbbb"), &resolved_at(6_000))
            .expect("host B's span — this used to be None");
        assert_eq!(tracker.pending(), 0);

        // Distinct traces: the ids are seeded from the origin as well as the
        // hash, so the two incidents do not collapse into one trace either.
        assert_ne!(span_a.trace_id, span_b.trace_id);
    }

    /// **#1146, the ratchet.** A `Resolved` was the ONLY thing that drained
    /// this map, so a host that died mid-alert left its entry forever — and
    /// at `MAX_PENDING` new firings are dropped with a warning and **no span
    /// is ever emitted again for the process's lifetime**.
    ///
    /// The TTL is the backstop; `drop_origin` is the prompt answer when the
    /// producer's liveliness token goes away.
    #[test]
    fn a_lifecycle_nothing_refreshes_expires() {
        let mut tracker = AlertSpanTracker::new();
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000));
        assert_eq!(tracker.pending(), 1);

        // Nothing expires under a TTL it has not outlived.
        assert_eq!(tracker.expire(Duration::from_secs(3600)), 0);
        assert_eq!(tracker.pending(), 1);

        // A zero TTL expires everything that has not been refreshed at all.
        assert_eq!(tracker.expire(Duration::ZERO), 1);
        assert_eq!(tracker.pending(), 0);
    }

    /// A refresh keeps a long-running incident alive. Sensors re-publish a
    /// firing alert, so a real incident outlives any TTL set above the
    /// refresh interval — which is what makes expiring safe.
    #[test]
    fn a_refreshed_lifecycle_is_not_expired_and_keeps_its_start() {
        let mut tracker = AlertSpanTracker::new();
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000));
        std::thread::sleep(Duration::from_millis(20));
        // The refresh carries a later alert timestamp, as a real one does.
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(9_000));

        assert_eq!(
            tracker.expire(Duration::from_millis(15)),
            0,
            "the refresh reset the clock this expires against"
        );

        // And the span still starts at the FIRST firing, not the refresh.
        let span = tracker
            .on_alert(Some("h-aaaaaaaaaaaa"), &resolved_at(20_000))
            .expect("a span");
        assert_eq!(span.start_ms, 1_000);
        assert_eq!(span.end_ms, 20_000);
    }

    /// **#1146.** A sensor's liveliness token going away is the one signal
    /// that says the `Resolved` will never come. The Prometheus exporter has
    /// acted on it since #758; this tracker had no way to be told.
    #[test]
    fn a_departed_origin_loses_its_lifecycles_and_only_its_own() {
        let mut tracker = AlertSpanTracker::new();
        tracker.on_alert(Some("h-aaaaaaaaaaaa"), &firing_at(1_000));
        tracker.on_alert(Some("h-bbbbbbbbbbbb"), &firing_at(1_000));
        assert_eq!(tracker.pending(), 2);

        assert_eq!(tracker.drop_origin("h-aaaaaaaaaaaa"), 1);
        assert_eq!(tracker.pending(), 1);

        // The survivor still completes normally.
        assert!(
            tracker
                .on_alert(Some("h-bbbbbbbbbbbb"), &resolved_at(2_000))
                .is_some(),
            "the other host's lifecycle must be untouched"
        );
    }
}
