//! The timeline's acceptance (#908), against a live session and a real file.
//!
//! Two things the issue asks for and one the design rests on:
//! events survive a restart, an alert fire/resolve pair is visible in one
//! page, and a subscriber's history replay does not turn one firing into two.

use std::sync::Arc;

use zensight_common::history::{TimelineKind, TimelineReply};
use zensight_common::{AlertSeverity, EventRecord, Protocol};
use zensight_historian::ingest::SharedStore;
use zensight_historian::query::timeline;

fn tmp_db(tag: &str) -> std::path::PathBuf {
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("zensight-timeline-e2e-{tag}-{ns}.redb"))
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

fn event(ts: i64, id: &str) -> EventRecord {
    let mut e = EventRecord::new(
        "vm-dev-01",
        Protocol::Snmp,
        "trap/link_down",
        AlertSeverity::Warning,
        "link down on if3",
    );
    // A fixed id and timestamp: the assertions are about what survives a
    // restart, not about what a fresh ULID happens to be.
    e.id = id.into();
    e.timestamp = ts;
    e
}

/// The whole acceptance in one run: a fire and a resolve land in one page,
/// newest-first, and the events written beside them are still there after the
/// store is closed and reopened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fire_and_a_resolve_are_one_page_and_survive_a_restart() {
    let path = tmp_db("pair");
    let origin = "h-0123456789ab";
    let alert_key = format!("v1/{origin}/state/sysinfo/alert/cpu-hot");
    let event_key = format!("v1/{origin}/events/snmp/trap/01hq");

    // ── write through the store, as the subscribers do ───────────────────
    {
        let persistent = zensight_store::PersistentStore::open(&path).expect("open");
        let store: SharedStore = Arc::new(std::sync::Mutex::new(zensight_store::MetricStore::new(
            64,
            Some(persistent),
        )));
        {
            let mut g = store.lock().unwrap();
            g.record_timeline(zensight_store::timeline::TimelineRow::new(
                1_000,
                TimelineKind::Alert,
                origin,
                &alert_key,
                true,
                Some("cpu over threshold".into()),
            ));
            g.record_timeline(zensight_store::timeline::TimelineRow::new(
                5_000,
                TimelineKind::Alert,
                origin,
                &alert_key,
                false,
                None,
            ));
            g.record_timeline(zensight_store::timeline::TimelineRow::new(
                3_000,
                TimelineKind::Event,
                origin,
                &event_key,
                true,
                Some("link down on if3".into()),
            ));
            g.record_event(event(3_000, "01hq"));
        }
        zensight_historian::ingest::flush_once(&store).await;

        // A replay of the same transitions — what an AdvancedSubscriber does
        // on every reconnect — must not multiply them.
        {
            let mut g = store.lock().unwrap();
            g.record_timeline(zensight_store::timeline::TimelineRow::new(
                1_000,
                TimelineKind::Alert,
                origin,
                &alert_key,
                true,
                Some("cpu over threshold".into()),
            ));
        }
        zensight_historian::ingest::flush_once(&store).await;
    }

    // ── reopen: a different process, as far as the file is concerned ─────
    let persistent = zensight_store::PersistentStore::open(&path).expect("reopen");
    let store: SharedStore = Arc::new(std::sync::Mutex::new(zensight_store::MetricStore::new(
        64,
        Some(persistent.clone()),
    )));

    // The events written beside the timeline are still there (#908's first
    // acceptance): the whole point of a durable timeline is that a restart is
    // not an amnesia.
    let events = persistent.query_events(10).expect("events");
    assert_eq!(events.len(), 1, "the event survived the restart");
    assert_eq!(events[0].id, "01hq");

    // ── serve it and ask ─────────────────────────────────────────────────
    let session = session().await;
    let ctx = zensight_sensor_core::v1::for_producer("historian");
    let key = ctx.rpc_key(&["timeline"]).unwrap().to_string();
    let _h = timeline::serve_timeline(session.clone(), ctx, store, origin.to_string())
        .await
        .expect("timeline serves");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let reply: TimelineReply = get_one(&session, &key).await;
    assert_eq!(reply.historian, origin);
    assert_eq!(
        reply.entries.len(),
        3,
        "one fire, one resolve, one event — and the replayed fire is not a fourth"
    );

    // Newest first.
    let ts: Vec<i64> = reply.entries.iter().map(|e| e.ts).collect();
    assert_eq!(ts, vec![5_000, 3_000, 1_000]);

    // The fire/resolve pair is in this one page, and distinguishable.
    let alerts: Vec<_> = reply
        .entries
        .iter()
        .filter(|e| e.kind == TimelineKind::Alert)
        .collect();
    assert_eq!(alerts.len(), 2);
    assert!(alerts.iter().any(|e| e.active), "the fire");
    assert!(alerts.iter().any(|e| !e.active), "the resolve");
    assert!(
        alerts.iter().all(|e| e.key == alert_key),
        "both name the key they rode, so a reader can go back to the source"
    );

    // `kinds=` narrows it, and an unknown kind is refused rather than ignored.
    let only_events: TimelineReply = get_one(&session, &format!("{key}?kinds=event")).await;
    assert_eq!(only_events.entries.len(), 1);
    assert_eq!(only_events.entries[0].kind, TimelineKind::Event);

    let replies = session.get(format!("{key}?kinds=alerts")).await.unwrap();
    let r = replies.recv_async().await.unwrap();
    assert!(r.result().is_err(), "a misspelled kind must be an error");

    // A window excludes what falls outside it.
    let windowed: TimelineReply = get_one(&session, &format!("{key}?from=2000;to=4000")).await;
    assert_eq!(windowed.entries.len(), 1);
    assert_eq!(windowed.entries[0].ts, 3_000);

    // `limit` pages, and the cursor continues strictly older with no repeat.
    let page: TimelineReply = get_one(&session, &format!("{key}?limit=2")).await;
    assert_eq!(page.entries.len(), 2);
    let cursor = page
        .next_cursor
        .clone()
        .expect("a full page carries a cursor");
    let next: TimelineReply = get_one(&session, &format!("{key}?limit=2;after_uid={cursor}")).await;
    assert_eq!(next.entries.len(), 1);
    assert!(next.next_cursor.is_none(), "a short page is the end");
    assert!(
        next.entries
            .iter()
            .all(|e| !page.entries.iter().any(|p| p.uid == e.uid)),
        "no row appears in two pages"
    );

    session.close().await.unwrap();
    let _ = std::fs::remove_file(&path);
}

/// A memory-only historian answers an empty timeline rather than an error:
/// "this deployment is not durable" is a fact the caller can read in `stats`,
/// not a failed query.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_memory_only_historian_answers_empty_rather_than_erroring() {
    let session = session().await;
    let store: SharedStore = Arc::new(std::sync::Mutex::new(zensight_store::MetricStore::new(
        64, None,
    )));
    let ctx = zensight_sensor_core::v1::for_producer("historian");
    let key = ctx.rpc_key(&["timeline"]).unwrap().to_string();
    let _h = timeline::serve_timeline(session.clone(), ctx, store, "h-0123456789ab".into())
        .await
        .expect("timeline serves");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let reply: TimelineReply = get_one(&session, &key).await;
    assert!(reply.entries.is_empty());
    assert!(reply.next_cursor.is_none());
    session.close().await.unwrap();
}

async fn get_one<T: serde::de::DeserializeOwned>(session: &zenoh::Session, selector: &str) -> T {
    let replies = session.get(selector).await.unwrap();
    let reply = replies.recv_async().await.expect("a reply");
    let sample = reply.result().expect("a value reply, not an error");
    serde_json::from_slice(&sample.payload().to_bytes()).expect("decodes")
}
