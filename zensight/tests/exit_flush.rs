//! Closing the window writes what was pending (#1119).
//!
//! Flush batches drained every 15 ticks; `main.rs` registered no
//! `window::close_requests()` subscription, `iced::application` has no exit
//! hook, and `MetricStore` has no `Drop`. So closing discarded up to **fifteen
//! seconds** of buckets, logs and events — the fifteen seconds an operator was
//! watching when they decided to quit and go look.
//!
//! The write path is tested against a real redb file: record, flush on exit,
//! **reopen the file**, and read the row back. Asserting against the same
//! in-memory handle would prove only that the batch was handed over.

use std::time::Duration;

use zensight_common::{TelemetryPoint, TelemetryValue};
use zensight_store::{MetricStore, PersistentStore};

const ORIGIN: &str = "h-aabbccddeeff";
const BUDGET: Duration = Duration::from_secs(2);

fn point(value: f64, ts: i64) -> TelemetryPoint {
    TelemetryPoint {
        timestamp: ts,
        source: "dev1".to_string(),
        metric: "cpu/usage".to_string(),
        value: TelemetryValue::Gauge(value),
        labels: Default::default(),
        unit: None,
    }
}

/// **The acceptance criterion**: a sample recorded and then closed is in the
/// file.
#[test]
fn a_sample_recorded_before_close_is_in_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metrics.redb");

    let written = {
        let db = PersistentStore::open(&path).expect("open");
        let mut store = MetricStore::new(3_600, Some(db));
        store.record(
            ORIGIN,
            "sysinfo",
            "cpu/usage",
            &point(42.0, 1_700_000_000_000),
        );
        assert!(
            store.has_pending(),
            "the sample is buffered, not yet on disk — which is the whole problem"
        );
        zensight::app::exit_flush(&mut store, BUDGET)
    };
    assert!(written > 0, "the exit flush wrote buckets");

    // Reopen the FILE. The handle above is gone; if the write had only been
    // scheduled, this finds nothing — which is exactly what closing the window
    // used to do.
    let db = PersistentStore::open(&path).expect("reopen");
    let store = MetricStore::new(3_600, Some(db));
    let ids: Vec<_> = store
        .interner()
        .device_ids("sysinfo/h-aabbccddeeff/dev1")
        .collect();
    assert!(
        !ids.is_empty(),
        "the series is named in the reopened file — the metric row survived"
    );
}

/// An exit flush with nothing pending is a no-op, not an error. Closing a GUI
/// that has been idle must not log a failure.
#[test]
fn closing_with_nothing_pending_writes_nothing_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let db = PersistentStore::open(dir.path().join("metrics.redb")).expect("open");
    let mut store = MetricStore::new(3_600, Some(db));
    assert_eq!(zensight::app::exit_flush(&mut store, BUDGET), 0);
}

/// Demo mode has no persistent store at all. Closing must not panic on the
/// `expect("at least one batch is Some")` the periodic path can afford.
#[test]
fn closing_without_a_persistent_store_is_a_no_op() {
    let mut store = MetricStore::new(3_600, None);
    store.record(
        ORIGIN,
        "sysinfo",
        "cpu/usage",
        &point(42.0, 1_700_000_000_000),
    );
    assert_eq!(
        zensight::app::exit_flush(&mut store, BUDGET),
        0,
        "nothing to lose, and nothing to panic about"
    );
}

/// **The wiring, which the function above cannot prove.**
///
/// `exit_flush` being correct is worth nothing if nothing calls it. The bug
/// was never in a write path — it was that `iced::application` has no exit
/// hook and no subscription supplied one, so a source assertion is what
/// actually pins the fix. The same reasoning the registry-conformance guards
/// are built on.
#[test]
fn the_close_request_is_actually_subscribed_and_handled() {
    let src = include_str!("../src/app.rs");
    assert!(
        src.contains("iced::window::close_requests()"),
        "no close_requests subscription — the flush below it can never run"
    );
    assert!(
        src.contains("Message::CloseRequested(id) =>"),
        "the message is subscribed but not handled"
    );
    // The stream closes must be batched BEFORE the window close, or the
    // runtime drops the `@rpc` GETs with the event loop and every parallax
    // sensor keeps the viewer refcount until its idle reaper fires.
    let handler = src
        .split("Message::CloseRequested(id) =>")
        .nth(1)
        .expect("checked above");
    let closes = handler
        .find("teardown_parallax_tiles")
        .expect("tiles torn down");
    let flush = handler.find("flush_on_exit").expect("store flushed");
    let close = handler
        .find("iced::window::close(id)")
        .expect("window closed");
    assert!(
        closes < close && flush < close,
        "the window close must come last, or the tasks before it are dropped"
    );
}
