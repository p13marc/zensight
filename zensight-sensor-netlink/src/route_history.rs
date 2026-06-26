//! Default-route flap tracking (#111).
//!
//! The RTNETLINK event stream already folds route changes into bulk
//! `events/route/*` counters, but a flapping **default** route — the single most
//! common connectivity incident — leaves only a counter with no per-change
//! history. This module keeps a bounded ring of default-route transitions
//! (gateway change / withdrawal / (re)appearance) with timestamps, served on
//! demand via `@/query/route_changes`, plus a `routes/default_v4_flaps_total`
//! counter streamed onto the bus.
//!
//! The collector calls [`RouteHistory::observe`] each route poll with the
//! current default-v4 state; the first observation seeds the baseline without
//! counting as a flap. Cloning is cheap (`Arc`); the ring is shared with the
//! query channel.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// One default-route transition, served via `@/query/route_changes`.
///
/// Defined locally (this sensor owns only its own crate); the GUI decoder
/// mirrors this shape from the JSON reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteChangeRecord {
    /// Wall-clock seconds since the Unix epoch when observed.
    pub ts_unix: u64,
    /// IP family of the default route (`"v4"`).
    pub family: String,
    /// `"added"` (appeared), `"changed"` (new gateway), or `"withdrawn"` (gone).
    pub action: String,
    /// Gateway after the transition (`None` on withdrawal).
    pub gateway: Option<String>,
    /// Gateway before the transition (`None` on first appearance).
    pub prev_gateway: Option<String>,
}

/// The observed default-route state for one family.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DefaultRoute {
    present: bool,
    gateway: Option<String>,
}

/// Bounded ring of default-route transitions + a flap counter. Cheap to clone.
#[derive(Clone)]
pub struct RouteHistory {
    inner: Arc<Inner>,
}

struct Inner {
    /// Last observed default-v4 state; `None` until the first observation.
    last_v4: Mutex<Option<DefaultRoute>>,
    /// Most-recent transitions (drop-oldest), bounded by `capacity`.
    ring: Mutex<VecDeque<RouteChangeRecord>>,
    capacity: usize,
    /// Count of recorded default-v4 transitions (the flap counter).
    flaps_v4: AtomicU64,
    /// Set once the baseline has been seeded (so the first poll isn't a flap).
    seeded: AtomicBool,
}

impl RouteHistory {
    /// Create with a bounded transition ring (`capacity` rows).
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                last_v4: Mutex::new(None),
                ring: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
                capacity: capacity.max(1),
                flaps_v4: AtomicU64::new(0),
                seeded: AtomicBool::new(false),
            }),
        }
    }

    /// Observe the current default-v4 state at the current wall-clock time.
    /// Returns the recorded transition, or `None` if nothing changed (or this is
    /// the seeding observation).
    pub fn observe(&self, present: bool, gateway: Option<&str>) -> Option<RouteChangeRecord> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.observe_at(present, gateway, now)
    }

    /// Observe stamped at `now_unix` (testable form of [`observe`]).
    pub fn observe_at(
        &self,
        present: bool,
        gateway: Option<&str>,
        now_unix: u64,
    ) -> Option<RouteChangeRecord> {
        let cur = DefaultRoute {
            present,
            gateway: gateway.map(str::to_string),
        };
        let mut last = self.inner.last_v4.lock().unwrap();

        // First observation seeds the baseline silently — we can't tell a flap
        // from "this is just the state at startup".
        if !self.inner.seeded.swap(true, Ordering::Relaxed) {
            *last = Some(cur);
            return None;
        }

        let prev = last.clone().unwrap_or(DefaultRoute {
            present: false,
            gateway: None,
        });
        let action = match (prev.present, cur.present) {
            (false, true) => "added",
            (true, false) => "withdrawn",
            (true, true) if prev.gateway != cur.gateway => "changed",
            _ => {
                // No transition; keep the baseline current and return.
                *last = Some(cur);
                return None;
            }
        };

        let record = RouteChangeRecord {
            ts_unix: now_unix,
            family: "v4".to_string(),
            action: action.to_string(),
            gateway: cur.gateway.clone(),
            prev_gateway: prev.gateway.clone(),
        };
        *last = Some(cur);
        drop(last);

        self.inner.flaps_v4.fetch_add(1, Ordering::Relaxed);
        let mut ring = self.inner.ring.lock().unwrap();
        if ring.len() == self.inner.capacity {
            ring.pop_front();
        }
        ring.push_back(record.clone());
        Some(record)
    }

    /// The streamed flap counter as a telemetry point
    /// (`routes/default_v4_flaps_total`).
    pub fn flap_points(&self, host: &str) -> Vec<TelemetryPoint> {
        vec![TelemetryPoint::new(
            host,
            Protocol::Netlink,
            "routes/default_v4_flaps_total".to_string(),
            TelemetryValue::Counter(self.inner.flaps_v4.load(Ordering::Relaxed)),
        )]
    }

    /// Snapshot of the transition ring (oldest first), for `@/query/route_changes`.
    pub fn recent(&self) -> Vec<RouteChangeRecord> {
        self.inner.ring.lock().unwrap().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_observation_seeds_without_a_flap() {
        let h = RouteHistory::new(16);
        assert!(h.observe_at(true, Some("10.0.0.1"), 100).is_none());
        assert!(h.recent().is_empty());
        // flap counter still zero
        let pts = h.flap_points("host");
        assert_eq!(pts[0].value, TelemetryValue::Counter(0));
    }

    #[test]
    fn gateway_change_is_a_changed_flap() {
        let h = RouteHistory::new(16);
        h.observe_at(true, Some("10.0.0.1"), 100); // seed
        let rec = h.observe_at(true, Some("10.0.0.254"), 200).expect("change");
        assert_eq!(rec.action, "changed");
        assert_eq!(rec.gateway.as_deref(), Some("10.0.0.254"));
        assert_eq!(rec.prev_gateway.as_deref(), Some("10.0.0.1"));
        assert_eq!(h.recent().len(), 1);
        assert_eq!(h.flap_points("h")[0].value, TelemetryValue::Counter(1));
    }

    #[test]
    fn withdrawal_then_readd_are_two_flaps() {
        let h = RouteHistory::new(16);
        h.observe_at(true, Some("10.0.0.1"), 1); // seed
        let w = h.observe_at(false, None, 2).expect("withdraw");
        assert_eq!(w.action, "withdrawn");
        assert_eq!(w.gateway, None);
        assert_eq!(w.prev_gateway.as_deref(), Some("10.0.0.1"));
        let a = h.observe_at(true, Some("10.0.0.2"), 3).expect("re-add");
        assert_eq!(a.action, "added");
        assert_eq!(a.prev_gateway, None);
        assert_eq!(h.flap_points("h")[0].value, TelemetryValue::Counter(2));
    }

    #[test]
    fn steady_state_records_nothing() {
        let h = RouteHistory::new(16);
        h.observe_at(true, Some("10.0.0.1"), 1); // seed
        assert!(h.observe_at(true, Some("10.0.0.1"), 2).is_none());
        assert!(h.observe_at(true, Some("10.0.0.1"), 3).is_none());
        assert!(h.recent().is_empty());
    }

    #[test]
    fn ring_is_bounded_drop_oldest() {
        let h = RouteHistory::new(2);
        h.observe_at(true, Some("a"), 1); // seed
        h.observe_at(true, Some("b"), 2); // flap 1
        h.observe_at(true, Some("c"), 3); // flap 2
        h.observe_at(true, Some("d"), 4); // flap 3 → evicts the oldest
        let recent = h.recent();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].gateway.as_deref(), Some("c"));
        assert_eq!(recent[1].gateway.as_deref(), Some("d"));
        // counter still reflects all three transitions
        assert_eq!(h.flap_points("h")[0].value, TelemetryValue::Counter(3));
    }
}
