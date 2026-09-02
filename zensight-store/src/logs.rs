//! The `logs` table, and the walk over it — one definition, two records.
//!
//! Per-line log events are text with unbounded cardinality, so they do not go
//! through the numeric tiers. They get their own redb table keyed by a
//! time-sortable uid (`<13-digit ts_ms><12-digit seq>`), which makes the table
//! time-ordered by construction: "the newest N in a window" is a bounded
//! reverse range walk, and paginating older is the same walk bounded above by
//! the last uid returned.
//!
//! Two records ride this table, in two different files:
//!
//! | Record | Written by | File |
//! |---|---|---|
//! | [`crate::StoredLog`] | the GUI, as a local cache | `~/.local/share/zensight/metrics.redb` |
//! | [`zensight_common::query_detail::LogRecord`] | the logs sensor, as the durable store behind `@rpc/logs/events` | `$STATE_DIRECTORY/logs.redb` |
//!
//! They are **not** the same record and are not being unified here (#904):
//! `StoredLog` lifts `unit` and `template_id` into typed fields, `LogRecord`
//! has neither and carries `pid` plus a `labels` catch-all instead, and
//! `LogRecord` is the richer, lossless one. Nothing reads the other's file, so
//! there is nothing to migrate and no reason to make either lossy.
//!
//! What *was* duplicated is everything under the record: the table definition
//! (declared twice, identically), the uid keying, the reverse range walk with
//! its two window bounds, and the oldest-first eviction. That is what lives
//! here now, generic over [`LogRow`].

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// redb table: log-event uid (time-sortable `<ts><seq>`) -> the serialized
/// record (JSON).
///
/// Distinct from the numeric `samples` table — per-line log events are text
/// and unbounded-cardinality, so they get their own keyed store with
/// template-aware sampling rather than the downsampled tiers (#107, C9).
pub const LOGS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("logs");

/// A record that can ride the [`LOGS_TABLE`].
///
/// The uid is the key and must be the time-sortable form, or the range walks
/// below are walking an order that is not time. `ts` is read back out of the
/// decoded record rather than parsed from the key, because the key's precision
/// is the millisecond and a window bound is not.
pub trait LogRow: Serialize + DeserializeOwned {
    /// The time-sortable uid: `<13-digit ts_ms><12-digit seq>`.
    fn uid(&self) -> &str;
    /// Event time, Unix epoch milliseconds.
    fn ts(&self) -> i64;
}

impl LogRow for crate::StoredLog {
    fn uid(&self) -> &str {
        &self.uid
    }
    fn ts(&self) -> i64 {
        self.ts
    }
}

impl LogRow for zensight_common::query_detail::LogRecord {
    fn uid(&self) -> &str {
        &self.uid
    }
    fn ts(&self) -> i64 {
        self.ts
    }
}

/// Ensure the table exists, so a read on a fresh database does not error.
pub fn ensure_table(db: &Database) -> Result<(), redb::Error> {
    let txn = db.begin_write()?;
    {
        let _ = txn.open_table(LOGS_TABLE)?;
    }
    txn.commit()?;
    Ok(())
}

/// Persist a batch, keyed by uid. Records with an empty uid are skipped (no
/// stable key), as are records that fail to serialize — one bad row must not
/// abort the batch. Returns the count written. Blocking I/O.
pub fn write_batch<T: LogRow>(db: &Database, rows: &[T]) -> Result<usize, redb::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let txn = db.begin_write()?;
    let mut written = 0usize;
    {
        let mut table = txn.open_table(LOGS_TABLE)?;
        for row in rows {
            if row.uid().is_empty() {
                continue;
            }
            let Ok(bytes) = serde_json::to_vec(row) else {
                continue;
            };
            table.insert(row.uid(), bytes.as_slice())?;
            written += 1;
        }
    }
    txn.commit()?;
    Ok(written)
}

/// Read records newest-first in one bounded page.
///
/// - `from_ms`/`to_ms`: inclusive `ts` window (`i64::MIN`/`MAX` for open).
/// - `after_uid`: cursor — only records strictly *older* than this uid (a
///   previous page's last, and therefore oldest, uid). `None` starts at the
///   newest.
/// - `limit`: page size cap.
///
/// A short page is the only end-of-walk signal there is; the caller must not
/// read one as an error. Blocking I/O.
pub fn query<T: LogRow>(
    db: &Database,
    from_ms: i64,
    to_ms: i64,
    after_uid: Option<&str>,
    limit: usize,
) -> Result<Vec<T>, redb::Error> {
    let txn = db.begin_read()?;
    let table = txn.open_table(LOGS_TABLE)?;
    let mut out = Vec::new();
    // `..cursor` excludes the cursor itself and everything newer; `.rev()`
    // yields newest-first among the remaining (older) keys.
    let iter = match after_uid {
        Some(cursor) => table.range::<&str>(..cursor)?.rev(),
        None => table.range::<&str>(..)?.rev(),
    };
    for entry in iter {
        let (_key, value) = entry?;
        let Ok(row) = serde_json::from_slice::<T>(value.value()) else {
            continue;
        };
        if row.ts() > to_ms {
            continue;
        }
        if row.ts() < from_ms {
            break; // keys are time-ordered: nothing older can qualify
        }
        out.push(row);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Evict by age, then by size. Returns the number of rows removed.
///
/// Age first, because it is the bound an operator reasons about ("a week of
/// logs"); size second, as the backstop for a burst that fills a week's budget
/// in an hour. Pass `max_age_ms = i64::MAX` for size-only. Blocking I/O.
pub fn prune<T: LogRow>(
    db: &Database,
    now_ms: i64,
    max_age_ms: i64,
    keep_max: usize,
) -> Result<usize, redb::Error> {
    let cutoff = now_ms.saturating_sub(max_age_ms);
    let txn = db.begin_write()?;
    let mut removed = 0usize;
    {
        let mut table = txn.open_table(LOGS_TABLE)?;

        // Age: the oldest keys are at the front; stop at the first in-window.
        // A row that will not decode has no readable `ts`, so it is treated as
        // infinitely old and swept — it can never be served either.
        let mut expired: Vec<String> = Vec::new();
        for entry in table.range::<&str>(..)? {
            let (key, value) = entry?;
            let ts = serde_json::from_slice::<T>(value.value())
                .map(|r| r.ts())
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

/// Row count and the oldest record's timestamp (`None` if empty). Blocking I/O.
pub fn stats<T: LogRow>(db: &Database) -> Result<(u64, Option<i64>), redb::Error> {
    let txn = db.begin_read()?;
    let table = txn.open_table(LOGS_TABLE)?;
    let records = table.len()?;
    let oldest = table
        .range::<&str>(..)?
        .next()
        .transpose()?
        .and_then(|(_, v)| serde_json::from_slice::<T>(v.value()).ok())
        .map(|r| r.ts());
    Ok((records, oldest))
}
