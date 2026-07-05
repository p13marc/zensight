//! Capture-to-disk engine (#327): rotating pcap spool + anomaly-triggered
//! pre-trigger-ring capture.
//!
//! A dedicated packet-tier subscription (see `monitor.rs`) forwards every
//! matching frame into a **bounded** channel (`try_send` + drop counter — the
//! capture hot loop is never blocked); this engine owns the write side:
//!
//! * **`rotating`** — stream every frame into a
//!   [`netring::pcap_rotate::RotatingPcapWriter`] (size/duration rotation,
//!   file-count / total-byte retention). A local forensics spool: files are
//!   listed on `@/query/captures` but their bytes are not served over the bus.
//! * **`triggered`** — buffer recent frames in a bytes-bounded in-memory ring;
//!   when an anomaly passes the severity/kind gate (see [`should_trigger`],
//!   applied in `publish.rs`) or a `capture_now` command arrives, flush the
//!   lead-up plus `post_trigger_secs` of aftermath to a pcap file, optionally
//!   zstd-compress it, and register it as a TTL'd Tier-1 blob on the engine's
//!   own [`BlobServer`] (same `@/artifact/blob` prefix as the artifact channel
//!   — a blob server ignores ids it doesn't own, so both coexist). The GUI
//!   downloads it by `artifact_id` exactly like a #333 on-demand capture.
//!
//! The engine is the **single owner** of every file it writes: retention
//! eviction (oldest first) and artifact-TTL expiry are both enforced here, so
//! the file lifecycle has no second writer to race with.
//!
//! Like the runtime detector registry, a mode that is `off` at startup is not
//! armed (no packet subscription is registered) — switching it on takes a
//! restart; switching between `rotating`/`triggered` (and to `off`) is live via
//! `@/commands/capture_disk`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use netring::packet::Timestamp;
use netring::pcap_rotate::{FileNaming, RotatingConfig, RotatingPcapWriter};
use tokio::sync::mpsc;
use zenoh_blob::{BlobServer, FileBlobSource, FixedSizeChunker, Manifest, Sha256Digest};
use zensight_common::query_detail::CaptureRecord;
use zensight_common::{AlertSeverity, TelemetryPoint};

use crate::config::{CaptureDiskMode, CaptureToDiskConfig};

/// Bounded capacity of the packet-sub → engine channel (frames). Sized like the
/// on-demand tap channel; over it the subscription drops and counts.
pub const DISK_CHANNEL_CAP: usize = 8192;

/// Max entries the served capture index keeps (newest first).
pub const CAPTURE_INDEX_CAP: usize = 64;

/// Chunk size for triggered-capture blob manifests, bytes.
const BLOB_CHUNK_SIZE: u32 = 512 * 1024;

/// One frame lifted off the wire by the continuous capture-to-disk
/// subscription. `data` is already snap-truncated at copy time (the handler
/// applies `snaplen` before the copy to keep the per-frame allocation small);
/// `original_len` keeps the on-wire length for the pcap record header.
pub struct DiskFrame {
    pub data: Vec<u8>,
    pub ts: Timestamp,
    pub original_len: usize,
}

/// The bounded ring of recent [`CaptureRecord`]s served on `@/query/captures`.
pub type CaptureIndex = Arc<Mutex<VecDeque<CaptureRecord>>>;

/// Live engine counters, shared with the packet subscription (drop counter),
/// the command/status channel and the telemetry emitter. All loads are relaxed
/// — these are monitoring numbers, not synchronization.
#[derive(Default)]
pub struct CaptureDiskStats {
    /// Live mode as `CaptureDiskMode as u8` (hot-switchable).
    mode: AtomicU8,
    /// True while a triggered capture is streaming its post-trigger window.
    pub recording: AtomicBool,
    /// Pre-trigger ring occupancy (triggered mode).
    pub ring_packets: AtomicU64,
    pub ring_bytes: AtomicU64,
    /// Retained on-disk capture files / bytes (this engine's files only).
    pub retained_files: AtomicU64,
    pub retained_bytes: AtomicU64,
    /// Frames dropped because the engine channel was full.
    pub dropped: AtomicU64,
    /// Files deleted by the retention caps.
    pub evictions: AtomicU64,
    /// Trigger firings accepted (manual + anomaly).
    pub triggers: AtomicU64,
    /// Human-readable last lifecycle event, for the status queryable.
    pub last_event: Mutex<Option<String>>,
}

impl CaptureDiskStats {
    pub fn new(mode: CaptureDiskMode) -> Self {
        let s = Self::default();
        s.set_mode(mode);
        s
    }

    pub fn set_mode(&self, mode: CaptureDiskMode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    pub fn mode(&self) -> CaptureDiskMode {
        match self.mode.load(Ordering::Relaxed) {
            1 => CaptureDiskMode::Rotating,
            2 => CaptureDiskMode::Triggered,
            _ => CaptureDiskMode::Off,
        }
    }

    fn note(&self, event: impl Into<String>) {
        let event = event.into();
        tracing::info!(event = %event, "netring: capture-to-disk");
        if let Ok(mut last) = self.last_event.lock() {
            *last = Some(event);
        }
    }
}

/// A control verb for the engine, sent by the command channel or the anomaly
/// drain. Unbounded: triggers are rare (anomaly-rate) by construction.
pub enum DiskCtl {
    /// Fire the trigger (triggered mode) or rotate the spool (rotating mode).
    Trigger { kind: String, tag: Option<String> },
    /// Hot-switch the live mode (between the armed modes; see module docs).
    SetMode { mode: CaptureDiskMode },
}

/// Cheap clonable handle: the anomaly drain fires triggers through it, the
/// command channel drives mode switches and reads stats for status replies.
#[derive(Clone)]
pub struct CaptureDiskHandle {
    ctl: mpsc::UnboundedSender<DiskCtl>,
    stats: Arc<CaptureDiskStats>,
}

impl CaptureDiskHandle {
    pub fn new(ctl: mpsc::UnboundedSender<DiskCtl>, stats: Arc<CaptureDiskStats>) -> Self {
        Self { ctl, stats }
    }

    pub fn stats(&self) -> &CaptureDiskStats {
        &self.stats
    }

    /// Fire the trigger for a detector `kind` (anomaly path). A no-op unless
    /// the live mode is `triggered` — checked here so the hot drain doesn't
    /// queue ctl messages the engine would discard.
    pub fn trigger(&self, kind: &str, tag: Option<String>) {
        if self.stats.mode() == CaptureDiskMode::Triggered {
            let _ = self.ctl.send(DiskCtl::Trigger {
                kind: kind.to_string(),
                tag,
            });
        }
    }

    /// Manual `capture_now`: fires the trigger in triggered mode, forces a
    /// rotation in rotating mode (the engine dispatches on its live mode).
    pub fn capture_now(&self, tag: Option<String>) {
        let _ = self.ctl.send(DiskCtl::Trigger {
            kind: "manual".to_string(),
            tag,
        });
    }

    pub fn set_mode(&self, mode: CaptureDiskMode) {
        let _ = self.ctl.send(DiskCtl::SetMode { mode });
    }
}

/// Whether an anomaly of `kind` at `severity` passes the configured trigger
/// gate. Pure — the severity floor and the (optional) detector allowlist; the
/// live-mode check happens on the handle.
pub fn should_trigger(cfg: &CaptureToDiskConfig, kind: &str, severity: AlertSeverity) -> bool {
    severity >= cfg.trigger_min_severity
        && (cfg.trigger_kinds.is_empty() || cfg.trigger_kinds.iter().any(|k| k == kind))
}

/// A bytes-bounded pre-trigger ring. netring 0.29 ships `PreTriggerRing`, but
/// it exposes no byte-occupancy introspection (needed for the status queryable
/// and the `capture/disk/*` telemetry), so this keeps its own tally with the
/// same evict-oldest-past-cap behaviour.
struct PreRing {
    max_bytes: usize,
    buffered: usize,
    ring: VecDeque<(Vec<u8>, Timestamp, usize)>,
}

impl PreRing {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            buffered: 0,
            ring: VecDeque::new(),
        }
    }

    fn push(&mut self, data: Vec<u8>, ts: Timestamp, original_len: usize) {
        self.buffered += data.len();
        self.ring.push_back((data, ts, original_len));
        while self.buffered > self.max_bytes && self.ring.len() > 1 {
            if let Some((d, _, _)) = self.ring.pop_front() {
                self.buffered -= d.len();
            }
        }
    }

    /// Drain oldest-first into `w`; returns (approx bytes, packets, first ts).
    fn drain_into(
        &mut self,
        w: &mut RotatingPcapWriter,
    ) -> std::io::Result<(u64, u64, Option<Timestamp>)> {
        let mut bytes = 0u64;
        let mut packets = 0u64;
        let mut first = None;
        for (data, ts, orig) in self.ring.drain(..) {
            w.write_raw(&data, ts, orig, None)?;
            bytes += 16 + data.len() as u64;
            packets += 1;
            if first.is_none() {
                first = Some(ts);
            }
        }
        self.buffered = 0;
        Ok((bytes, packets, first))
    }

    fn len(&self) -> usize {
        self.ring.len()
    }

    fn bytes(&self) -> usize {
        self.buffered
    }
}

/// A finished triggered-capture file the engine retains (and possibly serves).
struct RetainedFile {
    path: PathBuf,
    bytes: u64,
    filename: String,
    /// Blob id while served; unregistered at `expires` or on eviction.
    artifact_id: Option<String>,
    expires: Option<tokio::time::Instant>,
}

/// One in-flight triggered recording.
struct Recording {
    writer: RotatingPcapWriter,
    /// Grabbed from `current_path()` as soon as the lazily-opened file exists.
    path: Option<PathBuf>,
    deadline: tokio::time::Instant,
    kind: String,
    tag: Option<String>,
    written: u64,
    packets: u64,
    first_ts: Option<Timestamp>,
    last_ts: Option<Timestamp>,
    truncated: bool,
}

enum WriterState {
    /// Live mode `off` (hot-switched; frames are discarded).
    Idle,
    Rotating(RotatingPcapWriter),
    /// Triggered mode, buffering the pre-trigger window.
    Armed(PreRing),
    /// Triggered mode, streaming the post-trigger window.
    Recording(Box<Recording>),
}

fn epoch_ms(ts: Timestamp) -> i64 {
    ts.to_duration().as_millis() as i64
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Rotating-spool writer config from the sensor config.
#[allow(clippy::field_reassign_with_default)] // RotatingConfig is #[non_exhaustive]
fn spool_config(cfg: &CaptureToDiskConfig) -> RotatingConfig {
    let mut rc = RotatingConfig::default();
    rc.max_bytes = Some(cfg.max_file_bytes);
    rc.max_duration = (cfg.rotate_secs > 0).then(|| Duration::from_secs(cfg.rotate_secs));
    rc.max_files = Some(cfg.max_files.max(1));
    rc.max_total_bytes = Some(cfg.max_total_bytes);
    rc.naming = FileNaming::EpochSeconds;
    rc
}

/// Single-file writer config for one triggered capture (no rotation — the
/// engine enforces the byte cap + retention itself so it always knows the
/// produced path).
#[allow(clippy::field_reassign_with_default)] // RotatingConfig is #[non_exhaustive]
fn single_file_config() -> RotatingConfig {
    let mut rc = RotatingConfig::default();
    rc.max_bytes = None;
    rc.max_duration = None;
    rc.max_files = None;
    rc.max_total_bytes = None;
    rc.naming = FileNaming::EpochSeconds;
    rc
}

/// Run the capture-to-disk engine until the frame channel closes (monitor
/// shutdown). `blob` is `Some` on a live sensor; `None` keeps triggered files
/// on disk without serving them (still indexed).
#[allow(clippy::too_many_arguments)]
pub async fn run_engine(
    cfg: CaptureToDiskConfig,
    source: String,
    mut frames: mpsc::Receiver<DiskFrame>,
    mut ctl: mpsc::UnboundedReceiver<DiskCtl>,
    stats: Arc<CaptureDiskStats>,
    index: CaptureIndex,
    blob: Option<BlobServer>,
    events: mpsc::UnboundedSender<TelemetryPoint>,
) {
    let Some(dir) = cfg.dir.clone() else {
        tracing::error!("netring: capture.to_disk enabled without a dir (validation gap)");
        return;
    };
    let dir = PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::error!(error = %e, dir = %dir.display(), "netring: capture.to_disk dir not writable; engine disabled");
        return;
    }

    let mut state = match new_state(cfg.mode, &cfg, &dir, &source, &stats) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "netring: capture-to-disk writer create failed; engine disabled");
            return;
        }
    };
    let mut retained: Vec<RetainedFile> = Vec::new();

    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    stats.note(format!("armed ({})", mode_label(stats.mode())));

    loop {
        tokio::select! {
            maybe = frames.recv() => {
                let Some(f) = maybe else { break }; // monitor gone
                if let Err(e) = feed(&mut state, f) {
                    tracing::warn!(error = %e, "netring: capture-to-disk write failed");
                }
                // Byte cap reached mid-window → finalize early.
                if matches!(&state, WriterState::Recording(r) if r.written >= cfg.max_file_bytes) {
                    if let WriterState::Recording(mut r) = std::mem::replace(&mut state, WriterState::Idle) {
                        r.truncated = true;
                        finalize_recording(*r, &cfg, &source, &stats, &index, &blob, &events, &mut retained).await;
                    }
                    state = rearm(&cfg, &stats);
                }
            }
            Some(msg) = ctl.recv() => {
                handle_ctl(msg, &mut state, &cfg, &dir, &source, &stats, &index, &blob, &events, &mut retained).await;
            }
            _ = tick.tick() => {
                // Post-trigger window elapsed → finalize.
                if matches!(&state, WriterState::Recording(r) if tokio::time::Instant::now() >= r.deadline) {
                    if let WriterState::Recording(r) = std::mem::replace(&mut state, WriterState::Idle) {
                        finalize_recording(*r, &cfg, &source, &stats, &index, &blob, &events, &mut retained).await;
                    }
                    state = rearm(&cfg, &stats);
                }
                refresh_stats(&state, &stats, &retained);
                if let WriterState::Rotating(w) = &state {
                    refresh_spool_index(w, &index);
                }
                reap_expired_artifacts(&mut retained, &blob, &index, &stats).await;
            }
        }
    }

    // Shutdown: close writers; finalize an in-flight recording so its packets
    // aren't lost with the process.
    match state {
        WriterState::Rotating(mut w) => {
            let _ = w.sync_and_close();
        }
        WriterState::Recording(r) => {
            finalize_recording(
                *r,
                &cfg,
                &source,
                &stats,
                &index,
                &blob,
                &events,
                &mut retained,
            )
            .await;
        }
        _ => {}
    }
}

fn mode_label(mode: CaptureDiskMode) -> &'static str {
    match mode {
        CaptureDiskMode::Off => "off",
        CaptureDiskMode::Rotating => "rotating",
        CaptureDiskMode::Triggered => "triggered",
    }
}

/// Build the writer state for `mode` (used at startup and on SetMode).
fn new_state(
    mode: CaptureDiskMode,
    cfg: &CaptureToDiskConfig,
    dir: &Path,
    source: &str,
    stats: &CaptureDiskStats,
) -> std::io::Result<WriterState> {
    stats.set_mode(mode);
    stats.recording.store(false, Ordering::Relaxed);
    Ok(match mode {
        CaptureDiskMode::Off => WriterState::Idle,
        CaptureDiskMode::Rotating => WriterState::Rotating(RotatingPcapWriter::create(
            dir,
            format!("zensight-{source}-spool"),
            spool_config(cfg),
        )?),
        CaptureDiskMode::Triggered => WriterState::Armed(PreRing::new(cfg.ring_bytes as usize)),
    })
}

/// Fresh armed state after a triggered capture finishes.
fn rearm(cfg: &CaptureToDiskConfig, stats: &CaptureDiskStats) -> WriterState {
    stats.recording.store(false, Ordering::Relaxed);
    WriterState::Armed(PreRing::new(cfg.ring_bytes as usize))
}

/// Route one frame into the current writer state.
fn feed(state: &mut WriterState, f: DiskFrame) -> std::io::Result<()> {
    match state {
        WriterState::Idle => Ok(()),
        WriterState::Rotating(w) => w.write_raw(&f.data, f.ts, f.original_len, None),
        WriterState::Armed(ring) => {
            ring.push(f.data, f.ts, f.original_len);
            Ok(())
        }
        WriterState::Recording(r) => {
            r.writer.write_raw(&f.data, f.ts, f.original_len, None)?;
            if r.path.is_none() {
                r.path = r.writer.current_path().map(Path::to_path_buf);
            }
            r.written += 16 + f.data.len() as u64;
            r.packets += 1;
            if r.first_ts.is_none() {
                r.first_ts = Some(f.ts);
            }
            r.last_ts = Some(f.ts);
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_ctl(
    msg: DiskCtl,
    state: &mut WriterState,
    cfg: &CaptureToDiskConfig,
    dir: &Path,
    source: &str,
    stats: &Arc<CaptureDiskStats>,
    index: &CaptureIndex,
    blob: &Option<BlobServer>,
    events: &mpsc::UnboundedSender<TelemetryPoint>,
    retained: &mut Vec<RetainedFile>,
) {
    match msg {
        DiskCtl::Trigger { kind, tag } => match state {
            WriterState::Armed(_) => {
                let WriterState::Armed(mut ring) = std::mem::replace(state, WriterState::Idle)
                else {
                    unreachable!()
                };
                let seq = stats.triggers.load(Ordering::Relaxed);
                match start_recording(&mut ring, cfg, dir, source, &kind, tag, seq) {
                    Ok(rec) => {
                        stats.triggers.fetch_add(1, Ordering::Relaxed);
                        stats.recording.store(true, Ordering::Relaxed);
                        stats.note(format!("trigger fired ({kind})"));
                        let _ = events.send(crate::map::capture_event_point(
                            source,
                            "trigger",
                            &format!("trigger fired: {kind}"),
                        ));
                        *state = WriterState::Recording(rec);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "netring: trigger capture create failed");
                        stats.note(format!("trigger failed: {e}"));
                        *state = rearm(cfg, stats);
                    }
                }
            }
            WriterState::Recording(_) => {
                stats.note(format!("trigger ignored ({kind}): already recording"));
            }
            WriterState::Rotating(w) => {
                // `capture_now` in rotating mode: finalize the current spool
                // file so its contents are complete on disk right now.
                if let Err(e) = w.rotate_now() {
                    tracing::warn!(error = %e, "netring: rotate_now failed");
                }
                stats.note("spool rotated (capture_now)");
            }
            WriterState::Idle => {
                stats.note(format!("trigger ignored ({kind}): mode is off"));
            }
        },
        DiskCtl::SetMode { mode } => {
            if mode == stats.mode() {
                return;
            }
            // Close out the current state first (an in-flight recording is
            // finalized so its packets are preserved).
            match std::mem::replace(state, WriterState::Idle) {
                WriterState::Rotating(mut w) => {
                    let _ = w.sync_and_close();
                }
                WriterState::Recording(r) => {
                    finalize_recording(*r, cfg, source, stats, index, blob, events, retained).await;
                }
                _ => {}
            }
            match new_state(mode, cfg, dir, source, stats) {
                Ok(s) => {
                    *state = s;
                    stats.note(format!("mode set to {}", mode_label(mode)));
                    let _ = events.send(crate::map::capture_event_point(
                        source,
                        "mode",
                        &format!("capture-to-disk mode set to {}", mode_label(mode)),
                    ));
                }
                Err(e) => {
                    tracing::error!(error = %e, "netring: SetMode writer create failed; capture-to-disk idle");
                    stats.set_mode(CaptureDiskMode::Off);
                    stats.note(format!("mode switch failed: {e}"));
                }
            }
        }
    }
}

/// Open the per-trigger file and flush the pre-trigger ring into it. `seq` is
/// an engine-local counter making the base unique even for two triggers in the
/// same millisecond (each trigger uses a fresh writer, so the upstream
/// per-writer collision guard can't see the previous one's files).
fn start_recording(
    ring: &mut PreRing,
    cfg: &CaptureToDiskConfig,
    dir: &Path,
    source: &str,
    kind: &str,
    tag: Option<String>,
    seq: u64,
) -> std::io::Result<Box<Recording>> {
    let base = format!(
        "zensight-{source}-trigger-{}-{}-{seq}",
        slug(kind),
        now_ms()
    );
    let mut writer = RotatingPcapWriter::create(dir, base, single_file_config())?;
    let (bytes, packets, first_ts) = ring.drain_into(&mut writer)?;
    let path = writer.current_path().map(Path::to_path_buf);
    Ok(Box::new(Recording {
        writer,
        path,
        deadline: tokio::time::Instant::now() + Duration::from_secs(cfg.post_trigger_secs.max(1)),
        kind: kind.to_string(),
        tag,
        written: bytes,
        packets,
        first_ts,
        last_ts: first_ts,
        truncated: false,
    }))
}

/// Keep filenames tame for detector slugs like `cleartext_http_credentials`.
fn slug(kind: &str) -> String {
    kind.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Close, compress, serve and index one finished triggered capture, then
/// enforce the retention caps over the engine's files.
#[allow(clippy::too_many_arguments)]
async fn finalize_recording(
    mut rec: Recording,
    cfg: &CaptureToDiskConfig,
    source: &str,
    stats: &Arc<CaptureDiskStats>,
    index: &CaptureIndex,
    blob: &Option<BlobServer>,
    events: &mpsc::UnboundedSender<TelemetryPoint>,
    retained: &mut Vec<RetainedFile>,
) {
    stats.recording.store(false, Ordering::Relaxed);
    if let Err(e) = rec.writer.sync_and_close() {
        tracing::warn!(error = %e, "netring: triggered capture close failed");
    }
    let Some(raw_path) = rec.path.clone() else {
        // Nothing was ever written (empty ring + silent wire) — no file to serve.
        stats.note(format!("trigger ({}) produced no packets", rec.kind));
        return;
    };

    // Optional zstd compression (blocking; off the async loop).
    let (path, filename) = if cfg.compress {
        let zst = raw_path.with_extension("pcap.zst");
        let (src, dst) = (raw_path.clone(), zst.clone());
        let compressed =
            tokio::task::spawn_blocking(move || crate::capture::zstd_compress(&src, &dst)).await;
        match compressed {
            Ok(Ok(())) => {
                let _ = std::fs::remove_file(&raw_path);
                let name = zst
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (zst, name)
            }
            other => {
                let err = match other {
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                    Ok(Ok(())) => unreachable!(),
                };
                tracing::warn!(error = %err, "netring: capture compression failed; serving raw pcap");
                let name = raw_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (raw_path, name)
            }
        }
    } else {
        let name = raw_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        (raw_path, name)
    };

    let disk_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    // Serve as a TTL'd Tier-1 blob (see module docs on server coexistence).
    let mut artifact_id = None;
    let mut expires_ms = None;
    let mut expires = None;
    if let Some(blob) = blob {
        match register_blob(blob, &path, &filename).await {
            Ok(id) => {
                expires_ms = Some(now_ms() + (cfg.artifact_ttl_secs as i64) * 1000);
                expires =
                    Some(tokio::time::Instant::now() + Duration::from_secs(cfg.artifact_ttl_secs));
                artifact_id = Some(id);
            }
            Err(e) => {
                tracing::warn!(error = %e, "netring: capture blob registration failed");
            }
        }
    }

    let record = CaptureRecord {
        filename: filename.clone(),
        bytes: disk_bytes,
        packets: rec.packets,
        mode: "triggered".to_string(),
        trigger_kind: Some(rec.kind.clone()),
        tag: rec.tag.clone(),
        start_ms: rec.first_ts.map(epoch_ms).unwrap_or(0),
        end_ms: rec.last_ts.map(epoch_ms).unwrap_or(0),
        artifact_id: artifact_id.clone(),
        expires_ms,
        truncated: rec.truncated,
    };
    if let Ok(mut idx) = index.lock() {
        idx.push_front(record);
        while idx.len() > CAPTURE_INDEX_CAP {
            idx.pop_back();
        }
    }

    retained.push(RetainedFile {
        path,
        bytes: disk_bytes,
        filename: filename.clone(),
        artifact_id,
        expires,
    });
    enforce_retention(cfg, retained, blob, index, stats).await;

    let detail = format!(
        "capture ready: {filename} · {} pkts · {:.1} MiB{}",
        rec.packets,
        disk_bytes as f64 / (1024.0 * 1024.0),
        if rec.truncated { " · truncated" } else { "" },
    );
    stats.note(detail.clone());
    let _ = events.send(crate::map::capture_event_point(source, "ready", &detail));
}

/// Compute the manifest and register the file on the blob server; the returned
/// id is what the GUI downloads by.
async fn register_blob(blob: &BlobServer, path: &Path, filename: &str) -> anyhow::Result<String> {
    let id = ulid::Ulid::new().to_string();
    let mut reader = tokio::fs::File::open(path).await?;
    let manifest = Manifest::compute::<_, Sha256Digest>(
        &mut reader,
        &FixedSizeChunker::new(BLOB_CHUNK_SIZE),
        id.clone(),
        filename.to_string(),
        now_ms(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("manifest: {e}"))?;
    blob.register(manifest, Arc::new(FileBlobSource::new(path)))
        .await;
    Ok(id)
}

/// Evict oldest triggered files past the file-count / total-byte caps.
async fn enforce_retention(
    cfg: &CaptureToDiskConfig,
    retained: &mut Vec<RetainedFile>,
    blob: &Option<BlobServer>,
    index: &CaptureIndex,
    stats: &CaptureDiskStats,
) {
    loop {
        let total: u64 = retained.iter().map(|f| f.bytes).sum();
        let over = retained.len() > cfg.max_files.max(1)
            || (total > cfg.max_total_bytes && retained.len() > 1);
        if !over {
            break;
        }
        let victim = retained.remove(0);
        if let (Some(blob), Some(id)) = (blob.as_ref(), victim.artifact_id.as_ref()) {
            blob.unregister(id).await;
        }
        let _ = std::fs::remove_file(&victim.path);
        stats.evictions.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut idx) = index.lock() {
            idx.retain(|r| r.filename != victim.filename);
        }
        tracing::info!(file = %victim.filename, "netring: capture retention evicted oldest");
    }
}

/// Unregister blobs whose serve TTL elapsed (the file stays until retention
/// evicts it); clear the download affordance from the served index.
async fn reap_expired_artifacts(
    retained: &mut Vec<RetainedFile>,
    blob: &Option<BlobServer>,
    index: &CaptureIndex,
    stats: &CaptureDiskStats,
) {
    let now = tokio::time::Instant::now();
    for f in retained.iter_mut() {
        if let (Some(exp), Some(id)) = (f.expires, f.artifact_id.clone())
            && exp <= now
        {
            if let Some(blob) = blob {
                blob.unregister(&id).await;
            }
            f.artifact_id = None;
            f.expires = None;
            if let Ok(mut idx) = index.lock() {
                for r in idx.iter_mut().filter(|r| r.filename == f.filename) {
                    r.artifact_id = None;
                    r.expires_ms = None;
                }
            }
            stats.note(format!("capture artifact expired: {}", f.filename));
        }
    }
}

/// Refresh the shared occupancy/retention counters from the live state.
fn refresh_stats(state: &WriterState, stats: &CaptureDiskStats, retained: &[RetainedFile]) {
    match state {
        WriterState::Armed(ring) => {
            stats
                .ring_packets
                .store(ring.len() as u64, Ordering::Relaxed);
            stats
                .ring_bytes
                .store(ring.bytes() as u64, Ordering::Relaxed);
        }
        _ => {
            stats.ring_packets.store(0, Ordering::Relaxed);
            stats.ring_bytes.store(0, Ordering::Relaxed);
        }
    }
    match state {
        WriterState::Rotating(w) => {
            let paths = w.file_paths();
            let bytes: u64 = paths
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .sum();
            stats
                .retained_files
                .store(paths.len() as u64, Ordering::Relaxed);
            stats.retained_bytes.store(bytes, Ordering::Relaxed);
        }
        _ => {
            let bytes: u64 = retained.iter().map(|f| f.bytes).sum();
            stats
                .retained_files
                .store(retained.len() as u64, Ordering::Relaxed);
            stats.retained_bytes.store(bytes, Ordering::Relaxed);
        }
    }
}

/// Rebuild the served index from the spool's on-disk files (rotating mode —
/// there is no trigger metadata, just the listing).
fn refresh_spool_index(w: &RotatingPcapWriter, index: &CaptureIndex) {
    let records: VecDeque<CaptureRecord> = w
        .file_paths()
        .iter()
        .rev() // newest first
        .map(|p| CaptureRecord {
            filename: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            bytes: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            mode: "rotating".to_string(),
            ..CaptureRecord::default()
        })
        .collect();
    if let Ok(mut idx) = index.lock() {
        *idx = records;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::config::CaptureDiskMode;

    fn cfg(mode: CaptureDiskMode, dir: &Path) -> CaptureToDiskConfig {
        CaptureToDiskConfig {
            mode,
            dir: Some(dir.to_string_lossy().into_owned()),
            ring_bytes: 1024 * 1024,
            max_file_bytes: 1024 * 1024,
            post_trigger_secs: 1,
            compress: false,
            ..CaptureToDiskConfig::default()
        }
    }

    fn frame(n: usize, secs: u32) -> DiskFrame {
        DiskFrame {
            data: vec![0xAB; n],
            ts: Timestamp::new(secs, 0),
            original_len: n,
        }
    }

    #[test]
    fn should_trigger_severity_floor_and_allowlist() {
        let mut c = CaptureToDiskConfig::default();
        // Default floor is warning: info is gated, warning/critical pass.
        assert!(!should_trigger(&c, "RitaBeacon", AlertSeverity::Info));
        assert!(should_trigger(&c, "RitaBeacon", AlertSeverity::Warning));
        assert!(should_trigger(&c, "RitaBeacon", AlertSeverity::Critical));
        // Allowlist narrows by detector slug.
        c.trigger_kinds = vec!["DataExfiltration".into()];
        assert!(!should_trigger(&c, "RitaBeacon", AlertSeverity::Critical));
        assert!(should_trigger(
            &c,
            "DataExfiltration",
            AlertSeverity::Warning
        ));
    }

    #[test]
    fn pre_ring_evicts_oldest_and_tracks_bytes() {
        let mut ring = PreRing::new(100);
        for i in 0..10u32 {
            ring.push(vec![i as u8; 40], Timestamp::new(i, 0), 40);
        }
        assert!(ring.len() <= 3, "40B frames under a 100B cap");
        assert!(ring.bytes() <= 120);
        // Newest survives eviction.
        assert_eq!(ring.ring.back().unwrap().0[0], 9);
    }

    #[test]
    fn stats_mode_roundtrips() {
        let stats = CaptureDiskStats::new(CaptureDiskMode::Triggered);
        assert_eq!(stats.mode(), CaptureDiskMode::Triggered);
        stats.set_mode(CaptureDiskMode::Rotating);
        assert_eq!(stats.mode(), CaptureDiskMode::Rotating);
        stats.set_mode(CaptureDiskMode::Off);
        assert_eq!(stats.mode(), CaptureDiskMode::Off);
    }

    #[test]
    fn handle_trigger_gated_by_live_mode() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stats = Arc::new(CaptureDiskStats::new(CaptureDiskMode::Rotating));
        let handle = CaptureDiskHandle::new(tx, stats.clone());
        // Anomaly-path trigger is a no-op outside triggered mode…
        handle.trigger("RitaBeacon", None);
        assert!(rx.try_recv().is_err());
        // …but fires once the live mode is triggered.
        stats.set_mode(CaptureDiskMode::Triggered);
        handle.trigger("RitaBeacon", None);
        assert!(matches!(
            rx.try_recv(),
            Ok(DiskCtl::Trigger { ref kind, .. }) if kind == "RitaBeacon"
        ));
        // capture_now always reaches the engine (it dispatches on live mode).
        stats.set_mode(CaptureDiskMode::Rotating);
        handle.capture_now(Some("incident-42".into()));
        assert!(
            matches!(rx.try_recv(), Ok(DiskCtl::Trigger { ref kind, ref tag })
            if kind == "manual" && tag.as_deref() == Some("incident-42"))
        );
    }

    /// End-to-end triggered flow: pre-trigger frames buffer in the ring, the
    /// trigger flushes lead-up + aftermath into a pcap, the index records it.
    #[tokio::test(start_paused = true)]
    async fn triggered_capture_writes_pre_and_post_trigger_packets() {
        let dir = std::env::temp_dir().join(format!("ztest-disk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = cfg(CaptureDiskMode::Triggered, &dir);
        let source = "host01".to_string();
        let (frames_tx, frames_rx) = mpsc::channel(64);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let stats = Arc::new(CaptureDiskStats::new(CaptureDiskMode::Triggered));
        let index: CaptureIndex = Arc::new(Mutex::new(VecDeque::new()));
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();

        let engine = tokio::spawn(run_engine(
            cfg,
            source,
            frames_rx,
            ctl_rx,
            stats.clone(),
            index.clone(),
            None, // no blob server in unit tests
            ev_tx,
        ));

        // Three pre-trigger frames buffer in the ring.
        for i in 0..3u32 {
            frames_tx.send(frame(64, i)).await.unwrap();
        }
        tokio::task::yield_now().await;
        let handle = CaptureDiskHandle::new(ctl_tx.clone(), stats.clone());
        handle.trigger("RitaBeacon", None);
        tokio::task::yield_now().await;
        // Two post-trigger frames stream straight through.
        for i in 10..12u32 {
            frames_tx.send(frame(64, i)).await.unwrap();
        }
        tokio::task::yield_now().await;
        // Let the post-trigger window elapse (paused clock: advance passes the
        // deadline; the 500 ms tick then finalizes).
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        // Closing the frame channel shuts the engine down cleanly.
        drop(frames_tx);
        engine.await.unwrap();

        let idx = index.lock().unwrap();
        assert_eq!(idx.len(), 1, "one capture record expected");
        let rec = &idx[0];
        assert_eq!(rec.mode, "triggered");
        assert_eq!(rec.trigger_kind.as_deref(), Some("RitaBeacon"));
        assert_eq!(rec.packets, 5, "3 pre-trigger + 2 post-trigger");
        assert!(!rec.truncated);
        assert!(rec.artifact_id.is_none(), "no blob server armed");
        assert_eq!(stats.triggers.load(Ordering::Relaxed), 1);

        // The file exists, is a valid pcap, and holds all 5 records.
        let path = dir.join(&rec.filename);
        let bytes = std::fs::read(&path).expect("capture file on disk");
        let mut reader = pcap_file::pcap::PcapReader::new(&bytes[..]).expect("valid pcap");
        let mut count = 0;
        while let Some(pkt) = reader.next_packet() {
            pkt.expect("record parses");
            count += 1;
        }
        assert_eq!(count, 5);

        // Lifecycle events surfaced: trigger fired, then capture ready.
        let mut events = Vec::new();
        while let Ok(p) = ev_rx.try_recv() {
            events.push(p.labels.get("event").cloned().unwrap_or_default());
        }
        assert!(events.contains(&"trigger".to_string()));
        assert!(events.contains(&"ready".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Retention: oldest triggered file is evicted past `max_files`.
    #[tokio::test(start_paused = true)]
    async fn retention_evicts_oldest_triggered_file() {
        let dir = std::env::temp_dir().join(format!("ztest-disk-ret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = cfg(CaptureDiskMode::Triggered, &dir);
        c.max_files = 2;
        let (frames_tx, frames_rx) = mpsc::channel(64);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let stats = Arc::new(CaptureDiskStats::new(CaptureDiskMode::Triggered));
        let index: CaptureIndex = Arc::new(Mutex::new(VecDeque::new()));
        let (ev_tx, _ev_rx) = mpsc::unbounded_channel();

        let engine = tokio::spawn(run_engine(
            c,
            "host01".to_string(),
            frames_rx,
            ctl_rx,
            stats.clone(),
            index.clone(),
            None,
            ev_tx,
        ));

        let handle = CaptureDiskHandle::new(ctl_tx.clone(), stats.clone());
        for round in 0..3u32 {
            frames_tx.send(frame(64, round)).await.unwrap();
            tokio::task::yield_now().await;
            handle.trigger("PortScanTRW", None);
            tokio::task::yield_now().await;
            frames_tx.send(frame(64, round + 100)).await.unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(3)).await;
            tokio::task::yield_now().await;
        }
        drop(frames_tx);
        engine.await.unwrap();

        assert_eq!(stats.evictions.load(Ordering::Relaxed), 1, "3 files, cap 2");
        let on_disk = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(on_disk, 2);
        assert_eq!(index.lock().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
