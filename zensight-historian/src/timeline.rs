//! Events and alert transitions into the durable timeline (#908).
//!
//! Two subscribers, one table. The tiers answer "what was this number"; this
//! answers "what happened", and the two are different questions that want
//! different storage — downsampling a transition would be meaningless.
//!
//! # Why both subscribers ask for history
//!
//! **Events** are rare, deliberate records that nothing will restate: a missed
//! trap is not a gap in a chart, it is an event that never existed as far as
//! every later reader is concerned. The plane is append-only with immutable
//! ULID keys, so a replay is idempotent for a consumer keyed by the record's
//! own id — which this is.
//!
//! **Alerts** are LWW state, and a replay of "what is currently firing" would
//! be a lie about *when* if the row were minted fresh. It is not: the row's key
//! is derived from `(ts, kind, key, active)`, so a replayed put overwrites the
//! row it already wrote and the answer to "when did this fire" stays "once,
//! then". That is what makes history safe to ask for here, and it is why the
//! uid is derived rather than generated — see [`zensight_store::timeline`].
//!
//! A tombstone (a `delete` sample) is the clear. An alert timeline that
//! recorded only firings would show every incident as permanent, and the clear
//! is the half that says whether anyone still needs to look.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;
use zensight_store::timeline::{TimelineKind, TimelineRow};

use crate::ingest::SharedStore;

/// What the timeline subscribers have seen.
#[derive(Debug, Default)]
pub struct TimelineCounters {
    /// Event records stored.
    pub events: AtomicU64,
    /// Alert transitions stored (fires and clears both).
    pub alerts: AtomicU64,
    /// Samples whose class guard or payload rejected them.
    pub rejected: AtomicU64,
}

/// Subscribe `v1/*/events/**` and record each event on the timeline.
pub async fn run_events(
    session: Arc<zenoh::Session>,
    key_expr: String,
    store: SharedStore,
    counters: Arc<TimelineCounters>,
    mut shutdown: watch::Receiver<bool>,
) {
    let subscriber =
        match zensight_common::subscribe::declare_events_subscriber(&session, &key_expr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, key_expr = %key_expr,
                    "historian: events subscriber failed to declare; no event history \
                     will be recorded");
                return;
            }
        };
    tracing::info!(key_expr = %key_expr, "historian: recording fleet events");

    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { return; },
            sample = subscriber.recv_async() => {
                let Ok(sample) = sample else {
                    tracing::warn!("historian: events subscriber closed");
                    return;
                };
                let key = sample.key_expr().as_str().to_string();
                match zensight_common::subscribe::decode_event(&sample) {
                    Ok(ev) => {
                        let origin = origin_of(&key).unwrap_or_default();
                        let row = TimelineRow::new(
                            ev.timestamp,
                            TimelineKind::Event,
                            origin,
                            &key,
                            // An event happened; there is no "un-happening" of
                            // one. `active` is what an alert's clear sets to
                            // false, and an event has no counterpart.
                            true,
                            Some(ev.summary.clone()),
                        );
                        record(&store, row, &counters.events);
                        // The full record goes in the events table too, so a
                        // reader that wants the payload and not just the line
                        // has somewhere to get it.
                        let mut g = store.lock().unwrap_or_else(|e| e.into_inner());
                        g.record_event(ev);
                    }
                    Err(_) => { counters.rejected.fetch_add(1, Ordering::Relaxed); }
                }
            }
        }
    }
}

/// Subscribe `v1/*/state/*/alert/*` and record each transition.
///
/// A **plain** subscriber plus an explicit history GET would be the other
/// shape; this uses the advanced one because the derived uid makes the replay
/// harmless and the recovery is worth having: an alert that fired while the
/// historian was between restarts is exactly the transition someone will go
/// looking for.
pub async fn run_alerts(
    session: Arc<zenoh::Session>,
    store: SharedStore,
    counters: Arc<TimelineCounters>,
    mut shutdown: watch::Receiver<bool>,
) {
    let key_expr = zensight_common::keyexpr::all_alerts_wildcard();
    let subscriber =
        match zensight_common::subscribe::declare_events_subscriber(&session, &key_expr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, key_expr = %key_expr,
                    "historian: alert subscriber failed to declare; no alert history \
                     will be recorded");
                return;
            }
        };
    tracing::info!(key_expr = %key_expr, "historian: recording alert transitions");

    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { return; },
            sample = subscriber.recv_async() => {
                let Ok(sample) = sample else {
                    tracing::warn!("historian: alert subscriber closed");
                    return;
                };
                let key = sample.key_expr().as_str().to_string();
                let Some(origin) = origin_of(&key) else {
                    counters.rejected.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                // Two ways an alert stops: a `Resolved` document, and a
                // tombstone. Both are clears, and a timeline that only
                // understood one would show half the incidents as permanent —
                // which half depending on which producer published them.
                let is_put = sample.kind() == zenoh::sample::SampleKind::Put;
                let (ts, active, summary) = if is_put {
                    match zensight_common::decode_auto::<zensight_common::Alert>(
                        &sample.payload().to_bytes(),
                    ) {
                        Ok(a) => (
                            a.timestamp,
                            a.state == zensight_common::AlertState::Firing,
                            Some(a.summary.clone()),
                        ),
                        Err(_) => {
                            counters.rejected.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    }
                } else {
                    // A tombstone carries no payload to summarise, and no
                    // timestamp of its own that this process can read — the
                    // clear happened when the delete was published, which is
                    // now as far as this subscriber can tell.
                    (now_ms(), false, None)
                };
                let row = TimelineRow::new(ts, TimelineKind::Alert, origin, &key, active, summary);
                record(&store, row, &counters.alerts);
            }
        }
    }
}

/// Buffer one row for the next flush.
fn record(store: &SharedStore, row: TimelineRow, counter: &AtomicU64) {
    let mut g = store.lock().unwrap_or_else(|e| e.into_inner());
    g.record_timeline(row);
    counter.fetch_add(1, Ordering::Relaxed);
}

/// The origin chunk of a v1 key.
fn origin_of(key: &str) -> Option<String> {
    zensight_common::keyexpr::parse_key(key).map(|k| k.origin.chunk().to_string())
}

fn now_ms() -> i64 {
    zensight_common::telemetry::current_timestamp_millis()
}
