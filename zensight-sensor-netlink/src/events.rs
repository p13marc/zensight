//! Real-time RTNETLINK event handling (issue #8).
//!
//! The collector subscribes to RTNETLINK multicast groups and consumes
//! `Connection::<Route>::events()` (a `Stream`) via `tokio::select!` against the
//! poll tick. Each [`NetworkEvent`] is folded into:
//! 1. bounded **counters** `events/{link,addr,route,neighbor}/{added,removed,
//!    changed}_total` (streamed), and
//! 2. a bounded **recent-events ring** served on demand via `@/query/events`.
//!
//! Relevant events (a `DelLink`, default-route withdrawal, gateway-neighbor
//! failure, …) also *nudge* the sentinel so the matching expectation is
//! re-evaluated instantly (~0s) instead of at the next poll tick.
//!
//! The classification (`classify_event`, `is_sentinel_relevant`, `event_record`)
//! is pure and unit-tested on synthetic events; [`EventState`] is the
//! atomics+ring container shared between the event task and the query channel.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use nlink::netlink::events::NetworkEvent;
use nlink::netlink::neigh::State as NeighborState;

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

/// The four RTNETLINK families we track for event counting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventFamily {
    Link,
    Addr,
    Route,
    Neighbor,
}

impl EventFamily {
    /// Stable lowercase label used in the metric path / records.
    pub fn label(self) -> &'static str {
        match self {
            EventFamily::Link => "link",
            EventFamily::Addr => "addr",
            EventFamily::Route => "route",
            EventFamily::Neighbor => "neighbor",
        }
    }

    fn index(self) -> usize {
        match self {
            EventFamily::Link => 0,
            EventFamily::Addr => 1,
            EventFamily::Route => 2,
            EventFamily::Neighbor => 3,
        }
    }

    const ALL: [EventFamily; 4] = [
        EventFamily::Link,
        EventFamily::Addr,
        EventFamily::Route,
        EventFamily::Neighbor,
    ];
}

/// What happened to an entity. `Changed` is only inferred for links (a
/// `RTM_NEWLINK` for an already-known ifindex is a state/attribute change, not a
/// creation); for addr/route/neighbor we report `added`/`removed` only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventAction {
    Added,
    Removed,
    Changed,
}

impl EventAction {
    pub fn label(self) -> &'static str {
        match self {
            EventAction::Added => "added",
            EventAction::Removed => "removed",
            EventAction::Changed => "changed",
        }
    }

    fn index(self) -> usize {
        match self {
            EventAction::Added => 0,
            EventAction::Removed => 1,
            EventAction::Changed => 2,
        }
    }

    const ALL: [EventAction; 3] = [
        EventAction::Added,
        EventAction::Removed,
        EventAction::Changed,
    ];
}

/// Family + raw add/remove sense of an event, before the link add-vs-change
/// refinement. Pure output of [`classify_event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventClass {
    pub family: EventFamily,
    /// `true` for `RTM_NEW*` (add/change), `false` for `RTM_DEL*`.
    pub is_new: bool,
}

/// Classify a [`NetworkEvent`] into its family + add/remove sense. Returns
/// `None` for families we do not subscribe to in Wave 1 (FDB, TC).
pub fn classify_event(ev: &NetworkEvent) -> Option<EventClass> {
    let family = match ev {
        NetworkEvent::NewLink(_) | NetworkEvent::DelLink(_) => EventFamily::Link,
        NetworkEvent::NewAddress(_) | NetworkEvent::DelAddress(_) => EventFamily::Addr,
        NetworkEvent::NewRoute(_) | NetworkEvent::DelRoute(_) => EventFamily::Route,
        NetworkEvent::NewNeighbor(_) | NetworkEvent::DelNeighbor(_) => EventFamily::Neighbor,
        // FDB / TC events are not subscribed in Wave 1.
        _ => return None,
    };
    Some(EventClass {
        family,
        is_new: ev.is_new(),
    })
}

/// Whether an event should trigger an *immediate* sentinel re-evaluation.
///
/// Covers the cases the sentinel reasons about: link presence/up-state changes,
/// any route change (default-route withdrawal/appearance), address changes, and
/// neighbor removals or transitions into `Failed` (gateway unreachable). Routine
/// `NewNeighbor` notifications for healthy entries are skipped to avoid sweep
/// storms under normal ARP churn.
pub fn is_sentinel_relevant(ev: &NetworkEvent) -> bool {
    match ev {
        NetworkEvent::NewLink(_)
        | NetworkEvent::DelLink(_)
        | NetworkEvent::NewAddress(_)
        | NetworkEvent::DelAddress(_)
        | NetworkEvent::NewRoute(_)
        | NetworkEvent::DelRoute(_)
        | NetworkEvent::DelNeighbor(_) => true,
        NetworkEvent::NewNeighbor(n) => {
            matches!(n.state(), NeighborState::Failed | NeighborState::Incomplete)
        }
        _ => false,
    }
}

/// One row of the recent-events ring, served via `@/query/events`.
///
/// Defined locally (not in `zensight-common`) because this sensor owns only its
/// own crate; the GUI decoder mirrors this shape from the JSON reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Wall-clock seconds since the Unix epoch when observed.
    pub ts_unix: u64,
    /// `"link"` / `"addr"` / `"route"` / `"neighbor"`.
    pub family: String,
    /// `"added"` / `"removed"` / `"changed"`.
    pub action: String,
    /// Interface index, when the event carries one.
    pub ifindex: Option<u32>,
    /// Short human detail: iface name, IP, or route destination.
    pub detail: String,
}

/// Build a recent-events [`EventRecord`] for an event observed at `now_unix`.
/// Returns `None` for unsupported families. `action` carries the refined
/// add/remove/change sense already decided by the caller.
pub fn event_record(ev: &NetworkEvent, action: EventAction, now_unix: u64) -> Option<EventRecord> {
    let class = classify_event(ev)?;
    let detail = event_detail(ev);
    Some(EventRecord {
        ts_unix: now_unix,
        family: class.family.label().to_string(),
        action: action.label().to_string(),
        ifindex: ev.ifindex(),
        detail,
    })
}

/// A short, human-readable descriptor for the event (best-effort).
fn event_detail(ev: &NetworkEvent) -> String {
    if let Some(link) = ev.as_link() {
        return link.name_or("?").to_string();
    }
    if let Some(addr) = ev.as_address() {
        if let Some(ip) = addr.address().or_else(|| addr.local()) {
            return format!("{}/{}", ip, addr.prefix_len());
        }
        return format!("ifindex {}", addr.ifindex());
    }
    if let Some(rt) = ev.as_route() {
        if rt.is_default() {
            return "default".to_string();
        }
        if let Some(d) = rt.destination() {
            return format!("{}/{}", d, rt.dst_len());
        }
        return format!("/{}", rt.dst_len());
    }
    if let Some(nb) = ev.as_neighbor() {
        return nb
            .destination()
            .map(|d| d.to_string())
            .unwrap_or_else(|| format!("ifindex {}", nb.ifindex()));
    }
    "?".to_string()
}

/// Atomics for event counters + a bounded recent-events ring. Cloning is cheap
/// (`Arc`); shared between the collector's event task and the query channel.
#[derive(Clone)]
pub struct EventState {
    inner: Arc<EventStateInner>,
}

struct EventStateInner {
    /// `[family][action]` counters.
    counters: [[AtomicU64; 3]; 4],
    /// Known link ifindexes, to tell an `add` from a `change` (`RTM_NEWLINK`).
    seen_links: Mutex<HashSet<u32>>,
    /// Most-recent events (drop-oldest), bounded by `capacity`.
    ring: Mutex<VecDeque<EventRecord>>,
    capacity: usize,
}

impl EventState {
    /// Create with a bounded recent-events ring (`capacity` rows).
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(EventStateInner {
                counters: Default::default(),
                seen_links: Mutex::new(HashSet::new()),
                ring: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
                capacity: capacity.max(1),
            }),
        }
    }

    /// Observe an event at the current wall-clock time. Returns the refined
    /// [`EventAction`] applied, or `None` for unsupported families.
    pub fn observe(&self, ev: &NetworkEvent) -> Option<EventAction> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.observe_at(ev, now)
    }

    /// Observe an event stamped at `now_unix` (testable form of [`observe`]).
    pub fn observe_at(&self, ev: &NetworkEvent, now_unix: u64) -> Option<EventAction> {
        let class = classify_event(ev)?;
        let action = self.refine_action(&class, ev);
        self.inner.counters[class.family.index()][action.index()].fetch_add(1, Ordering::Relaxed);
        if let Some(rec) = event_record(ev, action, now_unix) {
            let mut ring = self.inner.ring.lock().unwrap();
            if ring.len() == self.inner.capacity {
                ring.pop_front();
            }
            ring.push_back(rec);
        }
        Some(action)
    }

    /// Decide add vs change vs remove. Only links distinguish add from change
    /// (a `RTM_NEWLINK` for a known ifindex is a state change).
    fn refine_action(&self, class: &EventClass, ev: &NetworkEvent) -> EventAction {
        if !class.is_new {
            if class.family == EventFamily::Link
                && let Some(idx) = ev.ifindex()
            {
                self.inner.seen_links.lock().unwrap().remove(&idx);
            }
            return EventAction::Removed;
        }
        if class.family == EventFamily::Link
            && let Some(idx) = ev.ifindex()
        {
            let mut seen = self.inner.seen_links.lock().unwrap();
            if seen.insert(idx) {
                return EventAction::Added;
            }
            return EventAction::Changed;
        }
        EventAction::Added
    }

    /// Current counter values as telemetry points
    /// (`events/<family>/<action>_total`).
    pub fn counter_points(&self, host: &str) -> Vec<TelemetryPoint> {
        let mut out = Vec::with_capacity(12);
        for family in EventFamily::ALL {
            for action in EventAction::ALL {
                let v = self.inner.counters[family.index()][action.index()].load(Ordering::Relaxed);
                out.push(TelemetryPoint::new(
                    host,
                    Protocol::Netlink,
                    format!("events/{}/{}_total", family.label(), action.label()),
                    TelemetryValue::Counter(v),
                ));
            }
        }
        out
    }

    /// Snapshot of the recent-events ring (oldest first).
    pub fn recent(&self) -> Vec<EventRecord> {
        self.inner.ring.lock().unwrap().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nlink::netlink::messages::{LinkMessageBuilder, NeighborMessageBuilder};

    // Synthetic link event: builders are unprivileged, no live socket needed.
    fn new_link(idx: u32, name: &str) -> NetworkEvent {
        let m = LinkMessageBuilder::new()
            .ifindex(idx as i32)
            .name(name)
            .build();
        NetworkEvent::NewLink(m)
    }
    fn del_link(idx: u32, name: &str) -> NetworkEvent {
        let m = LinkMessageBuilder::new()
            .ifindex(idx as i32)
            .name(name)
            .build();
        NetworkEvent::DelLink(m)
    }

    #[test]
    fn classify_maps_families() {
        let c = classify_event(&new_link(2, "eth0")).unwrap();
        assert_eq!(c.family, EventFamily::Link);
        assert!(c.is_new);
        let c = classify_event(&del_link(2, "eth0")).unwrap();
        assert!(!c.is_new);
    }

    #[test]
    fn link_add_then_change_then_remove() {
        let st = EventState::new(8);
        // First NEWLINK for ifindex 2 → added.
        assert_eq!(
            st.observe_at(&new_link(2, "eth0"), 100),
            Some(EventAction::Added)
        );
        // Second NEWLINK same ifindex → changed.
        assert_eq!(
            st.observe_at(&new_link(2, "eth0"), 101),
            Some(EventAction::Changed)
        );
        // DELLINK → removed.
        assert_eq!(
            st.observe_at(&del_link(2, "eth0"), 102),
            Some(EventAction::Removed)
        );
        // After removal a fresh NEWLINK is an add again.
        assert_eq!(
            st.observe_at(&new_link(2, "eth0"), 103),
            Some(EventAction::Added)
        );

        let pts = st.counter_points("h");
        let find = |m: &str| {
            pts.iter()
                .find(|p| p.metric == m)
                .map(|p| p.value.clone())
                .unwrap()
        };
        assert_eq!(find("events/link/added_total"), TelemetryValue::Counter(2));
        assert_eq!(
            find("events/link/changed_total"),
            TelemetryValue::Counter(1)
        );
        assert_eq!(
            find("events/link/removed_total"),
            TelemetryValue::Counter(1)
        );
    }

    #[test]
    fn ring_is_bounded_drop_oldest() {
        let st = EventState::new(2);
        st.observe_at(&new_link(1, "a"), 1);
        st.observe_at(&new_link(2, "b"), 2);
        st.observe_at(&new_link(3, "c"), 3);
        let recent = st.recent();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].detail, "b");
        assert_eq!(recent[1].detail, "c");
        assert_eq!(recent[1].action, "added");
        assert_eq!(recent[1].family, "link");
    }

    #[test]
    fn sentinel_relevance() {
        assert!(is_sentinel_relevant(&del_link(2, "eth0")));
        assert!(is_sentinel_relevant(&new_link(2, "eth0")));
        // A healthy new neighbor is not relevant; a failed/incomplete one is.
        let reachable = NeighborMessageBuilder::new()
            .ifindex(2)
            .state(NeighborState::Reachable)
            .build();
        assert!(!is_sentinel_relevant(&NetworkEvent::NewNeighbor(reachable)));
        let failed = NeighborMessageBuilder::new()
            .ifindex(2)
            .state(NeighborState::Failed)
            .build();
        assert!(is_sentinel_relevant(&NetworkEvent::NewNeighbor(failed)));
    }
}
