//! Fleet telemetry ingest into the tiers (#906).
//!
//! One subscriber, one store behind a mutex, and two timers. The subscriber is
//! the shared [`zensight_common::subscribe::declare_telemetry_subscriber`] —
//! an `AdvancedSubscriber` with history, recovery and late-publisher detection,
//! which is also what the CI ban on hand-rolled `declare_subscriber` enforces.
//! For a history service those are not niceties: a gap in a chart is a claim
//! about the world, and "the service started after the sensor" is not a reason
//! to make it.
//!
//! # Where the series name comes from
//!
//! The **key**, not the payload. `(origin, producer, subject)` is the wire key
//! minus the class chunk, and the payload cannot reconstruct it: for a proxy
//! producer the subject is `{device}/{metric...}` while
//! [`zensight_common::TelemetryPoint::metric`] is only the second half. Taking
//! the name from the key is also what makes it the same name the GUI's local
//! cache uses (#904), which is what lets a chart fall back between them.
//!
//! # What is dropped, and counted
//!
//! A sample is dropped when its key does not parse as a telemetry key, when
//! its payload decodes as neither JSON nor CBOR, or when its value is text or
//! binary — not a numeric series, and a fabricated `0.0` would be a claim.
//! Each is counted separately, because they are three different faults: the
//! first is a selector that reaches too far, the second is a producer on a
//! format nobody expected, the third is normal and expected.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use zensight_common::subscribe::{DecodeReject, decode_telemetry};
use zensight_store::MetricStore;

/// The shared store handle.
///
/// A `std::sync::Mutex`, not tokio's: every critical section here is a bounded
/// in-memory append or a batch hand-off, and nothing awaits while holding it.
/// The redb work happens *after* the lock is dropped, on a blocking thread.
pub type SharedStore = Arc<std::sync::Mutex<MetricStore>>;

/// What ingest has seen, for the health document and `@rpc/historian/stats`.
///
/// Every counter is reported even at zero. "Nothing was dropped" and "nobody
/// asked" are different states, and a metric that only appears once it is
/// nonzero cannot tell them apart.
#[derive(Debug, Default)]
pub struct IngestCounters {
    /// Samples recorded into a series.
    pub recorded: AtomicU64,
    /// Keys the telemetry-class guard rejected — a selector reaching further
    /// than the class it names.
    pub not_telemetry: AtomicU64,
    /// Payloads that decoded as neither JSON nor CBOR.
    pub undecodable: AtomicU64,
    /// Text and binary values: not numeric series. Expected, and counted so
    /// that "the database is empty" can be told apart from "everything
    /// arriving is text".
    pub non_numeric: AtomicU64,
    /// Samples shed by the governor while degraded.
    pub shed: AtomicU64,
}

impl IngestCounters {
    /// Everything that arrived and was not recorded.
    pub fn dropped_total(&self) -> u64 {
        self.not_telemetry.load(Ordering::Relaxed)
            + self.undecodable.load(Ordering::Relaxed)
            + self.non_numeric.load(Ordering::Relaxed)
            + self.shed.load(Ordering::Relaxed)
    }
}

/// Run the ingest loop until `shutdown` flips.
pub async fn run(
    session: Arc<zenoh::Session>,
    key_expr: String,
    store: SharedStore,
    counters: Arc<IngestCounters>,
    shedding: Arc<std::sync::atomic::AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let subscriber =
        match zensight_common::subscribe::declare_telemetry_subscriber(&session, &key_expr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, key_expr = %key_expr,
                    "historian: telemetry subscriber failed to declare; no history will be \
                     recorded");
                return;
            }
        };
    tracing::info!(key_expr = %key_expr, "historian: ingesting fleet telemetry");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    tracing::info!("historian: ingest stopping");
                    return;
                }
            }
            sample = subscriber.recv_async() => {
                let Ok(sample) = sample else {
                    tracing::warn!("historian: telemetry subscriber closed");
                    return;
                };
                record_sample(&sample, &store, &counters, &shedding);
            }
        }
    }
}

/// Decode one sample and record it.
pub fn record_sample(
    sample: &zenoh::sample::Sample,
    store: &SharedStore,
    counters: &IngestCounters,
    shedding: &std::sync::atomic::AtomicBool,
) {
    let key = sample.key_expr().as_str();
    match decode_telemetry(sample) {
        Ok(point) => record_point(key, &point, store, counters, shedding),
        Err(DecodeReject::NotTelemetry) => {
            counters.not_telemetry.fetch_add(1, Ordering::Relaxed);
        }
        Err(DecodeReject::Undecodable) => {
            counters.undecodable.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(key = %key, "historian: payload decoded as neither JSON nor CBOR");
        }
    }
}

/// Record one already-decoded point that arrived on `key`.
///
/// The whole ingest rule, minus the decode: which values are series, what is
/// shed, and where the series name comes from. Taking `(key, point)` rather
/// than a `Sample` is what makes it testable without standing up a session —
/// and a rule that can only be exercised through a live bus is a rule that
/// gets exercised rarely.
pub fn record_point(
    key: &str,
    point: &zensight_common::TelemetryPoint,
    store: &SharedStore,
    counters: &IngestCounters,
    shedding: &std::sync::atomic::AtomicBool,
) {
    use zensight_common::TelemetryValue as V;

    // Text and binary are not numeric series, and a fabricated `0.0` would be
    // a claim. Counted rather than ignored so "the database is empty" and
    // "everything arriving is text" are distinguishable without a capture.
    if matches!(point.value, V::Text(_) | V::Binary(_)) {
        counters.non_numeric.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // Shedding drops bools first: a 0/1 step series is the cheapest history to
    // lose and the easiest to re-derive — the alert that made it interesting
    // is on the bus anyway.
    if shedding.load(Ordering::Relaxed) && matches!(point.value, V::Boolean(_)) {
        counters.shed.fetch_add(1, Ordering::Relaxed);
        return;
    }

    let Some((origin, subject)) = series_of(key) else {
        // The class guard passed, so this is a v1 telemetry key whose origin
        // or subject we could not name — structurally impossible today, and
        // worth a counter rather than a panic if the grammar ever widens.
        counters.not_telemetry.fetch_add(1, Ordering::Relaxed);
        return;
    };

    // A poisoned lock means a previous holder panicked. The store is a plain
    // in-memory structure with no invariant a panic can half-break, so
    // recovering beats losing every subsequent sample.
    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
    s.record(&origin, &subject, point);
    counters.recorded.fetch_add(1, Ordering::Relaxed);
}

/// `(origin, subject)` for a telemetry key, from the key alone.
///
/// The producer is not returned: it is `point.protocol`, which the store takes
/// from the payload, and the two agree by construction — the producer chunk is
/// what the sensor's own `V1Context` built the key from.
fn series_of(key: &str) -> Option<(String, String)> {
    let parsed = zensight_common::keyexpr::parse_key(key)?;
    let origin = parsed.origin.chunk().to_string();
    if parsed.subject.is_empty() {
        return None;
    }
    Some((origin, parsed.subject.join("/")))
}

/// Flush pending samples to disk on an interval, off the runtime.
pub async fn flush_loop(
    store: SharedStore,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    // One last flush: a service told to stop should not throw
                    // away the samples it has already accepted.
                    flush_once(&store).await;
                    return;
                }
            }
            _ = ticker.tick() => flush_once(&store).await,
        }
    }
}

/// Take everything pending — samples, event records and timeline rows — and
/// write it.
///
/// Three transactions, not one: they are different tables answering different
/// questions, and a sample batch that failed should not take an alert
/// transition down with it. The transitions and the events are the rarer and
/// less replaceable of the three — a telemetry sample will be restated a
/// second later, and a trap will not.
///
/// Every batch that is buffered must be taken here. `record_event` buffers,
/// and for a while nothing drained it: the events reached the store, sat in
/// memory, and vanished on restart — which the timeline's own restart test
/// caught, because "events survive a historian restart" is #908's first
/// acceptance and it did not.
pub async fn flush_once(store: &SharedStore) {
    let (samples, events, timeline) = {
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        (
            s.take_flush_batch(),
            s.take_event_flush_batch(),
            s.take_timeline_flush_batch(),
        )
    };

    if let Some((handle, batch)) = samples {
        let rows = batch.rows.len();
        match tokio::task::spawn_blocking(move || handle.write_batch(&batch)).await {
            Ok(Ok(written)) => tracing::debug!(written, rows, "historian: flushed samples"),
            Ok(Err(e)) => tracing::warn!(error = %e, rows, "historian: sample flush failed"),
            Err(e) => tracing::warn!(error = %e, "historian: sample flush task panicked"),
        }
    }

    if let Some((handle, records)) = events {
        let n = records.len();
        match tokio::task::spawn_blocking(move || handle.write_events(&records)).await {
            Ok(Ok(written)) => tracing::debug!(written, "historian: flushed events"),
            Ok(Err(e)) => tracing::warn!(error = %e, n, "historian: event flush failed"),
            Err(e) => tracing::warn!(error = %e, "historian: event flush task panicked"),
        }
    }

    if let Some((handle, rows)) = timeline {
        let n = rows.len();
        match tokio::task::spawn_blocking(move || handle.write_timeline(&rows)).await {
            Ok(Ok(written)) => tracing::debug!(written, "historian: flushed timeline"),
            Ok(Err(e)) => tracing::warn!(error = %e, n, "historian: timeline flush failed"),
            Err(e) => tracing::warn!(error = %e, "historian: timeline flush task panicked"),
        }
    }
}

/// Apply retention on an interval, off the runtime.
pub async fn prune_loop(
    store: SharedStore,
    interval: Duration,
    last_prune_ms: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; skip it so a restart loop cannot turn
    // into a prune loop.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = ticker.tick() => {
                let Some(handle) = ({
                    let s = store.lock().unwrap_or_else(|e| e.into_inner());
                    s.persistent()
                }) else { continue };
                let started = std::time::Instant::now();
                match tokio::task::spawn_blocking(move || {
                    let now_ms = zensight_common::telemetry::current_timestamp_millis();
                    let tiers = handle.prune(now_ms)?;
                    // The timeline is bounded by row count, not by age: how
                    // far back a reader can scrub is the question it answers,
                    // and a transition does not become less interesting for
                    // being old.
                    let timeline =
                        handle.prune_timeline(zensight_store::TIMELINE_STORE_MAX_ROWS)?;
                    let events = handle.prune_events(zensight_store::EVENT_STORE_MAX_ROWS)?;
                    Ok::<_, zensight_store::redb::Error>(tiers + timeline + events)
                })
                .await
                {
                    Ok(Ok(removed)) => {
                        let ms = started.elapsed().as_millis() as u64;
                        last_prune_ms.store(ms, Ordering::Relaxed);
                        tracing::info!(removed, ms, "historian: retention applied");
                    }
                    Ok(Err(e)) => tracing::warn!(error = %e, "historian: prune failed"),
                    Err(e) => tracing::warn!(error = %e, "historian: prune task panicked"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

    fn store() -> SharedStore {
        Arc::new(std::sync::Mutex::new(MetricStore::new(64, None)))
    }

    fn point(protocol: Protocol, metric: &str, value: TelemetryValue) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: 1_000,
            source: "dev1".to_string(),
            protocol,
            metric: metric.to_string(),
            value,
            labels: Default::default(),
            unit: None,
        }
    }

    /// The series name comes from the KEY, not the payload — and for a proxy
    /// producer the two differ. snmp's wire subject is `{device}/{metric...}`
    /// while `TelemetryPoint::metric` is only the second half, so a store that
    /// rebuilt the path from the payload would file `sw1`'s interface counter
    /// under a series with no device in it, and every polled device's counters
    /// would land in one another's.
    #[test]
    fn the_series_name_comes_from_the_key_not_the_payload() {
        let s = store();
        let c = IngestCounters::default();
        let shed = AtomicBool::new(false);

        record_point(
            "v1/h-0123456789ab/telemetry/snmp/sw1/if/3/in_octets",
            &point(
                Protocol::Snmp,
                "if/3/in_octets",
                TelemetryValue::Counter(10),
            ),
            &s,
            &c,
            &shed,
        );

        let g = s.lock().unwrap();
        assert_eq!(
            g.interner()
                .with_prefix("h-0123456789ab/snmp/")
                .map(|(_, p)| p.to_string())
                .collect::<Vec<_>>(),
            vec!["h-0123456789ab/snmp/sw1/if/3/in_octets".to_string()],
            "the device chunk is part of the subject and must survive into the series path"
        );
        assert_eq!(c.recorded.load(Ordering::Relaxed), 1);
    }

    /// The kind is taken from the value, and it is the distinction the whole
    /// of #904 exists to keep: a counter that goes backwards restarted, a
    /// gauge that goes backwards fell.
    #[test]
    fn the_kind_follows_the_value() {
        let s = store();
        let c = IngestCounters::default();
        let shed = AtomicBool::new(false);
        let base = "v1/h-0123456789ab/telemetry/sysinfo";

        for (subject, value, want) in [
            (
                "network/eth0/rx_bytes",
                TelemetryValue::Counter(1),
                zensight_store::MetricKind::Counter,
            ),
            (
                "system/load",
                TelemetryValue::Gauge(0.5),
                zensight_store::MetricKind::Gauge,
            ),
            (
                "network/eth0/carrier",
                TelemetryValue::Boolean(true),
                zensight_store::MetricKind::Bool,
            ),
        ] {
            record_point(
                &format!("{base}/{subject}"),
                &point(Protocol::Sysinfo, subject, value),
                &s,
                &c,
                &shed,
            );
            let g = s.lock().unwrap();
            let path = format!("h-0123456789ab/sysinfo/{subject}");
            let id = g.interner().get(&path).expect("interned");
            assert_eq!(
                g.interner().meta(id).map(|m| m.kind),
                Some(want),
                "{subject} must be recorded as {want}"
            );
        }
        assert_eq!(c.recorded.load(Ordering::Relaxed), 3);
    }

    /// Text and binary are not numeric series. Counted, not silently ignored:
    /// "the database is empty" and "everything arriving is text" are different
    /// diagnoses and only the counter tells them apart.
    #[test]
    fn text_and_binary_are_counted_rather_than_coerced() {
        let s = store();
        let c = IngestCounters::default();
        let shed = AtomicBool::new(false);
        let key = "v1/h-0123456789ab/telemetry/logs/line";

        record_point(
            key,
            &point(Protocol::Logs, "line", TelemetryValue::Text("hello".into())),
            &s,
            &c,
            &shed,
        );
        record_point(
            key,
            &point(Protocol::Logs, "line", TelemetryValue::Binary(vec![1, 2])),
            &s,
            &c,
            &shed,
        );

        assert_eq!(c.recorded.load(Ordering::Relaxed), 0);
        assert_eq!(c.non_numeric.load(Ordering::Relaxed), 2);
        assert_eq!(c.dropped_total(), 2);
        assert!(
            s.lock().unwrap().interner().is_empty(),
            "nothing was interned"
        );
    }

    /// Shedding drops booleans and nothing else: the point of degrading is to
    /// keep the expensive, irreplaceable series while giving up the ones an
    /// alert already covers.
    #[test]
    fn shedding_drops_booleans_and_keeps_the_rest() {
        let s = store();
        let c = IngestCounters::default();
        let shed = AtomicBool::new(true);
        let base = "v1/h-0123456789ab/telemetry/sysinfo";

        record_point(
            &format!("{base}/network/eth0/carrier"),
            &point(
                Protocol::Sysinfo,
                "network/eth0/carrier",
                TelemetryValue::Boolean(true),
            ),
            &s,
            &c,
            &shed,
        );
        record_point(
            &format!("{base}/system/load"),
            &point(Protocol::Sysinfo, "system/load", TelemetryValue::Gauge(0.5)),
            &s,
            &c,
            &shed,
        );

        assert_eq!(c.shed.load(Ordering::Relaxed), 1);
        assert_eq!(
            c.recorded.load(Ordering::Relaxed),
            1,
            "the gauge still lands"
        );

        // …and restoring lets the bool back in. The governor replays
        // transitions, so this must be a switch, not a latch.
        shed.store(false, Ordering::Relaxed);
        record_point(
            &format!("{base}/network/eth0/carrier"),
            &point(
                Protocol::Sysinfo,
                "network/eth0/carrier",
                TelemetryValue::Boolean(false),
            ),
            &s,
            &c,
            &shed,
        );
        assert_eq!(c.recorded.load(Ordering::Relaxed), 2);
    }

    /// A key outside the telemetry class never reaches the store. The
    /// subscriber's selector already scopes this, but the selector is
    /// operator-overridable and a widened one must not smuggle state or
    /// `@rpc` keys into the metric path.
    #[test]
    fn a_non_telemetry_key_is_refused_by_the_name_parse() {
        assert_eq!(
            series_of("v1/h-0123456789ab/telemetry/sysinfo/system/load"),
            Some(("h-0123456789ab".to_string(), "system/load".to_string()))
        );
        assert_eq!(series_of("not a key at all"), None);
        // A telemetry key with no subject names no series.
        assert_eq!(series_of("v1/h-0123456789ab/telemetry/sysinfo"), None);
    }

    /// Every counter is reported even at zero: "nothing was dropped" and
    /// "nobody asked" are different states.
    #[test]
    fn dropped_total_sums_every_reason() {
        let c = IngestCounters::default();
        assert_eq!(c.dropped_total(), 0);
        c.not_telemetry.fetch_add(2, Ordering::Relaxed);
        c.undecodable.fetch_add(3, Ordering::Relaxed);
        c.non_numeric.fetch_add(5, Ordering::Relaxed);
        c.shed.fetch_add(7, Ordering::Relaxed);
        assert_eq!(c.dropped_total(), 17);
    }
}
