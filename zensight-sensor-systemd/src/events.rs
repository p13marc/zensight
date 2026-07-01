//! D-Bus event stream → control-plane timeline (#275).
//!
//! Calls `Manager.Subscribe()` once and consumes the `UnitNew`/`UnitRemoved`/
//! `JobNew`/`JobRemoved` signals, filtered to watched units to bound volume. Each
//! becomes a structured [`EventRecord`] in a bounded ring served on
//! `@/query/events` (newest-first), and optionally nudges the sentinel (#277) for
//! instant re-evaluation. Job completions also carry the resulting `ActiveState`
//! transition (`from`→`to`), tracked across events.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use crate::dbus::ManagerProxy;

/// One control-plane timeline event (#275). Structured (no raw strings) so the
/// GUI can filter by kind/unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Wall-clock seconds when observed.
    pub ts_unix: u64,
    /// `unit_new` / `unit_removed` / `job_new` / `job_removed`.
    pub kind: String,
    pub unit: Option<String>,
    /// `ActiveState` before the change (job_removed only, when known).
    #[serde(default)]
    pub from: Option<String>,
    /// `ActiveState` after the change (job_removed only).
    #[serde(default)]
    pub to: Option<String>,
    /// Job result (`done`/`failed`/`canceled`/…) for `job_removed`.
    #[serde(default)]
    pub job_result: Option<String>,
}

struct Inner {
    ring: Mutex<VecDeque<EventRecord>>,
    counters: Mutex<HashMap<String, u64>>,
    capacity: usize,
}

/// Shared, bounded ring of recent control-plane events + per-kind counters.
#[derive(Clone)]
pub struct EventState {
    inner: Arc<Inner>,
}

impl EventState {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                ring: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
                counters: Mutex::new(HashMap::new()),
                capacity: capacity.max(1),
            }),
        }
    }

    /// Record an event: push into the bounded ring (dropping the oldest) and bump
    /// the per-kind counter.
    pub fn record(&self, rec: EventRecord) {
        if let Ok(mut c) = self.inner.counters.lock() {
            *c.entry(rec.kind.clone()).or_default() += 1;
        }
        if let Ok(mut r) = self.inner.ring.lock() {
            if r.len() == self.inner.capacity {
                r.pop_front();
            }
            r.push_back(rec);
        }
    }

    /// Recent events, newest-first (for `@/query/events`).
    pub fn recent(&self) -> Vec<EventRecord> {
        self.inner
            .ring
            .lock()
            .map(|r| r.iter().rev().cloned().collect())
            .unwrap_or_default()
    }

    /// Optional streamed per-kind counters: `events/<kind>_total`.
    pub fn counter_points(&self, source: &str) -> Vec<TelemetryPoint> {
        let snapshot: Vec<(String, u64)> = self
            .inner
            .counters
            .lock()
            .map(|c| c.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();
        snapshot
            .into_iter()
            .map(|(kind, total)| {
                TelemetryPoint::new(
                    source,
                    Protocol::Systemd,
                    format!("events/{kind}_total"),
                    TelemetryValue::Counter(total),
                )
            })
            .collect()
    }
}

/// Current wall-clock seconds (runtime code — `chrono` is fine here).
fn now_unix() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

/// Run the D-Bus event stream until the session/bus closes. Filters signals to
/// units matching `watch`; on a watched job completion, nudges `wake` (if set)
/// and records the resulting `ActiveState` transition.
pub async fn run(watch: Vec<glob::Pattern>, state: EventState, wake: Option<Arc<Notify>>) {
    let conn = match zbus::Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "events: system bus connect failed");
            return;
        }
    };
    let manager = match ManagerProxy::new(&conn).await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, "events: Manager proxy failed");
            return;
        }
    };
    if let Err(e) = manager.subscribe().await {
        tracing::warn!(error = %e, "events: Manager.Subscribe failed (signals may be limited)");
    }

    let (mut unit_new, mut unit_removed, mut job_new, mut job_removed) = match (
        manager.receive_unit_new().await,
        manager.receive_unit_removed().await,
        manager.receive_job_new().await,
        manager.receive_job_removed().await,
    ) {
        (Ok(a), Ok(b), Ok(c), Ok(d)) => (a, b, c, d),
        _ => {
            tracing::error!("events: failed to subscribe to Manager signals");
            return;
        }
    };

    let watched = |name: &str| watch.iter().any(|g| g.matches(name));
    // Last-seen ActiveState per watched unit, to fill `from` on transitions.
    let mut last_state: HashMap<String, String> = HashMap::new();
    tracing::info!("systemd event stream ready");

    loop {
        tokio::select! {
            Some(sig) = unit_new.next() => {
                if let Ok(a) = sig.args() && watched(&a.id) {
                    state.record(EventRecord { ts_unix: now_unix(), kind: "unit_new".into(),
                        unit: Some(a.id.to_string()), from: None, to: None, job_result: None });
                }
            }
            Some(sig) = unit_removed.next() => {
                if let Ok(a) = sig.args() && watched(&a.id) {
                    last_state.remove(a.id.as_str());
                    state.record(EventRecord { ts_unix: now_unix(), kind: "unit_removed".into(),
                        unit: Some(a.id.to_string()), from: None, to: None, job_result: None });
                }
            }
            Some(sig) = job_new.next() => {
                if let Ok(a) = sig.args() && watched(&a.unit) {
                    state.record(EventRecord { ts_unix: now_unix(), kind: "job_new".into(),
                        unit: Some(a.unit.to_string()), from: None, to: None, job_result: None });
                }
            }
            Some(sig) = job_removed.next() => {
                if let Ok(a) = sig.args() && watched(&a.unit) {
                    let unit = a.unit.to_string();
                    // Read the resulting state; fill `from` from the last sighting.
                    let to = current_active_state(&manager, &conn, &unit).await;
                    let from = last_state.get(&unit).cloned();
                    if let Some(t) = &to {
                        last_state.insert(unit.clone(), t.clone());
                    }
                    state.record(EventRecord {
                        ts_unix: now_unix(),
                        kind: "job_removed".into(),
                        unit: Some(unit),
                        from,
                        to,
                        job_result: Some(a.result.to_string()),
                    });
                    if let Some(w) = &wake {
                        w.notify_one();
                    }
                }
            }
            else => break,
        }
    }
    tracing::info!("systemd event stream closed");
}

/// Best-effort read of a unit's current `ActiveState` (`None` on any failure).
async fn current_active_state(
    manager: &ManagerProxy<'_>,
    conn: &zbus::Connection,
    unit: &str,
) -> Option<String> {
    let path = manager.load_unit(unit).await.ok()?;
    let proxy = crate::dbus::UnitProxy::builder(conn)
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;
    proxy.active_state().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, unit: &str) -> EventRecord {
        EventRecord {
            ts_unix: 100,
            kind: kind.into(),
            unit: Some(unit.into()),
            from: None,
            to: None,
            job_result: None,
        }
    }

    #[test]
    fn ring_is_bounded_and_newest_first() {
        let s = EventState::new(2);
        s.record(rec("job_new", "a.service"));
        s.record(rec("job_removed", "b.service"));
        s.record(rec("unit_new", "c.service")); // evicts the oldest
        let recent = s.recent();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].unit.as_deref(), Some("c.service")); // newest first
        assert_eq!(recent[1].unit.as_deref(), Some("b.service"));
    }

    #[test]
    fn counters_track_per_kind() {
        let s = EventState::new(10);
        s.record(rec("job_removed", "a"));
        s.record(rec("job_removed", "b"));
        s.record(rec("unit_new", "c"));
        let by: HashMap<_, _> = s
            .counter_points("h")
            .into_iter()
            .map(|p| (p.metric, p.value))
            .collect();
        assert_eq!(by["events/job_removed_total"], TelemetryValue::Counter(2));
        assert_eq!(by["events/unit_new_total"], TelemetryValue::Counter(1));
    }
}
