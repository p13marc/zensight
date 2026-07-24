//! Socket→ring→query end-to-end tests (#548). Real localhost sockets, an
//! in-process isolated Zenoh session, no external services.

mod harness;

use std::time::Duration;

use harness::*;
use zensight_sensor_logs::config::{Framing, OverflowPolicy};

const RFC3164: &str = "<34>Oct 11 22:14:15 mymachine su: auth failure";
const DEADLINE: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn udp_datagram_reaches_the_ring() {
    let port = free_udp_port();
    let rig = RigBuilder::udp(port).start().await;

    send_udp(port, RFC3164).await;

    let records = rig.events_until(1, DEADLINE).await;
    assert_eq!(records.len(), 1, "one datagram → one record");
    assert_eq!(records[0].host, "mymachine");
    assert!(records[0].message.contains("auth failure"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_octet_framing_reaches_the_ring() {
    let port = free_tcp_port();
    let rig = RigBuilder::tcp(port, Framing::Auto).start().await;

    send_tcp_octet(port, &[RFC3164, RFC3164, RFC3164]).await;

    let records = rig.events_until(3, DEADLINE).await;
    assert_eq!(
        records.len(),
        3,
        "three octet-counted frames → three records"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_socket_reaches_the_ring() {
    let dir = tempdir();
    let path = dir.join("syslog.sock");
    let rig = RigBuilder::unix(&path).start().await;

    send_unix(&path, RFC3164).await;

    let records = rig.events_until(1, DEADLINE).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].host, "mymachine");
}

/// The `since`/`max` selectors on `@rpc/logs/events` filter and cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn events_selectors_filter_and_cap() {
    let port = free_udp_port();
    let rig = RigBuilder::udp(port).start().await;
    for i in 0..5 {
        send_udp(port, &format!("<34>Oct 11 22:14:15 host{i} su: line {i}")).await;
    }
    rig.events_until(5, DEADLINE).await;

    let capped = rig.events("max=2").await;
    assert_eq!(capped.len(), 2, "max caps the reply");
}

/// Backpressure: a parked (never-drained) channel under DropNewest sheds and
/// counts drops; a drained rig drops nothing for the same load.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backpressure_drops_are_counted() {
    let port = free_tcp_port();
    // Parked: intake loop not spawned, so the capacity-1000 channel fills.
    let rig = RigBuilder::tcp(port, Framing::Auto)
        .overflow(OverflowPolicy::DropNewest)
        .no_drain()
        .start()
        .await;

    // 3000 reliably-delivered framed messages over one TCP connection.
    let msgs: Vec<String> = (0..3000)
        .map(|i| format!("<34>Oct 11 22:14:15 h su: line {i}"))
        .collect();
    let refs: Vec<&str> = msgs.iter().map(String::as_str).collect();
    send_tcp_octet(port, &refs).await;

    // Give the listener time to drain the socket into the full channel.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let dropped = rig.ingest_stats.snapshot().dropped;
    assert!(
        dropped > 0,
        "a full channel under DropNewest must count drops"
    );
}

/// Raising `channel_capacity` measurably reduces drops for the same burst
/// (#546). Both rigs park the intake (never drain), so the only difference is
/// how many messages the channel can hold before `drop_newest` sheds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn larger_channel_capacity_reduces_drops() {
    async fn drops_for_capacity(cap: usize) -> u64 {
        let port = free_tcp_port();
        let rig = RigBuilder::tcp(port, Framing::Auto)
            .overflow(OverflowPolicy::DropNewest)
            .channel_capacity(cap)
            .no_drain()
            .start()
            .await;
        let msgs: Vec<String> = (0..3000)
            .map(|i| format!("<34>Oct 11 22:14:15 h su: line {i}"))
            .collect();
        let refs: Vec<&str> = msgs.iter().map(String::as_str).collect();
        send_tcp_octet(port, &refs).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        rig.ingest_stats.snapshot().dropped
    }

    let small = drops_for_capacity(100).await;
    let large = drops_for_capacity(5000).await;
    assert!(
        large < small,
        "a larger channel must drop fewer of the same burst (small={small}, large={large})"
    );
}

/// Repeat collapse (#546): a burst of identical lines folds into one ring
/// record carrying `repeat_count = N`; a following different line lands
/// separately.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_lines_collapse_with_repeat_count() {
    let port = free_udp_port();
    let rig = RigBuilder::udp(port)
        .collapse(Duration::from_millis(300))
        .start()
        .await;

    for _ in 0..50 {
        send_udp(port, "<34>Oct 11 22:14:15 h app: disk full").await;
    }
    // A different line closes the run and lands on its own.
    send_udp(port, "<34>Oct 11 22:14:15 h app: all clear").await;

    let records = rig.events_until(2, DEADLINE).await;
    let collapsed = records
        .iter()
        .find(|r| r.message.contains("disk full"))
        .expect("collapsed record present");
    assert_eq!(
        collapsed.labels.get("repeat_count").map(String::as_str),
        Some("50"),
        "the burst folds into one record counting all copies"
    );
    assert!(
        records.iter().any(|r| r.message.contains("all clear")),
        "the trailing different line lands separately"
    );
}

/// Multiline idle-flush: a single line buffered by the joiner is emitted after
/// an idle gap past `flush_timeout_ms` — without closing the connection. This
/// is the `select!` idle-flush arm in `handle_stream_connection` (previously
/// untested), distinct from the EOF-flush path the socket tests hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiline_idle_flush_emits_buffered_line() {
    let port = free_tcp_port();
    let rig = RigBuilder::tcp(port, Framing::Lf).start().await;

    let mut stream = tcp_connect(port).await;
    // One line: the joiner buffers it (has_pending) and, seeing no follow-up
    // within flush_timeout_ms (100), the idle-flush arm emits it — while the
    // connection stays open (so this is not the EOF path).
    send_line(
        &mut stream,
        "<34>Oct 11 22:14:15 idlehost app: buffered line",
    )
    .await;

    let records = rig.events_until(1, DEADLINE).await;
    assert_eq!(records.len(), 1, "idle flush emits the buffered line");
    assert_eq!(records[0].host, "idlehost");
    assert!(records[0].message.contains("buffered line"));
    let _ = stream; // hold the connection open across the poll
}

/// #584: a `<PRI>`-framed head + indented continuation over a stream listener is
/// folded by the `MultilineJoiner` and now arrives as ONE record — envelope from
/// the head, every continuation line in the message — instead of being dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiline_stacktrace_folds_into_one_record() {
    let port = free_tcp_port();
    let rig = RigBuilder::tcp(port, Framing::Lf).start().await;

    let mut stream = tcp_connect(port).await;
    // Head line + two indented continuation frames (`is_continuation`), then hold
    // the connection open so the idle-flush arm emits the folded record.
    send_line(
        &mut stream,
        "<11>Oct 11 22:14:15 web01 app: Traceback (most recent call last):",
    )
    .await;
    send_line(&mut stream, "    File \"x.py\", line 1, in <module>").await;
    send_line(&mut stream, "    raise ValueError(\"boom\")").await;

    let records = rig.events_until(1, DEADLINE).await;
    assert_eq!(
        records.len(),
        1,
        "the folded trace is one record, not dropped"
    );
    let rec = &records[0];
    assert_eq!(rec.host, "web01", "envelope host from the head line");
    assert_eq!(rec.severity, "err", "<11> = user.err, from the head");
    assert!(rec.message.contains("Traceback (most recent call last):"));
    assert!(rec.message.contains("File \"x.py\", line 1"));
    assert!(
        rec.message.contains("raise ValueError(\"boom\")"),
        "all continuation frames fold into the message, got {:?}",
        rec.message
    );
    let _ = stream;
}

/// Durable store (#544): the `events` queryable answers `from`/`to` +
/// `after_uid` pagination from the disk store, newest-first, in bounded pages —
/// history that survives beyond the ring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_store_serves_paginated_time_range() {
    use std::sync::Arc;
    use zensight_common::LogRecord;
    use zensight_sensor_logs::query;
    use zensight_sensor_logs::store::LogStore;

    fn rec(ts: i64, seq: u64, msg: &str) -> LogRecord {
        LogRecord {
            uid: format!("{:013}{:012}", ts, seq),
            ts,
            host: "web01".into(),
            facility: "daemon".into(),
            severity: "info".into(),
            severity_number: 9,
            app: None,
            pid: None,
            message: msg.into(),
            labels: Default::default(),
        }
    }

    let dir = tempdir();
    let store = Arc::new(LogStore::open(dir.join("logs.redb")).unwrap());
    // 20 records across ts 1000..1019.
    let recs: Vec<LogRecord> = (0..20)
        .map(|i| rec(1000 + i, i as u64, &format!("line {i}")))
        .collect();
    store.write_batch(&recs).unwrap();

    let session = Arc::new(
        zenoh::open(harness::isolated_config())
            .await
            .expect("open zenoh"),
    );
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let producer = format!("test_{nanos}/logs");
    let (ring, _cap) = query::new_ring(100); // empty ring; the store answers
    tokio::spawn(query::run_events(
        session.clone(),
        producer.clone(),
        ring,
        Some(store),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let events_key = zensight_common::command::query_key(&producer, "events");
    let get = |sel: String| {
        let session = session.clone();
        async move {
            let replies = session
                .get(&sel)
                .timeout(Duration::from_secs(5))
                .await
                .expect("get");
            let reply = replies.recv_async().await.expect("reply");
            let sample = reply.result().expect("ok");
            serde_json::from_slice::<Vec<LogRecord>>(&sample.payload().to_bytes()).expect("decode")
        }
    };

    // Time window [1005, 1012], first page of 4 → newest-first 1012..1009.
    let page1 = get(format!("{events_key}?from=1005;to=1012;limit=4")).await;
    assert_eq!(
        page1.iter().map(|r| r.ts).collect::<Vec<_>>(),
        vec![1012, 1011, 1010, 1009]
    );
    // Next page via the cursor (last/oldest uid of page1).
    let cursor = &page1.last().unwrap().uid;
    let page2 = get(format!(
        "{events_key}?from=1005;to=1012;after_uid={cursor};limit=4"
    ))
    .await;
    assert_eq!(
        page2.iter().map(|r| r.ts).collect::<Vec<_>>(),
        vec![1008, 1007, 1006, 1005],
        "pagination continues strictly older, still bounded to the window"
    );
}

/// Server-side search (#553): `pattern` + `severity_min` selectors filter the
/// durable store server-side, returning only matching records over the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_side_search_filters_the_store() {
    use std::sync::Arc;
    use zensight_common::LogRecord;
    use zensight_sensor_logs::query;
    use zensight_sensor_logs::store::LogStore;

    fn rec(ts: i64, seq: u64, sev: &str, sev_num: u8, msg: &str) -> LogRecord {
        LogRecord {
            uid: format!("{ts:013}{seq:012}"),
            ts,
            host: "web01".into(),
            facility: "daemon".into(),
            severity: sev.into(),
            severity_number: sev_num,
            app: None,
            pid: None,
            message: msg.into(),
            labels: Default::default(),
        }
    }

    let dir = tempdir();
    let store = Arc::new(LogStore::open(dir.join("logs.redb")).unwrap());
    store
        .write_batch(&[
            rec(1000, 0, "info", 9, "startup ok"),
            rec(1001, 1, "err", 17, "I/O error on sda"),
            rec(1002, 2, "warning", 13, "disk nearly full"),
            rec(1003, 3, "info", 9, "I/O error but only info"),
            rec(1004, 4, "err", 17, "kernel I/O error"),
        ])
        .unwrap();

    let session = Arc::new(
        zenoh::open(harness::isolated_config())
            .await
            .expect("open zenoh"),
    );
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let producer = format!("test_{nanos}/logs");
    let (ring, _cap) = query::new_ring(100);
    tokio::spawn(query::run_events(
        session.clone(),
        producer.clone(),
        ring,
        Some(store),
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let events_key = zensight_common::command::query_key(&producer, "events");
    let replies = session
        .get(&format!(
            "{events_key}?from=0;to=9999;pattern=(?i)i/o error;severity_min=warning"
        ))
        .timeout(Duration::from_secs(5))
        .await
        .expect("get");
    let reply = replies.recv_async().await.expect("reply");
    let sample = reply.result().expect("ok");
    let hits: Vec<LogRecord> =
        serde_json::from_slice(&sample.payload().to_bytes()).expect("decode");

    // Only the two err-severity "I/O error" lines match (the info one is excluded
    // by severity_min=warning; the disk-full and startup lines by the pattern).
    assert_eq!(
        hits.len(),
        2,
        "got {:?}",
        hits.iter().map(|r| &r.message).collect::<Vec<_>>()
    );
    assert!(
        hits.iter()
            .all(|r| r.message.to_lowercase().contains("i/o error"))
    );
    assert!(hits.iter().all(|r| r.severity == "err"));
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "zensight-logs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Dynamic filter over a live session: `filter/set` narrows what reaches the
/// ring, and `filter` reads the active set back (#548).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_filter_set_changes_what_lands() {
    use zensight_sensor_logs::commands::FilterCommand;
    use zensight_sensor_logs::filter::SyslogFilterConfig;

    // TCP (ordered, lossless) so the assertion can't flake on a dropped or
    // reordered UDP datagram — the filter behavior is what's under test.
    let port = free_tcp_port();
    let rig = RigBuilder::tcp(port, Framing::Auto).start().await;
    rig.serve_filters().await;

    // Push a Warning-and-above dynamic filter (min_severity=4 keeps 0..=4).
    rig.set_filter(&FilterCommand::AddFilter {
        id: Some("warn-only".into()),
        filter: SyslogFilterConfig {
            min_severity: Some(4),
            ..Default::default()
        },
    })
    .await;

    // <20> = facility 2, severity 4 (warning) — kept.
    // <23> = facility 2, severity 7 (debug)   — dropped.
    // One connection, in order: the debug line arrives no later than the warning.
    send_tcp_octet(
        port,
        &[
            "<20>Oct 11 22:14:15 h app: warned",
            "<23>Oct 11 22:14:15 h app: debugged",
        ],
    )
    .await;

    // Wait for the kept line, then settle so a (wrongly) passed debug line
    // would have arrived too.
    rig.events_until(1, DEADLINE).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let records = rig.events("").await;
    assert!(
        records.iter().any(|r| r.message.contains("warned")),
        "the warning passes the filter"
    );
    assert!(
        !records.iter().any(|r| r.message.contains("debugged")),
        "the debug line is filtered out, got {:?}",
        records.iter().map(|r| &r.message).collect::<Vec<_>>()
    );
}
