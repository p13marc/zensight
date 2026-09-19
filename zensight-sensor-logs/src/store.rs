//! Durable per-line log store (#544): disk-backed history behind the hot ring.
//!
//! The in-memory ring ([`crate::query`]) is a bounded hot cache — minutes-to-
//! hours, lost on restart. This store persists every retained line to redb
//! keyed by the time-sortable `uid` (`<13-digit ts_ms><12-digit seq>`), so the
//! key order *is* time order and a time-range query is a bounded range walk (no
//! secondary index). It reuses the GUI store's `LOGS_TABLE` layout so the two
//! stay wire-compatible.
//!
//! Writes are batched **off the hot intake loop** (a dedicated writer task on a
//! blocking thread) — the intake path only pushes to an mpsc channel, so a slow
//! disk can never add latency to ingestion (promtail's journald-lag failure
//! mode is the cautionary tale).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{Database, ReadableDatabase};
use serde::{Deserialize, Serialize};
use zensight_common::LogRecord;
use zensight_common::page::Page;

/// Per-line log table: `uid -> serde_json(LogRecord)`. The table, the uid
/// keying and the walks over it live in [`zensight_store::logs`] (#904) —
/// this store and the GUI's cache declared the same table twice and walked it
/// with two copies of the same range logic. The *records* stay different:
/// `LogRecord` carries `pid` and a `labels` catch-all that `StoredLog` has no
/// field for, and it is the lossless one.
use zensight_store::logs::LOGS_TABLE;

/// Everything that decides what one page holds (#1147).
///
/// A struct rather than seven positional arguments, because the bug this
/// replaces was a filter applied on the wrong side of a seam — and a call site
/// that names its filters cannot put one on the wrong side by accident.
#[derive(Clone, Copy)]
pub struct PageQuery<'a> {
    /// Inclusive `ts` window (`i64::MIN`/`MAX` for open).
    pub from_ms: i64,
    pub to_ms: i64,
    /// Cursor: only records strictly *older* than this uid. `None` starts at
    /// the newest row inside the window.
    pub after_uid: Option<&'a str>,
    /// Page size, counted in rows the caller asked for.
    pub limit: usize,
    /// The observed device, filtered **inside** the walk.
    pub host: Option<&'a str>,
    /// Content selectors, also inside the walk.
    pub matcher: &'a crate::search::LogMatcher,
    /// Rows examined before the walk stops and reports itself partial.
    pub max_scan: usize,
}

/// The smallest uid strictly greater than every uid at `ts_ms` (#1147).
///
/// The uid is `<13-digit ts_ms><12-digit seq>`, so this is `ts_ms + 1` padded
/// with zeroes: an exclusive upper bound that is cheap to compute and needs no
/// row read. It is what lets a `to=`-only walk START at the window instead of
/// beginning at the newest row and skipping down to it — which was unbounded,
/// on a blocking thread the query handler awaits.
fn uid_ceiling(to_ms: i64) -> String {
    if to_ms == i64::MAX {
        // No ceiling asked for: everything, which `..` cannot express here, so
        // use a string that sorts above every 13-digit prefix.
        return "9".repeat(25);
    }
    // A negative or absurd `to` yields an empty range rather than a panic,
    // which is the honest answer to "logs before the epoch".
    let next = to_ms.saturating_add(1).max(0);
    format!("{next:013}{}", "0".repeat(12))
}

/// A disk-backed, uid-keyed log store.
#[derive(Clone)]
pub struct LogStore {
    db: Arc<Database>,
}

impl LogStore {
    /// Open (creating if absent) the store at `path`, ensuring the table
    /// exists, with an explicit redb page-cache budget (`store.cache_bytes`).
    /// The budget is a required parameter because redb's own default is
    /// 1 GiB (#625) — on the small hosts this sensor targets, that reads as
    /// a slow multi-day RSS climb toward OOM as the database grows.
    pub fn open(path: impl AsRef<Path>, cache_bytes: usize) -> Result<Self, redb::Error> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(redb::StorageError::from)?;
        }
        let db = redb::Builder::new()
            .set_cache_size(cache_bytes)
            .create(path)?;
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(LOGS_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Persist a batch of records, keyed by uid. Skips records with an empty uid
    /// or that fail to serialize. Returns the count written. Blocking I/O.
    pub fn write_batch(&self, records: &[LogRecord]) -> Result<usize, redb::Error> {
        zensight_store::logs::write_batch(&self.db, records)
    }

    /// One bounded page of persisted records, newest-first (#1147).
    ///
    /// Replaces the old `query`/`search` pair, because the bug was the seam
    /// between them: `query` returned `limit` rows and the **caller** filtered
    /// by host afterwards, so `?source=web01;max=500` on a receiver holding
    /// twenty hosts fetched five hundred rows from all of them and replied
    /// with web01's twenty-five. The caller then paged from web01's
    /// twenty-fifth row, the next page held none of web01's, and an empty
    /// reply read as end-of-history with days of matches behind the cursor.
    ///
    /// Everything that decides whether a row is in the answer — the time
    /// window, the host, the content matcher — is applied **inside** the walk,
    /// so `limit` counts rows the caller asked for and nothing else.
    ///
    /// # The cursor is where the WALK stopped, not where the results did
    ///
    /// This is the second half of #1147 and the subtler one. The walk is
    /// bounded by `max_scan`, and when it stops early with **no matches**
    /// there is no last-result uid to page from — so the reply was `[]` with
    /// nothing else, and `?pattern=OOM;from=<7d>` over two million rows with
    /// the last OOM nine hundred thousand back replied "no OOM this week",
    /// deterministically, forever.
    ///
    /// The cursor is therefore the last uid **examined**. A truncated page
    /// always carries one, which is what RFC 05 §3.2 requires: `partial: true`
    /// with a null cursor is a contract violation an observer may report, and
    /// `zenkey_fleet::CallAnswer::page_signal()` computes exactly it.
    ///
    /// Blocking I/O.
    pub fn page(&self, q: PageQuery<'_>) -> Result<Page<LogRecord>, redb::Error> {
        let PageQuery {
            from_ms,
            to_ms,
            after_uid,
            limit,
            host,
            matcher,
            max_scan,
        } = q;
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LOGS_TABLE)?;
        let mut out = Vec::new();
        let mut scanned = 0usize;
        let mut last_seen: Option<String> = None;
        let mut truncated = false;

        // START AT THE WINDOW, DO NOT WALK DOWN TO IT (#1147). The uid is
        // `<13-digit ts_ms><12-digit seq>`, so the first key that can be in
        // window is anything below `(to_ms + 1)` padded with zeroes. A
        // `to=`-only query used to begin at the newest row and `continue` past
        // every row above the window — unbounded, on a `spawn_blocking` thread
        // the handler awaits, so `?to=<a week ago>` walked the whole table
        // before finding its first candidate.
        let ceiling = after_uid
            .map(str::to_string)
            .unwrap_or_else(|| uid_ceiling(to_ms));
        let iter = table.range::<&str>(..ceiling.as_str())?.rev();

        for entry in iter {
            let (key, value) = entry?;
            let uid = key.value().to_string();
            let Ok(rec) = serde_json::from_slice::<LogRecord>(value.value()) else {
                // Still a row examined: a corrupt row must not make the cursor
                // stand still, or a page that hits one never advances.
                last_seen = Some(uid);
                scanned += 1;
                continue;
            };
            if rec.ts > to_ms {
                // Only reachable through an `after_uid` a caller supplied.
                last_seen = Some(uid);
                scanned += 1;
                continue;
            }
            if rec.ts < from_ms {
                break; // keys are time-ordered: nothing older can qualify
            }
            scanned += 1;
            last_seen = Some(uid);
            if host.is_none_or(|h| rec.host == h) && matcher.matches(&rec) {
                out.push(rec);
                if out.len() >= limit {
                    // A full page is not the end of the walk either: there may
                    // be more behind it, and the cursor says where.
                    truncated = true;
                    break;
                }
            }
            if scanned >= max_scan {
                truncated = true;
                break;
            }
        }

        let page = match (truncated, last_seen) {
            (true, Some(cursor)) => Page::more(out, cursor),
            // Truncated with nothing examined cannot happen — `truncated` is
            // only set after a row — but a `Page` that claimed otherwise would
            // be the contract violation, so it is expressed as impossible
            // rather than asserted.
            (true, None) | (false, _) => Page::complete(out),
        };
        Ok(page.scanned(scanned as u64))
    }

    /// Evict by age, then by size. Returns the number of rows removed.
    /// Blocking I/O — see [`zensight_store::logs::prune`].
    pub fn prune(
        &self,
        now_ms: i64,
        max_age_ms: i64,
        keep_max: usize,
    ) -> Result<usize, redb::Error> {
        zensight_store::logs::prune::<LogRecord>(&self.db, now_ms, max_age_ms, keep_max)
    }

    /// Row count + oldest record timestamp (`None` if empty). Blocking I/O.
    pub fn stats(&self) -> Result<StoreStats, redb::Error> {
        let (records, oldest_ts) = zensight_store::logs::stats::<LogRecord>(&self.db)?;
        Ok(StoreStats { records, oldest_ts })
    }
}

/// Point-in-time store metrics for health/rollups.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct StoreStats {
    pub records: u64,
    pub oldest_ts: Option<i64>,
}

/// Cross-task write accounting (records persisted, batches, drops on a full
/// writer channel, write errors) — surfaced in health/rollups.
#[derive(Debug, Default)]
pub struct StoreCounters {
    pub written: AtomicU64,
    pub dropped: AtomicU64,
    pub errors: AtomicU64,
}

impl StoreCounters {
    pub fn inc(field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add(field: &AtomicU64, n: u64) {
        field.fetch_add(n, Ordering::Relaxed);
    }
}

/// Resolve the store directory: explicit config, else systemd `STATE_DIRECTORY`
/// / XDG state / `~/.local/state`. Mirrors the journald cursor resolver so the
/// store and cursor live together. `None` means "no durable location" (disabled).
pub fn resolve_store_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(state) = std::env::var("STATE_DIRECTORY") {
        let first = state.split(':').next().unwrap_or(state.as_str());
        if !first.is_empty() {
            return Some(Path::new(first).join("logs.redb"));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        // Split the join so no source literal spells the deployment base
        // (CI guard #466) — this is a filesystem path, not a Zenoh key.
        return Some(Path::new(&xdg).join("zensight").join("logs.redb"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(Path::new(&home).join(".local/state/zensight/logs.redb"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(uid: &str, ts: i64, msg: &str) -> LogRecord {
        LogRecord {
            uid: uid.to_string(),
            ts,
            host: "h".into(),
            facility: "daemon".into(),
            severity: "info".into(),
            severity_number: 9,
            app: None,
            pid: None,
            message: msg.into(),
            labels: Default::default(),
        }
    }

    /// uids are `<13-ts><12-seq>`; build one matching a ts for realistic keys.
    fn uid(ts: i64, seq: u64) -> String {
        format!("{:013}{:012}", ts.max(0), seq)
    }

    /// The old `query`'s shape, for the tests that are about the walk and not
    /// about filtering — one `LogMatcher::trivial()` and no host.
    fn q(
        s: &LogStore,
        from: i64,
        to: i64,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LogRecord>, redb::Error> {
        let m = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();
        Ok(s.page(PageQuery {
            from_ms: from,
            to_ms: to,
            after_uid: after,
            limit,
            host: None,
            matcher: &m,
            max_scan: usize::MAX,
        })?
        .items)
    }

    fn tmp_store() -> (LogStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = LogStore::open(dir.path().join("logs.redb"), 8 * 1024 * 1024).unwrap();
        (store, dir)
    }

    #[test]
    fn write_then_query_newest_first_windowed() {
        let (s, _d) = tmp_store();
        let recs: Vec<LogRecord> = (0..10)
            .map(|i| rec(&uid(1000 + i, i as u64), 1000 + i, &format!("m{i}")))
            .collect();
        assert_eq!(s.write_batch(&recs).unwrap(), 10);

        // Full window, newest first.
        let all = q(&s, i64::MIN, i64::MAX, None, 100).unwrap();
        assert_eq!(all.len(), 10);
        assert_eq!(all[0].ts, 1009, "newest first");

        // Time window [1003, 1006].
        let win = q(&s, 1003, 1006, None, 100).unwrap();
        assert_eq!(
            win.iter().map(|r| r.ts).collect::<Vec<_>>(),
            vec![1006, 1005, 1004, 1003]
        );
    }

    #[test]
    fn pagination_walks_older_pages() {
        let (s, _d) = tmp_store();
        let recs: Vec<LogRecord> = (0..10)
            .map(|i| rec(&uid(2000 + i, i as u64), 2000 + i, "m"))
            .collect();
        s.write_batch(&recs).unwrap();

        let page1 = q(&s, i64::MIN, i64::MAX, None, 4).unwrap();
        assert_eq!(page1.len(), 4);
        assert_eq!(page1[0].ts, 2009);
        // Next page: cursor = last (oldest) uid of page1.
        let cursor = page1.last().unwrap().uid.clone();
        let page2 = q(&s, i64::MIN, i64::MAX, Some(&cursor), 4).unwrap();
        assert_eq!(page2.len(), 4);
        assert_eq!(page2[0].ts, 2005, "page 2 continues strictly older");
        // No overlap between pages.
        assert!(page2.iter().all(|r| r.uid < cursor));
    }

    #[test]
    fn prune_by_age_and_size() {
        let (s, _d) = tmp_store();
        let recs: Vec<LogRecord> = (0..10)
            .map(|i| rec(&uid(3000 + i, i as u64), 3000 + i, "m"))
            .collect();
        s.write_batch(&recs).unwrap();

        // now=3010, max_age=5 → cutoff 3005: ts 3000..3004 expire (5 removed).
        let removed = s.prune(3010, 5, 1000).unwrap();
        assert_eq!(removed, 5);
        assert_eq!(s.stats().unwrap().records, 5);
        assert_eq!(s.stats().unwrap().oldest_ts, Some(3005));

        // Size cap: keep only 2 newest.
        let removed2 = s.prune(3010, 1_000_000, 2).unwrap();
        assert_eq!(removed2, 3);
        assert_eq!(s.stats().unwrap().records, 2);
        assert_eq!(s.stats().unwrap().oldest_ts, Some(3008));
    }

    #[test]
    fn search_filters_while_scanning() {
        let (s, _d) = tmp_store();
        let recs: Vec<LogRecord> = (0..20)
            .map(|i| {
                let mut r = rec(&uid(5000 + i, i as u64), 5000 + i, "m");
                // Even ts carry "boom", odd carry "ok".
                r.message = if i % 2 == 0 { "boom here" } else { "ok" }.into();
                r
            })
            .collect();
        s.write_batch(&recs).unwrap();

        let m = crate::search::LogMatcher::new(Some("boom"), None, None, None, None).unwrap();
        // limit 3 matches → the 3 newest "boom" records, filtered while scanning.
        let hits = s
            .page(PageQuery {
                from_ms: i64::MIN,
                to_ms: i64::MAX,
                after_uid: None,
                limit: 3,
                host: None,
                matcher: &m,
                max_scan: 100_000,
            })
            .unwrap()
            .items;
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|r| r.message.contains("boom")));
        assert_eq!(hits[0].ts, 5018, "newest matching first");
    }

    #[test]
    fn reopen_keeps_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.redb");
        {
            let s = LogStore::open(&path, 8 * 1024 * 1024).unwrap();
            s.write_batch(&[rec(&uid(4000, 0), 4000, "persisted")])
                .unwrap();
        }
        // Reopen: the record survives (restart-durability).
        let s2 = LogStore::open(&path, 8 * 1024 * 1024).unwrap();
        let out = q(&s2, i64::MIN, i64::MAX, None, 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "persisted");
    }

    /// **#1147, the acceptance.** The host filter is applied INSIDE the walk,
    /// so `limit` counts rows the caller asked for.
    ///
    /// It used to be applied by the query handler, after the store had
    /// already returned `limit` rows: on a receiver holding twenty hosts,
    /// `?source=web01;max=500` fetched five hundred rows from all of them and
    /// replied with web01's twenty-five. The caller then paged from web01's
    /// twenty-fifth row, the next page held none of web01's, and `[]` read as
    /// end-of-history with days of matches still behind the cursor.
    #[test]
    fn a_host_filter_is_applied_inside_the_walk() {
        let (s, _d) = tmp_store();
        // Twenty hosts interleaved, one row each per tick: web01 is one in
        // twenty, which is the shape the issue describes.
        let mut recs = Vec::new();
        for i in 0..400u64 {
            let mut r = rec(&uid(6000 + i as i64, i), 6000 + i as i64, "m");
            r.host = format!("host{:02}", i % 20);
            recs.push(r);
        }
        s.write_batch(&recs).unwrap();
        let m = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();

        let base = PageQuery {
            from_ms: i64::MIN,
            to_ms: i64::MAX,
            after_uid: None,
            limit: 10,
            host: Some("host07"),
            matcher: &m,
            max_scan: usize::MAX,
        };
        let page = s.page(base).unwrap();
        assert_eq!(
            page.items.len(),
            10,
            "a full page of the host asked for, not a fraction of one"
        );
        assert!(page.items.iter().all(|r| r.host == "host07"));

        // And paging continues over that host's rows, not over everyone's.
        let cursor = page.items.last().unwrap().uid.clone();
        let page2 = s
            .page(PageQuery {
                after_uid: Some(&cursor),
                ..base
            })
            .unwrap();
        assert_eq!(
            page2.items.len(),
            10,
            "host07 has twenty rows; the second page holds the other ten"
        );
        assert!(page2.items.iter().all(|r| r.host == "host07"));
        assert!(page2.items.iter().all(|r| r.uid < cursor));
    }

    /// **#1147.** A walk truncated by `max_scan` with NO matches carries a
    /// cursor and says it is partial.
    ///
    /// The cursor is the last uid **examined**, not the last matched — which
    /// is the whole point: with no matches there is no last-matched uid, so
    /// `?pattern=OOM;from=<7d>` over two million rows with the last OOM nine
    /// hundred thousand back replied `[]`, deterministically, forever, and an
    /// operator read "no OOM this week".
    ///
    /// The envelope is not on the wire yet (RFC 08 §3 calls a changed reply
    /// type incompatible), but the walk produces it, and this is what the
    /// wire will carry.
    #[test]
    fn a_truncated_search_with_no_matches_still_says_where_to_resume() {
        let (s, _d) = tmp_store();
        let recs: Vec<LogRecord> = (0..500u64)
            .map(|i| rec(&uid(7000 + i as i64, i), 7000 + i as i64, "nothing here"))
            .collect();
        s.write_batch(&recs).unwrap();

        let m = crate::search::LogMatcher::new(Some("OOM"), None, None, None, None).unwrap();
        let search = PageQuery {
            from_ms: i64::MIN,
            to_ms: i64::MAX,
            after_uid: None,
            limit: 10,
            host: None,
            matcher: &m,
            max_scan: 50,
        };
        let page = s.page(search).unwrap();

        assert!(page.items.is_empty(), "nothing matched, which is the case");
        assert!(page.partial, "and the walk did not finish");
        assert!(
            page.next_cursor.is_some(),
            "a truncated page must say where to resume — RFC 05 §3.2 calls \
             `partial` with a null cursor a contract violation"
        );
        assert!(!page.is_contract_violation());
        assert_eq!(page.scanned, Some(50), "and what it cost");

        // Resuming from it reaches rows the first page never examined.
        let next = s
            .page(PageQuery {
                after_uid: page.next_cursor.as_deref(),
                ..search
            })
            .unwrap();
        assert!(next.next_cursor < page.next_cursor, "strictly older");
    }

    /// A walk that finishes is not partial and offers no cursor, whatever it
    /// found — otherwise every page looks like there is more.
    #[test]
    fn a_completed_walk_is_not_partial() {
        let (s, _d) = tmp_store();
        let recs: Vec<LogRecord> = (0..5u64)
            .map(|i| rec(&uid(8000 + i as i64, i), 8000 + i as i64, "m"))
            .collect();
        s.write_batch(&recs).unwrap();
        let m = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();

        let page = s
            .page(PageQuery {
                from_ms: i64::MIN,
                to_ms: i64::MAX,
                after_uid: None,
                limit: 100,
                host: None,
                matcher: &m,
                max_scan: usize::MAX,
            })
            .unwrap();
        assert_eq!(page.items.len(), 5);
        assert!(!page.partial);
        assert!(page.next_cursor.is_none());
    }

    /// **#1147.** A `to=`-only walk STARTS at the window instead of beginning
    /// at the newest row and skipping down to it.
    ///
    /// The skip was unbounded, on a blocking thread the query handler awaits:
    /// `?to=<a week ago>` walked every row above the window before finding
    /// its first candidate. `scanned` is what makes the difference visible —
    /// it counts rows examined, and the rows above the ceiling are no longer
    /// among them.
    #[test]
    fn a_to_only_walk_does_not_scan_the_rows_above_it() {
        let (s, _d) = tmp_store();
        let recs: Vec<LogRecord> = (0..1000u64)
            .map(|i| rec(&uid(9000 + i as i64, i), 9000 + i as i64, "m"))
            .collect();
        s.write_batch(&recs).unwrap();
        let m = crate::search::LogMatcher::new(None, None, None, None, None).unwrap();

        // Ask for the oldest five, with 995 rows above the window.
        let page = s
            .page(PageQuery {
                from_ms: i64::MIN,
                to_ms: 9004,
                after_uid: None,
                limit: 5,
                host: None,
                matcher: &m,
                max_scan: usize::MAX,
            })
            .unwrap();
        assert_eq!(page.items.len(), 5);
        assert_eq!(page.items[0].ts, 9004, "newest in window first");
        assert_eq!(
            page.scanned,
            Some(5),
            "the 995 rows above the window must not be examined at all"
        );
    }
}
