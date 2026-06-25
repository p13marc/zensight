//! Local tiered time-series store (Plan v3-04 §A, Plan v3-05 §5).
//!
//! Metric history used to live only in an in-memory `VecDeque` (max 500/metric),
//! lost on restart. This module adds a Netdata-style tiered store:
//!
//! - **Hot tier:** a fixed-size in-memory [`RingBuffer`] of per-second [`Sample`]s
//!   per metric — O(1) append, bounded, read directly by charts.
//! - **Warm/cold tiers:** periodic downsample to per-minute / per-hour buckets,
//!   flushed to a [`redb`]-backed [`PersistentStore`] keyed by
//!   `(metric_id, tier, bucket_ts)` so trends survive restart.
//!
//! Strong typing per the architecture contract: metric paths are interned to a
//! compact [`MetricId`]`(u32)`; samples are a plain `{ ts: i64, value: f64 }`
//! record; the `TelemetryValue` → `f64` projection lives in one place
//! ([`telemetry_to_f64`]).
//!
//! **Async discipline:** the in-memory ring append is O(1) and runs inline on the
//! Iced update thread, but every `redb` read/write is performed off the UI thread
//! via `Task::future` + `spawn_blocking` (see [`PersistentStore`] which is `Send +
//! Sync` and cloned behind an `Arc`). The UI thread never blocks on disk I/O.

// `redb::Error` is a large enum (~160 bytes); propagating it by value in `Result`
// is the natural, allocation-free API here, so we accept the size.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// redb 4 moved `begin_read` onto the `ReadableDatabase` trait.
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use zensight_common::{TelemetryPoint, TelemetryValue};

/// Default hot-ring capacity: one hour of per-second samples.
pub const DEFAULT_HOT_CAPACITY: usize = 3_600;

/// redb table: packed `(metric_id, tier, bucket_ts)` key -> downsampled value.
const SAMPLES_TABLE: TableDefinition<u128, f64> = TableDefinition::new("samples");

/// A single downsampled bucket queued for persistence: `(metric, tier, bucket_ts, value)`.
pub type FlushRow = (MetricId, Tier, i64, f64);

/// Interned identifier for a metric path. Compact key for the store, per Plan 05 §5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MetricId(pub u32);

/// A single time-series sample: a millisecond timestamp and an `f64` value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// Unix timestamp in milliseconds.
    pub ts: i64,
    /// Sample value, projected from [`TelemetryValue`].
    pub value: f64,
}

/// A downsampling tier. Each tier has a fixed bucket width in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Per-second (hot) tier.
    Second,
    /// Per-minute (warm) tier.
    Minute,
    /// Per-hour (cold) tier.
    Hour,
}

impl Tier {
    /// All tiers, coarsest last.
    pub const ALL: [Tier; 3] = [Tier::Second, Tier::Minute, Tier::Hour];

    /// Bucket width in seconds for this tier.
    pub const fn bucket_secs(self) -> i64 {
        match self {
            Tier::Second => 1,
            Tier::Minute => 60,
            Tier::Hour => 3_600,
        }
    }

    /// Stable on-disk code for this tier (used in the packed key).
    pub const fn code(self) -> u8 {
        match self {
            Tier::Second => 0,
            Tier::Minute => 1,
            Tier::Hour => 2,
        }
    }

    /// Decode a tier from its on-disk [`code`](Self::code). `None` for unknown
    /// codes (forward-compat: a future tier in an old binary is skipped).
    pub const fn from_code(code: u8) -> Option<Tier> {
        match code {
            0 => Some(Tier::Second),
            1 => Some(Tier::Minute),
            2 => Some(Tier::Hour),
            _ => None,
        }
    }

    /// How long this tier is retained on disk, in seconds (#131). Past this age a
    /// tier's buckets are eligible for eviction so the redb file stops growing
    /// unbounded. Coarser tiers are kept far longer (they cost far less per day):
    /// per-second 2 days, per-minute 30 days, per-hour 1 year — a Netdata-style
    /// progressive-retention curve.
    pub const fn retention_secs(self) -> i64 {
        match self {
            Tier::Second => 2 * 86_400,
            Tier::Minute => 30 * 86_400,
            Tier::Hour => 365 * 86_400,
        }
    }
}

/// Project a [`TelemetryValue`] to an `f64` for storage. The single typed place
/// for this conversion (counters and gauges are numeric; other variants aren't
/// charted, so they're skipped rather than coerced to a misleading zero).
pub fn telemetry_to_f64(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Counter(v) => Some(*v as f64),
        TelemetryValue::Gauge(v) => Some(*v),
        // Booleans/text/binary are not numeric series — skip, don't fake a 0.
        TelemetryValue::Boolean(_) | TelemetryValue::Text(_) | TelemetryValue::Binary(_) => None,
    }
}

/// Pack a `(metric_id, tier, bucket_ts)` triple into a single `u128` redb key.
///
/// Layout (most-significant first) keeps range scans within one
/// `(metric, tier)` contiguous and time-ordered: `metric_id` (32 bits) | `tier`
/// (8 bits) | `bucket_ts` seconds (64 bits). Bucket timestamps are non-negative,
/// so the `i64 -> u64` reinterpretation preserves ordering.
pub fn pack_key(metric: MetricId, tier: Tier, bucket_ts: i64) -> u128 {
    ((metric.0 as u128) << 72) | ((tier.code() as u128) << 64) | (bucket_ts as u64 as u128)
}

/// The lowest/highest packed keys for a `(metric, tier)` range scan.
/// Bucket timestamps are non-negative, so the low bound is bucket 0.
fn key_range(metric: MetricId, tier: Tier) -> std::ops::RangeInclusive<u128> {
    pack_key(metric, tier, 0)..=pack_key(metric, tier, i64::MAX)
}

/// Interns metric paths into compact [`MetricId`]s.
#[derive(Debug, Default)]
pub struct MetricInterner {
    ids: HashMap<String, MetricId>,
    paths: Vec<String>,
}

impl MetricInterner {
    /// Create an empty interner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `path`, returning its (possibly new) id.
    pub fn intern(&mut self, path: &str) -> MetricId {
        if let Some(id) = self.ids.get(path) {
            return *id;
        }
        let id = MetricId(self.paths.len() as u32);
        self.paths.push(path.to_string());
        self.ids.insert(path.to_string(), id);
        id
    }

    /// Look up an already-interned path's id, if present.
    pub fn get(&self, path: &str) -> Option<MetricId> {
        self.ids.get(path).copied()
    }

    /// Resolve an id back to its path.
    pub fn resolve(&self, id: MetricId) -> Option<&str> {
        self.paths.get(id.0 as usize).map(String::as_str)
    }

    /// Number of interned metrics.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether no metrics are interned yet.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Ids of all interned paths starting with `prefix` (with their path).
    pub fn with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (MetricId, &'a str)> {
        self.paths
            .iter()
            .enumerate()
            .filter(move |(_, p)| p.starts_with(prefix))
            .map(|(i, p)| (MetricId(i as u32), p.as_str()))
    }
}

/// A fixed-capacity ring of samples. Appends are O(1); the oldest sample is
/// dropped once capacity is reached (drop-oldest, bounded memory).
#[derive(Debug, Clone)]
pub struct RingBuffer {
    buf: VecDeque<Sample>,
    capacity: usize,
}

impl RingBuffer {
    /// Create a ring with the given fixed capacity (minimum 1).
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Append a sample, dropping the oldest if at capacity.
    pub fn push(&mut self, sample: Sample) {
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(sample);
    }

    /// Number of buffered samples.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Iterate over samples oldest-first.
    pub fn iter(&self) -> impl Iterator<Item = &Sample> {
        self.buf.iter()
    }

    /// Collect samples into an owned, oldest-first `Vec`.
    pub fn to_vec(&self) -> Vec<Sample> {
        self.buf.iter().copied().collect()
    }
}

/// Downsample samples into `(bucket_ts_secs, value)` pairs for a tier, using
/// last-observation-per-bucket semantics (the most recent sample in each bucket
/// wins). Pure function — the unit of testing for the tier logic.
///
/// `samples` need not be sorted; the result is sorted ascending by bucket.
pub fn downsample(samples: &[Sample], tier: Tier) -> Vec<(i64, f64)> {
    let width = tier.bucket_secs();
    // bucket_ts -> (latest_ts, value)
    let mut buckets: HashMap<i64, (i64, f64)> = HashMap::new();
    for s in samples {
        let secs = s.ts.div_euclid(1_000);
        let bucket = secs.div_euclid(width) * width;
        let entry = buckets.entry(bucket).or_insert((i64::MIN, 0.0));
        if s.ts >= entry.0 {
            *entry = (s.ts, s.value);
        }
    }
    let mut out: Vec<(i64, f64)> = buckets.into_iter().map(|(b, (_, v))| (b, v)).collect();
    out.sort_by_key(|(b, _)| *b);
    out
}

/// A redb-backed persistent store for downsampled tiers. Cloneable handle
/// (`Arc<Database>`) that is `Send + Sync`, so all of its I/O can run inside
/// `tokio::task::spawn_blocking` off the UI thread.
#[derive(Clone)]
pub struct PersistentStore {
    db: Arc<Database>,
}

impl PersistentStore {
    /// Open (creating if needed) the store database at `path`. The parent
    /// directory is created if missing. Returns an error rather than panicking
    /// so the caller can degrade gracefully to an in-memory-only store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path)?;
        // Ensure the table exists so reads on a fresh DB don't error.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(SAMPLES_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// The default on-disk location: `~/.local/share/zensight/metrics.redb`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("zensight").join("metrics.redb"))
    }

    /// Persist a batch of downsampled buckets across all tiers. `batch` is a list
    /// of `(metric_id, tier, bucket_ts, value)` tuples. Blocking I/O — call from
    /// `spawn_blocking`.
    pub fn write_batch(&self, batch: &[FlushRow]) -> Result<usize, redb::Error> {
        if batch.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin_write()?;
        let mut written = 0usize;
        {
            let mut table = txn.open_table(SAMPLES_TABLE)?;
            for (metric, tier, bucket_ts, value) in batch {
                table.insert(pack_key(*metric, *tier, *bucket_ts), *value)?;
                written += 1;
            }
        }
        txn.commit()?;
        Ok(written)
    }

    /// Read all samples for `(metric, tier)` within the inclusive millisecond
    /// time range `[from_ms, to_ms]`. Returns oldest-first `Sample`s (bucket
    /// timestamps are converted back to milliseconds). Blocking I/O.
    pub fn query(
        &self,
        metric: MetricId,
        tier: Tier,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<Sample>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SAMPLES_TABLE)?;
        let mut out = Vec::new();
        for entry in table.range(key_range(metric, tier))? {
            let (key, value) = entry?;
            let bucket_secs = (key.value() & u64::MAX as u128) as u64 as i64;
            let ts = bucket_secs * 1_000;
            if ts >= from_ms && ts <= to_ms {
                out.push(Sample {
                    ts,
                    value: value.value(),
                });
            }
        }
        Ok(out)
    }

    /// Evict buckets older than each tier's [retention](Tier::retention_secs)
    /// relative to `now_ms`, bounding on-disk growth (#131). Returns the number
    /// of buckets removed. Blocking I/O — call from `spawn_blocking`.
    ///
    /// The packed key sorts by `metric_id` first, so a tier's aged-out buckets
    /// are scattered rather than contiguous; this does one full scan to collect
    /// expired keys, then removes them. Run it infrequently (not every flush) —
    /// in steady state each pass only finds the handful of buckets that aged out
    /// since the last run.
    pub fn prune(&self, now_ms: i64) -> Result<usize, redb::Error> {
        let now_secs = now_ms.div_euclid(1_000);
        let txn = self.db.begin_write()?;
        let mut removed = 0usize;
        {
            let mut table = txn.open_table(SAMPLES_TABLE)?;
            let mut expired: Vec<u128> = Vec::new();
            for entry in table.range(0u128..=u128::MAX)? {
                let (key, _) = entry?;
                let key = key.value();
                let tier_code = ((key >> 64) & 0xFF) as u8;
                let bucket_secs = (key & u64::MAX as u128) as u64 as i64;
                if let Some(tier) = Tier::from_code(tier_code)
                    && bucket_secs < now_secs - tier.retention_secs()
                {
                    expired.push(key);
                }
            }
            for key in expired {
                table.remove(key)?;
                removed += 1;
            }
        }
        txn.commit()?;
        Ok(removed)
    }
}

/// Per-metric flush bookkeeping: pending (not-yet-persisted) samples buffered
/// since the last flush, plus the hot ring for fast in-memory reads.
#[derive(Debug)]
struct MetricSeries {
    hot: RingBuffer,
    pending: Vec<Sample>,
}

/// The UI-side metric store: interner + hot rings + a buffer of pending samples
/// awaiting flush to the persistent tiers. The `redb` handle is held behind an
/// `Arc` and only touched off-thread.
pub struct MetricStore {
    interner: MetricInterner,
    series: HashMap<MetricId, MetricSeries>,
    hot_capacity: usize,
    persistent: Option<PersistentStore>,
}

impl MetricStore {
    /// Create a store. If `persistent` is `None` the store is in-memory only
    /// (graceful degradation when the DB can't be opened).
    pub fn new(hot_capacity: usize, persistent: Option<PersistentStore>) -> Self {
        Self {
            interner: MetricInterner::new(),
            series: HashMap::new(),
            hot_capacity: hot_capacity.max(1),
            persistent,
        }
    }

    /// Build the default store: opens (or creates) the redb DB at the standard
    /// data path, degrading to in-memory only on any failure (logged, never
    /// fatal — a missing/locked DB must not crash the GUI).
    pub fn with_default_persistence() -> Self {
        let persistent = match PersistentStore::default_path() {
            Some(path) => match PersistentStore::open(&path) {
                Ok(store) => {
                    tracing::info!(path = %path.display(), "Opened metric history store");
                    Some(store)
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(),
                        "Failed to open metric store; history will be in-memory only");
                    None
                }
            },
            None => {
                tracing::warn!("No data dir available; metric history will be in-memory only");
                None
            }
        };
        Self::new(DEFAULT_HOT_CAPACITY, persistent)
    }

    /// The interned key for a device metric: `"<protocol>/<source>|<metric>"`.
    fn metric_key(point: &TelemetryPoint) -> String {
        format!("{}/{}|{}", point.protocol, point.source, point.metric)
    }

    /// Record a telemetry point. Interns its path, projects the value, and
    /// appends to the hot ring + pending buffer. Non-numeric values are ignored.
    /// O(1), safe to call inline on the UI thread.
    pub fn record(&mut self, point: &TelemetryPoint) {
        let Some(value) = telemetry_to_f64(&point.value) else {
            return;
        };
        let key = Self::metric_key(point);
        let id = self.interner.intern(&key);
        let sample = Sample {
            ts: point.timestamp,
            value,
        };
        let capacity = self.hot_capacity;
        let series = self.series.entry(id).or_insert_with(|| MetricSeries {
            hot: RingBuffer::new(capacity),
            pending: Vec::new(),
        });
        series.hot.push(sample);
        series.pending.push(sample);
    }

    /// Whether there are pending samples awaiting flush.
    pub fn has_pending(&self) -> bool {
        self.series.values().any(|s| !s.pending.is_empty())
    }

    /// Drain pending samples and build a persist batch across all tiers,
    /// downsampling each metric's pending samples. Returns the batch and a clone
    /// of the persistent handle (so the caller can run [`PersistentStore::write_batch`]
    /// in `spawn_blocking`). Returns `None` if there's nothing to flush or no DB.
    pub fn take_flush_batch(&mut self) -> Option<(PersistentStore, Vec<FlushRow>)> {
        let store = self.persistent.clone()?;
        let mut batch = Vec::new();
        for (id, series) in self.series.iter_mut() {
            if series.pending.is_empty() {
                continue;
            }
            let pending = std::mem::take(&mut series.pending);
            for tier in Tier::ALL {
                for (bucket, value) in downsample(&pending, tier) {
                    batch.push((*id, tier, bucket, value));
                }
            }
        }
        if batch.is_empty() {
            return None;
        }
        Some((store, batch))
    }

    /// Hot (in-memory) samples for a metric path, oldest-first.
    pub fn hot_samples(&self, metric_key: &str) -> Vec<Sample> {
        self.interner
            .get(metric_key)
            .and_then(|id| self.series.get(&id))
            .map(|s| s.hot.to_vec())
            .unwrap_or_default()
    }

    /// Hot (in-memory) samples for every metric of a device, oldest-first.
    /// Returns `(metric_suffix, samples)` pairs. Reads only the in-memory ring
    /// (no disk), so it's cheap to call per dashboard render (#24 sparklines).
    pub fn device_hot_samples(&self, protocol: &str, source: &str) -> Vec<(String, Vec<Sample>)> {
        let prefix = format!("{protocol}/{source}|");
        self.interner
            .with_prefix(&prefix)
            .filter_map(|(id, path)| {
                let metric = path.split_once('|').map(|(_, m)| m.to_string())?;
                let samples = self.series.get(&id).map(|s| s.hot.to_vec())?;
                Some((metric, samples))
            })
            .collect()
    }

    /// Resolve the interned ids + paths for a device, for a history pre-load.
    /// Returns `(metric_suffix, metric_id)` pairs where `metric_suffix` is the
    /// metric name (the part after `|`).
    pub fn device_metric_ids(&self, protocol: &str, source: &str) -> Vec<(String, MetricId)> {
        let prefix = format!("{protocol}/{source}|");
        self.interner
            .with_prefix(&prefix)
            .filter_map(|(id, path)| {
                path.split_once('|')
                    .map(|(_, metric)| (metric.to_string(), id))
            })
            .collect()
    }

    /// A clone of the persistent handle, if any (for off-thread queries).
    pub fn persistent(&self) -> Option<PersistentStore> {
        self.persistent.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;
    use zensight_common::Protocol;

    fn point(metric: &str, value: f64, ts: i64) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: ts,
            source: "dev1".to_string(),
            protocol: Protocol::Sysinfo,
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(value),
            labels: Map::new(),
        }
    }

    #[test]
    fn interner_assigns_stable_ids() {
        let mut i = MetricInterner::new();
        let a = i.intern("cpu");
        let b = i.intern("mem");
        let a2 = i.intern("cpu");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(i.resolve(a), Some("cpu"));
        assert_eq!(i.resolve(b), Some("mem"));
        assert_eq!(i.len(), 2);
        assert_eq!(i.get("mem"), Some(b));
        assert_eq!(i.get("nope"), None);
    }

    #[test]
    fn interner_prefix_scan() {
        let mut i = MetricInterner::new();
        i.intern("snmp/r1|cpu");
        i.intern("snmp/r1|mem");
        i.intern("snmp/r2|cpu");
        let mut found: Vec<_> = i
            .with_prefix("snmp/r1|")
            .map(|(_, p)| p.to_string())
            .collect();
        found.sort();
        assert_eq!(found, vec!["snmp/r1|cpu", "snmp/r1|mem"]);
    }

    #[test]
    fn ring_buffer_drops_oldest() {
        let mut r = RingBuffer::new(3);
        for ts in 0..5 {
            r.push(Sample {
                ts,
                value: ts as f64,
            });
        }
        assert_eq!(r.len(), 3);
        let tss: Vec<i64> = r.iter().map(|s| s.ts).collect();
        assert_eq!(tss, vec![2, 3, 4]);
    }

    #[test]
    fn ring_buffer_minimum_capacity() {
        let mut r = RingBuffer::new(0);
        r.push(Sample { ts: 1, value: 1.0 });
        r.push(Sample { ts: 2, value: 2.0 });
        assert_eq!(r.len(), 1);
        assert_eq!(r.iter().next().unwrap().ts, 2);
    }

    #[test]
    fn downsample_last_per_bucket() {
        // Two samples in the same minute, one in the next.
        let samples = vec![
            Sample {
                ts: 60_000,
                value: 1.0,
            },
            Sample {
                ts: 90_000,
                value: 2.0,
            },
            Sample {
                ts: 120_000,
                value: 3.0,
            },
        ];
        // Minute tier: buckets at 60s and 120s; 90s>60s so last-in-bucket = 2.0.
        let minute = downsample(&samples, Tier::Minute);
        assert_eq!(minute, vec![(60, 2.0), (120, 3.0)]);
        // Hour tier: all three fall in the 0s bucket; last (ts=120_000) wins.
        let hour = downsample(&samples, Tier::Hour);
        assert_eq!(hour, vec![(0, 3.0)]);
    }

    #[test]
    fn downsample_unsorted_input() {
        let samples = vec![
            Sample {
                ts: 5_000,
                value: 5.0,
            },
            Sample {
                ts: 1_000,
                value: 1.0,
            },
        ];
        // Both in the 0s minute/hour bucket; the later ts (5_000) wins.
        assert_eq!(downsample(&samples, Tier::Minute), vec![(0, 5.0)]);
    }

    #[test]
    fn pack_key_orders_by_metric_tier_bucket() {
        let m0 = MetricId(0);
        let m1 = MetricId(1);
        // Same metric+tier: ordered by bucket.
        assert!(pack_key(m0, Tier::Second, 1) < pack_key(m0, Tier::Second, 2));
        // Tier ordering within a metric.
        assert!(pack_key(m0, Tier::Second, i64::MAX) < pack_key(m0, Tier::Minute, 0));
        // Metric ordering dominates.
        assert!(pack_key(m0, Tier::Hour, i64::MAX) < pack_key(m1, Tier::Second, 0));
    }

    #[test]
    fn store_records_only_numeric() {
        let mut store = MetricStore::new(10, None);
        store.record(&point("cpu", 50.0, 1_000));
        let mut p = point("name", 0.0, 2_000);
        p.value = TelemetryValue::Text("hello".into());
        store.record(&p);
        // Only the numeric metric is tracked.
        assert_eq!(store.hot_samples("sysinfo/dev1|cpu").len(), 1);
        assert_eq!(store.hot_samples("sysinfo/dev1|name").len(), 0);
    }

    #[test]
    fn store_hot_samples_and_device_ids() {
        let mut store = MetricStore::new(10, None);
        store.record(&point("cpu", 50.0, 1_000));
        store.record(&point("cpu", 55.0, 2_000));
        store.record(&point("mem", 10.0, 1_500));
        let cpu = store.hot_samples("sysinfo/dev1|cpu");
        assert_eq!(cpu.len(), 2);
        assert_eq!(cpu[1].value, 55.0);
        let mut ids = store.device_metric_ids("sysinfo", "dev1");
        ids.sort();
        let names: Vec<String> = ids.into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["cpu".to_string(), "mem".to_string()]);
    }

    #[test]
    fn store_no_persistence_no_flush() {
        let mut store = MetricStore::new(10, None);
        store.record(&point("cpu", 1.0, 1_000));
        assert!(store.has_pending());
        // No persistent handle => no flush batch.
        assert!(store.take_flush_batch().is_none());
    }

    fn temp_db_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("zensight-store-test-{tag}-{nanos}.redb"));
        p
    }

    #[test]
    fn persistent_round_trip() {
        let path = temp_db_path("rt");
        let store = PersistentStore::open(&path).expect("open");
        let m = MetricId(7);
        let batch = vec![
            (m, Tier::Minute, 60, 1.5),
            (m, Tier::Minute, 120, 2.5),
            (m, Tier::Hour, 0, 9.0),
        ];
        assert_eq!(store.write_batch(&batch).unwrap(), 3);
        // Minute tier within [60_000, 120_000] ms returns both buckets.
        let got = store.query(m, Tier::Minute, 0, 200_000).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ts, 60_000);
        assert_eq!(got[0].value, 1.5);
        assert_eq!(got[1].ts, 120_000);
        // Hour tier is a separate keyspace.
        let hour = store.query(m, Tier::Hour, 0, 200_000).unwrap();
        assert_eq!(hour, vec![Sample { ts: 0, value: 9.0 }]);
        // A different metric id is isolated.
        assert!(
            store
                .query(MetricId(8), Tier::Minute, 0, 200_000)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tier_code_roundtrip_and_retention_ordering() {
        for tier in Tier::ALL {
            assert_eq!(Tier::from_code(tier.code()), Some(tier));
        }
        assert_eq!(Tier::from_code(99), None);
        // Coarser tiers retain strictly longer.
        assert!(Tier::Second.retention_secs() < Tier::Minute.retention_secs());
        assert!(Tier::Minute.retention_secs() < Tier::Hour.retention_secs());
    }

    #[test]
    fn prune_evicts_only_aged_out_buckets_per_tier() {
        let path = temp_db_path("prune");
        let store = PersistentStore::open(&path).expect("open");
        let m = MetricId(3);
        let day = 86_400i64;
        let now_secs = 400 * day; // far enough out that all retentions are exceeded by bucket 0
        let now_ms = now_secs * 1_000;
        // For each tier: one ancient bucket (older than retention -> evicted) and
        // one fresh bucket (within retention -> kept).
        let fresh_minute = now_secs - day; // < 30d old
        let fresh_hour = now_secs - 100 * day; // < 365d old
        let fresh_second = now_secs - day; // 1d < 2d retention -> kept
        let batch = vec![
            (m, Tier::Minute, 0, 1.0),            // ancient -> evicted
            (m, Tier::Minute, fresh_minute, 2.0), // fresh -> kept
            (m, Tier::Hour, 0, 3.0),              // ancient -> evicted
            (m, Tier::Hour, fresh_hour, 4.0),     // fresh -> kept
            (m, Tier::Second, 0, 5.0),            // ancient -> evicted
            (m, Tier::Second, fresh_second, 6.0), // fresh -> kept
        ];
        store.write_batch(&batch).unwrap();

        let removed = store.prune(now_ms).unwrap();
        assert_eq!(removed, 3, "the three ancient buckets are evicted");

        // The fresh buckets survive; the ancient ones are gone.
        let minute = store.query(m, Tier::Minute, 0, now_ms).unwrap();
        assert_eq!(
            minute,
            vec![Sample {
                ts: fresh_minute * 1_000,
                value: 2.0
            }]
        );
        let hour = store.query(m, Tier::Hour, 0, now_ms).unwrap();
        assert_eq!(
            hour,
            vec![Sample {
                ts: fresh_hour * 1_000,
                value: 4.0
            }]
        );
        let second = store.query(m, Tier::Second, 0, now_ms).unwrap();
        assert_eq!(
            second,
            vec![Sample {
                ts: fresh_second * 1_000,
                value: 6.0
            }]
        );

        // Idempotent: a second prune with the same clock removes nothing.
        assert_eq!(store.prune(now_ms).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn store_flush_persists_and_clears_pending() {
        let path = temp_db_path("flush");
        let persistent = PersistentStore::open(&path).expect("open");
        let mut store = MetricStore::new(10, Some(persistent.clone()));
        store.record(&point("cpu", 42.0, 60_000));
        store.record(&point("cpu", 43.0, 90_000));
        let (handle, batch) = store.take_flush_batch().expect("batch");
        assert!(!batch.is_empty());
        handle.write_batch(&batch).unwrap();
        // Pending cleared after taking the batch.
        assert!(!store.has_pending());
        // Read back the minute tier for the cpu metric.
        let id = store
            .device_metric_ids("sysinfo", "dev1")
            .into_iter()
            .find(|(n, _)| n == "cpu")
            .map(|(_, id)| id)
            .unwrap();
        let got = persistent.query(id, Tier::Minute, 0, 200_000).unwrap();
        assert_eq!(
            got,
            vec![Sample {
                ts: 60_000,
                value: 43.0
            }]
        );
        let _ = std::fs::remove_file(&path);
    }
}
