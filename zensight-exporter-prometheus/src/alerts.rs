//! Sensor-alert export.
//!
//! Sensors publish fully-formed alerts on
//! `zensight/v1/<origin>/state/<producer>/alert/<key>` (firing → resolved →
//! tombstone). The metric exporters normally subscribe only to the telemetry
//! class, so those alerts only ever reached the desktop GUI. This
//! store mirrors the firing set and renders it as a Prometheus gauge so external
//! monitoring (Prometheus / Alertmanager) can route on ZenSight alerts.
//!
//! Each firing alert is one `<prefix>_alert` series with value `1`. When the
//! alert resolves (a `Resolved` Put or a Zenoh `Delete` tombstone) the series is
//! removed — Alertmanager treats a vanished `ALERTS`-style series as resolved,
//! so no explicit `0` is needed. Alerts are high-value, so (unlike metrics) they
//! are not subject to the metric filter or `max_series`.
//!
//! # Absence means resolved, so absence must never be an accident (#758)
//!
//! This store used to evict any alert not *re-received* within
//! `stale_timeout_secs` (300 s). But sensors publish alerts **edge-triggered**:
//! `zensight-sensor-core`'s alert engine returns `Action::None` when an alert is
//! already published and its severity has not changed, so a firing alert is put
//! **once**, and again only to resolve.
//!
//! The two facts together meant every alert older than five minutes silently
//! removed itself, and — because absence is the resolve signal — Alertmanager
//! closed a live incident. The staleness sweep did the exact opposite of its
//! stated purpose.
//!
//! A firing alert now leaves this store for exactly three reasons, all of them
//! real events:
//!
//! 1. a `Resolved` put from the sensor,
//! 2. a Zenoh `Delete` tombstone,
//! 3. the sensor's **liveliness token vanishing** ([`AlertStore::drop_origin`])
//!    — the actual "the sensor died" signal, which a 300 s timer was only ever
//!    standing in for.
//!
//! # On `summary` and cardinality
//!
//! `summary` is a label, and sensor summaries embed live values
//! (`"disk at 91.3%"`), which looks like a cardinality hazard: every re-publish
//! with a different number would mint a new series.
//!
//! It is not, and the reason is the same edge-triggering above. A sensor
//! updates its stored alert every evaluation but returns `Action::None` unless
//! the **severity** changed — a changed summary alone never reaches the wire.
//! So the number of label sets per alert is bounded by how many severity
//! transitions it makes, not by how often it is evaluated, and the old series
//! goes stale naturally when a transition does happen.

use std::collections::HashMap;
use std::io::Write;
use std::time::Instant;

use parking_lot::RwLock;
use zensight_common::ack::AlertAck;
use zensight_common::alert::{Alert, AlertRef, AlertState};
use zensight_common::incident::Incident;

use crate::collector::escape_label_value;
use crate::mapping::sanitize_label_name;

/// Reserved label names the exporter sets itself; an alert's own structured
/// labels are skipped if they would collide with one of these.
const RESERVED: &[&str] = &[
    "alert_key",
    "acked",
    "source",
    "protocol",
    "rule",
    "severity",
    "kind",
    "summary",
];

/// A firing alert plus when it was last seen.
///
/// `received` is diagnostic only. It is deliberately NOT an eviction input:
/// see the module note on why a timer is the wrong liveness signal for an
/// edge-triggered publisher.
struct StoredAlert {
    alert: Alert,
    #[allow(dead_code)]
    received: Instant,
}

/// The catalog's acknowledgements and incidents (#926).
///
/// Kept beside the alert store rather than inside it because they have
/// different writers and different lifecycles: an alert comes from a sensor
/// and an ack from the catalog, and an ack can legitimately arrive for an
/// alert this exporter has not seen (it started mid-incident) or outlive one
/// it has.
#[derive(Default)]
pub struct CatalogStore {
    acks: RwLock<HashMap<AlertRef, AlertAck>>,
    incidents: RwLock<HashMap<String, Incident>>,
}

impl CatalogStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply_ack(&self, ack: AlertAck) {
        self.acks.write().insert(ack.alert_ref.clone(), ack);
    }

    pub fn remove_ack(&self, r: &AlertRef) {
        self.acks.write().remove(r);
    }

    pub fn apply_incident(&self, inc: Incident) {
        self.incidents.write().insert(inc.id.clone(), inc);
    }

    pub fn remove_incident(&self, id: &str) {
        self.incidents.write().remove(id);
    }

    pub fn incidents(&self) -> usize {
        self.incidents.read().len()
    }

    /// Whether an ack applies to this firing alert — the **projection rule**
    /// (RFC 06 §5.5), applied here so a scrape never reports an orphan as an
    /// acknowledgement.
    ///
    /// This is the rule's whole point: it is stated normatively in the RFC so
    /// that a consumer which is not the catalog reaches the same conclusion
    /// the catalog does, from the documents alone. This exporter is exactly
    /// such a consumer.
    pub fn is_acked(&self, origin: Option<&str>, alert: &Alert) -> bool {
        let Some(origin) = origin else {
            // No origin, no ref, no ack. Guessing one would attribute another
            // host's acknowledgement to this alert.
            return false;
        };
        let Ok(r) = AlertRef::parse(&format!(
            "{origin}.{}.{}",
            alert.protocol,
            alert.alert_key()
        )) else {
            return false;
        };
        self.acks
            .read()
            .get(&r)
            .is_some_and(|ack| ack.applies_to(Some(alert)))
    }

    /// Append the incident series (#926).
    ///
    /// One gauge per incident, valued at its **open** member count — the
    /// members that are neither acknowledged nor silenced, which is what an
    /// operator's queue actually is. `symptom_of` rides as a label, so an
    /// Alertmanager deployment gets inhibition-by-label for free: an incident
    /// explained by an upstream failure is one an inhibit rule can suppress.
    pub fn render(&self, prefix: &str, out: &mut Vec<u8>) {
        let map = self.incidents.read();
        if map.is_empty() {
            return;
        }
        let name = format!("{prefix}_incident");
        let _ = writeln!(
            out,
            "# HELP {name} ZenSight incident: firing alerts grouped by entity \
             (value = members neither acknowledged nor silenced)."
        );
        let _ = writeln!(out, "# TYPE {name} gauge");

        let mut ids: Vec<&String> = map.keys().collect();
        ids.sort();
        for id in ids {
            let i = &map[id];
            let mut labels: Vec<(String, String)> = vec![
                ("incident".into(), i.id.clone()),
                ("entity".into(), i.entity_id.clone().unwrap_or_default()),
                ("severity".into(), i.severity.as_str().to_string()),
                // The origins are plural by design — that is what keying by
                // entity buys — and a label must be one value, so they are
                // joined. A consumer that wants one origin has the incident
                // id and the catalog's document.
                ("origins".into(), i.origins.join(",")),
                (
                    "symptom_of".into(),
                    i.symptom_of
                        .as_ref()
                        .map(|c| c.entity_id().to_string())
                        .unwrap_or_default(),
                ),
            ];
            labels.sort_by(|a, b| a.0.cmp(&b.0));
            let rendered: Vec<String> = labels
                .iter()
                .map(|(k, v)| format!("{k}=\"{}\"", escape_label_value(v)))
                .collect();
            let _ = writeln!(out, "{name}{{{}}} {}", rendered.join(","), i.open());
        }
    }
}

/// Thread-safe store of currently-firing alerts, keyed by
/// **`(origin, alert_key)`**.
///
/// Not by `alert_key` alone, and the difference is not academic. Since epic
/// #453 the key hash no longer includes the source — the wire key's origin
/// chunk scopes it — so **two hosts firing the identical rule have the
/// identical `alert_key`**. Keyed by the hash alone this store showed one of
/// them, with whichever `source` label arrived last; and because absence is
/// the resolve signal, the surviving host resolving removed the series and
/// Alertmanager closed the *other* host's live incident.
///
/// An alert that arrived with no origin (a test, or a sample whose key did not
/// parse) keys under `None`, which keeps those distinct from every real host
/// rather than lumping them together.
#[derive(Default)]
pub struct AlertStore {
    alerts: RwLock<HashMap<AlertId, StoredAlert>>,
}

/// The store's key: the publishing origin and the RFC 11 §3.1 hash.
type AlertId = (Option<String>, String);

impl AlertStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an alert update. A firing alert is inserted/updated; a resolved
    /// alert clears its series.
    pub fn apply(&self, alert: Alert) {
        self.apply_from(None, alert);
    }

    /// [`apply`](Self::apply), recording the origin chunk of the key the
    /// alert arrived on so a departed sensor's alerts can be found again.
    pub fn apply_from(&self, origin: Option<String>, alert: Alert) {
        // The origin lives in the KEY now, not beside the alert: it is what
        // distinguishes two hosts firing the identical rule, and a copy in the
        // value would be a second place for the same fact to drift.
        let id: AlertId = (origin, alert.alert_key());
        let mut map = self.alerts.write();
        if alert.state == AlertState::Resolved {
            map.remove(&id);
        } else {
            map.insert(
                id,
                StoredAlert {
                    alert,
                    received: Instant::now(),
                },
            );
        }
    }

    /// Clear an alert by the origin it was published from and its `alert_key`
    /// (both read from the key of a Zenoh `Delete` tombstone).
    ///
    /// The origin is required for the same reason it is part of the store's
    /// key: two hosts firing the identical rule share an `alert_key`, so a
    /// tombstone that named only the hash would retire **both** — one of them
    /// still firing, and silently gone from Prometheus.
    pub fn remove(&self, origin: Option<&str>, alert_key: &str) {
        self.alerts
            .write()
            .remove(&(origin.map(str::to_string), alert_key.to_string()));
    }

    /// Number of firing alerts.
    pub fn len(&self) -> usize {
        self.alerts.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.alerts.read().is_empty()
    }

    /// Drop every firing alert from one source, because its liveliness token
    /// vanished.
    ///
    /// This is the honest replacement for the staleness sweep: a sensor that
    /// died without tombstoning its alerts is exactly what a disappearing
    /// liveliness token means (RFC 04 §5), and unlike a timer it cannot fire
    /// for a sensor that is alive and simply had nothing new to say.
    ///
    /// Returns the number removed.
    ///
    /// `origin` is the token's origin chunk (`h-<12hex>`), matched against
    /// the chunk each alert *arrived under* — never against `Alert::source`,
    /// which is a hostname. The two were compared for a while and could not
    /// be equal, so a SIGKILLed sensor's `zensight_alert` series was exported
    /// forever, with the staleness sweep deliberately not touching alerts.
    pub fn drop_origin(&self, origin: &str) -> usize {
        let mut map = self.alerts.write();
        let before = map.len();
        map.retain(|(o, _), _| o.as_deref() != Some(origin));
        before - map.len()
    }

    /// Append the alert series to a Prometheus exposition buffer.
    ///
    /// `catalog` supplies the `acked` label (#926). `None` — no catalog
    /// configured, or none seen — renders `acked="false"` on every alert,
    /// which is the honest answer: nobody has said they are on it.
    pub fn render(&self, prefix: &str, catalog: Option<&CatalogStore>, out: &mut Vec<u8>) {
        let map = self.alerts.read();
        if map.is_empty() {
            return;
        }
        let name = format!("{prefix}_alert");

        let _ = writeln!(
            out,
            "# HELP {name} ZenSight sensor alert (1 = firing; series absent once resolved)."
        );
        let _ = writeln!(out, "# TYPE {name} gauge");

        // Deterministic output order so scrapes/diffs are stable. Sorting by
        // the whole id, not just the hash, keeps two hosts' identical rule in
        // a stable order relative to each other.
        let mut ids: Vec<&AlertId> = map.keys().collect();
        ids.sort();

        for id in ids {
            let a = &map[id].alert;
            let acked = catalog.is_some_and(|c| c.is_acked(id.0.as_deref(), a));
            let mut labels: Vec<(String, String)> = vec![
                ("alert_key".into(), id.1.clone()),
                // The fact every headless consumer was missing: an
                // acknowledged alert and a new one looked identical here, so
                // an on-call tool could not tell "someone is on this" from
                // "nobody has seen this yet".
                ("acked".into(), acked.to_string()),
                ("source".into(), a.source.clone()),
                ("protocol".into(), a.protocol.to_string()),
                ("rule".into(), a.rule.clone()),
                ("severity".into(), a.severity.as_str().to_string()),
                ("kind".into(), a.kind.as_str().to_string()),
                ("summary".into(), a.summary.clone()),
            ];
            // Merge the alert's structured labels (sanitized), skipping reserved
            // names and any that collapse to a duplicate.
            for (k, v) in &a.labels {
                let lk = sanitize_label_name(k);
                if RESERVED.contains(&lk.as_str()) || labels.iter().any(|(e, _)| e == &lk) {
                    continue;
                }
                labels.push((lk, v.clone()));
            }
            labels.sort_by(|x, y| x.0.cmp(&y.0));

            let label_str = labels
                .iter()
                .map(|(k, v)| format!("{}=\"{}\"", k, escape_label_value(v)))
                .collect::<Vec<_>>()
                .join(",");
            let _ = writeln!(out, "{name}{{{label_str}}} 1");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::Protocol;
    use zensight_common::alert::{AlertKind, AlertSeverity};

    fn firing() -> Alert {
        Alert::new(
            "host01",
            Protocol::Netlink,
            AlertKind::Expectation,
            "ssh-listening",
            AlertSeverity::Critical,
            "sshd not listening on :22",
        )
        .with_label("port", "22")
    }

    fn render(store: &AlertStore) -> String {
        let mut out = Vec::new();
        store.render("zensight", None, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn firing_alert_renders_gauge() {
        let store = AlertStore::new();
        store.apply(firing());
        let out = render(&store);
        assert!(out.contains("# TYPE zensight_alert gauge"));
        assert!(out.contains("source=\"host01\""));
        assert!(out.contains("protocol=\"netlink\""));
        assert!(out.contains("rule=\"ssh-listening\""));
        assert!(out.contains("severity=\"critical\""));
        assert!(out.contains("kind=\"expectation\""));
        assert!(out.contains("port=\"22\""), "structured label merged");
        assert!(out.trim_end().ends_with("} 1"), "value is 1 while firing");
    }

    #[test]
    fn resolved_alert_clears_series() {
        let store = AlertStore::new();
        let a = firing();
        store.apply(a.clone());
        assert_eq!(store.len(), 1);
        store.apply(a.resolved());
        assert_eq!(store.len(), 0);
        assert!(render(&store).is_empty(), "no block when nothing firing");
    }

    #[test]
    fn tombstone_removes_by_origin_and_alert_key() {
        let store = AlertStore::new();
        let a = firing();
        let key = a.alert_key();
        store.apply_from(Some("h-aaaaaaaaaaaa".into()), a);
        // A tombstone from a DIFFERENT host does not retire this one.
        store.remove(Some("h-bbbbbbbbbbbb"), &key);
        assert_eq!(store.len(), 1, "another host's tombstone is not ours");
        store.remove(Some("h-aaaaaaaaaaaa"), &key);
        assert_eq!(store.len(), 0);
    }

    /// **Two hosts firing the identical rule are two series** (epic #453).
    ///
    /// The alert-key hash no longer includes the source — the wire key's
    /// origin chunk scopes it — so keyed by the hash alone this store showed
    /// one of them, with whichever `source` label arrived last. And because
    /// **absence is the resolve signal**, the surviving host resolving removed
    /// the series and Alertmanager closed the other host's live incident.
    #[test]
    fn two_hosts_firing_one_rule_are_two_series() {
        let store = AlertStore::new();
        let mk = |source: &str| {
            Alert::new(
                source,
                zensight_common::Protocol::Netlink,
                zensight_common::AlertKind::Expectation,
                "socket:sshd",
                zensight_common::AlertSeverity::Critical,
                "sshd is not listening",
            )
        };
        let (a, b) = (mk("web01"), mk("web02"));
        assert_eq!(a.alert_key(), b.alert_key(), "the premise of this test");
        let key = a.alert_key();
        store.apply_from(Some("h-aaaaaaaaaaaa".into()), a);
        store.apply_from(Some("h-bbbbbbbbbbbb".into()), b);
        assert_eq!(store.len(), 2);

        let out = render(&store);
        assert!(out.contains(r#"source="web01""#), "{out}");
        assert!(out.contains(r#"source="web02""#), "{out}");

        // web02 resolving must leave web01 firing — the failure that closed a
        // live incident in Alertmanager.
        store.remove(Some("h-bbbbbbbbbbbb"), &key);
        let out = render(&store);
        assert!(out.contains(r#"source="web01""#), "{out}");
        assert!(!out.contains(r#"source="web02""#), "{out}");
    }

    /// The same, through the liveliness path: one host's sensor dying does not
    /// retire the other's identical alert.
    #[test]
    fn dropping_one_origin_leaves_the_others_identical_alert() {
        let store = AlertStore::new();
        let mk = |source: &str| {
            Alert::new(
                source,
                zensight_common::Protocol::Netlink,
                zensight_common::AlertKind::Expectation,
                "socket:sshd",
                zensight_common::AlertSeverity::Critical,
                "sshd is not listening",
            )
        };
        store.apply_from(Some("h-aaaaaaaaaaaa".into()), mk("web01"));
        store.apply_from(Some("h-bbbbbbbbbbbb".into()), mk("web02"));
        assert_eq!(store.drop_origin("h-aaaaaaaaaaaa"), 1);
        assert_eq!(store.len(), 1);
        assert!(render(&store).contains(r#"source="web02""#));
    }

    #[test]
    fn update_in_place_keeps_one_series() {
        let store = AlertStore::new();
        store.apply(firing());
        store.apply(firing()); // same source+rule+labels -> same alert_key
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn reserved_label_is_not_overridden_by_alert_label() {
        // An alert whose structured label collides with a reserved name must not
        // produce a duplicate `source=` label.
        let store = AlertStore::new();
        let a = Alert::new(
            "host01",
            Protocol::Netring,
            AlertKind::Anomaly,
            "PortScan",
            AlertSeverity::Warning,
            "scan",
        )
        .with_label("source", "spoofed");
        store.apply(a);
        let out = render(&store);
        assert_eq!(out.matches("source=").count(), 1);
        assert!(out.contains("source=\"host01\""));
    }

    #[test]
    fn label_values_are_escaped() {
        let store = AlertStore::new();
        let a = Alert::new(
            "host01",
            Protocol::Netring,
            AlertKind::Anomaly,
            "Beacon",
            AlertSeverity::Warning,
            "saw \"quotes\" and \\slash",
        );
        store.apply(a);
        let out = render(&store);
        assert!(out.contains(r#"summary=\"quotes\""#) || out.contains("\\\""));
    }

    /// A firing alert leaves only when its SOURCE goes away (#758).
    ///
    /// This replaces `stale_alerts_are_evicted`, which asserted the bug:
    /// sensors publish alerts edge-triggered, so "not re-received in 300s"
    /// almost always meant "still firing, nothing changed" — and because
    /// absence is the resolve signal, evicting on that timer silently closed
    /// live incidents.
    #[test]
    fn a_departed_source_loses_its_alerts() {
        let store = AlertStore::new();
        // The alert arrives on a key whose origin chunk is the host id's
        // — NOT its hostname, which is what `Alert::source` carries.
        store.apply_from(Some("h-0123456789ab".into()), firing());
        assert_eq!(store.len(), 1);

        // A different host going away must not touch it.
        assert_eq!(store.drop_origin("h-ffffffffffff"), 0);
        assert_eq!(store.len(), 1, "another host's death is not evidence");

        // The hostname is not the origin; a liveliness token never carries
        // it, so matching on it would drop nothing — which is the bug this
        // guards against.
        let source = firing().source;
        assert_eq!(store.drop_origin(&source), 0);

        assert_eq!(store.drop_origin("h-0123456789ab"), 1);
        assert_eq!(store.len(), 0);
    }

    /// Time alone never removes a firing alert. If this ever fails, something
    /// has reintroduced a staleness sweep and live incidents will close
    /// themselves again.
    #[test]
    fn nothing_evicts_a_firing_alert_on_a_timer() {
        let store = AlertStore::new();
        store.apply(firing());

        // The only public ways out are a resolve, a tombstone, or a departed
        // source. There is deliberately no timer-based entry point at all.
        assert_eq!(store.len(), 1);
        assert_eq!(store.drop_origin("unrelated"), 0);
        assert_eq!(store.len(), 1);
    }

    fn aref(origin: &str, a: &Alert) -> AlertRef {
        AlertRef::parse(&format!("{origin}.{}.{}", a.protocol, a.alert_key())).unwrap()
    }

    fn ack_of(r: &AlertRef, fired_at: i64) -> AlertAck {
        AlertAck {
            alert_ref: r.clone(),
            fired_at,
            by: "marc".into(),
            note: String::new(),
            at: fired_at,
        }
    }

    /// **The fact every headless consumer was missing.** An acknowledged alert
    /// and a new one looked identical in `zensight_alert`, so an on-call tool
    /// could not tell "someone is on this" from "nobody has seen this yet".
    #[test]
    fn an_acknowledged_alert_carries_acked_true() {
        let store = AlertStore::new();
        let catalog = CatalogStore::new();
        let mut a = firing();
        a.timestamp = 1_000;
        let r = aref("h-aaaaaaaaaaaa", &a);
        store.apply_from(Some("h-aaaaaaaaaaaa".into()), a);

        let mut out = Vec::new();
        store.render("zensight", Some(&catalog), &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#"acked="false""#), "{s}");

        catalog.apply_ack(ack_of(&r, 1_000));
        let mut out = Vec::new();
        store.render("zensight", Some(&catalog), &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#"acked="true""#), "{s}");
    }

    /// **The projection rule reaches the exporter** (RFC 06 §5.5). It is
    /// normative precisely so a consumer that is not the catalog reaches the
    /// catalog's conclusion from the documents alone — and this exporter is
    /// exactly such a consumer.
    #[test]
    fn a_re_fire_is_not_acknowledged() {
        let store = AlertStore::new();
        let catalog = CatalogStore::new();
        let mut a = firing();
        a.timestamp = 1_000;
        let r = aref("h-aaaaaaaaaaaa", &a);
        catalog.apply_ack(ack_of(&r, 1_000));

        // The occurrence that was acknowledged.
        store.apply_from(Some("h-aaaaaaaaaaaa".into()), a.clone());
        let mut out = Vec::new();
        store.render("zensight", Some(&catalog), &mut out);
        assert!(String::from_utf8(out).unwrap().contains(r#"acked="true""#));

        // It cleared and came back — a different problem, and it must page.
        let mut again = a;
        again.timestamp = 2_000;
        store.apply_from(Some("h-aaaaaaaaaaaa".into()), again);
        let mut out = Vec::new();
        store.render("zensight", Some(&catalog), &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains(r#"acked="false""#),
            "a re-fire is not acked: {s}"
        );
    }

    /// An ack for one host does not acknowledge another host's identical rule
    /// — the same collision this store's key fixes, one layer up.
    #[test]
    fn an_ack_does_not_cross_hosts() {
        let store = AlertStore::new();
        let catalog = CatalogStore::new();
        let mut a = firing();
        a.timestamp = 1_000;
        catalog.apply_ack(ack_of(&aref("h-aaaaaaaaaaaa", &a), 1_000));
        store.apply_from(Some("h-bbbbbbbbbbbb".into()), a);

        let mut out = Vec::new();
        store.render("zensight", Some(&catalog), &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#"acked="false""#), "{s}");
    }

    /// An incident renders with its entity, its origins and — when it has one
    /// — what it is a symptom **of**, which is what buys an Alertmanager
    /// deployment inhibition-by-label for free.
    #[test]
    fn an_incident_renders_with_symptom_of() {
        use zensight_common::impact::{AlertSite, Cause};
        let catalog = CatalogStore::new();
        catalog.apply_incident(Incident {
            id: "inc-h_guest".into(),
            entity_id: Some("h_guest".into()),
            origins: vec!["h-aaaaaaaaaaaa".into(), "h-bbbbbbbbbbbb".into()],
            severity: zensight_common::AlertSeverity::Critical,
            started: 1,
            last_change: 2,
            summary: "probe-down on vm101".into(),
            alerts: Vec::new(),
            symptom_of: Some(Cause::Alert(AlertSite {
                entity_id: "h_hyp".into(),
                origin: "h-cccccccccccc".into(),
                alert_key: "k1".into(),
            })),
            impacted: Vec::new(),
            acked: 0,
            silenced: 0,
            last_updated: 3,
        });

        let mut out = Vec::new();
        catalog.render("zensight", &mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("zensight_incident{"), "{s}");
        assert!(s.contains(r#"entity="h_guest""#), "{s}");
        assert!(s.contains(r#"symptom_of="h_hyp""#), "{s}");
        assert!(
            s.contains(r#"origins="h-aaaaaaaaaaaa,h-bbbbbbbbbbbb""#),
            "both origins, which is what keying by entity buys: {s}"
        );
    }

    /// The gauge's value is the **open** member count — what is neither
    /// acknowledged nor silenced, which is an operator's actual queue. A
    /// fully-handled incident reads 0 without vanishing, so a dashboard can
    /// still show that it exists.
    #[test]
    fn the_incident_gauge_counts_the_open_members() {
        let catalog = CatalogStore::new();
        let mk = |acked, silenced| Incident {
            id: "inc-x".into(),
            entity_id: None,
            origins: vec!["h-aaaaaaaaaaaa".into()],
            severity: zensight_common::AlertSeverity::Warning,
            started: 1,
            last_change: 2,
            summary: "x".into(),
            alerts: vec![
                AlertRef::new("h-aaaaaaaaaaaa", "netlink", "k1"),
                AlertRef::new("h-aaaaaaaaaaaa", "netlink", "k2"),
                AlertRef::new("h-aaaaaaaaaaaa", "netlink", "k3"),
            ],
            symptom_of: None,
            impacted: Vec::new(),
            acked,
            silenced,
            last_updated: 3,
        };
        catalog.apply_incident(mk(0, 0));
        let mut out = Vec::new();
        catalog.render("zensight", &mut out);
        assert!(String::from_utf8(out).unwrap().trim_end().ends_with(" 3"));

        catalog.apply_incident(mk(1, 1));
        let mut out = Vec::new();
        catalog.render("zensight", &mut out);
        assert!(String::from_utf8(out).unwrap().trim_end().ends_with(" 1"));
    }

    /// A tombstone removes the series — the same "absence is resolved"
    /// contract the alert gauge has.
    #[test]
    fn an_incident_tombstone_removes_the_series() {
        let catalog = CatalogStore::new();
        catalog.apply_incident(Incident {
            id: "inc-x".into(),
            entity_id: None,
            origins: vec!["h-aaaaaaaaaaaa".into()],
            severity: zensight_common::AlertSeverity::Warning,
            started: 1,
            last_change: 2,
            summary: "x".into(),
            alerts: Vec::new(),
            symptom_of: None,
            impacted: Vec::new(),
            acked: 0,
            silenced: 0,
            last_updated: 3,
        });
        assert_eq!(catalog.incidents(), 1);
        catalog.remove_incident("inc-x");
        assert_eq!(catalog.incidents(), 0);
        let mut out = Vec::new();
        catalog.render("zensight", &mut out);
        assert!(out.is_empty(), "no incidents, no HELP/TYPE preamble either");
    }
}
