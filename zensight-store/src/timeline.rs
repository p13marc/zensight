//! The `timeline` table: durable events and alert transitions (#908).
//!
//! The tiers answer "what was this number"; this answers "what *happened*".
//! They are different questions and want different storage: a transition is a
//! rare, textual, append-only record, and downsampling one would be
//! meaningless.
//!
//! # Why the uid is derived, not minted
//!
//! Every row's key is `<13-digit ts_ms><16-hex digest of the row's identity>`,
//! computed from the record rather than generated. That makes a write
//! **idempotent**, which is what lets the historian subscribe with history and
//! recovery without inventing transitions.
//!
//! The alternative — a fresh ULID per observed sample — breaks on restart. An
//! AdvancedSubscriber replays the alerts that are currently active, and a
//! timeline that minted a new id for each replay would show one firing as
//! three, once per restart, all stamped with the original time. Deriving the
//! key means the replay overwrites the row it already wrote, and the answer to
//! "when did this fire" stays "once, then".
//!
//! The digest is FNV-1a, written out here rather than taken from
//! `DefaultHasher`: this value is **on disk**, so it has to mean the same
//! thing in the next build, and `DefaultHasher`'s output is explicitly not
//! guaranteed stable across Rust releases.

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};

/// redb table: timeline uid -> serialized [`TimelineRow`].
pub const TIMELINE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("timeline");

/// What kind of thing a row records. Mirrors
/// [`zensight_common::history::TimelineKind`] on the wire.
pub type TimelineKind = zensight_common::history::TimelineKind;

/// One durable transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineRow {
    /// Derived key — see the module doc.
    pub uid: String,
    /// When it happened (epoch ms).
    pub ts: i64,
    pub kind: TimelineKind,
    /// The publishing host's v1 origin.
    pub origin: String,
    /// The key it rode, so a reader can go back to the source of truth.
    pub key: String,
    /// Whether this is the thing appearing or clearing.
    ///
    /// An alert timeline that recorded only firings would show every incident
    /// as permanent — the clear is half the story, and it is the half that
    /// says whether anyone needs to look.
    pub active: bool,
    /// A short human summary; the full record is at `key`.
    pub summary: Option<String>,
}

impl TimelineRow {
    /// Build a row, deriving its uid from `(ts, kind, key, active)`.
    ///
    /// `active` is part of the identity on purpose: a fire and a clear on the
    /// same key in the same millisecond are two transitions, and collapsing
    /// them would lose the one that matters.
    pub fn new(
        ts: i64,
        kind: TimelineKind,
        origin: impl Into<String>,
        key: impl Into<String>,
        active: bool,
        summary: Option<String>,
    ) -> Self {
        let key = key.into();
        let digest = fnv1a(&format!("{}|{}|{}", kind.as_str(), key, active));
        Self {
            uid: format!("{:013}{digest:016x}", ts.max(0)),
            ts,
            kind,
            origin: origin.into(),
            key,
            active,
            summary,
        }
    }
}

/// FNV-1a, 64-bit. Spelled out because the value is persisted: it has to mean
/// the same thing in the next build, and `std`'s hasher makes no such promise.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Ensure the table exists.
pub fn ensure_table(db: &Database) -> Result<(), redb::Error> {
    let txn = db.begin_write()?;
    {
        let _ = txn.open_table(TIMELINE_TABLE)?;
    }
    txn.commit()?;
    Ok(())
}

/// Persist rows, keyed by their derived uid. Idempotent: a row that was
/// already written is overwritten with itself.
pub fn write_batch(db: &Database, rows: &[TimelineRow]) -> Result<usize, redb::Error> {
    if rows.is_empty() {
        return Ok(0);
    }
    let txn = db.begin_write()?;
    let mut written = 0usize;
    {
        let mut table = txn.open_table(TIMELINE_TABLE)?;
        for row in rows {
            let Ok(bytes) = serde_json::to_vec(row) else {
                continue;
            };
            table.insert(row.uid.as_str(), bytes.as_slice())?;
            written += 1;
        }
    }
    txn.commit()?;
    Ok(written)
}

/// Read rows newest-first in one bounded page.
///
/// - `from_ms`/`to_ms`: inclusive `ts` window.
/// - `kinds`: empty means both.
/// - `origin`: `None` means every host.
/// - `after_uid`: cursor — rows strictly older than this uid.
/// - `limit`: page size cap.
///
/// The uid's leading 13 digits are the timestamp, so the table is time-ordered
/// by construction and this is a bounded reverse range walk — the same shape
/// as the logs table, which is where the pattern comes from.
pub fn query(
    db: &Database,
    from_ms: i64,
    to_ms: i64,
    kinds: &[TimelineKind],
    origin: Option<&str>,
    after_uid: Option<&str>,
    limit: usize,
) -> Result<Vec<TimelineRow>, redb::Error> {
    let txn = db.begin_read()?;
    let table = txn.open_table(TIMELINE_TABLE)?;
    let mut out = Vec::new();
    let iter = match after_uid {
        Some(cursor) => table.range::<&str>(..cursor)?.rev(),
        None => table.range::<&str>(..)?.rev(),
    };
    for entry in iter {
        let (_k, v) = entry?;
        let Ok(row) = serde_json::from_slice::<TimelineRow>(v.value()) else {
            continue;
        };
        if row.ts > to_ms {
            continue;
        }
        if row.ts < from_ms {
            break; // time-ordered: nothing older can qualify
        }
        if !kinds.is_empty() && !kinds.contains(&row.kind) {
            continue;
        }
        if let Some(o) = origin
            && row.origin != o
        {
            continue;
        }
        out.push(row);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Evict the oldest rows beyond `keep_max`. Returns the number removed.
pub fn prune(db: &Database, keep_max: usize) -> Result<usize, redb::Error> {
    let txn = db.begin_write()?;
    let mut removed = 0usize;
    {
        let mut table = txn.open_table(TIMELINE_TABLE)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("zensight-timeline-{tag}-{ns}.redb"))
    }

    fn row(ts: i64, key: &str, active: bool) -> TimelineRow {
        TimelineRow::new(ts, TimelineKind::Alert, "h-0123456789ab", key, active, None)
    }

    /// The property the whole design rests on: writing the same transition
    /// twice leaves one row. An AdvancedSubscriber replays the alerts that are
    /// currently active on every restart, and a minted id would turn one
    /// firing into one row per restart — all stamped with the original time,
    /// so nothing downstream could tell them apart.
    #[test]
    fn replaying_a_transition_does_not_duplicate_it() {
        let path = tmp("idempotent");
        let db = Database::create(&path).unwrap();
        ensure_table(&db).unwrap();

        let r = row(1_000, "v1/h-0123456789ab/state/sysinfo/alert/cpu", true);
        // Three separate writes of the identical transition — what a
        // reconnecting subscriber produces.
        for _ in 0..3 {
            write_batch(&db, std::slice::from_ref(&r)).unwrap();
        }

        let all = query(&db, i64::MIN, i64::MAX, &[], None, None, 100).unwrap();
        assert_eq!(all.len(), 1, "three writes of one transition are one row");
        assert_eq!(all[0], r);
        let _ = std::fs::remove_file(&path);
    }

    /// A fire and a clear are two transitions even in the same millisecond —
    /// `active` is part of the identity, because collapsing them would lose
    /// the one that says whether anyone still needs to look.
    #[test]
    fn a_fire_and_a_clear_are_two_rows() {
        let path = tmp("fire-clear");
        let db = Database::create(&path).unwrap();
        ensure_table(&db).unwrap();
        let key = "v1/h-0123456789ab/state/sysinfo/alert/cpu";
        write_batch(&db, &[row(1_000, key, true), row(1_000, key, false)]).unwrap();
        let all = query(&db, i64::MIN, i64::MAX, &[], None, None, 100).unwrap();
        assert_eq!(all.len(), 2);
        assert_ne!(all[0].uid, all[1].uid);
        let _ = std::fs::remove_file(&path);
    }

    /// Newest-first, windowed, filtered, and paged by cursor — the
    /// `@rpc/logs/events` contract, on a different table.
    #[test]
    fn the_walk_is_newest_first_windowed_and_cursor_paged() {
        let path = tmp("walk");
        let db = Database::create(&path).unwrap();
        ensure_table(&db).unwrap();

        let mut rows = Vec::new();
        for i in 0..10i64 {
            rows.push(row(i * 1_000, &format!("k{i}"), true));
        }
        rows.push(TimelineRow::new(
            5_500,
            TimelineKind::Event,
            "h-ffffffffffff",
            "ev",
            true,
            Some("a trap".into()),
        ));
        write_batch(&db, &rows).unwrap();

        // Newest first.
        let page = query(&db, i64::MIN, i64::MAX, &[], None, None, 3).unwrap();
        assert_eq!(page.len(), 3);
        assert!(page[0].ts >= page[1].ts && page[1].ts >= page[2].ts);

        // The cursor continues strictly older, with no repeat.
        let next = query(&db, i64::MIN, i64::MAX, &[], None, Some(&page[2].uid), 3).unwrap();
        assert!(next.iter().all(|r| r.uid < page[2].uid));
        assert!(next.iter().all(|r| !page.iter().any(|p| p.uid == r.uid)));

        // A window excludes what falls outside it.
        let windowed = query(&db, 3_000, 6_000, &[], None, None, 100).unwrap();
        assert!(windowed.iter().all(|r| r.ts >= 3_000 && r.ts <= 6_000));

        // Kind and origin narrow it.
        let events = query(
            &db,
            i64::MIN,
            i64::MAX,
            &[TimelineKind::Event],
            None,
            None,
            100,
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary.as_deref(), Some("a trap"));

        let by_origin = query(
            &db,
            i64::MIN,
            i64::MAX,
            &[],
            Some("h-ffffffffffff"),
            None,
            100,
        )
        .unwrap();
        assert_eq!(by_origin.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_keeps_the_newest() {
        let path = tmp("prune");
        let db = Database::create(&path).unwrap();
        ensure_table(&db).unwrap();
        let rows: Vec<TimelineRow> = (0..10i64)
            .map(|i| row(i * 1_000, &format!("k{i}"), true))
            .collect();
        write_batch(&db, &rows).unwrap();
        assert_eq!(prune(&db, 4).unwrap(), 6);
        let left = query(&db, i64::MIN, i64::MAX, &[], None, None, 100).unwrap();
        assert_eq!(left.len(), 4);
        assert!(left.iter().all(|r| r.ts >= 6_000), "the newest survive");
        let _ = std::fs::remove_file(&path);
    }

    /// The digest is on disk, so it must not move with the toolchain.
    #[test]
    fn the_derived_uid_is_stable() {
        let r = TimelineRow::new(
            1_700_000_000_000,
            TimelineKind::Alert,
            "h-0123456789ab",
            "v1/h-0123456789ab/state/sysinfo/alert/cpu",
            true,
            None,
        );
        assert_eq!(r.uid.len(), 13 + 16);
        assert!(r.uid.starts_with("1700000000000"));
        // Recomputing the same identity gives the same key — the whole point.
        let again = TimelineRow::new(
            1_700_000_000_000,
            TimelineKind::Alert,
            "h-0123456789ab",
            "v1/h-0123456789ab/state/sysinfo/alert/cpu",
            true,
            Some("summaries do not change identity".into()),
        );
        assert_eq!(r.uid, again.uid);
    }
}
