//! A live round trip for `@rpc/historian/range` and `/series` (#907).
//!
//! In the `zensight-sensor-logs::query::events_queryable_round_trip` style: an
//! in-process session with multicast scouting off, because a test that scouts
//! is not a test, it is a participant.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use zensight_common::history::{RangeReply, SeriesInfo};
use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};
use zensight_historian::ingest::{IngestCounters, SharedStore, record_point};
use zensight_historian::query::range;

fn point(protocol: Protocol, metric: &str, value: TelemetryValue, ts: i64) -> TelemetryPoint {
    TelemetryPoint {
        timestamp: ts,
        source: "dev1".to_string(),
        protocol,
        metric: metric.to_string(),
        value,
        labels: Default::default(),
        unit: Some("By".to_string()),
    }
}

async fn session() -> Arc<zenoh::Session> {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    Arc::new(zenoh::open(config).await.unwrap())
}

/// Publish a counter and a gauge into the store, then GET `range` for each
/// aggregate over a live session and check what comes back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn range_answers_each_aggregate_over_a_live_session() {
    let session = session().await;
    let store: SharedStore = Arc::new(std::sync::Mutex::new(zensight_store::MetricStore::new(
        4_096, None,
    )));
    let counters = IngestCounters::default();
    let shed = AtomicBool::new(false);

    // A counter climbing 100/s and a gauge, two minutes of per-second samples.
    let origin = "h-0123456789ab";
    for i in 0..120i64 {
        let ts = i * 1_000;
        record_point(
            &format!("v1/{origin}/telemetry/sysinfo/network/eth0/rx_bytes"),
            &point(
                Protocol::Sysinfo,
                "network/eth0/rx_bytes",
                TelemetryValue::Counter((i * 100) as u64),
                ts,
            ),
            &store,
            &counters,
            &shed,
        );
        record_point(
            &format!("v1/{origin}/telemetry/sysinfo/system/load"),
            &point(
                Protocol::Sysinfo,
                "system/load",
                TelemetryValue::Gauge(i as f64),
                ts,
            ),
            &store,
            &counters,
            &shed,
        );
    }

    let ctx = zensight_sensor_core::v1::for_producer("historian");
    let key = ctx.rpc_key(&["range"]).unwrap().to_string();
    let _h = range::serve_range(
        session.clone(),
        ctx.clone(),
        store.clone(),
        origin.to_string(),
    )
    .await
    .expect("range serves");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // A sub-minute step reads the hot ring, which is where the per-second
    // resolution lives.
    let sel = format!("{key}?from=0;to=200000;step=1;agg=last");
    let reply: RangeReply = get_one(&session, &sel).await;
    assert_eq!(reply.historian, origin);
    assert_eq!(reply.step_s, 1);
    assert_eq!(reply.series.len(), 2, "both series match */*/**");
    assert!(!reply.truncated);
    assert!(
        reply.next_cursor.is_none(),
        "a complete window has no cursor"
    );

    // A counter defaults to `rate`, and a steady 100/s counter reads as 100/s.
    let sel = format!("{key}?from=0;to=200000;step=1;subject=network/eth0/rx_bytes");
    let reply: RangeReply = get_one(&session, &sel).await;
    assert_eq!(reply.series.len(), 1);
    let s = &reply.series[0];
    assert_eq!(s.agg, zensight_common::history::Aggregate::Rate);
    assert_eq!(s.kind, zensight_common::history::SeriesKind::Counter);
    assert_eq!(s.subject, "network/eth0/rx_bytes");
    assert_eq!(s.unit.as_deref(), Some("By"));
    for (_, v) in &s.points {
        assert!((v - 100.0).abs() < 1e-6, "steady 100/s, got {v}");
    }

    // …and a gauge defaults to `avg`, in the same reply shape.
    let sel = format!("{key}?from=0;to=200000;step=1;subject=system/load");
    let reply: RangeReply = get_one(&session, &sel).await;
    assert_eq!(
        reply.series[0].agg,
        zensight_common::history::Aggregate::Avg
    );

    // An unknown aggregate is an error reply, not a default.
    let sel = format!("{key}?agg=mean");
    let replies = session.get(&sel).await.unwrap();
    let r = replies.recv_async().await.unwrap();
    assert!(r.result().is_err(), "agg=mean must be an error reply");

    session.close().await.unwrap();
}

/// A window larger than `limit` is paged, and the pages reassemble into the
/// unpaged answer exactly once — no gap and no repeat. Pagination that quietly
/// skips is worse than pagination that refuses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pages_reassemble_into_the_unpaged_answer() {
    let session = session().await;
    let store: SharedStore = Arc::new(std::sync::Mutex::new(zensight_store::MetricStore::new(
        4_096, None,
    )));
    let counters = IngestCounters::default();
    let shed = AtomicBool::new(false);
    let origin = "h-0123456789ab";

    for i in 0..50i64 {
        for name in ["a", "b"] {
            record_point(
                &format!("v1/{origin}/telemetry/sysinfo/{name}"),
                &point(
                    Protocol::Sysinfo,
                    name,
                    TelemetryValue::Gauge(i as f64),
                    i * 1_000,
                ),
                &store,
                &counters,
                &shed,
            );
        }
    }

    let ctx = zensight_sensor_core::v1::for_producer("historian");
    let key = ctx.rpc_key(&["range"]).unwrap().to_string();
    let _h = range::serve_range(session.clone(), ctx, store, origin.to_string())
        .await
        .expect("range serves");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let base = format!("{key}?from=0;to=100000;step=1;agg=last");
    let whole: RangeReply = get_one(&session, &base).await;
    let total: usize = whole.series.iter().map(|s| s.points.len()).sum();
    assert!(total > 0);
    assert!(!whole.truncated);

    // Page it at a third of the total, following the cursor to the end.
    let mut seen: Vec<(String, i64, f64)> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let sel = match &cursor {
            Some(c) => format!("{base};limit={};cursor={c}", total / 3 + 1),
            None => format!("{base};limit={}", total / 3 + 1),
        };
        let page: RangeReply = get_one(&session, &sel).await;
        for s in &page.series {
            for (ts, v) in &s.points {
                seen.push((s.subject.clone(), *ts, *v));
            }
        }
        pages += 1;
        assert!(pages < 20, "pagination did not terminate");
        match page.next_cursor {
            Some(c) => {
                assert!(
                    page.truncated,
                    "a page with a cursor is truncated by definition"
                );
                cursor = Some(c);
            }
            None => break,
        }
    }
    assert!(pages > 1, "the window must actually have been paged");

    let mut expected: Vec<(String, i64, f64)> = Vec::new();
    for s in &whole.series {
        for (ts, v) in &s.points {
            expected.push((s.subject.clone(), *ts, *v));
        }
    }
    seen.sort_by(|a, b| a.partial_cmp(b).unwrap());
    expected.sort_by(|a, b| a.partial_cmp(b).unwrap());
    assert_eq!(
        seen, expected,
        "every point appears exactly once across the pages — no gap, no repeat"
    );

    session.close().await.unwrap();
}

async fn get_one<T: serde::de::DeserializeOwned>(session: &zenoh::Session, selector: &str) -> T {
    let replies = session.get(selector).await.unwrap();
    let reply = replies.recv_async().await.expect("a reply");
    let sample = reply.result().expect("a value reply, not an error");
    serde_json::from_slice(&sample.payload().to_bytes()).expect("decodes")
}

/// `series` lists what the historian holds, which is the call a caller makes
/// before it can ask a sensible range question.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn series_lists_what_is_held() {
    let session = session().await;
    let store: SharedStore = Arc::new(std::sync::Mutex::new(zensight_store::MetricStore::new(
        64, None,
    )));
    let counters = IngestCounters::default();
    let shed = AtomicBool::new(false);
    let origin = "h-0123456789ab";

    record_point(
        &format!("v1/{origin}/telemetry/snmp/sw1/if/3/in_octets"),
        &point(
            Protocol::Snmp,
            "if/3/in_octets",
            TelemetryValue::Counter(1),
            0,
        ),
        &store,
        &counters,
        &shed,
    );
    record_point(
        &format!("v1/{origin}/telemetry/sysinfo/system/load"),
        &point(
            Protocol::Sysinfo,
            "system/load",
            TelemetryValue::Gauge(1.0),
            0,
        ),
        &store,
        &counters,
        &shed,
    );

    let ctx = zensight_sensor_core::v1::for_producer("historian");
    let key = ctx.rpc_key(&["series"]).unwrap().to_string();
    let _h = range::serve_series(session.clone(), ctx, store)
        .await
        .expect("series serves");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let all: Vec<SeriesInfo> = get_one(&session, &key).await;
    assert_eq!(all.len(), 2);

    // The subject keeps the proxy producer's device chunk, and `source` names
    // the device it was observed on — neither is recoverable from the other.
    let snmp = all
        .iter()
        .find(|s| s.producer == "snmp")
        .expect("snmp series");
    assert_eq!(snmp.subject, "sw1/if/3/in_octets");
    assert_eq!(snmp.source.as_deref(), Some("dev1"));
    assert_eq!(snmp.kind, zensight_common::history::SeriesKind::Counter);

    // A producer filter narrows it, using the same key-expression matching.
    let only: Vec<SeriesInfo> = get_one(&session, &format!("{key}?producer=sysinfo")).await;
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].subject, "system/load");

    session.close().await.unwrap();
}
