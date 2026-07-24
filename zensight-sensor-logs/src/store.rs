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

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use zensight_common::LogRecord;

/// Per-line log table: `uid -> serde_json(LogRecord)`. Same name/layout as the
/// GUI store's logs table (#107, C9) so both read the same shape.
const LOGS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("logs");

/// A disk-backed, uid-keyed log store.
#[derive(Clone)]
pub struct LogStore {
    db: Arc<Database>,
}

impl LogStore {
    /// Open (creating if absent) the store at `path`, ensuring the table exists.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(redb::StorageError::from)?;
        }
        let db = Database::create(path)?;
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
        if records.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin_write()?;
        let mut written = 0usize;
        {
            let mut table = txn.open_table(LOGS_TABLE)?;
            for rec in records {
                if rec.uid.is_empty() {
                    continue;
                }
                let Ok(bytes) = serde_json::to_vec(rec) else {
                    continue;
                };
                table.insert(rec.uid.as_str(), bytes.as_slice())?;
                written += 1;
            }
        }
        txn.commit()?;
        Ok(written)
    }

    /// Query persisted records, newest-first, in one bounded page.
    ///
    /// - `from_ms`/`to_ms`: inclusive `ts` window (`i64::MIN`/`MAX` for open).
    /// - `after_uid`: cursor for pagination — only records strictly *older* than
    ///   this uid (a previous page's last/oldest uid). `None` starts at newest.
    /// - `limit`: page size cap.
    ///
    /// Because the key is the time-sortable uid, this is a reverse range walk
    /// bounded by `after_uid` and short-circuited once older than `from_ms`.
    /// Blocking I/O.
    pub fn query(
        &self,
        from_ms: i64,
        to_ms: i64,
        after_uid: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LogRecord>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LOGS_TABLE)?;
        let mut out = Vec::new();
        // `..after_uid` excludes the cursor itself and everything newer; `.rev()`
        // yields newest-first among the remaining (older) keys.
        let iter = match after_uid {
            Some(cursor) => table.range::<&str>(..cursor)?.rev(),
            None => table.range::<&str>(..)?.rev(),
        };
        for entry in iter {
            let (_key, value) = entry?;
            let Ok(rec) = serde_json::from_slice::<LogRecord>(value.value()) else {
                continue;
            };
            if rec.ts > to_ms {
                continue;
            }
            if rec.ts < from_ms {
                break; // keys are time-ordered: nothing older can qualify
            }
            out.push(rec);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Prune by age then size (#544): drop everything older than `max_age_ms`
    /// before `now_ms`, then, if still over `keep_max` rows, drop the oldest
    /// excess. Returns the number removed. Blocking I/O.
    pub fn prune(
        &self,
        now_ms: i64,
        max_age_ms: i64,
        keep_max: usize,
    ) -> Result<usize, redb::Error> {
        let cutoff = now_ms.saturating_sub(max_age_ms);
        let txn = self.db.begin_write()?;
        let mut removed = 0usize;
        {
            let mut table = txn.open_table(LOGS_TABLE)?;

            // Age: the oldest keys are at the front; stop at the first in-window.
            let mut expired: Vec<String> = Vec::new();
            for entry in table.range::<&str>(..)? {
                let (key, value) = entry?;
                let ts = serde_json::from_slice::<LogRecord>(value.value())
                    .map(|r| r.ts)
                    .unwrap_or(i64::MIN);
                if ts < cutoff {
                    expired.push(key.value().to_string());
                } else {
                    break;
                }
            }
            for key in &expired {
                table.remove(key.as_str())?;
                removed += 1;
            }

            // Size: drop the oldest excess beyond keep_max.
            let total = table.len()? as usize;
            if total > keep_max {
                let excess = total - keep_max;
                let oldest: Vec<String> = table
                    .range::<&str>(..)?
                    .take(excess)
                    .filter_map(|e| e.ok().map(|(k, _)| k.value().to_string()))
                    .collect();
                for key in oldest {
                    table.remove(key.as_str())?;
                    removed += 1;
                }
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Row count + oldest record timestamp (`None` if empty). Blocking I/O.
    pub fn stats(&self) -> Result<StoreStats, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(LOGS_TABLE)?;
        let records = table.len()?;
        let oldest_ts = table
            .range::<&str>(..)?
            .next()
            .and_then(|e| e.ok())
            .and_then(|(_, v)| serde_json::from_slice::<LogRecord>(v.value()).ok())
            .map(|r| r.ts);
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
        return Some(Path::new(&xdg).join("zensight/logs.redb"));
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

    fn tmp_store() -> (LogStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = LogStore::open(dir.path().join("logs.redb")).unwrap();
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
        let all = s.query(i64::MIN, i64::MAX, None, 100).unwrap();
        assert_eq!(all.len(), 10);
        assert_eq!(all[0].ts, 1009, "newest first");

        // Time window [1003, 1006].
        let win = s.query(1003, 1006, None, 100).unwrap();
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

        let page1 = s.query(i64::MIN, i64::MAX, None, 4).unwrap();
        assert_eq!(page1.len(), 4);
        assert_eq!(page1[0].ts, 2009);
        // Next page: cursor = last (oldest) uid of page1.
        let cursor = page1.last().unwrap().uid.clone();
        let page2 = s.query(i64::MIN, i64::MAX, Some(&cursor), 4).unwrap();
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
    fn reopen_keeps_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs.redb");
        {
            let s = LogStore::open(&path).unwrap();
            s.write_batch(&[rec(&uid(4000, 0), 4000, "persisted")])
                .unwrap();
        }
        // Reopen: the record survives (restart-durability).
        let s2 = LogStore::open(&path).unwrap();
        let out = s2.query(i64::MIN, i64::MAX, None, 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "persisted");
    }
}
