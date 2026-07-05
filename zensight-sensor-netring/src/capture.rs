//! On-demand packet capture over the unified `@/artifact` channel (#333).
//!
//! Two pieces live here:
//!
//! * [`CaptureTap`] — a lock-free, clonable slot the monitor's dedicated
//!   packet-tier subscription forwards frames into. When disarmed (the idle
//!   state) `offer` is a single atomic load and does nothing; when armed by an
//!   in-flight capture it copies each frame into a bounded channel, **dropping**
//!   (never blocking) under burst so a lossy capture never starves global
//!   telemetry (risk 2.7).
//! * [`CaptureProducer`] — an [`ArtifactProducer`] that, on request, arms the
//!   tap, optionally narrows the tap's live filter, streams frames to a
//!   `pcap[.zst]` file for the clamped duration/byte budget, and hands the file
//!   back to the channel for Tier-1 blob delivery.
//!
//! **Known limitation**: the packet tier only sees frames that extract to a
//! 5-tuple — ARP/LLDP/other non-IP frames match no packet subscription, so
//! on-demand captures contain IP/L4 traffic only.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use tokio::sync::mpsc;
use zensight_common::CommonArtifactLimits;
use zensight_common::artifact::{ArtifactKind, KindAdvert};
use zensight_sensor_core::artifact::{
    ArtifactProducer, DeliveryKind, ProduceCtx, Produced, ProgressUpdate,
};

use crate::config::CaptureOnDemandConfig;

/// Bounded capacity of the tap→producer channel (frames). ~4096 full frames is
/// ~6 MiB worst case; over that the tap drops and counts (see [`CaptureTap`]).
const TAP_CHANNEL_CAP: usize = 4096;

/// The permissive packet-filter base the on-demand tap subscription is built
/// with. It must be at least as wide as any per-request filter (which only ever
/// *narrows* it) so the kernel cBPF/XDP prefilter union covers live captures.
pub const CAPTURE_TAP_BASE_EXPR: &str = "tcp or udp or icmp";

/// One frame lifted off the wire by the tap, en route to the pcap writer.
pub struct TapFrame {
    /// The raw ethernet frame bytes.
    pub data: Vec<u8>,
    /// Capture timestamp (flowscope re-export; feeds `write_raw` directly).
    pub ts: netring::packet::Timestamp,
    /// The on-wire frame length (kept as the pcap record's `orig_len` even when
    /// `snaplen` truncates the captured bytes).
    pub original_len: usize,
}

/// A lock-free hand-off from the monitor's packet handler to an in-flight
/// capture. Cloning shares the underlying slot + drop counter.
#[derive(Clone, Default)]
pub struct CaptureTap {
    slot: Arc<ArcSwapOption<mpsc::Sender<TapFrame>>>,
    dropped: Arc<AtomicU64>,
}

impl CaptureTap {
    /// Arm the tap: subsequent `offer`s copy frames into `tx`. Resets the drop
    /// counter so it measures this capture only.
    pub fn arm(&self, tx: mpsc::Sender<TapFrame>) {
        self.dropped.store(0, Ordering::Relaxed);
        self.slot.store(Some(Arc::new(tx)));
    }

    /// Disarm the tap: `offer` becomes a no-op again (idle state).
    pub fn disarm(&self) {
        self.slot.store(None);
    }

    /// Frames dropped since the last `arm` because the channel was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Offer a frame to the armed capture, if any. Called from the packet
    /// handler on the capture hot loop — MUST NOT block, so a full channel
    /// drops the frame and bumps the counter.
    pub fn offer(&self, pkt: &flowscope::PacketView<'_>) {
        if let Some(tx) = self.slot.load_full() {
            let frame = TapFrame {
                data: pkt.frame.to_vec(),
                ts: pkt.timestamp,
                original_len: pkt.frame.len(),
            };
            if tx.try_send(frame).is_err() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// The clamped, request-independent capture budget derived from a request +
/// the producer's configured limits. Extracted as a pure value for testability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clamped {
    /// Effective capture duration, seconds.
    pub duration_secs: u32,
    /// Effective byte cap (early-stop → truncated).
    pub max_bytes: u64,
    /// Effective per-packet snap length, `None` ⇒ full frames.
    pub snaplen: Option<u32>,
    /// Whether any bound was actually reduced from what the request asked.
    pub clamped: bool,
}

/// Clamp a `Capture` request against the configured limits (never trust the
/// request). Over-limit duration/bytes/snaplen are accepted and reduced here.
pub fn clamp(kind: &ArtifactKind, limits: &CaptureOnDemandConfig) -> Clamped {
    let (duration_secs, max_bytes, snaplen) = match kind {
        ArtifactKind::Capture {
            duration_secs,
            max_bytes,
            snaplen,
            ..
        } => (*duration_secs, *max_bytes, *snaplen),
        _ => (0, None, None),
    };

    let mut clamped = false;

    let dur = if duration_secs > limits.max_duration_secs {
        clamped = true;
        limits.max_duration_secs
    } else {
        duration_secs
    };

    let req_bytes = max_bytes.unwrap_or(limits.max_bytes);
    let cap = if req_bytes > limits.max_bytes {
        clamped = true;
        limits.max_bytes
    } else {
        req_bytes
    };

    // snaplen_max == 0 means "full frames allowed" — a request snaplen passes
    // through untouched; otherwise clamp down and default an absent request to
    // the configured max.
    let snap = if limits.snaplen_max > 0 {
        match snaplen {
            Some(s) if s > limits.snaplen_max => {
                clamped = true;
                Some(limits.snaplen_max)
            }
            Some(s) => Some(s),
            None => Some(limits.snaplen_max),
        }
    } else {
        snaplen
    };

    Clamped {
        duration_secs: dur,
        max_bytes: cap,
        snaplen: snap,
        clamped,
    }
}

/// Validate + authorize a `Capture` request (pure; no live state). Clamping is
/// deferred to [`clamp`] in `produce` — this only rejects the unrecoverable:
/// wrong kind, zero duration, a filter when filters are disabled, or a filter
/// that does not parse.
pub fn validate_capture(kind: &ArtifactKind, allow_filter: bool) -> Result<(), String> {
    let ArtifactKind::Capture {
        duration_secs,
        filter,
        ..
    } = kind
    else {
        return Err("capture producer given a non-capture request".into());
    };
    if *duration_secs == 0 {
        return Err("capture duration must be > 0".into());
    }
    if let Some(expr) = filter {
        if !allow_filter {
            return Err("capture filters are disabled by config".into());
        }
        // Validate now so a bad expression fails fast (produce re-validates
        // before swapping the live filter).
        netring::monitor::subscription::parse_expr(expr)
            .map_err(|e| format!("invalid capture filter: {e}"))?;
    }
    Ok(())
}

/// The outcome of one tap→writer capture loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOutcome {
    /// Bytes written to the pcap file (records incl. headers, approx).
    pub written: u64,
    /// Packet records written.
    pub packets: u64,
    /// True if the byte cap stopped the capture before its duration elapsed.
    pub truncated: bool,
    /// Frames the tap dropped under burst during this capture.
    pub dropped: u64,
}

/// Approximate on-disk record size for the byte budget: a 16-byte pcap record
/// header + the captured (possibly snap-truncated) payload.
fn record_len(frame_len: usize, snaplen: Option<u32>) -> u64 {
    let cap = match snaplen {
        Some(s) if (s as usize) < frame_len => s as usize,
        _ => frame_len,
    };
    16 + cap as u64
}

/// Stream frames from `rx` into a fresh pcap file at `path` until the duration
/// deadline, the byte cap, or cancellation. Reload/tap-arming is the caller's
/// responsibility — this is the pure capture loop (unit-tested with synthetic
/// frames, no monitor/reload needed).
async fn capture_to_file(
    path: &std::path::Path,
    mut rx: mpsc::Receiver<TapFrame>,
    clamped: &Clamped,
    tap: &CaptureTap,
    ctx: &ProduceCtx,
) -> anyhow::Result<CaptureOutcome> {
    use std::fs::File;
    use std::io::BufWriter;

    let mut writer = netring::pcap::CaptureWriter::create(BufWriter::new(File::create(path)?))
        .map_err(|e| anyhow::anyhow!("pcap create: {e}"))?;

    let dur = clamped.duration_secs.max(1) as u64;
    let cap = clamped.max_bytes;
    let snap = clamped.snaplen;

    let start = tokio::time::Instant::now();
    let deadline = start + Duration::from_secs(dur);
    // Poll cadence for cancellation + progress emission (progress throttled to 1 s).
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_progress = 0u64;

    let mut written = 0u64;
    let mut packets = 0u64;
    let mut truncated = false;

    let emit_progress = |elapsed_s: u64, written: u64, packets: u64| {
        let dropped = tap.dropped();
        let drop_suffix = if dropped > 0 {
            format!(" · dropped {dropped}")
        } else {
            String::new()
        };
        let detail = format!(
            "capturing {elapsed_s}s/{dur}s · {:.1} MiB · {packets} pkts{drop_suffix}",
            written as f64 / (1024.0 * 1024.0),
        );
        let _ = ctx.progress.send(ProgressUpdate {
            detail: Some(detail),
            progress: Some((elapsed_s as f32 / dur as f32).min(1.0)),
        });
    };

    loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => break,
            maybe = rx.recv() => match maybe {
                Some(f) => {
                    writer
                        .write_raw(&f.data, f.ts, f.original_len, snap)
                        .map_err(|e| anyhow::anyhow!("pcap write: {e}"))?;
                    written += record_len(f.data.len(), snap);
                    packets += 1;
                    if written >= cap {
                        truncated = true;
                        break;
                    }
                }
                None => break, // tap gone (shouldn't happen while armed)
            },
            _ = tick.tick() => {
                if ctx.cancel.is_cancelled() {
                    anyhow::bail!("cancelled");
                }
                let elapsed_s = start.elapsed().as_secs();
                if elapsed_s != last_progress {
                    last_progress = elapsed_s;
                    emit_progress(elapsed_s, written, packets);
                }
            }
        }
    }

    // Flush + close the file before it is read back / compressed.
    drop(writer);
    Ok(CaptureOutcome {
        written,
        packets,
        truncated,
        dropped: tap.dropped(),
    })
}

/// zstd-compress `src` → `dst` (level 3), returning `dst`. Blocking; call under
/// `spawn_blocking`. Shared with the capture-to-disk engine (#327).
pub(crate) fn zstd_compress(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    use std::fs::File;
    use std::io::BufReader;
    let reader = BufReader::new(File::open(src)?);
    let writer = File::create(dst)?;
    zstd::stream::copy_encode(reader, writer, 3)?;
    Ok(())
}

/// RAII guard: on any exit from `produce` (return, error, cancel) it disarms the
/// tap and, if a per-request filter was installed, restores the permissive base
/// so the tap subscription is left as it was found.
struct TapGuard {
    tap: CaptureTap,
    reload: netring::monitor::ReloadHandle,
    tap_index: usize,
    base_expr: String,
    filter_applied: bool,
}

impl Drop for TapGuard {
    fn drop(&mut self) {
        self.tap.disarm();
        if self.filter_applied {
            // Known-good base expr; a swap failure is non-fatal (best-effort restore).
            let _ = self
                .reload
                .set_packet_filter(self.tap_index, &self.base_expr);
        }
    }
}

/// The on-demand packet-capture artifact producer (#333). Delivers a Tier-1
/// `pcap[.zst]` blob.
pub struct CaptureProducer {
    common: CommonArtifactLimits,
    limits: CaptureOnDemandConfig,
    tap: CaptureTap,
    reload: netring::monitor::ReloadHandle,
    tap_index: usize,
    source: String,
}

impl CaptureProducer {
    /// Build the producer from the on-demand config, the shared tap, the
    /// monitor's reload handle + the tap subscription's registration index, and
    /// the sensor source (used in the download filename).
    pub fn new(
        limits: &CaptureOnDemandConfig,
        tap: CaptureTap,
        reload: netring::monitor::ReloadHandle,
        tap_index: usize,
        source: impl Into<String>,
    ) -> Self {
        CaptureProducer {
            common: limits.common(),
            limits: limits.clone(),
            tap,
            reload,
            tap_index,
            source: source.into(),
        }
    }
}

#[async_trait]
impl ArtifactProducer for CaptureProducer {
    fn kind(&self) -> &'static str {
        "capture"
    }
    fn common(&self) -> &CommonArtifactLimits {
        &self.common
    }
    fn delivery_kind(&self) -> DeliveryKind {
        DeliveryKind::Blob
    }
    fn advert(&self) -> KindAdvert {
        KindAdvert::Capture {
            max_duration_secs: self.limits.max_duration_secs,
            filter_allowed: self.limits.allow_filter,
            snaplen_max: self.limits.snaplen_max,
        }
    }

    fn accepts(&self, kind: &ArtifactKind) -> Result<(), String> {
        validate_capture(kind, self.limits.allow_filter)
    }

    async fn produce(&self, kind: ArtifactKind, ctx: ProduceCtx) -> anyhow::Result<Produced> {
        let clamped = clamp(&kind, &self.limits);
        let filter = match &kind {
            ArtifactKind::Capture { filter, .. } => filter.clone(),
            _ => anyhow::bail!("capture producer given a non-capture request"),
        };

        // Arm the tap + install the RAII guard before any early return.
        let (tx, rx) = mpsc::channel::<TapFrame>(TAP_CHANNEL_CAP);
        self.tap.arm(tx);
        let mut guard = TapGuard {
            tap: self.tap.clone(),
            reload: self.reload.clone(),
            tap_index: self.tap_index,
            base_expr: CAPTURE_TAP_BASE_EXPR.to_string(),
            filter_applied: false,
        };

        // Narrow the live tap filter for this request (parse-before-swap: a bad
        // expr leaves the live filter untouched).
        if let Some(expr) = &filter {
            match self.reload.set_packet_filter(self.tap_index, expr) {
                Ok(true) => guard.filter_applied = true,
                Ok(false) => anyhow::bail!("capture tap not registered (index out of range)"),
                Err(e) => anyhow::bail!("invalid capture filter: {e}"),
            }
        }

        let created_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let raw_path = ctx.workdir.join(format!(
            "zensight-capture-{}-{created_ms}.pcap",
            self.source
        ));

        let outcome = capture_to_file(&raw_path, rx, &clamped, &self.tap, &ctx).await;

        // Restore the tap immediately; keep the file for finalize/compress.
        drop(guard);

        let outcome = match outcome {
            Ok(o) => o,
            Err(e) => {
                let _ = std::fs::remove_file(&raw_path);
                return Err(e);
            }
        };

        let compress = matches!(kind, ArtifactKind::Capture { compress: true, .. });

        // Final progress line carries the truncation / drop summary (the channel
        // republishes it as the last Generating detail).
        let mut summary = format!(
            "captured {} pkts · {:.1} MiB",
            outcome.packets,
            outcome.written as f64 / (1024.0 * 1024.0),
        );
        if outcome.truncated {
            summary.push_str(" · truncated (byte cap)");
        }
        if outcome.dropped > 0 {
            summary.push_str(&format!(" · dropped {}", outcome.dropped));
        }
        if clamped.clamped {
            summary.push_str(" · clamped to config limits");
        }
        let _ = ctx.progress.send(ProgressUpdate {
            detail: Some(summary),
            progress: Some(1.0),
        });

        let (path, filename) = if compress {
            let zst_path = ctx.workdir.join(format!(
                "zensight-capture-{}-{created_ms}.pcap.zst",
                self.source
            ));
            let (src, dst) = (raw_path.clone(), zst_path.clone());
            tokio::task::spawn_blocking(move || zstd_compress(&src, &dst)).await??;
            let _ = std::fs::remove_file(&raw_path);
            (
                zst_path,
                format!("capture-{}-{created_ms}.pcap.zst", self.source),
            )
        } else {
            (
                raw_path,
                format!("capture-{}-{created_ms}.pcap", self.source),
            )
        };

        Ok(Produced::File { path, filename })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use zenoh_blob::CancelToken;

    fn cfg(allow_filter: bool, snaplen_max: u32) -> CaptureOnDemandConfig {
        CaptureOnDemandConfig {
            enabled: true,
            max_duration_secs: 300,
            max_bytes: 256 * 1024 * 1024,
            snaplen_max,
            allow_filter,
            cooldown_secs: 60,
            ttl_secs: 600,
            chunk_size: 512 * 1024,
        }
    }

    fn capture_kind(
        duration_secs: u32,
        max_bytes: Option<u64>,
        filter: Option<&str>,
        snaplen: Option<u32>,
        compress: bool,
    ) -> ArtifactKind {
        ArtifactKind::Capture {
            duration_secs,
            max_bytes,
            filter: filter.map(String::from),
            snaplen,
            compress,
        }
    }

    /// A minimal ethernet + IPv4 + UDP frame (payload optional) for the tap.
    fn synthetic_udp_frame(payload: &[u8]) -> Vec<u8> {
        let mut f = Vec::new();
        // Ethernet: dst, src, ethertype IPv4.
        f.extend_from_slice(&[0x02, 0, 0, 0, 0, 1]);
        f.extend_from_slice(&[0x02, 0, 0, 0, 0, 2]);
        f.extend_from_slice(&[0x08, 0x00]);
        // IPv4 header (20 bytes), proto 17 (UDP).
        let total_len = (20 + 8 + payload.len()) as u16;
        f.extend_from_slice(&[0x45, 0x00]);
        f.extend_from_slice(&total_len.to_be_bytes());
        f.extend_from_slice(&[0, 0, 0, 0]); // id + frag
        f.extend_from_slice(&[64, 17]); // ttl, proto=UDP
        f.extend_from_slice(&[0, 0]); // checksum (unchecked)
        f.extend_from_slice(&[10, 0, 0, 1]); // src ip
        f.extend_from_slice(&[10, 0, 0, 2]); // dst ip
        // UDP header (8 bytes).
        f.extend_from_slice(&53u16.to_be_bytes()); // src port
        f.extend_from_slice(&53u16.to_be_bytes()); // dst port
        f.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        f.extend_from_slice(&[0, 0]); // checksum
        f.extend_from_slice(payload);
        f
    }

    fn produce_ctx() -> (ProduceCtx, CancelToken) {
        let cancel = CancelToken::new();
        let (progress, _rx) = tokio::sync::watch::channel(ProgressUpdate::default());
        (
            ProduceCtx {
                workdir: std::env::temp_dir(),
                cancel: cancel.clone(),
                progress,
            },
            cancel,
        )
    }

    #[test]
    fn clamp_over_limit_reduces() {
        let limits = cfg(true, 128);
        let c = clamp(
            &capture_kind(9999, Some(1 << 40), None, Some(9999), false),
            &limits,
        );
        assert_eq!(c.duration_secs, 300);
        assert_eq!(c.max_bytes, 256 * 1024 * 1024);
        assert_eq!(c.snaplen, Some(128));
        assert!(c.clamped);
    }

    #[test]
    fn clamp_within_limits_untouched() {
        let limits = cfg(true, 0);
        let c = clamp(
            &capture_kind(30, Some(1 << 20), None, Some(96), false),
            &limits,
        );
        assert_eq!(c.duration_secs, 30);
        assert_eq!(c.max_bytes, 1 << 20);
        assert_eq!(c.snaplen, Some(96)); // snaplen_max=0 ⇒ passthrough
        assert!(!c.clamped);
    }

    #[test]
    fn clamp_snaplen_max_defaults_absent_request() {
        let limits = cfg(true, 256);
        let c = clamp(&capture_kind(30, None, None, None, false), &limits);
        assert_eq!(c.snaplen, Some(256));
    }

    #[test]
    fn accepts_matrix() {
        // ok
        assert!(validate_capture(&capture_kind(30, None, Some("udp"), None, false), true).is_ok());
        // zero duration
        assert!(validate_capture(&capture_kind(0, None, None, None, false), true).is_err());
        // wrong kind
        assert!(validate_capture(&ArtifactKind::Report {}, true).is_err());
        // unparseable filter — netring's message flows through
        let err = validate_capture(
            &capture_kind(30, None, Some("!!bogus!!"), None, false),
            true,
        )
        .unwrap_err();
        assert!(err.contains("invalid capture filter"), "got: {err}");
        // filter when disallowed
        assert!(
            validate_capture(&capture_kind(30, None, Some("udp"), None, false), false)
                .unwrap_err()
                .contains("disabled")
        );
    }

    #[tokio::test]
    async fn tap_to_pcap_roundtrip() {
        let tap = CaptureTap::default();
        let (tx, rx) = mpsc::channel::<TapFrame>(64);
        tap.arm(tx);

        // Feed frames through the real offer() path (as the monitor would).
        for i in 0..5u8 {
            let frame = synthetic_udp_frame(&[i; 20]);
            let pv = flowscope::PacketView::new(
                &frame,
                netring::packet::Timestamp::new(100 + i as u32, 0),
            );
            tap.offer(&pv);
        }

        let (ctx, _cancel) = produce_ctx();
        let clamped = Clamped {
            duration_secs: 1,
            max_bytes: 1 << 30,
            snaplen: None,
            clamped: false,
        };
        let path = std::env::temp_dir().join(format!("ztest-cap-{}.pcap", std::process::id()));
        let outcome = capture_to_file(&path, rx, &clamped, &tap, &ctx)
            .await
            .expect("capture loop ok");
        assert_eq!(outcome.packets, 5);
        assert!(!outcome.truncated);

        // Pin pcap magic + record count (the "wireshark-openable" check).
        let mut bytes = Vec::new();
        std::fs::File::open(&path)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert!(
            magic == 0xa1b2_c3d4 || magic == 0xd4c3_b2a1 || magic == 0xa1b2_3c4d,
            "unexpected pcap magic {magic:#x}"
        );
        let mut reader = pcap_file::pcap::PcapReader::new(&bytes[..]).expect("valid pcap");
        let mut count = 0;
        while let Some(pkt) = reader.next_packet() {
            pkt.expect("record parses");
            count += 1;
        }
        assert_eq!(count, 5);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn byte_cap_truncates() {
        let tap = CaptureTap::default();
        let (tx, rx) = mpsc::channel::<TapFrame>(64);
        tap.arm(tx.clone());
        for _ in 0..10 {
            let frame = synthetic_udp_frame(&[0xAB; 100]);
            let pv = flowscope::PacketView::new(&frame, netring::packet::Timestamp::new(1, 0));
            tap.offer(&pv);
        }
        let (ctx, _cancel) = produce_ctx();
        // Cap after ~3 records (each ~ 16 + 142 bytes).
        let clamped = Clamped {
            duration_secs: 30,
            max_bytes: 300,
            snaplen: None,
            clamped: false,
        };
        let path = std::env::temp_dir().join(format!("ztest-trunc-{}.pcap", std::process::id()));
        let outcome = capture_to_file(&path, rx, &clamped, &tap, &ctx)
            .await
            .unwrap();
        assert!(outcome.truncated);
        assert!(outcome.packets >= 1 && outcome.packets < 10);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn cancel_stops_and_errs() {
        let tap = CaptureTap::default();
        let (tx, rx) = mpsc::channel::<TapFrame>(4);
        tap.arm(tx);
        let (ctx, cancel) = produce_ctx();
        cancel.cancel(); // pre-cancelled
        let clamped = Clamped {
            duration_secs: 30,
            max_bytes: 1 << 30,
            snaplen: None,
            clamped: false,
        };
        let path = std::env::temp_dir().join(format!("ztest-cancel-{}.pcap", std::process::id()));
        let err = capture_to_file(&path, rx, &clamped, &tap, &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cancelled"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dropped_counts_when_channel_full() {
        let tap = CaptureTap::default();
        let (tx, _rx) = mpsc::channel::<TapFrame>(1);
        tap.arm(tx);
        // First fits, rest are dropped (receiver never drains).
        for _ in 0..5 {
            let frame = synthetic_udp_frame(&[0; 4]);
            let pv = flowscope::PacketView::new(&frame, netring::packet::Timestamp::new(1, 0));
            tap.offer(&pv);
        }
        assert_eq!(tap.dropped(), 4);
        // Disarm ⇒ offer is a no-op, counter unchanged.
        tap.disarm();
        let frame = synthetic_udp_frame(&[0; 4]);
        let pv = flowscope::PacketView::new(&frame, netring::packet::Timestamp::new(1, 0));
        tap.offer(&pv);
        assert_eq!(tap.dropped(), 4);
    }

    #[tokio::test]
    async fn compress_roundtrip() {
        // Build a small pcap, compress it, decode, and compare bytes.
        let tap = CaptureTap::default();
        let (tx, rx) = mpsc::channel::<TapFrame>(16);
        tap.arm(tx);
        for i in 0..3u8 {
            let frame = synthetic_udp_frame(&[i; 10]);
            let pv = flowscope::PacketView::new(&frame, netring::packet::Timestamp::new(1, 0));
            tap.offer(&pv);
        }
        let (ctx, _cancel) = produce_ctx();
        let clamped = Clamped {
            duration_secs: 1,
            max_bytes: 1 << 30,
            snaplen: None,
            clamped: false,
        };
        let raw = std::env::temp_dir().join(format!("ztest-zraw-{}.pcap", std::process::id()));
        capture_to_file(&raw, rx, &clamped, &tap, &ctx)
            .await
            .unwrap();
        let mut raw_bytes = Vec::new();
        std::fs::File::open(&raw)
            .unwrap()
            .read_to_end(&mut raw_bytes)
            .unwrap();

        let zst = std::env::temp_dir().join(format!("ztest-zraw-{}.pcap.zst", std::process::id()));
        zstd_compress(&raw, &zst).unwrap();
        let decoded = zstd::stream::decode_all(std::fs::File::open(&zst).unwrap()).unwrap();
        assert_eq!(decoded, raw_bytes);
        let _ = std::fs::remove_file(&raw);
        let _ = std::fs::remove_file(&zst);
    }
}
