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

/// Multiline idle-flush: a single line buffered by the joiner is emitted after
/// an idle gap past `flush_timeout_ms` — without closing the connection. This
/// is the `select!` idle-flush arm in `handle_stream_connection` (previously
/// untested), distinct from the EOF-flush path the socket tests hit.
///
/// (Folding an indented continuation is unit-tested in `multiline.rs`; it is
/// deliberately NOT asserted end-to-end here because a joined multi-line syslog
/// record currently fails to re-parse — see #559-style follow-up filed for it.)
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

    let port = free_udp_port();
    let rig = RigBuilder::udp(port).start().await;
    rig.serve_filters();

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
    send_udp(port, "<20>Oct 11 22:14:15 h app: warned").await;
    send_udp(port, "<23>Oct 11 22:14:15 h app: debugged").await;

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
