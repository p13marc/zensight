//! On-demand per-line log-event query channel (#358, principle P2).
//!
//! Per-line log events are high-cardinality, high-volume detail; streaming
//! them per line rode (and could dominate) the `zensight/**` telemetry bus.
//! They now live in a bounded in-memory ring served via a Zenoh queryable at
//! `zensight/v1/<origin>/@rpc/logs/events` — pulled by the GUI on open + a slow refresh
//! tick, never streamed. The low-rate rollups (`by_severity/*`,
//! `by_unit/*`, …) stay on the bus for charts/alerts.
//!
//! Selector parameters (zenoh `Parameters`, `;`-separated — e.g.
//! `…/@rpc/logs/events?since=1719999000000;max=500`):
//! - `since=<epoch_ms>` — only records with `ts >= since` (inclusive);
//! - `max=<n>` / `limit=<n>` — reply cap (default 500);
//! - `source=<name>` (alias `host=`) — only records from one originating host.
//!
//! Durable-store selectors (#544, served from the disk store when configured):
//! - `from=<epoch_ms>` / `to=<epoch_ms>` — inclusive time window;
//! - `after_uid=<uid>` — pagination cursor: records strictly older than this
//!   uid (pass the previous page's last/oldest uid). Newest-first pages.
//!
//! A query using any of `from`/`to`/`after_uid` is answered from the durable
//! store (days of history, survives restart); otherwise from the hot ring.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use zensight_common::LogRecord;
use zensight_common::page::Page;

use crate::store::LogStore;

/// Default reply cap when no `?max=` selector is supplied.
pub const DEFAULT_EVENTS_REPLY_MAX: usize = 500;

/// The largest `?max=` a caller may ask for (#1147).
///
/// The documentation claimed a clamp and there was none: any `usize` parsed,
/// so `?max=5000000` asked one blocking thread — which the query handler
/// awaits — to materialise five million `LogRecord`s into a `Vec` and then
/// serialise them into one reply. A reply cap is a memory bound on this
/// process, not a courtesy to the caller, and `partial` + `next_cursor` is
/// how a caller asks for more.
pub const MAX_EVENTS_REPLY_MAX: usize = 10_000;

/// Minimum ring capacity (config values below this are clamped up).
pub const MIN_EVENTS_RING_CAPACITY: usize = 100;

/// The bounded ring of recent per-line log events, shared between the intake
/// loop (producer) and the queryable task (consumer).
pub type EventRing = Arc<Mutex<VecDeque<LogRecord>>>;

/// Create an empty ring for `capacity` records (clamped to
/// [`MIN_EVENTS_RING_CAPACITY`]).
pub fn new_ring(capacity: usize) -> (EventRing, usize) {
    let capacity = capacity.max(MIN_EVENTS_RING_CAPACITY);
    (
        Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
        capacity,
    )
}

/// Append one record, evicting the oldest past `capacity`.
pub fn push(ring: &EventRing, capacity: usize, record: LogRecord) {
    if let Ok(mut r) = ring.lock() {
        r.push_back(record);
        while r.len() > capacity {
            r.pop_front();
        }
    }
}

/// Cap on records scanned by a single content search over the durable store
/// (#553) — bounds cost over a huge range; beyond it a page is partial and the
/// client paginates on.
const MAX_SEARCH_SCAN: usize = 500_000;

/// Pure reply builder: newest-first, `since`/`host` + content-matcher filtered,
/// capped at `max` matches.
///
/// Returns one page rather than a bare `Vec` (#1147). A ring walk that stops
/// because it hit `max` has **not** finished: the comment here used to read
/// "the ring is the whole of recent history, so a short page really is the end
/// of it", which is true of a short page and false of a full one. `?max=2`
/// over a ring holding a hundred matches replied with two and
/// `partial: false` — the same "I stopped early and did not say so" the
/// durable path was fixed for, on the other branch of the same `if`.
///
/// Taking `max + 1` and keeping `max` is how the walk learns there was a next
/// row without paying for it.
fn filter_ring(
    records: &VecDeque<LogRecord>,
    since: Option<i64>,
    host: Option<&str>,
    max: usize,
    matcher: &crate::search::LogMatcher,
) -> Page<LogRecord> {
    let mut matches: Vec<LogRecord> = records
        .iter()
        .rev()
        .filter(|r| since.is_none_or(|s| r.ts >= s))
        .filter(|r| host.is_none_or(|h| r.host == h))
        .filter(|r| matcher.matches(r))
        .take(max.saturating_add(1))
        .cloned()
        .collect();

    let scanned = matches.len() as u64;
    if matches.len() > max {
        matches.truncate(max);
        // The cursor is the last row EMITTED — a value, never a position
        // (RFC 05 §3.2). An empty `max` is already rejected upstream
        // (`.filter(|n| *n > 0)`), so `matches` is non-empty here.
        let cursor = matches.last().map(|r| r.uid.clone()).unwrap_or_default();
        return Page::more(matches, cursor).scanned(scanned);
    }
    Page::complete(matches).scanned(scanned)
}

/// Run the log-event query channel until the session closes. Replies with
/// filtered records (most-recent first) as JSON `Vec<LogRecord>`. When `store`
/// is `Some`, `from`/`to`/`after_uid` queries are answered from the durable
/// store (#544); recent queries always come from the hot ring.
/// Which of the two sibling procedures this loop answers (#1147).
///
/// They run the **same walk over the same store** and differ only in what they
/// put on the wire, which is the whole point: `events` cannot be changed in
/// place — RFC 08 §3 calls a changed reply type on an existing path
/// incompatible, and the lock refuses it — so the envelope arrives as a
/// sibling and `events` keeps its contract for every caller already built
/// against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Procedure {
    /// `events` — `Vec<LogRecord>`. Cannot say "I stopped early".
    Bare,
    /// `events/page` — `Page<LogRecord>`, with `partial`, `next_cursor` and
    /// `scanned`.
    Paged,
}

impl Procedure {
    /// This procedure's `@rpc` key.
    ///
    /// Built, not formatted: `events/page` is **two chunks**, and
    /// `query_key(producer, "events/page")` panics on the embedded `/`
    /// (RFC 03 §3 — a reserved token inside a chunk). `nested_query_key` is
    /// the builder for a two-chunk read.
    #[must_use]
    pub fn key(self, producer: &str) -> String {
        match self {
            Self::Bare => zensight_common::command::query_key(producer, "events"),
            Self::Paged => zensight_common::command::nested_query_key(producer, "events", "page"),
        }
    }
}

/// Serve one of the two event procedures.
pub async fn run_events_procedure(
    session: Arc<zenoh::Session>,
    producer: String,
    ring: EventRing,
    store: Option<Arc<LogStore>>,
    procedure: Procedure,
) {
    let key = procedure.key(&producer);
    let queryable = match zensight_common::served::serve_queryable(&session, &key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "query: declare events failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand log-event query channel ready");

    while let Ok(query) = queryable.recv_async().await {
        let params = query.parameters();
        // Percent-decoded (#1122). Zenoh's `Parameters` splits on `;` and `=`
        // and does not decode, so a caller that did not encode broke the
        // grammar and one that did reached the matcher as `foo%20bar`. A value
        // with no `%` decodes to itself, so an older caller is unaffected.
        let text_param = |name: &str| params.get(name).map(zensight_common::percent_decode);
        let since = params.get("since").and_then(|v| v.parse::<i64>().ok());
        // v1 (RFC 05 §5): `source=` filters the observed device (a central
        // receiver holds many sources); `host=` accepted as the legacy alias.
        let host = text_param("source").or_else(|| text_param("host"));
        // `limit=` is the paginated alias of `max=`.
        let max = params
            .get("max")
            .or_else(|| params.get("limit"))
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_EVENTS_REPLY_MAX)
            // Clamped, as the documentation has always said it was (#1147).
            .min(MAX_EVENTS_REPLY_MAX);

        let from = params.get("from").and_then(|v| v.parse::<i64>().ok());
        let to = params.get("to").and_then(|v| v.parse::<i64>().ok());
        let after_uid = text_param("after_uid");
        let durable_query = from.is_some() || to.is_some() || after_uid.is_some();

        // Content-search selectors (#553): compile once per query. A bad/oversized
        // regex is rejected here rather than pinning a core.
        let (pattern, severity_min, unit, app, facility) = (
            text_param("pattern"),
            text_param("severity_min"),
            text_param("unit"),
            text_param("app"),
            text_param("facility"),
        );
        let matcher = match crate::search::LogMatcher::new(
            pattern.as_deref(),
            severity_min.as_deref(),
            unit.as_deref(),
            app.as_deref(),
            facility.as_deref(),
        ) {
            Ok(m) => m,
            Err(e) => {
                let err = zensight_sensor_core::rpc::RpcError::invalid_args(e);
                let _ = query
                    .reply_err(serde_json::to_vec(&err).unwrap_or_default())
                    .await;
                continue;
            }
        };

        let page: Page<LogRecord> = if durable_query && store.is_some() {
            // Durable, paginated path — blocking redb range walk off the runtime.
            let store = store.clone().expect("checked is_some");
            let host_f = host.clone();
            let (from_ms, to_ms) = (from.unwrap_or(i64::MIN), to.unwrap_or(i64::MAX));
            let after = after_uid.clone();
            tokio::task::spawn_blocking(move || {
                // ONE walk that knows every filter (#1147). The host filter
                // used to be applied by this closure, AFTER the store had
                // returned `max` rows — so `max` counted rows the caller had
                // not asked for and the page that came back was a fraction of
                // one.
                store
                    .page(crate::store::PageQuery {
                        from_ms,
                        to_ms,
                        after_uid: after.as_deref(),
                        limit: max,
                        host: host_f.as_deref(),
                        matcher: &matcher,
                        max_scan: MAX_SEARCH_SCAN,
                    })
                    .unwrap_or_else(|_| Page::complete(Vec::new()))
            })
            .await
            .unwrap_or_else(|_| Page::complete(Vec::new()))
        } else {
            // Hot path — snapshot the ring under the lock, reply outside it.
            // The ring is the whole of recent history, so a short page there
            // really is the end of it.
            match ring.lock() {
                Ok(r) => filter_ring(&r, since, host.as_deref(), max, &matcher),
                Err(_) => Page::complete(Vec::new()),
            }
        };
        debug_assert!(
            !page.is_contract_violation(),
            "a truncated page must carry a cursor (RFC 05 §3.2)"
        );
        // The same walk, two wire shapes (#1147).
        //
        // `events/page` sends the envelope `LogStore::page` already built —
        // `partial`, `next_cursor` and `scanned`, exactly the RFC 05 §3.2
        // fields `zenkey_fleet::CallAnswer::page_signal()` reads. `events`
        // sends the bare `Vec` its registry entry declares, unchanged, because
        // RFC 08 §3 calls a changed reply type on an existing path
        // incompatible and every caller already built against it is entitled
        // to the shape it was promised.
        //
        // The blind spot is closed on the sibling and remains on the original,
        // which is the honest arrangement: a caller that wants to know whether
        // the walk finished asks the procedure that can say.
        let payload = match procedure {
            Procedure::Paged => serde_json::to_vec(&page),
            Procedure::Bare => serde_json::to_vec(&page.items),
        };
        match payload {
            Ok(payload) => {
                if let Err(e) = query.reply(key.as_str(), payload).await {
                    tracing::warn!(error = %e, key = %key, "query: events reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, key = %key, "query: events serialize failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(uid: &str, ts: i64, host: &str, message: &str) -> LogRecord {
        LogRecord {
            uid: uid.to_string(),
            ts,
            host: host.to_string(),
            facility: "daemon".to_string(),
            severity: "info".to_string(),
            severity_number: 9,
            app: None,
            pid: None,
            message: message.to_string(),
            labels: Default::default(),
        }
    }

    #[test]
    fn filter_is_newest_first_and_since_inclusive() {
        let ring: VecDeque<LogRecord> = (0..5)
            .map(|i| rec(&format!("u{i}"), 100 + i, "web01", "m"))
            .collect();
        let m = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();
        let out = filter_ring(&ring, Some(102), None, 100, &m);
        assert_eq!(
            out.items.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![104, 103, 102],
            "since is an inclusive lower bound, newest first"
        );
    }

    #[test]
    fn filter_caps_at_max_and_honors_host() {
        let mut ring: VecDeque<LogRecord> = VecDeque::new();
        for i in 0..10 {
            let host = if i % 2 == 0 { "web01" } else { "db01" };
            ring.push_back(rec(&format!("u{i}"), i, host, "m"));
        }
        let m = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();
        let out = filter_ring(&ring, None, Some("web01"), 3, &m);
        assert_eq!(out.items.len(), 3);
        assert!(out.items.iter().all(|r| r.host == "web01"));
        assert_eq!(out.items[0].ts, 8, "newest matching first");
        assert!(
            out.partial,
            "web01 has five matches and only three were sent"
        );
    }

    /// Live round-trip: a single-session zenoh get against the running
    /// queryable returns the ring's records with selectors applied (#358).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_queryable_round_trip() {
        // Unique prefix so parallel test runs don't cross-talk.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // A *producer chunk*, not a key prefix: lowercase alnum + `-` (RFC 03
        // §1.5). It used to read `test_{nanos}/logs`, which zenkey 0.6
        // silently slugged into something else entirely — 0.7 refuses it,
        // which is how this was found.
        let prefix = format!("test-{nanos}-logs");

        // Multicast scouting OFF. A default-config session joins whatever mesh
        // it can reach — including a live fleet on the same host — so a test
        // that scouts is not a test, it is a participant (RFC 09 §0.1).
        let mut config = zenoh::Config::default();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .expect("disable multicast scouting");
        let session = Arc::new(zenoh::open(config).await.expect("open zenoh session"));

        let (ring, capacity) = new_ring(1000);
        for i in 0..5 {
            push(
                &ring,
                capacity,
                rec(&format!("u{i}"), 100 + i, "web01", "m"),
            );
        }
        tokio::spawn(run_events_procedure(
            session.clone(),
            prefix.clone(),
            ring,
            None,
            Procedure::Bare,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // zenoh selector params are `;`-separated (Parameters), not `&`.
        // v1 (RFC 05): the events read procedure lives on the @rpc plane.
        let events_key = zensight_common::command::query_key(&prefix, "events");
        let selector = format!("{events_key}?since=102;max=2");
        let replies = session
            .get(&selector)
            .timeout(std::time::Duration::from_secs(5))
            .await
            .expect("get events");
        let reply = replies.recv_async().await.expect("one reply");
        let sample = reply.result().expect("ok reply");
        let records: Vec<LogRecord> =
            serde_json::from_slice(&sample.payload().to_bytes()).expect("decode Vec<LogRecord>");
        assert_eq!(
            records.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![104, 103],
            "newest-first, since inclusive, capped at max=2"
        );
    }

    /// The acceptance criterion, on the wire (#1147).
    ///
    /// `events/page` must reply with an object carrying a **boolean field
    /// named exactly `partial`** — not "truncated", not an absent field.
    /// `zenkey_fleet::CallAnswer::page_signal()` returns `None` for anything
    /// else and deliberately does not synthesise `partial: false` for a bare
    /// list, so a reply without it is invisible to `zenctl call` and to every
    /// RFC 13 judge. That invisibility is the half of this issue that a bare
    /// `Vec` could not fix on the existing path.
    ///
    /// And `events` must be **unchanged**: a caller built against its declared
    /// `Vec<LogRecord>` is entitled to the shape it was promised.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_paged_sibling_sends_an_envelope_and_events_stays_a_bare_list() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!("test-{nanos}-logs");

        let mut config = zenoh::Config::default();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .expect("disable multicast scouting");
        let session = Arc::new(zenoh::open(config).await.expect("open zenoh session"));

        let (ring, capacity) = new_ring(1000);
        for i in 0..5 {
            push(
                &ring,
                capacity,
                rec(&format!("u{i}"), 100 + i, "web01", "m"),
            );
        }
        for procedure in [Procedure::Bare, Procedure::Paged] {
            tokio::spawn(run_events_procedure(
                session.clone(),
                prefix.clone(),
                ring.clone(),
                None,
                procedure,
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        // The sibling: an envelope, truncated, and it says so.
        let paged_key = Procedure::Paged.key(&prefix);
        let replies = session
            .get(&format!("{paged_key}?max=2"))
            .timeout(std::time::Duration::from_secs(5))
            .await
            .expect("get events/page");
        let reply = replies.recv_async().await.expect("one reply");
        let bytes = reply.result().expect("ok reply").payload().to_bytes();

        // Read as raw JSON first: the FIELD NAME is the contract, and a typed
        // decode would happily accept a struct that renamed it.
        let raw: serde_json::Value = serde_json::from_slice(&bytes).expect("json object");
        assert!(raw.is_object(), "an envelope, not a bare list");
        assert_eq!(
            raw.get("partial").and_then(serde_json::Value::as_bool),
            Some(true),
            "a boolean field named exactly `partial` — what page_signal() reads"
        );
        assert!(
            raw.get("next_cursor")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "a truncated page must carry a cursor (RFC 05 §3.2)"
        );

        let page: Page<LogRecord> = serde_json::from_slice(&bytes).expect("decode Page");
        assert_eq!(page.items.len(), 2);
        assert!(page.partial);

        // The original: unchanged, still a bare list.
        let bare_key = Procedure::Bare.key(&prefix);
        let replies = session
            .get(&format!("{bare_key}?max=2"))
            .timeout(std::time::Duration::from_secs(5))
            .await
            .expect("get events");
        let reply = replies.recv_async().await.expect("one reply");
        let bytes = reply.result().expect("ok reply").payload().to_bytes();
        let raw: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert!(raw.is_array(), "`events` keeps its declared Vec<LogRecord>");
        let records: Vec<LogRecord> = serde_json::from_slice(&bytes).expect("decode Vec");
        assert_eq!(records.len(), 2);
    }

    /// #1147, the other branch of the same `if`. A ring walk capped by `max`
    /// has not finished, and saying `partial: false` there is the same lie the
    /// durable path was fixed for.
    #[test]
    fn a_ring_page_capped_by_max_says_it_stopped_early() {
        let (ring, capacity) = new_ring(100);
        for i in 0..10u64 {
            push(
                &ring,
                capacity,
                rec(&format!("u{i}"), 100 + i as i64, "web01", "m"),
            );
        }
        let matcher = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();
        let r = ring.lock().unwrap();

        let page = filter_ring(&r, None, None, 3, &matcher);
        assert_eq!(page.items.len(), 3, "capped at max");
        assert!(page.partial, "and there were more behind it");
        assert_eq!(
            page.next_cursor.as_deref(),
            Some("u7"),
            "the cursor is the last row EMITTED (newest-first: u9, u8, u7)"
        );
        assert!(!page.is_contract_violation());
    }

    /// A walk that really did finish says so, with no cursor — the caller must
    /// be able to stop.
    #[test]
    fn a_ring_page_that_finished_carries_no_cursor() {
        let (ring, capacity) = new_ring(100);
        for i in 0..3u64 {
            push(
                &ring,
                capacity,
                rec(&format!("u{i}"), 100 + i as i64, "web01", "m"),
            );
        }
        let matcher = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();
        let r = ring.lock().unwrap();

        let page = filter_ring(&r, None, None, 50, &matcher);
        assert_eq!(page.items.len(), 3);
        assert!(!page.partial);
        assert_eq!(page.next_cursor, None);
    }

    /// Exactly `max` matches and nothing behind them is a **complete** walk.
    /// Reporting `partial` here would send the caller back for an empty page
    /// forever — the failure mode inverted.
    #[test]
    fn exactly_max_matches_with_nothing_behind_is_complete() {
        let (ring, capacity) = new_ring(100);
        for i in 0..3u64 {
            push(
                &ring,
                capacity,
                rec(&format!("u{i}"), 100 + i as i64, "web01", "m"),
            );
        }
        let matcher = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();
        let r = ring.lock().unwrap();

        let page = filter_ring(&r, None, None, 3, &matcher);
        assert_eq!(page.items.len(), 3);
        assert!(!page.partial, "there is no fourth row to page to");
        assert_eq!(page.next_cursor, None);
    }

    #[test]
    fn push_evicts_oldest_past_capacity() {
        let (ring, capacity) = new_ring(0); // clamps to MIN_EVENTS_RING_CAPACITY
        assert_eq!(capacity, MIN_EVENTS_RING_CAPACITY);
        for i in 0..(capacity + 10) {
            push(&ring, capacity, rec(&format!("u{i}"), i as i64, "h", "m"));
        }
        let r = ring.lock().unwrap();
        assert_eq!(r.len(), capacity);
        assert_eq!(r.front().unwrap().ts, 10, "oldest ten evicted");
    }
}
