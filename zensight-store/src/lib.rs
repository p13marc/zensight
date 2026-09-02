//! The tiered time-series store: hot ring, minute/hour redb tiers, and the
//! log, event and chunk tables that ride the same file.
//!
//! This was `zensight::store` — a module inside the Iced binary, writing
//! `~/.local/share/zensight/metrics.redb`, readable by nothing but the GUI
//! that wrote it (#904). On a fleet that GUI is open for minutes a week, so
//! the history it holds is mostly gaps. It is a crate now so a headless
//! service can write the same tiers and serve them to everyone.
//!
//! - **Hot tier:** a fixed-size in-memory [`RingBuffer`] of per-second
//!   [`Sample`]s per metric — O(1) append, bounded, read directly by charts.
//! - **Warm/cold tiers:** periodic downsample to per-minute / per-hour
//!   buckets, flushed to a [`redb`]-backed [`PersistentStore`] keyed by
//!   `(metric_id, tier, bucket_ts)` so trends survive restart.
//!
//! Strong typing per the architecture contract: metric paths are interned to a
//! compact [`MetricId`]`(u32)`; samples are a plain `{ ts: i64, value: f64 }`
//! record; the `TelemetryValue` → `f64` projection lives in one place
//! ([`telemetry_to_f64`]).
//!
//! **Async discipline:** the in-memory ring append is O(1) and runs inline on
//! the caller's thread (the Iced update thread, in the GUI), but every `redb`
//! read/write is meant to run off it via `spawn_blocking` — [`PersistentStore`]
//! is `Send + Sync` and cloned behind an `Arc` precisely so it can. The
//! batching seam is explicit in the API: `record` / `record_log` /
//! `record_event` accumulate, `take_*_flush_batch` hand off
//! `(PersistentStore, rows)`.

// `redb::Error` is a large enum (~160 bytes); propagating it by value in `Result`
// is the natural, allocation-free API here, so we accept the size.
#![allow(clippy::result_large_err)]

pub mod logs;
pub mod rate;
pub mod timeline;

// Re-exported so a caller can name the error type its store operations return
// without taking a direct dependency on redb — and without the version of
// redb it pinned mattering to whether that name resolves.
pub use redb;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// redb 4 moved `begin_read` onto the `ReadableDatabase` trait.
use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle,
};

use serde::{Deserialize, Serialize};
use zensight_common::{TelemetryPoint, TelemetryValue};

/// Default hot-ring capacity: one hour of per-second samples.
pub const DEFAULT_HOT_CAPACITY: usize = 3_600;

/// Default redb page-cache budget, used by [`PersistentStore::open`].
///
/// redb's own default is **1 GiB** (#625). The logs sensor has set an explicit
/// budget since that issue, because on a 1–2 GB VM the default reads as a slow
/// multi-day RSS climb toward OOM as the database grows; this store never did,
/// which was fine while its only caller was a desktop GUI and is not fine now
/// that a headless service on those same VMs opens it. 64 MiB is ample for a
/// file whose hot path is a bounded range walk.
pub const DEFAULT_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// redb table: packed `(metric_id, tier, bucket_ts)` key -> downsampled
/// [`Bucket`], stored as `(last, min, max)`.
///
/// v3 widened the value from a bare `f64` (#904). One number per bucket can
/// answer "what was it at the end of this minute" and nothing else, so a
/// query for a day at the hour tier could not say whether a gauge that reads
/// 12 now had touched 400 in between — the spike was averaged out of
/// existence by the downsample before any reader could ask. `min`/`max` are
/// `f32`: they bound a range for a chart, they are not the value, and 4 bytes
/// each keeps the bucket at 16.
const SAMPLES_TABLE: TableDefinition<u128, (f64, f32, f32)> = TableDefinition::new("samples");

/// redb table: interned metric path -> its [`MetricId`]. The samples table
/// is keyed by the id, so the id must mean the same path in every process
/// that opens the file. It did not, for a long time: ids were minted in
/// network-arrival order and never written down, so a restart re-numbered
/// every metric and a chart seeded "from history" read back another metric's
/// buckets — plausible numbers, wrong series, for up to 30 days.
/// v3 (#904) widened the row from a bare id to `(id, kind, source, metric)`:
///
/// - **kind** — a counter reset and a gauge that fell are the same negative
///   delta once every value is an `f64`, which is why three separate callers
///   in the GUI each re-inferred resets by hand. The store threw the
///   distinction away at ingest; now it keeps it, and the rate is computed
///   once where the kind is known.
/// - **unit** — UCUM-style (`"By"`, `"By/s"`, `"%"`), empty when the producer
///   declared none. A `range` reply promises one, and the caller that cannot
///   supply it from elsewhere is exactly the one that matters: a chart opening
///   on a fleet whose sensors are quiet has no live sample to read it from.
/// - **source** and **metric** — the series path is
///   `<origin>/<producer>/<subject>`, the wire key minus the class chunk, so
///   that a reader holding only a sample can name its series. Neither the
///   observed device nor the display metric name survives that on its own: a
///   proxy producer's subject is `{device}/{metric...}`, so recovering either
///   would mean un-slugging a device chunk — a guess, in the one place that
///   must not guess. Both are free at ingest, so both are written down.
const METRICS_TABLE: TableDefinition<&str, (u32, u8, &str, &str, &str)> =
    TableDefinition::new("metrics");

/// redb table: store-level metadata. One row, `schema` -> [`SCHEMA_VERSION`].
const META_TABLE: TableDefinition<&str, u64> = TableDefinition::new("meta");

/// The on-disk layout this code writes and can read. A file without a `meta`
/// row that already holds samples is from before ids were persisted (v1);
/// its rows are keyed by ids nobody can map back to a path, so it is moved
/// aside rather than read (see [`MetricStore::with_default_persistence`]).
///
/// v2: `metrics` (path -> id) and `meta` tables; metric paths carry the
/// publishing origin (`<protocol>/<origin>/<source>|<metric>`).
///
/// v3 (#904): series paths become `<origin>/<producer>/<subject>` — the wire
/// key minus the class chunk, so the GUI's cache and the fleet historian name
/// the same series the same way; `metrics` rows carry `(id, kind, source,
/// metric)`; `samples` values become `{last, min, max}` buckets. Every one of
/// those re-types a table, so a v2 file is not readable by this code and is
/// moved aside rather than migrated (it is a cache; the fleet history it
/// shadows outlives it).
///
/// v4 (#907): the `metrics` row also carries the series' **unit**. The
/// historian's `range` and `series` replies declare a `unit` field, and a
/// declared field that is structurally always absent is a lie in the schema —
/// a chart opening on a fleet whose sensors are quiet has no live sample to
/// take the unit from, so the store is where it has to survive.
pub const SCHEMA_VERSION: u64 = 4;

use crate::logs::LOGS_TABLE;

/// redb table: event ULID -> serialized [`zensight_common::EventRecord`]
/// (#578). Events are the `events` class's durable records (SNMP traps
/// today); ULIDs sort chronologically, so the table is time-ordered by
/// construction and a "recent events" read is a bounded reverse range walk —
/// the same shape as [`LOGS_TABLE`], without the template sampling (an event
/// is already a rare, deliberate record).
const EVENTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("events");

/// redb table: content-addressed chunk store (#199, Tier-2). Key is `<algo>/<hex>`
/// (the chunk's content hash); value is the raw chunk bytes. Immutable + idempotent
/// — a chunk is written once and read by hash, so this doubles as the directory-sync
/// dedup + resume substrate (resume = "which hashes are already on disk").
const CHUNKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("chunks");

/// OTel `severity_number` at/above which a log line is treated as an error and
/// always persisted (17 = ERROR; FATAL is 21-24).
pub const LOG_ERROR_SEVERITY: u8 = 17;

/// Keep 1-in-N repetitive (known-template, non-error) info lines on disk.
pub const LOG_SAMPLE_EVERY: u64 = 10;

/// Cap on persisted log rows; the oldest beyond this are pruned so the redb file
/// stops growing (the log analogue of [`Tier::retention_secs`]).
pub const LOG_STORE_MAX_ROWS: usize = 200_000;

/// Cap on persisted event rows (#578). Two orders below the log cap: traps are
/// rare by nature, and a trap storm should not evict a week of history.
pub const EVENT_STORE_MAX_ROWS: usize = 20_000;

/// Rows kept in the `timeline` table (#908).
///
/// Between the log cap and the event cap: a transition is rarer than a log
/// line and more common than a trap, and unlike either it is what a reader
/// scrubs back through — so the bound is set by "how far back can I scroll",
/// not by "how much can I afford".
pub const TIMELINE_STORE_MAX_ROWS: usize = 100_000;

/// A single downsampled bucket queued for persistence: `(metric, tier, bucket_ts, value)`.
pub type FlushRow = (MetricId, Tier, i64, Bucket);

/// One flush: the downsampled rows, and the `(path, id)` pairs interned since
/// the last flush, written in one transaction by [`PersistentStore::write_batch`].
#[derive(Debug, Default)]
pub struct FlushBatch {
    pub rows: Vec<FlushRow>,
    /// `(series path, id, meta)` for every metric interned since the last
    /// flush — written in the same transaction as `rows`, so a sample row
    /// never lands without the path and kind its id means.
    pub paths: Vec<(String, u32, MetricMeta)>,
}

/// Why a store file could not be opened.
#[derive(Debug)]
pub enum StoreOpenError {
    Redb(redb::Error),
    /// The file's layout is not [`SCHEMA_VERSION`]; `found` is what it is.
    Schema {
        found: u64,
    },
}

impl<E> From<E> for StoreOpenError
where
    redb::Error: From<E>,
{
    fn from(e: E) -> Self {
        StoreOpenError::Redb(redb::Error::from(e))
    }
}

impl std::fmt::Display for StoreOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreOpenError::Redb(e) => write!(f, "{e}"),
            StoreOpenError::Schema { found } => {
                write!(f, "metric store schema v{found} is not v{SCHEMA_VERSION}")
            }
        }
    }
}

/// What a series *is*, kept per metric since v3 (#904).
///
/// A counter and a gauge are the same `f64` on the way in and two different
/// questions on the way out: a counter that goes backwards restarted, a gauge
/// that goes backwards fell. Flattening both to a number is why three callers
/// in the GUI each had to re-infer resets from a negative delta, and why none
/// of them could be sure it was the same rule.
///
/// **The same type the wire uses.** It is
/// [`zensight_common::history::SeriesKind`] under an alias, not a parallel
/// enum: the on-disk code below and the lowercase wire token are two encodings
/// of one vocabulary, and two enums would be two things to keep in step for no
/// gain. The historian reads a kind out of this store and puts it in a
/// `RangeReply` without a conversion, which is the point.
pub type MetricKind = zensight_common::history::SeriesKind;

/// On-disk code for a [`MetricKind`], and back.
///
/// A free function pair rather than inherent methods, because the type is
/// `zensight-common`'s and the *storage* encoding is this crate's business.
/// Unknown codes read back as `Gauge`, the interpretation that invents
/// nothing: it never claims a reset.
pub const fn kind_code(kind: MetricKind) -> u8 {
    match kind {
        MetricKind::Gauge => 0,
        MetricKind::Counter => 1,
        MetricKind::Bool => 2,
    }
}

/// Decode a [`MetricKind`] from its on-disk [`kind_code`].
pub const fn kind_from_code(code: u8) -> MetricKind {
    match code {
        1 => MetricKind::Counter,
        2 => MetricKind::Bool,
        _ => MetricKind::Gauge,
    }
}

/// One measurement on the way in, with its kind intact.
///
/// [`telemetry_to_f64`] is the lossy projection this replaces at the ingest
/// seam; it stays for the callers that genuinely only want a number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SampleValue {
    Counter(u64),
    Gauge(f64),
    Bool(bool),
}

impl SampleValue {
    /// Project a [`TelemetryValue`], or `None` for text and binary — they are
    /// not numeric series, and a fabricated `0.0` would be a claim.
    pub fn from_telemetry(value: &TelemetryValue) -> Option<SampleValue> {
        match value {
            TelemetryValue::Counter(v) => Some(SampleValue::Counter(*v)),
            TelemetryValue::Gauge(v) => Some(SampleValue::Gauge(*v)),
            TelemetryValue::Boolean(b) => Some(SampleValue::Bool(*b)),
            TelemetryValue::Text(_) | TelemetryValue::Binary(_) => None,
        }
    }

    /// This value as the `f64` the tiers store.
    pub fn as_f64(self) -> f64 {
        match self {
            SampleValue::Counter(v) => v as f64,
            SampleValue::Gauge(v) => v,
            SampleValue::Bool(b) => {
                if b {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// The kind this value implies.
    pub const fn kind(self) -> MetricKind {
        match self {
            SampleValue::Counter(_) => MetricKind::Counter,
            SampleValue::Gauge(_) => MetricKind::Gauge,
            SampleValue::Bool(_) => MetricKind::Bool,
        }
    }
}

/// One downsampled bucket: the last observation in it, and the range it
/// covered.
///
/// `last` is the value — the tier semantics are last-observation-per-bucket,
/// unchanged from v2. `min`/`max` exist so a coarse tier can still say a spike
/// happened: at the hour tier a v2 bucket could only report where the value
/// landed on the hour, so a gauge that touched 400 and settled at 12 read as
/// twelve, flat.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    /// The most recent sample's value in this bucket.
    pub last: f64,
    /// The lowest value seen in this bucket.
    pub min: f32,
    /// The highest value seen in this bucket.
    pub max: f32,
}

impl Bucket {
    /// A bucket holding a single observation.
    pub fn point(value: f64) -> Bucket {
        Bucket {
            last: value,
            min: value as f32,
            max: value as f32,
        }
    }

    /// The on-disk triple.
    pub fn as_row(self) -> (f64, f32, f32) {
        (self.last, self.min, self.max)
    }

    /// Read back from the on-disk triple.
    pub fn from_row((last, min, max): (f64, f32, f32)) -> Bucket {
        Bucket { last, min, max }
    }
}

/// Everything about a metric that its series path does not carry (#904).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricMeta {
    /// Counter, gauge or bool — see [`MetricKind`].
    pub kind: MetricKind,
    /// UCUM-style unit (`"By"`, `"By/s"`, `"%"`, `"s"`), when the producer
    /// declared one. Absent is *unknown*, never *dimensionless*.
    pub unit: Option<String>,
    /// The observed device: [`TelemetryPoint::source`]. The publishing host
    /// for a host sensor, the polled device for a proxy sensor.
    pub source: String,
    /// The display metric name: [`TelemetryPoint::metric`]. For a proxy
    /// producer this is the subject *minus* its leading device chunk.
    pub metric: String,
}

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
        // Booleans become a 0/1 step series (#126) so flap-prone signals (iface
        // up/carrier, route present, wg up) get history + trend, not a snapshot.
        TelemetryValue::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
        // Text/binary aren't numeric series — skip, don't fake a 0.
        TelemetryValue::Text(_) | TelemetryValue::Binary(_) => None,
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

/// Interns metric paths into compact [`MetricId`]s, and holds the per-metric
/// [`MetricMeta`] the path itself does not carry.
///
/// Since v3 (#904) a series path is `"<origin>/<producer>/<subject>"` — the
/// wire key minus the class chunk, which is what makes a sample
/// self-identifying and lets the GUI's local cache and the fleet historian
/// name the same series the same way. It replaced
/// `"<protocol>/<origin>/<source>|<metric>"`, which encoded the observed
/// device and the display metric name in the path and so could be taken apart
/// with a `split_once('|')`. Those two are now stored beside the id instead:
/// `by_device` indexes `"<producer>/<origin>/<source>"` from the recorded
/// `source` rather than from a prefix of the path, so per-device lookups stay
/// O(metrics-for-that-device) without the path having to carry the device.
///
/// The origin is in the path for the reason `DeviceId` carries one (#474):
/// two hosts reporting the same hostname (`localhost`, a cloned image, two
/// containers named alike) are two devices, and a key without the origin
/// interleaved their samples into one sawtooth series.
#[derive(Debug, Default)]
pub struct MetricInterner {
    ids: HashMap<String, MetricId>,
    paths: Vec<String>,
    meta: Vec<Option<MetricMeta>>,
    by_device: HashMap<String, Vec<MetricId>>,
}

impl MetricInterner {
    /// Create an empty interner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild from persisted `(path, id, meta)` rows, so an id means the same
    /// path it meant in the process that wrote the samples. Ids are dense
    /// ordinals; a gap (a flush that never landed) is kept as a hole so no
    /// later mint can reuse an id that has rows on disk.
    pub fn restore(entries: Vec<(String, u32, MetricMeta)>) -> Self {
        let mut me = Self::new();
        let Some(max) = entries.iter().map(|(_, id, _)| *id).max() else {
            return me;
        };
        me.paths = vec![String::new(); max as usize + 1];
        me.meta = vec![None; max as usize + 1];
        for (path, id, meta) in entries {
            me.paths[id as usize] = path;
            me.meta[id as usize] = Some(meta);
        }
        for i in 0..me.paths.len() {
            if me.paths[i].is_empty() {
                continue;
            }
            let id = MetricId(i as u32);
            me.ids.insert(me.paths[i].clone(), id);
            if let Some(device) = me.device_key(id) {
                me.by_device.entry(device).or_default().push(id);
            }
        }
        me
    }

    /// `"<producer>/<origin>/<source>"` for an interned id, if it has meta.
    /// The producer and origin come from the path's first two chunks, the
    /// source from the recorded meta.
    fn device_key(&self, id: MetricId) -> Option<String> {
        let path = self.paths.get(id.0 as usize)?;
        let meta = self.meta.get(id.0 as usize)?.as_ref()?;
        let (origin, rest) = path.split_once('/')?;
        let (producer, _subject) = rest.split_once('/')?;
        Some(device_prefix(producer, origin, &meta.source))
    }

    /// Intern `path` with its metadata, returning its (possibly new) id. An
    /// already-interned path keeps its id; its metadata is refreshed, because
    /// a producer may start reporting a series it had only ever sent as a
    /// gauge as a counter (a `.rate` sibling appearing, a restart under a new
    /// build), and the newest statement of kind is the one to believe.
    pub fn intern(&mut self, path: &str, meta: MetricMeta) -> MetricId {
        if let Some(id) = self.ids.get(path).copied() {
            self.meta[id.0 as usize] = Some(meta);
            return id;
        }
        let id = MetricId(self.paths.len() as u32);
        self.paths.push(path.to_string());
        self.meta.push(Some(meta));
        self.ids.insert(path.to_string(), id);
        if let Some(device) = self.device_key(id) {
            self.by_device.entry(device).or_default().push(id);
        }
        id
    }

    /// Ids + display metric names for a device, where `device` is
    /// `"<producer>/<origin>/<source>"` (see [`device_prefix`]).
    /// O(metrics-for-that-device) via the `by_device` index —
    /// this replaced a per-render linear scan of every interned path.
    pub fn device_ids<'a>(&'a self, device: &str) -> impl Iterator<Item = (MetricId, &'a str)> {
        self.by_device
            .get(device)
            .into_iter()
            .flatten()
            .filter_map(move |&id| Some((id, self.meta[id.0 as usize].as_ref()?.metric.as_str())))
    }

    /// Every device key this interner has seen.
    pub fn devices(&self) -> impl Iterator<Item = &str> {
        self.by_device.keys().map(String::as_str)
    }

    /// Look up an already-interned path's id, if present.
    pub fn get(&self, path: &str) -> Option<MetricId> {
        self.ids.get(path).copied()
    }

    /// Resolve an id back to its path.
    pub fn resolve(&self, id: MetricId) -> Option<&str> {
        self.paths.get(id.0 as usize).map(String::as_str)
    }

    /// The recorded metadata for an id.
    pub fn meta(&self, id: MetricId) -> Option<&MetricMeta> {
        self.meta.get(id.0 as usize)?.as_ref()
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
    /// The historian's `series` listing walks this with an
    /// `"<origin>/<producer>/"` prefix.
    pub fn with_prefix<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = (MetricId, &'a str)> {
        self.paths
            .iter()
            .enumerate()
            .filter(move |(_, p)| !p.is_empty() && p.starts_with(prefix))
            .map(|(i, p)| (MetricId(i as u32), p.as_str()))
    }
}

/// The series path for one metric: `"<origin>/<producer>/<subject>"`, the wire
/// key minus the class chunk (#904).
///
/// This is the identity a reader can derive from a sample alone, which is what
/// lets a historian ingesting `v1/*/telemetry/**` and a GUI caching the same
/// stream agree on what to call a series without a catalog between them.
pub fn series_path(origin: &str, producer: &str, subject: &str) -> String {
    format!("{origin}/{producer}/{subject}")
}

/// The device-index key: `"<producer>/<origin>/<source>"`.
pub fn device_prefix(producer: &str, origin: &str, source: &str) -> String {
    format!("{producer}/{origin}/{source}")
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

    /// Shrink to a smaller capacity, dropping the oldest samples that no
    /// longer fit. A no-op if the ring is already at or below `capacity`.
    /// Floors at 1 for the same reason [`new`](Self::new) does.
    pub fn shrink_to(&mut self, capacity: usize) {
        let capacity = capacity.max(1);
        if capacity >= self.capacity {
            return;
        }
        self.capacity = capacity;
        while self.buf.len() > capacity {
            self.buf.pop_front();
        }
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

/// Downsample samples into `(bucket_ts_secs, `[`Bucket`]`)` pairs for a tier.
///
/// Last-observation-per-bucket for [`Bucket::last`] (the most recent sample in
/// each bucket wins), and the true min/max across every sample that fell in
/// it — a coarse tier that reported only the closing value could not say a
/// spike had happened at all (#904). Pure function — the unit of testing for
/// the tier logic.
///
/// `samples` need not be sorted; the result is sorted ascending by bucket.
pub fn downsample(samples: &[Sample], tier: Tier) -> Vec<(i64, Bucket)> {
    let width = tier.bucket_secs();
    // bucket_ts -> (latest_ts, bucket)
    let mut buckets: HashMap<i64, (i64, Bucket)> = HashMap::new();
    for s in samples {
        let secs = s.ts.div_euclid(1_000);
        let bucket = secs.div_euclid(width) * width;
        match buckets.get_mut(&bucket) {
            Some((latest_ts, acc)) => {
                acc.min = acc.min.min(s.value as f32);
                acc.max = acc.max.max(s.value as f32);
                if s.ts >= *latest_ts {
                    *latest_ts = s.ts;
                    acc.last = s.value;
                }
            }
            None => {
                buckets.insert(bucket, (s.ts, Bucket::point(s.value)));
            }
        }
    }
    let mut out: Vec<(i64, Bucket)> = buckets.into_iter().map(|(b, (_, v))| (b, v)).collect();
    out.sort_by_key(|(b, _)| *b);
    out
}

/// A redb-backed persistent store for downsampled tiers. Cloneable handle
/// (`Arc<Database>`) that is `Send + Sync`, so all of its I/O can run inside
/// `tokio::task::spawn_blocking` off the UI thread.
#[derive(Clone)]
pub struct PersistentStore {
    db: Arc<Database>,
    /// The file this store opened. Kept so [`db_bytes`](Self::db_bytes) can
    /// stat it: what matters to an operator with a budget is what `df` says,
    /// and redb's allocated-but-unused pages are part of that.
    path: PathBuf,
}

impl PersistentStore {
    /// Open (creating if needed) the store database at `path`. The parent
    /// directory is created if missing. Returns an error rather than panicking
    /// so the caller can degrade gracefully to an in-memory-only store.
    ///
    /// A file whose layout is not [`SCHEMA_VERSION`] is refused with
    /// [`StoreOpenError::Schema`] rather than read: its sample rows are keyed
    /// by ids this code cannot map to paths.
    ///
    /// **The schema marker is read first, in its own transaction, before any
    /// other table is opened.** v3 re-typed both `metrics` and `samples`
    /// (#904), and `open_table` on a re-typed table fails with a redb
    /// *table type mismatch* — a different error from
    /// [`StoreOpenError::Schema`], and one
    /// [`MetricStore::with_default_persistence`] does not recognise as
    /// "wrong layout, move it aside". Settling the schema question in the
    /// same transaction that opened the tables was fine while every version
    /// bump kept the types; it would have turned the first one that did not
    /// into a GUI that silently ran memory-only on every launch.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreOpenError> {
        Self::open_with_cache(path, DEFAULT_CACHE_BYTES)
    }

    /// [`open`](Self::open) with an explicit redb page-cache budget. A
    /// headless caller on a small host sets its own; see
    /// [`DEFAULT_CACHE_BYTES`] for why leaving it to redb is not an option.
    pub fn open_with_cache(
        path: impl AsRef<Path>,
        cache_bytes: usize,
    ) -> Result<Self, StoreOpenError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(redb::Error::from)?;
        }
        let db = redb::Builder::new()
            .set_cache_size(cache_bytes)
            .create(path)?;

        // Phase 1: the schema marker alone. `meta` is `<&str, u64>` in every
        // version, so this open cannot type-mismatch.
        let found = {
            let txn = db.begin_read()?;
            match txn.open_table(META_TABLE) {
                Ok(meta) => meta.get("schema")?.map(|v| v.value()),
                // No `meta` table at all: a fresh file, or v1 (samples, no ids).
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(StoreOpenError::Redb(e.into())),
            }
        };
        let found = match found {
            Some(v) => v,
            None => {
                // Distinguish "fresh" from "v1" without opening a re-typed
                // table: `list_tables` names them without needing their value
                // types, and a pre-v2 file is exactly one that wrote samples
                // and never wrote a marker. A fresh file has no tables at all
                // — phase 2 below is what creates them.
                let txn = db.begin_read()?;
                let has_samples = txn.list_tables()?.any(|t| t.name() == "samples");
                drop(txn);
                if has_samples { 1 } else { SCHEMA_VERSION }
            }
        };
        if found != SCHEMA_VERSION {
            return Err(StoreOpenError::Schema { found });
        }

        // Phase 2: the layout is ours, so the typed opens are safe. Ensure
        // every table exists (so reads on a fresh DB don't error) and stamp
        // the marker, in one transaction.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(SAMPLES_TABLE)?;
            // Ensure the logs table (#107, C9) exists too.
            let _ = txn.open_table(LOGS_TABLE)?;
            // Ensure the events table (#578) exists too.
            let _ = txn.open_table(EVENTS_TABLE)?;
            // Ensure the Tier-2 chunk store (#199) exists too. Unconditional:
            // it is created whether or not the `blob` feature is on, so the
            // on-disk file is the same either way.
            let _ = txn.open_table(CHUNKS_TABLE)?;
            let _ = txn.open_table(METRICS_TABLE)?;
            // The timeline (#908). A NEW table is additive — redb creates it
            // on first open and every existing row keeps meaning what it
            // meant — so this needs no SCHEMA_VERSION bump. Only re-typing an
            // existing table does.
            let _ = txn.open_table(crate::timeline::TIMELINE_TABLE)?;
            let mut meta = txn.open_table(META_TABLE)?;
            meta.insert("schema", SCHEMA_VERSION)?;
        }
        txn.commit()?;
        Ok(Self {
            db: Arc::new(db),
            path: path.to_path_buf(),
        })
    }

    /// Every persisted `(path, id, meta)` row, for rebuilding the interner on
    /// open.
    pub fn load_metrics(&self) -> Result<Vec<(String, u32, MetricMeta)>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(METRICS_TABLE)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            let (k, v) = row?;
            let (id, kind, source, metric, unit) = v.value();
            out.push((
                k.value().to_string(),
                id,
                MetricMeta {
                    kind: kind_from_code(kind),
                    // The empty string is how "no unit" rides a fixed-arity
                    // row; `Option` is how it reads in Rust.
                    unit: (!unit.is_empty()).then(|| unit.to_string()),
                    source: source.to_string(),
                    metric: metric.to_string(),
                },
            ));
        }
        Ok(out)
    }

    /// The default on-disk location: `~/.local/share/zensight/metrics.redb`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::data_dir().map(|d| d.join("zensight").join("metrics.redb"))
    }

    /// Persist a batch of downsampled buckets across all tiers, together with
    /// the paths of any metric interned since the last flush — in ONE
    /// transaction, so a sample row never lands without the path its id
    /// means. Blocking I/O — call from `spawn_blocking`.
    pub fn write_batch(&self, batch: &FlushBatch) -> Result<usize, redb::Error> {
        if batch.rows.is_empty() && batch.paths.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin_write()?;
        let mut written = 0usize;
        {
            let mut metrics = txn.open_table(METRICS_TABLE)?;
            for (path, id, meta) in &batch.paths {
                metrics.insert(
                    path.as_str(),
                    (
                        *id,
                        kind_code(meta.kind),
                        meta.source.as_str(),
                        meta.metric.as_str(),
                        meta.unit.as_deref().unwrap_or(""),
                    ),
                )?;
            }
            let mut table = txn.open_table(SAMPLES_TABLE)?;
            for (metric, tier, bucket_ts, bucket) in &batch.rows {
                table.insert(pack_key(*metric, *tier, *bucket_ts), bucket.as_row())?;
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
        Ok(self
            .query_buckets(metric, tier, from_ms, to_ms)?
            .into_iter()
            .map(|(ts, b)| Sample { ts, value: b.last })
            .collect())
    }

    /// Read the full [`Bucket`]s for `(metric, tier)` within the inclusive
    /// millisecond range, oldest-first. [`query`](Self::query) is this with
    /// only `last` kept — the historian's `min`/`max` aggregates need the rest.
    ///
    /// The redb range is bounded by the requested window rather than walking
    /// the whole `(metric, tier)` span and filtering in Rust: the packed key
    /// puts `bucket_ts` in its low bits, so a time window *is* a key range,
    /// and asking for a day out of a year of hour buckets should not read the
    /// year.
    pub fn query_buckets(
        &self,
        metric: MetricId,
        tier: Tier,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<(i64, Bucket)>, redb::Error> {
        if to_ms < from_ms {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SAMPLES_TABLE)?;
        // Bucket timestamps are non-negative seconds; clamp the window so a
        // caller asking from before the epoch cannot underflow the key.
        let from_secs = from_ms.div_euclid(1_000).max(0);
        let to_secs = to_ms.div_euclid(1_000).max(0);
        let range = pack_key(metric, tier, from_secs)..=pack_key(metric, tier, to_secs);
        let mut out = Vec::new();
        for entry in table.range(range)? {
            let (key, value) = entry?;
            let bucket_secs = (key.value() & u64::MAX as u128) as u64 as i64;
            out.push((bucket_secs * 1_000, Bucket::from_row(value.value())));
        }
        Ok(out)
    }

    /// The distinct metric ids that actually have sample rows, by skip-scan.
    ///
    /// The packed key puts `metric_id` in its top bits, so all of one metric's
    /// buckets are contiguous: read the first key at or after the cursor, take
    /// its id, then jump the cursor to the first key the *next* id could have.
    /// That is one seek per distinct series rather than one read per bucket —
    /// the difference between O(series) and O(rows) on a file where a single
    /// series holds a year of hour buckets.
    fn sample_metric_ids(&self) -> Result<Vec<MetricId>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SAMPLES_TABLE)?;
        let mut ids = Vec::new();
        let mut cursor: u128 = 0;
        loop {
            let Some(entry) = table.range(cursor..)?.next() else {
                break;
            };
            let key = entry?.0.value();
            let id = (key >> 72) as u32;
            ids.push(MetricId(id));
            // The first key any higher id could hold. `id` is a u32, so this
            // is at most 2^104 and cannot overflow a u128.
            cursor = ((id as u128) + 1) << 72;
        }
        Ok(ids)
    }

    /// Buckets stored in one tier, across every metric.
    ///
    /// Skip-scans by metric like [`prune`](Self::prune), then counts one
    /// bounded range per `(metric, tier)`: the packed key sorts by metric
    /// first, so a tier's rows are scattered through the table and counting
    /// them naively means reading all of it. This is the number #911 measures
    /// retention against, so it has to stay cheap enough to ask for on every
    /// `stats` call.
    pub fn tier_rows(&self, tier: Tier) -> Result<u64, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SAMPLES_TABLE)?;
        let mut total = 0u64;
        for id in self.sample_metric_ids()? {
            let lo = pack_key(id, tier, 0);
            let hi = pack_key(id, tier, i64::MAX);
            total += table.range(lo..=hi)?.count() as u64;
        }
        Ok(total)
    }

    /// The database file's size on disk, in bytes. `0` when it cannot be
    /// stated — an unreadable size is not a small one, but a caller charting
    /// a budget needs a number, and the file's absence is itself visible in
    /// the row counts beside it.
    pub fn db_bytes(&self) -> u64 {
        self.path.metadata().map(|m| m.len()).unwrap_or(0)
    }

    /// The oldest bucket held, in epoch milliseconds; `None` for an empty
    /// store.
    ///
    /// The honest answer to "how far back can I ask", which retention makes a
    /// moving target: a caller that assumes its configured retention is
    /// available will draw an empty left-hand half of a chart and call it an
    /// outage. One seek per `(metric, tier)`.
    pub fn oldest_bucket_ms(&self) -> Result<Option<i64>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SAMPLES_TABLE)?;
        let mut oldest: Option<i64> = None;
        for id in self.sample_metric_ids()? {
            for tier in Tier::ALL {
                let lo = pack_key(id, tier, 0);
                let hi = pack_key(id, tier, i64::MAX);
                if let Some(entry) = table.range(lo..=hi)?.next() {
                    let key = entry?.0.value();
                    let secs = (key & u64::MAX as u128) as u64 as i64;
                    let ms = secs * 1_000;
                    oldest = Some(oldest.map_or(ms, |o: i64| o.min(ms)));
                }
            }
        }
        Ok(oldest)
    }

    /// Evict buckets older than each tier's [retention](Tier::retention_secs)
    /// relative to `now_ms`, bounding on-disk growth (#131). Returns the number
    /// of buckets removed. Blocking I/O — call from `spawn_blocking`.
    ///
    /// Walks `(metric, tier)` by `(metric, tier)` and extracts each one's aged
    /// range directly (#904). The packed key sorts by `metric_id` first, so a
    /// single tier's expired buckets are scattered across the whole table —
    /// which is why this used to read every row in the file on every pass, to
    /// find the handful that had aged out since the last one. Within one
    /// `(metric, tier)` the low bits *are* the bucket timestamp, so the aged
    /// buckets are a contiguous prefix and `extract_from_if` can take them
    /// without the rest of the table being touched.
    ///
    /// The ids come from [`sample_metric_ids`](Self::sample_metric_ids), which
    /// skip-scans the samples table itself rather than reading the `metrics`
    /// table. Every id with sample rows has a `metrics` row in practice —
    /// `write_batch` writes both in one transaction — but "in practice" is the
    /// wrong basis for the code that stops a file growing without bound: an id
    /// the prune cannot see is an id whose buckets are kept for ever.
    pub fn prune(&self, now_ms: i64) -> Result<usize, redb::Error> {
        let now_secs = now_ms.div_euclid(1_000);
        let ids = self.sample_metric_ids()?;

        let txn = self.db.begin_write()?;
        let mut removed = 0usize;
        {
            let mut table = txn.open_table(SAMPLES_TABLE)?;
            for id in ids {
                for tier in Tier::ALL {
                    // Strictly older than the cutoff, as it always was: a
                    // bucket exactly at the retention edge is still inside it.
                    let cutoff = now_secs - tier.retention_secs();
                    if cutoff <= 0 {
                        continue;
                    }
                    let lo = pack_key(id, tier, 0);
                    let hi = pack_key(id, tier, cutoff - 1);
                    for entry in table.extract_from_if(lo..=hi, |_, _| true)? {
                        entry?;
                        removed += 1;
                    }
                }
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    // ---- log cold store (#107, C9) ------------------------------------------

    /// Persist a batch of log records keyed by uid. Blocking I/O — call from
    /// `spawn_blocking`. Records with an empty uid are skipped (no stable key).
    pub fn write_logs(&self, logs: &[StoredLog]) -> Result<usize, redb::Error> {
        crate::logs::write_batch(&self.db, logs)
    }

    /// Read persisted log records whose `ts` falls in `[from_ms, to_ms]`,
    /// newest-first, capped at `limit`. Blocking I/O.
    ///
    /// The cursor form is [`crate::logs::query`]; this cache has no paginating
    /// reader, because the Logs view paginates against the *sensor's* durable
    /// store, which is authoritative and unsampled (#603).
    pub fn query_logs(
        &self,
        from_ms: i64,
        to_ms: i64,
        limit: usize,
    ) -> Result<Vec<StoredLog>, redb::Error> {
        crate::logs::query(&self.db, from_ms, to_ms, None, limit)
    }

    /// Evict the oldest log rows beyond `keep_max`, bounding on-disk growth.
    /// Returns the number removed. Blocking I/O.
    ///
    /// Size-only: this is a per-viewer cache whose rows are template-sampled
    /// already, and the age bound that matters is the sensor's.
    pub fn prune_logs(&self, keep_max: usize) -> Result<usize, redb::Error> {
        crate::logs::prune::<StoredLog>(&self.db, 0, i64::MAX, keep_max)
    }

    // ---- Timeline: events and alert transitions (#908) ---------------------

    /// Persist timeline rows. Idempotent by their derived uid, which is what
    /// makes a subscriber's history replay safe — see [`crate::timeline`].
    pub fn write_timeline(
        &self,
        rows: &[crate::timeline::TimelineRow],
    ) -> Result<usize, redb::Error> {
        crate::timeline::write_batch(&self.db, rows)
    }

    /// Read timeline rows newest-first in one bounded page.
    pub fn query_timeline(
        &self,
        from_ms: i64,
        to_ms: i64,
        kinds: &[crate::timeline::TimelineKind],
        origin: Option<&str>,
        after_uid: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::timeline::TimelineRow>, redb::Error> {
        crate::timeline::query(&self.db, from_ms, to_ms, kinds, origin, after_uid, limit)
    }

    /// Evict the oldest timeline rows beyond `keep_max`.
    pub fn prune_timeline(&self, keep_max: usize) -> Result<usize, redb::Error> {
        crate::timeline::prune(&self.db, keep_max)
    }

    // ---- Event records (#578) ----------------------------------------------

    /// Persist a batch of event records keyed by ULID. Idempotent — a record
    /// that arrives twice (live subscriber overlapping the storage backfill)
    /// overwrites itself. Records with an empty id are skipped. Blocking I/O.
    pub fn write_events(
        &self,
        events: &[zensight_common::EventRecord],
    ) -> Result<usize, redb::Error> {
        if events.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin_write()?;
        let mut written = 0usize;
        {
            let mut table = txn.open_table(EVENTS_TABLE)?;
            for event in events {
                if event.id.is_empty() {
                    continue;
                }
                let Ok(bytes) = serde_json::to_vec(event) else {
                    continue;
                };
                table.insert(event.id.as_str(), bytes.as_slice())?;
                written += 1;
            }
        }
        txn.commit()?;
        Ok(written)
    }

    /// Read the `limit` most recent persisted events, newest-first. ULID keys
    /// sort chronologically, so this is a bounded reverse range walk.
    /// Blocking I/O.
    pub fn query_events(
        &self,
        limit: usize,
    ) -> Result<Vec<zensight_common::EventRecord>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(EVENTS_TABLE)?;
        let mut out = Vec::new();
        for entry in table.range::<&str>(..)?.rev() {
            let (_key, value) = entry?;
            let Ok(event) = serde_json::from_slice::<zensight_common::EventRecord>(value.value())
            else {
                continue;
            };
            out.push(event);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    /// Evict the oldest event rows beyond `keep_max`. Returns the number
    /// removed. Blocking I/O.
    pub fn prune_events(&self, keep_max: usize) -> Result<usize, redb::Error> {
        let txn = self.db.begin_write()?;
        let mut removed = 0usize;
        {
            let mut table = txn.open_table(EVENTS_TABLE)?;
            let total = table.len()? as usize;
            if total > keep_max {
                let to_remove = total - keep_max;
                let oldest: Vec<String> = table
                    .range::<&str>(..)?
                    .take(to_remove)
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

    // ---- Tier-2 content-addressed chunk store (#199) ------------------------

    /// Whether chunk `key` (`<algo>/<hex>`) is on disk. Blocking I/O.
    pub fn has_chunk(&self, key: &str) -> Result<bool, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS_TABLE)?;
        Ok(table.get(key)?.is_some())
    }

    /// Read chunk `key` (`<algo>/<hex>`), if present. Blocking I/O.
    pub fn read_chunk(&self, key: &str) -> Result<Option<Vec<u8>>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_vec()))
    }

    /// Store chunk `key` (`<algo>/<hex>`) → `bytes` (idempotent; content-addressed
    /// so an existing key already holds identical bytes). Blocking I/O.
    pub fn write_chunk(&self, key: &str, bytes: &[u8]) -> Result<(), redb::Error> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CHUNKS_TABLE)?;
            table.insert(key, bytes)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Every chunk key currently stored, as `<algo>/<hex>`. Blocking I/O, and
    /// O(chunks) — it is a garbage-collection input, not a hot path.
    pub fn chunk_keys(&self) -> Result<Vec<String>, redb::Error> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CHUNKS_TABLE)?;
        let mut out = Vec::new();
        for row in table.iter()? {
            out.push(row?.0.value().to_string());
        }
        Ok(out)
    }

    /// Remove chunk `key`; returns whether it was present. Blocking I/O.
    ///
    /// Content-addressed removal is only safe behind a liveness analysis — a
    /// chunk may be shared by many files and snapshots — so this is a
    /// primitive for a sweep, not something to call on a hunch.
    pub fn delete_chunk(&self, key: &str) -> Result<bool, redb::Error> {
        let txn = self.db.begin_write()?;
        let existed = {
            let mut table = txn.open_table(CHUNKS_TABLE)?;
            table.remove(key)?.is_some()
        };
        txn.commit()?;
        Ok(existed)
    }
}

// The one part of this crate that is not a time series: the zblob adapter.
// Gated so a consumer that only wants history (the historian) does not pull
// the blob stack. The `chunks` TABLE and `PersistentStore`'s chunk methods
// stay unconditional, so the on-disk file is identical either way and a GUI
// can open a file a feature-off writer made.
#[cfg(feature = "blob")]
/// A redb-backed [`zblob::ContentStore`] (#199): the durable, dedup-and-resume
/// substrate for Tier-2 directory sync. Wraps a [`PersistentStore`] so chunks share
/// the one metrics/logs database. The trait is sync; each call is a short blocking
/// redb transaction, so drive `TreeClient::download_tree` off the UI thread.
#[derive(Clone)]
pub struct RedbContentStore {
    store: PersistentStore,
}

// The one part of this crate that is not a time series: the zblob adapter.
// Gated so a consumer that only wants history (the historian) does not pull
// the blob stack. The `chunks` TABLE and `PersistentStore`'s chunk methods
// stay unconditional, so the on-disk file is identical either way and a GUI
// can open a file a feature-off writer made.
#[cfg(feature = "blob")]
impl RedbContentStore {
    /// Wrap a [`PersistentStore`] as a content store.
    pub fn new(store: PersistentStore) -> Self {
        RedbContentStore { store }
    }
}

/// The chunk-key prefix. zblob's wire is BLAKE3-only, so this is the `<algo>`
/// segment of RFC 07 §2.4's `store/<algo>/<hash>`. It changed from `sha256/`
/// with the 0.2 bump: dedup is per-algorithm, so keys minted under the old
/// digest name a different address space and are simply cold — the store
/// refills on the next fetch rather than pretending they still resolve.
#[cfg(feature = "blob")]
const CHUNK_ALGO: &str = "blake3";

// The one part of this crate that is not a time series: the zblob adapter.
// Gated so a consumer that only wants history (the historian) does not pull
// the blob stack. The `chunks` TABLE and `PersistentStore`'s chunk methods
// stay unconditional, so the on-disk file is identical either way and a GUI
// can open a file a feature-off writer made.
#[cfg(feature = "blob")]
impl zblob::ContentStore for RedbContentStore {
    // 0.3's trait returns `io::Result` from the read paths too, so a failing
    // redb read is a reported error rather than a silent "not cached" that
    // would send the client back to the network forever.
    fn has(&self, hash: &zblob::Hash) -> std::io::Result<bool> {
        self.store
            .has_chunk(&format!("{CHUNK_ALGO}/{hash}"))
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn get(&self, hash: &zblob::Hash) -> std::io::Result<Option<Vec<u8>>> {
        self.store
            .read_chunk(&format!("{CHUNK_ALGO}/{hash}"))
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    fn put(&self, hash: &zblob::Hash, bytes: &[u8]) -> std::io::Result<()> {
        self.store
            .write_chunk(&format!("{CHUNK_ALGO}/{hash}"), bytes)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }

    /// Keys that do not parse as `<CHUNK_ALGO>/<hex>` are skipped rather than
    /// erroring the sweep: this table has held `sha256/…` keys from before the
    /// 0.2 bump, and a garbage-collection input that refuses to enumerate is
    /// worse than one that reports only what it understands.
    fn for_each_hash(
        &self,
        f: &mut dyn FnMut(zblob::Hash) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let keys = self
            .store
            .chunk_keys()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        for hash in keys
            .iter()
            .filter_map(|k| k.strip_prefix(CHUNK_ALGO).and_then(|r| r.strip_prefix('/')))
            .filter_map(|hex| hex.parse::<zblob::Hash>().ok())
        {
            f(hash)?;
        }
        Ok(())
    }

    fn remove(&self, hash: &zblob::Hash) -> std::io::Result<bool> {
        self.store
            .delete_chunk(&format!("{CHUNK_ALGO}/{hash}"))
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

/// A persisted log line (#107, C9). The compact, restart-surviving form of a
/// per-line log event — the fields the Logs view needs to render and filter a
/// row. The richer journald drill-down structure isn't persisted (it stays in
/// the live in-memory ring); search-back reconstructs a display row from these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredLog {
    /// Time-sortable event uid (`<ts_ms><seq>`) — the redb key.
    pub uid: String,
    /// Event time (Unix epoch ms).
    pub ts: i64,
    /// Originating host.
    pub host: String,
    /// OTel severity number (1-24).
    pub severity_number: u8,
    /// Syslog facility slug (e.g. `auth`).
    pub facility: String,
    /// Syslog severity slug (e.g. `err`).
    pub severity: String,
    /// Application / program name, if any.
    pub app: Option<String>,
    /// systemd unit, if any (journald).
    pub unit: Option<String>,
    /// Drain-style template id, if templating mined one (#102).
    pub template_id: Option<String>,
    /// The log message text.
    pub message: String,
}

impl StoredLog {
    /// Build a record from a per-line log-event [`TelemetryPoint`] (the
    /// `events/<uid>` shape from #104). Returns `None` if the point isn't a Logs
    /// text event. The label reads mirror `syslog_message_from_point`.
    pub fn from_point(point: &TelemetryPoint) -> Option<StoredLog> {
        if point.protocol != zensight_common::Protocol::Logs {
            return None;
        }
        let TelemetryValue::Text(message) = &point.value else {
            return None;
        };
        let label = |k: &str| point.labels.get(k).cloned();
        let uid = label("log.record.uid").unwrap_or_else(|| point.metric.clone());
        let severity_number = point
            .labels
            .get("severity_number")
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(9);
        Some(StoredLog {
            uid,
            ts: point.timestamp,
            host: point.source.clone(),
            severity_number,
            facility: label("facility").unwrap_or_else(|| "unknown".to_string()),
            severity: label("severity").unwrap_or_else(|| "info".to_string()),
            app: label("app"),
            unit: label("sd.journald.unit").filter(|u| !u.is_empty()),
            template_id: label("template_id").filter(|t| !t.is_empty()),
            message: message.clone(),
        })
    }

    /// Reconstruct a [`TelemetryPoint`] carrying this record's labels so the
    /// existing `syslog_message_from_point` decoder can render a search-back row
    /// without a parallel code path.
    pub fn to_point(&self) -> TelemetryPoint {
        let mut labels = HashMap::new();
        labels.insert("facility".to_string(), self.facility.clone());
        labels.insert("severity".to_string(), self.severity.clone());
        labels.insert(
            "severity_number".to_string(),
            self.severity_number.to_string(),
        );
        labels.insert("log.record.uid".to_string(), self.uid.clone());
        if let Some(app) = &self.app {
            labels.insert("app".to_string(), app.clone());
        }
        if let Some(unit) = &self.unit {
            labels.insert("sd.journald.unit".to_string(), unit.clone());
        }
        if let Some(tid) = &self.template_id {
            labels.insert("template_id".to_string(), tid.clone());
        }
        TelemetryPoint {
            timestamp: self.ts,
            source: self.host.clone(),
            protocol: zensight_common::Protocol::Logs,
            metric: format!("events/{}", self.uid),
            value: TelemetryValue::Text(self.message.clone()),
            labels,
            unit: None,
        }
    }
}

/// Template-aware retention sampler (#107, C9). Decides which log lines reach the
/// cold store: **always keep errors and the first sighting of a template**
/// (novelty); **sample repetitive** known-template info lines 1-in-N so a chatty
/// service can't dominate the store. Pure + stateful (per-template counters), so
/// the policy is unit-testable.
#[derive(Debug)]
pub struct LogRetention {
    sample_every: u64,
    /// Per-template occurrence counter; first insert == novel.
    counters: HashMap<String, u64>,
    /// Counter for lines with no mined template (sampled globally).
    no_template: u64,
}

impl LogRetention {
    pub fn new(sample_every: u64) -> Self {
        Self {
            sample_every: sample_every.max(1),
            counters: HashMap::new(),
            no_template: 0,
        }
    }

    /// `true` to persist this line. Errors and novel templates always pass;
    /// repetitive known-template info lines pass 1-in-`sample_every`.
    pub fn keep(&mut self, severity_number: u8, template_id: Option<&str>) -> bool {
        let is_error = severity_number >= LOG_ERROR_SEVERITY;
        match template_id {
            Some(tid) => {
                let counter = self.counters.entry(tid.to_string()).or_insert(0);
                let novel = *counter == 0;
                *counter += 1;
                is_error || novel || counter.is_multiple_of(self.sample_every)
            }
            None => {
                self.no_template += 1;
                is_error || self.no_template.is_multiple_of(self.sample_every)
            }
        }
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
    /// `(path, id)` pairs interned since the last flush, written with it.
    /// Empty (never appended) when there is no persistent store.
    unsaved_paths: Vec<(String, u32, MetricMeta)>,
    /// Log records buffered for the next flush to the cold store (#107, C9),
    /// post-sampling.
    log_pending: Vec<StoredLog>,
    /// Template-aware sampler gating what enters `log_pending`.
    log_retention: LogRetention,
    /// Event records buffered for the next flush to the cold store (#578).
    event_pending: Vec<zensight_common::EventRecord>,
    timeline_pending: Vec<crate::timeline::TimelineRow>,
}

impl MetricStore {
    /// Create a store. If `persistent` is `None` the store is in-memory only
    /// (graceful degradation when the DB can't be opened).
    pub fn new(hot_capacity: usize, persistent: Option<PersistentStore>) -> Self {
        // The interner is rebuilt from the file, or the ids in the samples
        // table mean nothing this process can name.
        let interner = match persistent.as_ref().map(PersistentStore::load_metrics) {
            Some(Ok(entries)) => MetricInterner::restore(entries),
            Some(Err(e)) => {
                tracing::warn!(error = %e, "Could not load persisted metric ids; history will be in-memory only");
                MetricInterner::new()
            }
            None => MetricInterner::new(),
        };
        let persistent = match (&persistent, interner.is_empty()) {
            // A load failure above must not leave a store whose new ids can
            // collide with rows already on disk.
            (Some(store), true) if store.load_metrics().is_err() => None,
            _ => persistent,
        };
        Self {
            interner,
            series: HashMap::new(),
            hot_capacity: hot_capacity.max(1),
            persistent,
            unsaved_paths: Vec::new(),
            log_pending: Vec::new(),
            log_retention: LogRetention::new(LOG_SAMPLE_EVERY),
            event_pending: Vec::new(),
            timeline_pending: Vec::new(),
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
                // A redb major bump changes the on-disk file format and old
                // files can't be auto-upgraded (observed: v2 file vs v3 code
                // after the redb 2→4 bump). The store is a local history
                // cache, so losing it beats silently running memory-only on
                // every launch: move the old file aside and start fresh.
                //
                // The same move-aside covers our own schema (#SCHEMA_VERSION):
                // a v1 file's sample rows are keyed by ids that were never
                // written down, so nothing can read them back correctly —
                // the history it holds was already mislabelled on every
                // launch, and keeping it would only keep that going.
                Err(e @ StoreOpenError::Redb(redb::Error::UpgradeRequired(_)))
                | Err(e @ StoreOpenError::Schema { .. }) => {
                    let backup = match &e {
                        StoreOpenError::Redb(redb::Error::UpgradeRequired(v)) => {
                            path.with_extension(format!("redb.incompatible-v{v}"))
                        }
                        StoreOpenError::Schema { found } => {
                            path.with_extension(format!("redb.schema-v{found}"))
                        }
                        StoreOpenError::Redb(_) => unreachable!("matched above"),
                    };
                    tracing::warn!(path = %path.display(), backup = %backup.display(), error = %e,
                        "Metric store file layout is not this build's; moving it aside and starting fresh");
                    match std::fs::rename(&path, &backup)
                        .map_err(|e| StoreOpenError::Redb(redb::Error::from(e)))
                        .and_then(|()| PersistentStore::open(&path))
                    {
                        Ok(store) => Some(store),
                        Err(e) => {
                            tracing::warn!(error = %e, path = %path.display(),
                                "Failed to recreate metric store; history will be in-memory only");
                            None
                        }
                    }
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

    /// The series path for a point published by `origin` on `subject`:
    /// `"<origin>/<producer>/<subject>"` — see [`series_path`]. The producer
    /// is the point's protocol, which is the producer chunk of the key it
    /// arrived on.
    fn metric_key(origin: &str, subject: &str, point: &TelemetryPoint) -> String {
        series_path(origin, &point.protocol.to_string(), subject)
    }

    /// The device-index key for a device — see [`device_prefix`].
    pub fn device_key(producer: &str, origin: &str, source: &str) -> String {
        device_prefix(producer, origin, source)
    }

    /// Record a telemetry point published by `origin` on `subject` (the wire
    /// key's subject tail). Interns its series path with its kind and device,
    /// projects the value, and appends to the hot ring + pending buffer.
    /// Non-numeric values are ignored. O(1), safe to call inline on the UI
    /// thread.
    ///
    /// `subject` is taken rather than derived because it cannot be derived:
    /// for a proxy producer the wire subject is `{device}/{metric...}` while
    /// [`TelemetryPoint::metric`] is only the `{metric...}` half, so a store
    /// that reconstructed the path from the payload would be un-slugging a
    /// device chunk and guessing. Both callers already hold the parsed key.
    pub fn record(&mut self, origin: &str, subject: &str, point: &TelemetryPoint) {
        let Some(value) = SampleValue::from_telemetry(&point.value) else {
            return;
        };
        let key = Self::metric_key(origin, subject, point);
        let before = self.interner.len();
        let meta = MetricMeta {
            kind: value.kind(),
            unit: point.unit.clone(),
            source: point.source.clone(),
            metric: point.metric.clone(),
        };
        let id = self.interner.intern(&key, meta.clone());
        let sample = Sample {
            ts: point.timestamp,
            value: value.as_f64(),
        };
        let capacity = self.hot_capacity;
        let series = self.series.entry(id).or_insert_with(|| MetricSeries {
            hot: RingBuffer::new(capacity),
            pending: Vec::new(),
        });
        series.hot.push(sample);
        // Nothing is buffered for a flush that can never happen: with no
        // redb handle `take_flush_batch` returns early, and `pending` used to
        // grow by one sample per point forever — the demo's, and a degraded
        // launch's, slow leak.
        if self.persistent.is_some() {
            series.pending.push(sample);
            if self.interner.len() > before {
                self.unsaved_paths.push((key, id.0, meta));
            }
        }
    }

    /// Whether there are pending samples awaiting flush.
    pub fn has_pending(&self) -> bool {
        self.series.values().any(|s| !s.pending.is_empty())
    }

    /// Drain pending samples and build a persist batch across all tiers,
    /// downsampling each metric's pending samples. Returns the batch and a clone
    /// of the persistent handle (so the caller can run [`PersistentStore::write_batch`]
    /// in `spawn_blocking`). Returns `None` if there's nothing to flush or no DB.
    pub fn take_flush_batch(&mut self) -> Option<(PersistentStore, FlushBatch)> {
        let store = self.persistent.clone()?;
        let mut rows = Vec::new();
        for (id, series) in self.series.iter_mut() {
            if series.pending.is_empty() {
                continue;
            }
            let pending = std::mem::take(&mut series.pending);
            for tier in Tier::ALL {
                for (bucket, value) in downsample(&pending, tier) {
                    rows.push((*id, tier, bucket, value));
                }
            }
        }
        if rows.is_empty() {
            return None;
        }
        let paths = std::mem::take(&mut self.unsaved_paths);
        Some((store, FlushBatch { rows, paths }))
    }

    /// Offer a per-line log event to the cold store (#107, C9). Applies the
    /// template-aware retention sampler; kept records buffer for the next flush.
    /// No-op when there's no persistent store. O(1), safe inline on the UI thread.
    pub fn record_log(&mut self, log: StoredLog) {
        if self.persistent.is_none() {
            return;
        }
        if self
            .log_retention
            .keep(log.severity_number, log.template_id.as_deref())
        {
            self.log_pending.push(log);
        }
    }

    /// Drain buffered log records into a persist batch (#107, C9). Returns the
    /// batch and a clone of the persistent handle for an off-thread
    /// [`PersistentStore::write_logs`]. `None` if nothing is pending or no DB.
    pub fn take_log_flush_batch(&mut self) -> Option<(PersistentStore, Vec<StoredLog>)> {
        let store = self.persistent.clone()?;
        if self.log_pending.is_empty() {
            return None;
        }
        Some((store, std::mem::take(&mut self.log_pending)))
    }

    /// Offer an event record to the cold store (#578). Unlike logs there is no
    /// sampler: an event is already a rare, deliberate record, and dropping a
    /// trap would defeat the point of persisting them. No-op without a DB.
    pub fn record_event(&mut self, event: zensight_common::EventRecord) {
        if self.persistent.is_none() {
            return;
        }
        self.event_pending.push(event);
    }

    /// Drain buffered event records into a persist batch (#578). Same shape as
    /// [`take_log_flush_batch`](Self::take_log_flush_batch).
    pub fn take_event_flush_batch(
        &mut self,
    ) -> Option<(PersistentStore, Vec<zensight_common::EventRecord>)> {
        let store = self.persistent.clone()?;
        if self.event_pending.is_empty() {
            return None;
        }
        Some((store, std::mem::take(&mut self.event_pending)))
    }

    /// Hot samples for one metric of a `(producer, source)` whose origin the
    /// caller does not know — the topology panel and the systemd services
    /// table, which come from an entity or a hostname rather than a
    /// `DeviceId`. When several origins report that source (the #474
    /// collision) the series with the most recent sample wins; the two are
    /// never mixed.
    pub fn hot_samples_by_source(&self, producer: &str, source: &str, metric: &str) -> Vec<Sample> {
        let prefix = format!("{producer}/");
        let suffix = format!("/{source}");
        self.interner
            .devices()
            .filter(|d| d.starts_with(&prefix) && d.ends_with(&suffix))
            .flat_map(|d| self.interner.device_ids(d))
            .filter(|(_, m)| *m == metric)
            .filter_map(|(id, _)| self.series.get(&id))
            .max_by_key(|s| s.hot.to_vec().last().map(|x| x.ts).unwrap_or(i64::MIN))
            .map(|s| s.hot.to_vec())
            .unwrap_or_default()
    }

    /// Buffer a timeline row for the next flush (#908).
    ///
    /// The same seam as [`record_event`](Self::record_event): accumulate
    /// inline, hand the batch off to `spawn_blocking`. Nothing is buffered
    /// without a database to flush it to — a memory-only historian has no
    /// timeline, and holding rows for a write that can never happen is the
    /// slow leak `record` already learned not to have.
    pub fn record_timeline(&mut self, row: crate::timeline::TimelineRow) {
        if self.persistent.is_some() {
            self.timeline_pending.push(row);
        }
    }

    /// Drain buffered timeline rows and the persistent handle.
    pub fn take_timeline_flush_batch(
        &mut self,
    ) -> Option<(PersistentStore, Vec<crate::timeline::TimelineRow>)> {
        let store = self.persistent.clone()?;
        if self.timeline_pending.is_empty() {
            return None;
        }
        Some((store, std::mem::take(&mut self.timeline_pending)))
    }

    /// Hot (in-memory) samples for an interned id, oldest-first.
    ///
    /// The path-keyed form is for a caller holding a name; this one is for a
    /// caller that already walked the interner and holds the id — the range
    /// procedure, which would otherwise re-resolve a path it just came from.
    pub fn hot_samples_by_id(&self, id: MetricId) -> Vec<Sample> {
        self.series
            .get(&id)
            .map(|s| s.hot.to_vec())
            .unwrap_or_default()
    }

    /// Hot (in-memory) samples for a series path, oldest-first.
    pub fn hot_samples(&self, metric_key: &str) -> Vec<Sample> {
        self.interner
            .get(metric_key)
            .and_then(|id| self.series.get(&id))
            .map(|s| s.hot.to_vec())
            .unwrap_or_default()
    }

    /// Hot (in-memory) samples for every metric of a device, oldest-first.
    /// Returns `(metric_name, samples)` pairs. Reads only the in-memory ring
    /// (no disk), so it's cheap to call per dashboard render (#24 sparklines).
    pub fn device_hot_samples(
        &self,
        producer: &str,
        origin: &str,
        source: &str,
    ) -> Vec<(String, Vec<Sample>)> {
        let device = device_prefix(producer, origin, source);
        self.interner
            .device_ids(&device)
            .filter_map(|(id, metric)| {
                let samples = self.series.get(&id).map(|s| s.hot.to_vec())?;
                Some((metric.to_string(), samples))
            })
            .collect()
    }

    /// Resolve the interned ids + display metric names for a device, for a
    /// history pre-load.
    pub fn device_metric_ids(
        &self,
        producer: &str,
        origin: &str,
        source: &str,
    ) -> Vec<(String, MetricId)> {
        let device = device_prefix(producer, origin, source);
        self.interner
            .device_ids(&device)
            .map(|(id, metric)| (metric.to_string(), id))
            .collect()
    }

    /// Total samples held across every series' hot ring.
    ///
    /// What the memory governor accounts for (#811/#812): the ring is the one
    /// structure here that grows with the fleet rather than with the disk.
    pub fn hot_sample_count(&self) -> usize {
        self.series.values().map(|s| s.hot.len()).sum()
    }

    /// Halve the hot ring's capacity, dropping the oldest samples of every
    /// series to fit. Returns the new capacity.
    ///
    /// The governor's evict hook asks for a number of bytes freed, and this is
    /// the honest translation: the ring is per-series and bounded by capacity,
    /// so the only thing a caller can actually give back is *seconds held*.
    /// Halving rather than trimming to a target keeps the operation O(series)
    /// and its effect legible in a log line — "ten minutes became five" is a
    /// sentence an operator can act on, where "freed 3.7 MiB" is not.
    ///
    /// Floors at 1: a ring of zero would silently stop answering live
    /// questions, which is a worse failure than holding one sample.
    pub fn halve_hot_capacity(&mut self) -> usize {
        self.hot_capacity = (self.hot_capacity / 2).max(1);
        for series in self.series.values_mut() {
            series.hot.shrink_to(self.hot_capacity);
        }
        self.hot_capacity
    }

    /// The hot ring's current per-series capacity.
    pub fn hot_capacity(&self) -> usize {
        self.hot_capacity
    }

    /// The interner, for a reader that needs to enumerate series (the
    /// historian's `series` listing walks it by `"<origin>/<producer>/"`).
    pub fn interner(&self) -> &MetricInterner {
        &self.interner
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

    const ORIGIN: &str = "h-0123456789ab";

    /// Per-metric metadata for a test series: gauge, device `dev1`, whose
    /// display name is the last chunk of the subject.
    fn meta(metric: &str) -> MetricMeta {
        MetricMeta {
            kind: MetricKind::Gauge,
            unit: None,
            source: "dev1".to_string(),
            metric: metric.to_string(),
        }
    }

    /// The v3 series path for a subject published by [`ORIGIN`]'s sysinfo.
    fn series(subject: &str) -> String {
        series_path(ORIGIN, "sysinfo", subject)
    }

    fn point(metric: &str, value: f64, ts: i64) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: ts,
            source: "dev1".to_string(),
            protocol: Protocol::Sysinfo,
            metric: metric.to_string(),
            value: TelemetryValue::Gauge(value),
            labels: Map::new(),
            unit: None,
        }
    }

    #[test]
    fn interner_assigns_stable_ids() {
        let mut i = MetricInterner::new();
        let a = i.intern("cpu", meta("cpu"));
        let b = i.intern("mem", meta("mem"));
        let a2 = i.intern("cpu", meta("cpu"));
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
        // v3 paths: `<origin>/<producer>/<subject>`. The historian's `series`
        // listing walks exactly this prefix.
        i.intern("h-aaaaaaaaaaaa/snmp/r1/cpu", meta("cpu"));
        i.intern("h-aaaaaaaaaaaa/snmp/r1/mem", meta("mem"));
        i.intern("h-aaaaaaaaaaaa/snmp/r2/cpu", meta("cpu"));
        let mut found: Vec<_> = i
            .with_prefix("h-aaaaaaaaaaaa/snmp/r1/")
            .map(|(_, p)| p.to_string())
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec!["h-aaaaaaaaaaaa/snmp/r1/cpu", "h-aaaaaaaaaaaa/snmp/r1/mem"]
        );
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
        // Minute tier: buckets at 60s and 120s; 90s>60s so last-in-bucket = 2.0,
        // and the 60s bucket's range spans both samples that fell in it.
        let minute = downsample(&samples, Tier::Minute);
        assert_eq!(
            minute,
            vec![
                (
                    60,
                    Bucket {
                        last: 2.0,
                        min: 1.0,
                        max: 2.0
                    }
                ),
                (120, Bucket::point(3.0)),
            ]
        );
        // Hour tier: all three fall in the 0s bucket; last (ts=120_000) wins,
        // and min/max are what makes the coarse tier still able to say the
        // series moved between 1 and 3 rather than sat at 3 (#904).
        let hour = downsample(&samples, Tier::Hour);
        assert_eq!(
            hour,
            vec![(
                0,
                Bucket {
                    last: 3.0,
                    min: 1.0,
                    max: 3.0
                }
            )]
        );
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
        // Both in the 0s minute/hour bucket; the later ts (5_000) wins for
        // `last`, and the range covers both however they arrived.
        assert_eq!(
            downsample(&samples, Tier::Minute),
            vec![(
                0,
                Bucket {
                    last: 5.0,
                    min: 1.0,
                    max: 5.0
                }
            )]
        );
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
        store.record(ORIGIN, "cpu", &point("cpu", 50.0, 1_000));
        let mut p = point("name", 0.0, 2_000);
        p.value = TelemetryValue::Text("hello".into());
        store.record(ORIGIN, "name", &p);
        // Only the numeric metric is tracked.
        assert_eq!(store.hot_samples(&series("cpu")).len(), 1);
        assert_eq!(store.hot_samples(&series("name")).len(), 0);
    }

    #[test]
    fn store_hot_samples_and_device_ids() {
        let mut store = MetricStore::new(10, None);
        store.record(ORIGIN, "cpu", &point("cpu", 50.0, 1_000));
        store.record(ORIGIN, "cpu", &point("cpu", 55.0, 2_000));
        store.record(ORIGIN, "mem", &point("mem", 10.0, 1_500));
        let cpu = store.hot_samples(&series("cpu"));
        assert_eq!(cpu.len(), 2);
        assert_eq!(cpu[1].value, 55.0);
        let mut ids = store.device_metric_ids("sysinfo", ORIGIN, "dev1");
        ids.sort();
        let names: Vec<String> = ids.into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["cpu".to_string(), "mem".to_string()]);
    }

    #[test]
    fn store_no_persistence_no_flush() {
        let mut store = MetricStore::new(10, None);
        store.record(ORIGIN, "cpu", &point("cpu", 1.0, 1_000));
        // No persistent handle => nothing buffered for a flush that can never
        // happen (it used to buffer every sample forever), and no batch.
        assert!(!store.has_pending());
        assert!(store.take_flush_batch().is_none());
        assert_eq!(store.hot_samples(&series("cpu")).len(), 1);
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
            (m, Tier::Minute, 60, Bucket::point(1.5)),
            (m, Tier::Minute, 120, Bucket::point(2.5)),
            (m, Tier::Hour, 0, Bucket::point(9.0)),
        ];
        let batch = FlushBatch {
            rows: batch,
            paths: vec![],
        };
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
            (m, Tier::Minute, 0, Bucket::point(1.0)), // ancient -> evicted
            (m, Tier::Minute, fresh_minute, Bucket::point(2.0)), // fresh -> kept
            (m, Tier::Hour, 0, Bucket::point(3.0)),   // ancient -> evicted
            (m, Tier::Hour, fresh_hour, Bucket::point(4.0)), // fresh -> kept
            (m, Tier::Second, 0, Bucket::point(5.0)), // ancient -> evicted
            (m, Tier::Second, fresh_second, Bucket::point(6.0)), // fresh -> kept
        ];
        store
            .write_batch(&FlushBatch {
                rows: batch,
                paths: vec![],
            })
            .unwrap();

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

    // ---- log cold store (#107, C9) ------------------------------------------

    fn stored_log(uid: &str, ts: i64, sev_num: u8, template: Option<&str>, msg: &str) -> StoredLog {
        StoredLog {
            uid: uid.to_string(),
            ts,
            host: "host01".to_string(),
            severity_number: sev_num,
            facility: "daemon".to_string(),
            severity: if sev_num >= LOG_ERROR_SEVERITY {
                "err".to_string()
            } else {
                "info".to_string()
            },
            app: Some("nginx".to_string()),
            unit: None,
            template_id: template.map(String::from),
            message: msg.to_string(),
        }
    }

    #[test]
    fn log_retention_keeps_errors_and_novel_samples_rest() {
        let mut r = LogRetention::new(10);
        // Error always kept regardless of template repetition.
        assert!(r.keep(LOG_ERROR_SEVERITY, Some("t-err")));
        assert!(r.keep(LOG_ERROR_SEVERITY, Some("t-err")));
        // First sighting of an info template is novel → kept; repeats sampled.
        assert!(r.keep(9, Some("t-info"))); // novel
        let kept = (0..20).filter(|_| r.keep(9, Some("t-info"))).count();
        // After the novel one, counter runs 2..=21; multiples of 10 → 10, 20 = 2 kept.
        assert_eq!(kept, 2);
        // No-template info lines sample globally 1-in-10.
        let kept_nt = (0..10).filter(|_| r.keep(9, None)).count();
        assert_eq!(kept_nt, 1);
    }

    #[test]
    fn stored_log_point_round_trip_preserves_fields() {
        let log = stored_log(
            "0001700000000000000000042",
            1_700_000_000_000,
            17,
            Some("t9"),
            "boom",
        );
        // StoredLog -> point -> StoredLog is lossless for the persisted fields.
        let point = log.to_point();
        let back = StoredLog::from_point(&point).unwrap();
        assert_eq!(back, log);
        assert_eq!(point.metric, "events/0001700000000000000000042");
    }

    #[test]
    fn logs_write_query_newest_first_and_windowed() {
        let path = temp_db_path("logs-rt");
        let store = PersistentStore::open(&path).expect("open");
        let logs = vec![
            stored_log("0000000000100000000000001", 100, 9, Some("a"), "first"),
            stored_log("0000000000200000000000002", 200, 17, Some("b"), "second"),
            stored_log("0000000000300000000000003", 300, 9, Some("c"), "third"),
        ];
        assert_eq!(store.write_logs(&logs).unwrap(), 3);
        // Newest-first, capped by limit.
        let got = store.query_logs(0, 1_000, 10).unwrap();
        assert_eq!(
            got.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
            vec!["third", "second", "first"]
        );
        let limited = store.query_logs(0, 1_000, 2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].message, "third");
        // Time window excludes out-of-range rows.
        let windowed = store.query_logs(150, 250, 10).unwrap();
        assert_eq!(
            windowed
                .iter()
                .map(|l| l.message.as_str())
                .collect::<Vec<_>>(),
            vec!["second"]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_logs_evicts_oldest_beyond_cap() {
        let path = temp_db_path("logs-prune");
        let store = PersistentStore::open(&path).expect("open");
        let logs: Vec<StoredLog> = (1..=5)
            .map(|i| {
                stored_log(
                    &format!("{:013}{:012}", i * 100, i),
                    i * 100,
                    9,
                    Some("t"),
                    &format!("m{i}"),
                )
            })
            .collect();
        store.write_logs(&logs).unwrap();
        // Keep newest 2 → evict the 3 oldest.
        assert_eq!(store.prune_logs(2).unwrap(), 3);
        let got = store.query_logs(0, 10_000, 10).unwrap();
        assert_eq!(
            got.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
            vec!["m5", "m4"]
        );
        // Idempotent under the cap.
        assert_eq!(store.prune_logs(2).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    fn event(id: &str, ts: i64, source: &str, kind: &str) -> zensight_common::EventRecord {
        zensight_common::EventRecord {
            id: id.to_string(),
            timestamp: ts,
            source: source.to_string(),
            protocol: zensight_common::Protocol::Snmp,
            kind: kind.to_string(),
            severity: zensight_common::AlertSeverity::Warning,
            summary: format!("{kind} on {source}"),
            alert_key: None,
            fields: Default::default(),
        }
    }

    /// #578: events round-trip through redb newest-first, and a re-delivered
    /// record (live subscriber overlapping the backfill) overwrites rather
    /// than duplicating — the ULID is the key.
    #[test]
    fn events_round_trip_newest_first_and_dedup() {
        let path = temp_db_path("events-rt");
        let store = PersistentStore::open(&path).expect("open");
        let events = vec![
            event("01aaa", 100, "router01", "trap/link_down"),
            event("01aab", 200, "router01", "trap/link_up"),
            event("01aac", 300, "sw02", "trap/cold_start"),
        ];
        assert_eq!(store.write_events(&events).unwrap(), 3);
        // Same ULID again: an update, not a second row.
        store
            .write_events(&[event("01aac", 300, "sw02", "trap/cold_start")])
            .unwrap();

        let got = store.query_events(10).unwrap();
        assert_eq!(
            got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["01aac", "01aab", "01aaa"],
            "newest-first, deduped by ULID"
        );
        // Limit bounds the walk.
        assert_eq!(store.query_events(2).unwrap().len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn prune_events_evicts_oldest_beyond_cap() {
        let path = temp_db_path("events-prune");
        let store = PersistentStore::open(&path).expect("open");
        let events: Vec<_> = (1..=5)
            .map(|i| event(&format!("01aa{i}"), i * 100, "r1", "trap/x"))
            .collect();
        store.write_events(&events).unwrap();
        assert_eq!(store.prune_events(2).unwrap(), 3);
        let got = store.query_events(10).unwrap();
        assert_eq!(
            got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["01aa5", "01aa4"]
        );
        assert_eq!(store.prune_events(2).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// Events are buffered without sampling (unlike logs) and drained once.
    #[test]
    fn record_event_buffers_into_flush_batch() {
        let path = temp_db_path("events-flush");
        let store = PersistentStore::open(&path).expect("open");
        let mut ms = MetricStore::new(10, Some(store));
        ms.record_event(event("01aaa", 100, "r1", "trap/a"));
        ms.record_event(event("01aab", 200, "r1", "trap/b"));
        let (handle, batch) = ms.take_event_flush_batch().expect("a batch");
        assert_eq!(batch.len(), 2, "no sampler drops events");
        assert_eq!(handle.write_events(&batch).unwrap(), 2);
        assert!(ms.take_event_flush_batch().is_none(), "drained");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_event_noop_without_persistence() {
        let mut ms = MetricStore::new(10, None);
        ms.record_event(event("01aaa", 100, "r1", "trap/a"));
        assert!(ms.take_event_flush_batch().is_none());
    }

    #[test]
    fn record_log_applies_retention_into_flush_batch() {
        let path = temp_db_path("logs-flush");
        let store = PersistentStore::open(&path).expect("open");
        let mut ms = MetricStore::new(10, Some(store));
        // 1 error + 1 novel info template are kept; the next repeats are sampled.
        ms.record_log(stored_log(
            "0000000000100000000000001",
            100,
            17,
            Some("e"),
            "err1",
        ));
        ms.record_log(stored_log(
            "0000000000200000000000002",
            200,
            9,
            Some("i"),
            "info-novel",
        ));
        for i in 0..5 {
            ms.record_log(stored_log(
                &format!("{:013}{:012}", 300 + i, 10 + i),
                300 + i,
                9,
                Some("i"),
                "info-repeat",
            ));
        }
        let (handle, batch) = ms.take_log_flush_batch().expect("a batch");
        // error + novel kept; the 5 repeats (sample_every=10) all sampled out.
        assert_eq!(batch.len(), 2);
        assert_eq!(handle.write_logs(&batch).unwrap(), 2);
        // Drained.
        assert!(ms.take_log_flush_batch().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_log_noop_without_persistence() {
        let mut ms = MetricStore::new(10, None);
        ms.record_log(stored_log("x", 1, 17, None, "e"));
        assert!(ms.take_log_flush_batch().is_none());
    }

    #[test]
    fn store_flush_persists_and_clears_pending() {
        let path = temp_db_path("flush");
        let persistent = PersistentStore::open(&path).expect("open");
        let mut store = MetricStore::new(10, Some(persistent.clone()));
        store.record(ORIGIN, "cpu", &point("cpu", 42.0, 60_000));
        store.record(ORIGIN, "cpu", &point("cpu", 43.0, 90_000));
        let (handle, batch) = store.take_flush_batch().expect("batch");
        assert!(!batch.rows.is_empty());
        assert_eq!(
            batch.paths,
            vec![(
                series("cpu"),
                0,
                MetricMeta {
                    kind: MetricKind::Gauge,
                    unit: None,
                    source: "dev1".to_string(),
                    metric: "cpu".to_string(),
                }
            )],
            "the flush carries the series path, its id AND its kind/device, \
             all in the one transaction that writes the sample rows"
        );
        handle.write_batch(&batch).unwrap();
        // Pending cleared after taking the batch.
        assert!(!store.has_pending());
        // Read back the minute tier for the cpu metric.
        let id = store
            .device_metric_ids("sysinfo", ORIGIN, "dev1")
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

    /// The whole point of persisting ids: a second process that opens the
    /// file must resolve the same path to the same id, whatever order the
    /// network hands it metrics in. Before this, ids were minted in arrival
    /// order and never written, so a restart read another metric's rows.
    #[test]
    fn a_restart_resolves_the_same_path_to_the_same_id() {
        let path = temp_db_path("restart-ids");
        let persistent = PersistentStore::open(&path).expect("open");
        let mut first = MetricStore::new(10, Some(persistent.clone()));
        first.record(ORIGIN, "cpu", &point("cpu", 1.0, 60_000));
        first.record(ORIGIN, "mem", &point("mem", 2.0, 60_000));
        let (handle, batch) = first.take_flush_batch().expect("batch");
        handle.write_batch(&batch).unwrap();
        let cpu_id = first
            .device_metric_ids("sysinfo", ORIGIN, "dev1")
            .into_iter()
            .find(|(n, _)| n == "cpu")
            .map(|(_, id)| id)
            .unwrap();

        // Second launch, opposite arrival order.
        let mut second = MetricStore::new(10, Some(persistent.clone()));
        second.record(ORIGIN, "mem", &point("mem", 3.0, 120_000));
        second.record(ORIGIN, "cpu", &point("cpu", 4.0, 120_000));
        let ids = second.device_metric_ids("sysinfo", ORIGIN, "dev1");
        let cpu_again = ids
            .iter()
            .find(|(n, _)| n == "cpu")
            .map(|(_, id)| *id)
            .unwrap();
        assert_eq!(cpu_again, cpu_id, "cpu keeps its id across a restart");
        let got = persistent.query(cpu_id, Tier::Minute, 0, 200_000).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].value, 1.0, "and reads back its OWN history");
        // A brand-new metric mints an id past every persisted one.
        second.record(ORIGIN, "disk", &point("disk", 5.0, 120_000));
        let (_, batch) = second.take_flush_batch().expect("batch");
        assert_eq!(batch.paths.len(), 1);
        assert!(batch.paths[0].1 >= 2);
        let _ = std::fs::remove_file(&path);
    }

    /// A **v2** file must be refused as `Schema { found: 2 }` — not as a redb
    /// table-type mismatch (#904).
    ///
    /// v3 re-typed both `metrics` and `samples`, so `open_table` on a v2 file
    /// fails before any schema check that runs after it. That error is not one
    /// [`MetricStore::with_default_persistence`] recognises as "wrong layout,
    /// move it aside", so the GUI would have degraded to memory-only on every
    /// launch instead of rebuilding the cache — silently, since the store is
    /// designed never to be fatal. Reading the marker in its own transaction
    /// first is what makes this a clean refusal, and this test is the reason
    /// that ordering cannot be tidied away.
    #[test]
    fn a_v2_file_is_refused_by_schema_not_by_a_table_type_mismatch() {
        let path = temp_db_path("schema-v2");
        {
            // Exactly the v2 layout: `metrics` valued by a bare id, `samples`
            // valued by a bare f64, and a `meta` marker saying 2.
            const V2_METRICS: TableDefinition<&str, u32> = TableDefinition::new("metrics");
            const V2_SAMPLES: TableDefinition<u128, f64> = TableDefinition::new("samples");
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                txn.open_table(V2_METRICS)
                    .unwrap()
                    .insert("sysinfo/h-0123456789ab/dev1|cpu", 0u32)
                    .unwrap();
                txn.open_table(V2_SAMPLES)
                    .unwrap()
                    .insert(pack_key(MetricId(0), Tier::Minute, 60), 1.0)
                    .unwrap();
                txn.open_table(META_TABLE)
                    .unwrap()
                    .insert("schema", 2u64)
                    .unwrap();
            }
            txn.commit().unwrap();
        }
        match PersistentStore::open(&path) {
            Err(StoreOpenError::Schema { found: 2 }) => {}
            Err(e) => panic!("a v2 file must be refused as Schema {{ found: 2 }}, got: {e}"),
            Ok(_) => panic!("a v2 file must not open"),
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The kind and the device survive a reopen, and the device index is
    /// rebuilt from them (#904).
    ///
    /// The v2 path carried both — `<protocol>/<origin>/<source>|<metric>` could
    /// be taken apart with a `split_once`. A v3 path is the wire series
    /// identity and carries neither, so if the `metrics` row did not hold them
    /// a restart would lose every device grouping and every counter would read
    /// back as a gauge.
    #[test]
    fn metric_kind_and_device_survive_a_reopen() {
        let path = temp_db_path("meta-round-trip");
        {
            let store = PersistentStore::open(&path).expect("open");
            let mut m = MetricStore::new(10, Some(store));
            let mut counter = point("if/eth0/rx_bytes", 0.0, 60_000);
            counter.value = TelemetryValue::Counter(1_000);
            counter.unit = Some("By".to_string());
            m.record(ORIGIN, "if/eth0/rx_bytes", &counter);
            m.record(ORIGIN, "cpu", &point("cpu", 42.0, 60_000));
            let (handle, batch) = m.take_flush_batch().expect("batch");
            handle.write_batch(&batch).unwrap();
        }

        let store = PersistentStore::open(&path).expect("reopen");
        let m = MetricStore::new(10, Some(store));
        let i = m.interner();

        let counter_id = i
            .get(&series("if/eth0/rx_bytes"))
            .expect("counter interned");
        assert_eq!(
            i.meta(counter_id).map(|x| x.kind),
            Some(MetricKind::Counter),
            "a counter must not read back as a gauge — that is the distinction \
             three GUI call sites had to re-infer by hand"
        );
        assert_eq!(
            i.meta(counter_id).map(|x| x.metric.as_str()),
            Some("if/eth0/rx_bytes"),
        );
        // The unit too (#907): a `range` reply promises one, and a chart
        // opening on a quiet fleet has no live sample to take it from.
        assert_eq!(
            i.meta(counter_id).and_then(|x| x.unit.as_deref()),
            Some("By")
        );
        // …and a series whose producer declared none reads back as unknown,
        // not as dimensionless.
        assert_eq!(
            i.meta(i.get(&series("cpu")).unwrap())
                .and_then(|x| x.unit.as_deref()),
            None
        );
        assert_eq!(
            i.meta(i.get(&series("cpu")).unwrap()).map(|x| x.kind),
            Some(MetricKind::Gauge)
        );

        // The device index is rebuilt from the recorded source, not from the
        // path — which no longer contains it.
        let mut names: Vec<String> = m
            .device_metric_ids("sysinfo", ORIGIN, "dev1")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["cpu".to_string(), "if/eth0/rx_bytes".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// `query_buckets` asks redb for the window, not for the series (#904).
    #[test]
    fn a_range_query_returns_only_the_window_and_carries_min_max() {
        let path = temp_db_path("range");
        let store = PersistentStore::open(&path).expect("open");
        let m = MetricId(3);
        let rows: Vec<FlushRow> = (0..10)
            .map(|i| {
                (
                    m,
                    Tier::Minute,
                    i * 60,
                    Bucket {
                        last: i as f64,
                        min: 0.0,
                        max: (i * 2) as f32,
                    },
                )
            })
            .collect();
        store
            .write_batch(&FlushBatch {
                rows,
                paths: vec![],
            })
            .unwrap();

        // Buckets 2..=4 by their millisecond timestamps.
        let got = store
            .query_buckets(m, Tier::Minute, 120_000, 240_000)
            .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].0, 120_000);
        assert_eq!(got[2].0, 240_000);
        assert_eq!(got[2].1.last, 4.0);
        assert_eq!(
            got[2].1.max, 8.0,
            "the range a coarse bucket covered survives"
        );

        // `query` is the same walk with only `last` kept.
        let samples = store.query(m, Tier::Minute, 120_000, 240_000).unwrap();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[2].value, 4.0);

        // An inverted window is empty, not a panic and not the whole series.
        assert!(
            store
                .query_buckets(m, Tier::Minute, 240_000, 120_000)
                .unwrap()
                .is_empty()
        );
        // A window that starts before the epoch clamps rather than underflowing.
        assert_eq!(
            store
                .query_buckets(m, Tier::Minute, -5_000, 60_000)
                .unwrap()
                .len(),
            2
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The governor's evict hook (#906): halving the ring must actually drop
    /// the oldest samples of every series, not merely lower a number that
    /// nothing enforces — an eviction that frees nothing is worse than none,
    /// because the ladder reads it as relief and stops escalating.
    #[test]
    fn halving_the_hot_ring_drops_the_oldest_samples_of_every_series() {
        let mut store = MetricStore::new(8, None);
        for ts in 0..8 {
            store.record(ORIGIN, "cpu", &point("cpu", ts as f64, ts * 1_000));
            store.record(ORIGIN, "mem", &point("mem", ts as f64, ts * 1_000));
        }
        assert_eq!(store.hot_sample_count(), 16);
        assert_eq!(store.hot_capacity(), 8);

        assert_eq!(store.halve_hot_capacity(), 4);
        assert_eq!(
            store.hot_sample_count(),
            8,
            "both series shrank, not just one"
        );
        // The samples kept are the NEWEST: history is a window on now, and
        // dropping the recent half would leave a live chart empty.
        let cpu = store.hot_samples(&series("cpu"));
        assert_eq!(cpu.len(), 4);
        assert_eq!(cpu.first().map(|s| s.ts), Some(4_000));
        assert_eq!(cpu.last().map(|s| s.ts), Some(7_000));

        // New capacity applies to a series interned afterwards too.
        store.record(ORIGIN, "disk", &point("disk", 1.0, 9_000));
        assert_eq!(store.hot_samples(&series("disk")).len(), 1);

        // It floors at 1 rather than 0: a ring of zero silently stops
        // answering live questions, which is worse than holding one sample.
        for _ in 0..8 {
            store.halve_hot_capacity();
        }
        assert_eq!(store.hot_capacity(), 1);
        assert_eq!(store.hot_samples(&series("cpu")).len(), 1);
    }

    /// `stats` reports what the tiers hold and how far back they go (#906) —
    /// the numbers #911 measures retention against.
    #[test]
    fn the_store_reports_its_rows_size_and_oldest_bucket() {
        let path = temp_db_path("stats");
        let store = PersistentStore::open(&path).expect("open");
        let m = MetricId(1);
        store
            .write_batch(&FlushBatch {
                rows: vec![
                    (m, Tier::Minute, 60, Bucket::point(1.0)),
                    (m, Tier::Minute, 120, Bucket::point(2.0)),
                    (m, Tier::Hour, 3_600, Bucket::point(3.0)),
                ],
                paths: vec![],
            })
            .unwrap();

        assert_eq!(store.tier_rows(Tier::Minute).unwrap(), 2);
        assert_eq!(store.tier_rows(Tier::Hour).unwrap(), 1);
        assert_eq!(store.tier_rows(Tier::Second).unwrap(), 0);
        assert!(store.db_bytes() > 0, "the file is on disk and has a size");
        // The oldest bucket across every tier, in milliseconds — the honest
        // answer to "how far back can I ask", which retention keeps moving.
        assert_eq!(store.oldest_bucket_ms().unwrap(), Some(60_000));

        let empty = temp_db_path("stats-empty");
        let store = PersistentStore::open(&empty).expect("open");
        assert_eq!(store.oldest_bucket_ms().unwrap(), None);
        assert_eq!(store.tier_rows(Tier::Minute).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&empty);
    }

    /// A pre-v2 file — samples, no `metrics`/`meta` — is refused, not read:
    /// its rows are keyed by ids nobody can name. (A fresh, empty file is
    /// stamped with the current schema instead.)
    #[test]
    fn a_v1_file_with_samples_is_refused_as_schema_v1() {
        let path = temp_db_path("schema-v1");
        {
            let db = Database::create(&path).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut t = txn.open_table(SAMPLES_TABLE).unwrap();
                t.insert(
                    pack_key(MetricId(0), Tier::Minute, 60),
                    Bucket::point(1.0).as_row(),
                )
                .unwrap();
            }
            txn.commit().unwrap();
        }
        match PersistentStore::open(&path) {
            Err(StoreOpenError::Schema { found: 1 }) => {}
            Err(other) => panic!("expected a schema refusal, got {other}"),
            Ok(_) => panic!("a v1 file must not open"),
        }
        let _ = std::fs::remove_file(&path);

        let fresh = temp_db_path("schema-fresh");
        let store = PersistentStore::open(&fresh).expect("a fresh file opens");
        drop(store);
        PersistentStore::open(&fresh).expect("and reopens: it was stamped");
        let _ = std::fs::remove_file(&fresh);
    }

    // Needs the zblob adapter (#904 gated it behind `blob`).
    #[cfg(feature = "blob")]
    /// The GC contract behind the periodic chunk-cache sweep (#131's chunk
    /// half): chunks referenced by a snapshot tag survive, orphans go, and a
    /// temp-tagged chunk (an in-flight download's) is protected.
    #[test]
    fn chunk_store_sweep_keeps_tagged_and_temp_tagged() {
        use zblob::ContentStore;

        let path = temp_db_path("sweep");
        let persistent = PersistentStore::open(&path).expect("open");
        let cs = RedbContentStore::new(persistent.clone());

        // A real snapshot built straight into the redb-backed store, so the
        // tag's chunk set is exactly what a downloaded tree would pin.
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.bin"), vec![1u8; 100_000]).unwrap();
        let index =
            zblob::build_tree(src.path(), "sweep-test", &zblob::CdcParams::default(), &cs).unwrap();

        let tag_dir = tempfile::tempdir().unwrap();
        let tags = zblob::gc::SnapshotTags::open(tag_dir.path()).unwrap();
        tags.set("dl-1-abc", &index).unwrap();

        let orphan = zblob::Hash::of(b"orphaned chunk");
        cs.put(&orphan, b"orphaned chunk").unwrap();
        let inflight = zblob::Hash::of(b"in-flight chunk");
        cs.put(&inflight, b"in-flight chunk").unwrap();

        let temps = zblob::gc::TempTags::new();
        let _guard = temps.protect([inflight]);

        let stats = zblob::gc::sweep(&cs, &tags, &temps, []).unwrap();
        assert_eq!(stats.removed, 1, "exactly the orphan goes");
        assert!(!cs.has(&orphan).unwrap(), "orphan swept");
        assert!(cs.has(&inflight).unwrap(), "temp-tagged chunk survives");
        for h in index.needed_chunks() {
            assert!(cs.has(&h).unwrap(), "tagged snapshot chunk survives");
        }

        // Untag + drop the temp guard: everything is garbage now.
        tags.remove("dl-1-abc").unwrap();
        drop(_guard);
        zblob::gc::sweep(&cs, &tags, &temps, []).unwrap();
        assert!(
            cs.hashes().unwrap().is_empty(),
            "untagged store sweeps to empty"
        );

        let _ = std::fs::remove_file(&path);
    }

    // Needs the zblob adapter (#904 gated it behind `blob`).
    #[cfg(feature = "blob")]
    #[test]
    fn chunk_store_round_trip_and_persists() {
        use zblob::ContentStore;

        let path = temp_db_path("chunks");
        let persistent = PersistentStore::open(&path).expect("open");
        let cs = RedbContentStore::new(persistent.clone());

        let bytes = b"tier-2 chunk bytes";
        let hash = zblob::Hash::of(bytes);

        // Missing → put → present → readable. (0.3's trait returns
        // `io::Result` from the read paths too, so the unwraps here are the
        // "store is healthy" half of each assertion.)
        assert!(!cs.has(&hash).unwrap());
        assert!(cs.get(&hash).unwrap().is_none());
        cs.put(&hash, bytes).unwrap();
        assert!(cs.has(&hash).unwrap());
        assert_eq!(cs.get(&hash).unwrap().unwrap(), bytes);
        // Idempotent re-put.
        cs.put(&hash, bytes).unwrap();
        assert_eq!(cs.get(&hash).unwrap().unwrap(), bytes);

        // `hashes()` (now derived from `for_each_hash`) and `remove()` agree
        // with the rest: a store that enumerates a chunk `get` cannot return,
        // or keeps one `remove` claimed to drop, breaks garbage collection
        // silently.
        assert_eq!(cs.hashes().unwrap(), vec![hash]);
        assert!(cs.remove(&hash).unwrap(), "remove reports it was present");
        assert!(!cs.has(&hash).unwrap());
        assert!(cs.hashes().unwrap().is_empty());
        assert!(!cs.remove(&hash).unwrap(), "second remove is a no-op");
        cs.put(&hash, bytes).unwrap();

        // A pre-0.2 `sha256/…` key is inert rather than fatal: it belongs to a
        // different address space, so `hashes()` skips it instead of failing
        // the whole sweep.
        persistent.write_chunk("sha256/deadbeef", b"stale").unwrap();
        assert_eq!(cs.hashes().unwrap(), vec![hash]);

        // Reopening the database sees the persisted chunk (restart-proof resume).
        drop(cs);
        drop(persistent);
        let reopened = PersistentStore::open(&path).expect("reopen");
        let cs2 = RedbContentStore::new(reopened);
        assert!(cs2.has(&hash).unwrap());
        assert_eq!(cs2.get(&hash).unwrap().unwrap(), bytes);

        let _ = std::fs::remove_file(&path);
    }
}
