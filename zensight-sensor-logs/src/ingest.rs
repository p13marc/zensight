//! Shared network-ingest robustness layer (#106).
//!
//! The journald source (#62) has rate-limiting, drop-accounting and a loss
//! health alert; the UDP/TCP/Unix network paths historically had **none** — UDP
//! drops and parse failures were silent. This module lifts that pattern into a
//! source-neutral layer the network listeners share:
//!
//! - [`IngestStats`] — atomic `received` / `parsed` / `parse_failed` / `dropped`
//!   counters, mirroring [`crate::receiver::JournaldStats`], snapshotted for the
//!   `logs/ingest/*_total` telemetry and the loss health alert.
//! - [`TokenBucket`] / [`SharedRateLimiter`] — the journald token-bucket rate
//!   limiter, made shareable across the per-connection tasks (TCP/Unix spawn one
//!   task per connection, so the bucket lives behind a `Mutex`).
//! - [`take_frame`] — pure RFC 6587 framing: octet-counted (`MSG-LEN SP MSG`)
//!   with LF fallback, auto-detected per frame. Factored out of socket I/O so
//!   the framing/decision logic is unit-testable; [`FrameReader`] wraps it with
//!   the async read loop.
//!
//! ## RFC 5425 (syslog-over-TLS) — deferred
//!
//! Transport-layer security (RFC 5425) is intentionally **not** implemented here.
//! It would require pulling in a TLS stack (`tokio-rustls` + cert/key plumbing),
//! a non-trivial new dependency for this crate. Rather than half-add it behind a
//! config stub that silently does nothing, it is deferred: the octet-counted
//! framing a TLS listener would reuse is implemented and tested here, so adding
//! TLS later is a transport wrapper around the same [`FrameReader`].

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
use tokio::time::timeout;
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use crate::config::{Framing, OverflowPolicy};
use crate::receiver::ReceivedMessage;

/// Network-ingest throughput + loss accounting (#106), shared between the
/// listener tasks and the async health/telemetry monitor. Plain relaxed atomics
/// (monotonic counters read for periodic snapshots, no cross-counter invariant),
/// mirroring [`crate::receiver::JournaldStats`].
#[derive(Debug, Default)]
pub struct IngestStats {
    /// Frames/datagrams received off the wire (pre-parse).
    pub received: AtomicU64,
    /// Frames that parsed into a syslog message.
    pub parsed: AtomicU64,
    /// Frames we failed to parse (malformed — counted, not silent).
    pub parse_failed: AtomicU64,
    /// Parsed messages dropped: shed by the rate limiter or because the
    /// telemetry channel was full (`drop_newest`).
    pub dropped: AtomicU64,
}

impl IngestStats {
    /// Increment a counter (relaxed). Mirrors `JournaldStats::inc`.
    pub fn inc(field: &AtomicU64) {
        field.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the counters for a point-in-time read (health / telemetry).
    pub fn snapshot(&self) -> IngestStatsSnapshot {
        IngestStatsSnapshot {
            received: self.received.load(Ordering::Relaxed),
            parsed: self.parsed.load(Ordering::Relaxed),
            parse_failed: self.parse_failed.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

/// A plain (non-atomic) copy of [`IngestStats`] for a point-in-time read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestStatsSnapshot {
    pub received: u64,
    pub parsed: u64,
    pub parse_failed: u64,
    pub dropped: u64,
}

impl IngestStatsSnapshot {
    /// Fraction of received frames dropped over the delta since `prev`. `0.0`
    /// when nothing was received in the window (idle → not lossy, never NaN).
    pub fn loss_ratio_since(&self, prev: &IngestStatsSnapshot) -> f64 {
        let received = self.received.saturating_sub(prev.received);
        if received == 0 {
            return 0.0;
        }
        let dropped = self.dropped.saturating_sub(prev.dropped);
        dropped as f64 / received as f64
    }

    /// The four `logs/ingest/*_total` counters as telemetry points.
    pub fn to_points(self, source: &str) -> Vec<TelemetryPoint> {
        let counter = |metric: &str, v: u64| {
            TelemetryPoint::new(source, Protocol::Logs, metric, TelemetryValue::Counter(v))
        };
        vec![
            counter("logs/ingest/received_total", self.received),
            counter("logs/ingest/parsed_total", self.parsed),
            counter("logs/ingest/parse_failed_total", self.parse_failed),
            counter("logs/ingest/dropped_total", self.dropped),
        ]
    }
}

/// Global token-bucket rate limiter over a 1-second window (#106), lifted from
/// the journald reader. Beyond `max_eps`, keeps 1-in-`sample_ratio` over-budget
/// entries and sheds the rest. Pure (advance the window with the caller's
/// `now`), so the shedding logic is unit-testable; [`SharedRateLimiter`] wraps
/// it for cross-task sharing.
#[derive(Debug)]
pub struct TokenBucket {
    max_eps: Option<u64>,
    sample_ratio: u64,
    window_start: Instant,
    in_window: u64,
    over_budget: u64,
}

impl TokenBucket {
    /// `max_eps == None` means unlimited (the limiter is a no-op). `sample_ratio`
    /// is clamped to ≥1.
    pub fn new(max_eps: Option<u64>, sample_ratio: u64, now: Instant) -> Self {
        Self {
            max_eps,
            sample_ratio: sample_ratio.max(1),
            window_start: now,
            in_window: 0,
            over_budget: 0,
        }
    }

    /// `true` to forward this entry, `false` to shed it (rate-limited).
    pub fn allow(&mut self, now: Instant) -> bool {
        let Some(max) = self.max_eps else {
            return true;
        };
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.in_window = 0;
            self.over_budget = 0;
        }
        self.in_window += 1;
        if self.in_window <= max {
            return true;
        }
        // Over budget: keep 1-in-N, shed the rest.
        self.over_budget += 1;
        self.over_budget.is_multiple_of(self.sample_ratio)
    }
}

/// A [`TokenBucket`] shared across the per-connection listener tasks. The bucket
/// is tiny and the critical section is a few integer ops, so a `Mutex` is ample.
#[derive(Debug)]
pub struct SharedRateLimiter(Mutex<TokenBucket>);

impl SharedRateLimiter {
    pub fn new(max_eps: Option<u64>, sample_ratio: u64, now: Instant) -> Self {
        Self(Mutex::new(TokenBucket::new(max_eps, sample_ratio, now)))
    }

    /// `true` to forward, `false` to shed. Poison-safe: a poisoned lock still
    /// yields the inner bucket (we never panic while holding it).
    pub fn allow(&self, now: Instant) -> bool {
        let mut bucket = self.0.lock().unwrap_or_else(|p| p.into_inner());
        bucket.allow(now)
    }
}

/// Outcome of trying to extract one frame from the front of a stream buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum TakeResult {
    /// A complete frame (framing stripped); consume `consumed` bytes from the
    /// front of the buffer.
    Frame { message: Vec<u8>, consumed: usize },
    /// Not enough bytes buffered yet to extract a frame; read more.
    Incomplete,
}

/// Try to extract one syslog frame from the front of `buf` per `mode` (#106).
///
/// Pure (no I/O) so the framing + auto-detect is unit-testable. RFC 6587 defines
/// two TCP transports:
/// - **octet-counting**: `MSG-LEN SP MSG`, where `MSG-LEN` is the decimal byte
///   length of `MSG`. Robust under embedded newlines.
/// - **non-transparent-framing**: messages separated by a trailing `LF`.
///
/// In [`Framing::Auto`] the first non-space byte decides per frame: a digit ⇒
/// octet-counting, otherwise LF. A malformed octet length (no terminating space,
/// non-numeric, or longer than `max_frame_len`) falls back to LF parsing of the
/// same bytes so a bad sender can't wedge the stream.
pub fn take_frame(buf: &[u8], mode: Framing, max_frame_len: usize) -> TakeResult {
    if buf.is_empty() {
        return TakeResult::Incomplete;
    }
    let octet = match mode {
        Framing::Octet => true,
        Framing::Lf => false,
        Framing::Auto => match buf.iter().find(|&&b| b != b' ') {
            Some(&b) => b.is_ascii_digit(),
            // Only spaces buffered so far — can't decide yet.
            None => return TakeResult::Incomplete,
        },
    };
    if octet {
        take_octet(buf, max_frame_len)
    } else {
        take_lf(buf)
    }
}

/// RFC 6587 non-transparent (LF-delimited) framing.
fn take_lf(buf: &[u8]) -> TakeResult {
    match buf.iter().position(|&b| b == b'\n') {
        Some(nl) => {
            let mut line = &buf[..nl];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            TakeResult::Frame {
                message: line.to_vec(),
                consumed: nl + 1,
            }
        }
        None => TakeResult::Incomplete,
    }
}

/// RFC 6587 octet-counting framing: `MSG-LEN SP MSG`. Falls back to [`take_lf`]
/// on a malformed length so a bad frame degrades gracefully instead of wedging.
fn take_octet(buf: &[u8], max_frame_len: usize) -> TakeResult {
    // Tolerate leading spaces (some senders pad before MSG-LEN).
    let mut i = 0;
    while i < buf.len() && buf[i] == b' ' {
        i += 1;
    }
    let digits_start = i;
    while i < buf.len() && buf[i].is_ascii_digit() {
        i += 1;
    }
    let digits = &buf[digits_start..i];
    if digits.is_empty() {
        // Not actually octet-counted — degrade to LF.
        return take_lf(buf);
    }
    if i >= buf.len() {
        // Length digits not yet terminated; more may be coming.
        return TakeResult::Incomplete;
    }
    if buf[i] != b' ' {
        // Digits not followed by the required space ⇒ malformed; fall back.
        return take_lf(buf);
    }
    // Parse the length; reject absurd values (overflow / over the cap).
    let len: usize = match std::str::from_utf8(digits)
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(n) if n <= max_frame_len => n,
        _ => return take_lf(buf),
    };
    let msg_start = i + 1;
    let msg_end = match msg_start.checked_add(len) {
        Some(e) => e,
        None => return take_lf(buf),
    };
    if buf.len() < msg_end {
        // Wait for the full declared payload.
        return TakeResult::Incomplete;
    }
    TakeResult::Frame {
        message: buf[msg_start..msg_end].to_vec(),
        consumed: msg_end,
    }
}

/// Streaming frame reader over an async byte source (TCP/Unix). Wraps the pure
/// [`take_frame`] with the read loop + a bounded accumulator so a sender that
/// never completes a frame can't grow memory without bound.
pub struct FrameReader {
    framing: Framing,
    max_frame_len: usize,
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new(framing: Framing, max_frame_len: usize) -> Self {
        Self {
            framing,
            max_frame_len,
            buf: Vec::with_capacity(4096),
        }
    }

    /// Read the next complete frame, or `Ok(None)` at end of stream. Applies
    /// `read_timeout` to each socket read (matching the old per-line timeout).
    pub async fn next_frame<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        read_timeout: Duration,
    ) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            match take_frame(&self.buf, self.framing, self.max_frame_len) {
                TakeResult::Frame { message, consumed } => {
                    self.buf.drain(..consumed);
                    return Ok(Some(message));
                }
                TakeResult::Incomplete => {
                    // Guard against an unbounded buffer: a frame that never
                    // completes within the cap is rejected (connection closed).
                    if self.buf.len() > self.max_frame_len {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "syslog frame exceeds max_message_size without terminating",
                        ));
                    }
                    let mut chunk = [0u8; 8192];
                    let n = timeout(read_timeout, reader.read(&mut chunk))
                        .await
                        .map_err(|_| {
                            std::io::Error::new(std::io::ErrorKind::TimedOut, "read timed out")
                        })??;
                    if n == 0 {
                        // EOF. Emit any buffered remainder as a final frame so a
                        // last line without a trailing newline isn't lost (the
                        // old `lines()` reader did the same).
                        if self.buf.is_empty() {
                            return Ok(None);
                        }
                        let rest = std::mem::take(&mut self.buf);
                        let rest = rest.strip_suffix(b"\r").map(<[u8]>::to_vec).unwrap_or(rest);
                        return Ok(Some(rest));
                    }
                    self.buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
    }
}

/// Forward a parsed message downstream, applying the shared rate limiter and the
/// overflow policy, updating `stats` (#106). Returns `false` only when the
/// telemetry channel has closed (caller should stop); a rate-limited or
/// channel-full drop returns `true` (counted, keep going).
pub async fn forward_parsed(
    received: ReceivedMessage,
    tx: &mpsc::Sender<ReceivedMessage>,
    stats: &IngestStats,
    limiter: &SharedRateLimiter,
    overflow: OverflowPolicy,
) -> bool {
    if !limiter.allow(Instant::now()) {
        IngestStats::inc(&stats.dropped);
        return true;
    }
    match overflow {
        OverflowPolicy::Block => tx.send(received).await.is_ok(),
        OverflowPolicy::DropNewest => match tx.try_send(received) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                IngestStats::inc(&stats.dropped);
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receiver::MessageSource;

    // ---- rate limiter --------------------------------------------------------

    #[test]
    fn token_bucket_unlimited_when_no_max() {
        let now = Instant::now();
        let mut tb = TokenBucket::new(None, 100, now);
        for _ in 0..10_000 {
            assert!(tb.allow(now));
        }
    }

    #[test]
    fn token_bucket_samples_over_budget() {
        let now = Instant::now();
        let mut tb = TokenBucket::new(Some(2), 5, now);
        // First 2 are within budget.
        assert!(tb.allow(now));
        assert!(tb.allow(now));
        // Of the next 10 over-budget entries, keep 1-in-5.
        let kept = (0..10).filter(|_| tb.allow(now)).count();
        assert_eq!(kept, 2);
        // A fresh 1s window resets the budget.
        assert!(tb.allow(now + Duration::from_secs(1)));
    }

    #[test]
    fn token_bucket_sample_ratio_clamped_to_one() {
        let now = Instant::now();
        // sample_ratio 0 → clamped to 1 → every over-budget entry kept.
        let mut tb = TokenBucket::new(Some(1), 0, now);
        assert!(tb.allow(now)); // within budget
        let kept = (0..5).filter(|_| tb.allow(now)).count();
        assert_eq!(kept, 5);
    }

    #[test]
    fn shared_rate_limiter_matches_bucket() {
        let now = Instant::now();
        let rl = SharedRateLimiter::new(Some(1), 1_000_000, now);
        assert!(rl.allow(now)); // first within budget
        // Over budget with a huge sample ratio → effectively all shed.
        assert!(!rl.allow(now));
        assert!(!rl.allow(now));
    }

    // ---- ingest stats --------------------------------------------------------

    #[test]
    fn loss_ratio_is_dropped_over_received_window() {
        let prev = IngestStatsSnapshot {
            received: 1000,
            dropped: 10,
            ..Default::default()
        };
        let cur = IngestStatsSnapshot {
            received: 2000, // +1000 in window
            dropped: 110,   // +100 dropped → 10%
            ..Default::default()
        };
        assert!((cur.loss_ratio_since(&prev) - 0.10).abs() < 1e-9);
        // Idle window (no receives) → 0.0, not NaN.
        assert_eq!(cur.loss_ratio_since(&cur), 0.0);
    }

    #[test]
    fn snapshot_reflects_counters_and_points() {
        let stats = IngestStats::default();
        IngestStats::inc(&stats.received);
        IngestStats::inc(&stats.received);
        IngestStats::inc(&stats.parsed);
        IngestStats::inc(&stats.parse_failed);
        IngestStats::inc(&stats.dropped);
        let snap = stats.snapshot();
        assert_eq!(snap.received, 2);
        assert_eq!(snap.parsed, 1);
        assert_eq!(snap.parse_failed, 1);
        assert_eq!(snap.dropped, 1);

        let pts = snap.to_points("host01");
        let find = |m: &str| {
            pts.iter()
                .find(|p| p.metric == m)
                .map(|p| p.value.clone())
                .unwrap()
        };
        assert_eq!(
            find("logs/ingest/received_total"),
            TelemetryValue::Counter(2)
        );
        assert_eq!(find("logs/ingest/parsed_total"), TelemetryValue::Counter(1));
        assert_eq!(
            find("logs/ingest/parse_failed_total"),
            TelemetryValue::Counter(1)
        );
        assert_eq!(
            find("logs/ingest/dropped_total"),
            TelemetryValue::Counter(1)
        );
        assert!(pts.iter().all(|p| p.source == "host01"));
    }

    // ---- framing: LF ---------------------------------------------------------

    fn frame(buf: &[u8], mode: Framing) -> TakeResult {
        take_frame(buf, mode, 65535)
    }

    fn msg(r: &TakeResult) -> (&[u8], usize) {
        match r {
            TakeResult::Frame { message, consumed } => (message, *consumed),
            TakeResult::Incomplete => panic!("expected a frame, got Incomplete"),
        }
    }

    #[test]
    fn lf_splits_on_newline() {
        let r = frame(b"<14>hello\n<14>world\n", Framing::Lf);
        let (m, c) = msg(&r);
        assert_eq!(m, b"<14>hello");
        assert_eq!(c, 10);
    }

    #[test]
    fn lf_strips_trailing_cr() {
        let r = frame(b"<14>hi\r\nrest", Framing::Lf);
        let (m, c) = msg(&r);
        assert_eq!(m, b"<14>hi");
        assert_eq!(c, 8); // includes the \r and \n
    }

    #[test]
    fn lf_waits_without_newline() {
        assert_eq!(frame(b"<14>partial", Framing::Lf), TakeResult::Incomplete);
    }

    // ---- framing: octet ------------------------------------------------------

    #[test]
    fn octet_splits_two_frames() {
        // "11 hello world" + "5 abcde"
        let buf = b"11 hello world5 abcde";
        let r = frame(buf, Framing::Octet);
        let (m, c) = msg(&r);
        assert_eq!(m, b"hello world");
        assert_eq!(c, 14); // "11 " (3) + 11

        // Second frame from the remainder.
        let r2 = frame(&buf[c..], Framing::Octet);
        let (m2, _) = msg(&r2);
        assert_eq!(m2, b"abcde");
    }

    #[test]
    fn octet_preserves_embedded_newlines() {
        // Octet-counting is robust to newlines inside the payload.
        // "line1\nline2" is 11 bytes; the trailing 'x' starts the next frame.
        let buf = b"11 line1\nline2x";
        let r = frame(buf, Framing::Octet);
        let (m, c) = msg(&r);
        assert_eq!(m, b"line1\nline2");
        assert_eq!(c, 14); // "11 " (3) + 11
    }

    #[test]
    fn octet_waits_for_full_payload() {
        // Declares 20 bytes but only a few are present.
        assert_eq!(frame(b"20 short", Framing::Octet), TakeResult::Incomplete);
    }

    #[test]
    fn octet_waits_for_length_terminator() {
        // Digits not yet terminated by a space.
        assert_eq!(frame(b"123", Framing::Octet), TakeResult::Incomplete);
    }

    #[test]
    fn octet_malformed_length_falls_back_to_lf() {
        // A digit run not followed by a space, with a newline present → LF.
        let r = frame(b"12x not octet\n", Framing::Octet);
        let (m, _) = msg(&r);
        assert_eq!(m, b"12x not octet");
    }

    #[test]
    fn octet_oversized_length_falls_back_to_lf() {
        // Declared length over the cap → treat as LF rather than buffer forever.
        let r = take_frame(b"99 aaaa\n", Framing::Octet, 8);
        let (m, _) = msg(&r);
        assert_eq!(m, b"99 aaaa");
    }

    // ---- framing: auto-detect ------------------------------------------------

    #[test]
    fn auto_detects_octet_on_leading_digit() {
        let r = frame(b"11 hello world", Framing::Auto);
        let (m, _) = msg(&r);
        assert_eq!(m, b"hello world");
    }

    #[test]
    fn auto_detects_lf_on_leading_non_digit() {
        let r = frame(b"<14>hello\n", Framing::Auto);
        let (m, _) = msg(&r);
        assert_eq!(m, b"<14>hello");
    }

    #[test]
    fn auto_waits_on_only_spaces() {
        assert_eq!(frame(b"   ", Framing::Auto), TakeResult::Incomplete);
    }

    #[test]
    fn auto_mixed_stream_per_frame() {
        // An octet frame immediately followed by an LF frame.
        let buf = b"5 first<14>second\n";
        let r = frame(buf, Framing::Auto);
        let (m, c) = msg(&r);
        assert_eq!(m, b"first");
        let r2 = frame(&buf[c..], Framing::Auto);
        let (m2, _) = msg(&r2);
        assert_eq!(m2, b"<14>second");
    }

    // ---- FrameReader (async I/O over the pure logic) -------------------------

    #[tokio::test]
    async fn frame_reader_drains_octet_and_lf() {
        let data = b"5 hello<14>bye\n".to_vec();
        let mut cursor = std::io::Cursor::new(data);
        let mut fr = FrameReader::new(Framing::Auto, 65535);
        let t = Duration::from_secs(5);
        let f1 = fr.next_frame(&mut cursor, t).await.unwrap().unwrap();
        assert_eq!(f1, b"hello");
        let f2 = fr.next_frame(&mut cursor, t).await.unwrap().unwrap();
        assert_eq!(f2, b"<14>bye");
        assert!(fr.next_frame(&mut cursor, t).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn frame_reader_flushes_final_line_without_newline() {
        let mut cursor = std::io::Cursor::new(b"<14>trailing".to_vec());
        let mut fr = FrameReader::new(Framing::Lf, 65535);
        let f = fr
            .next_frame(&mut cursor, Duration::from_secs(5))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(f, b"<14>trailing");
    }

    #[tokio::test]
    async fn frame_reader_rejects_oversized_frame() {
        // No terminator within the cap → InvalidData (connection dropped).
        let mut cursor = std::io::Cursor::new(vec![b'a'; 100]);
        let mut fr = FrameReader::new(Framing::Lf, 16);
        let err = fr
            .next_frame(&mut cursor, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    // ---- forward_parsed (rate limit + overflow accounting) -------------------

    fn received(body: &str) -> ReceivedMessage {
        ReceivedMessage {
            message: crate::parser::parse(&format!("<14>{body}")).unwrap(),
            source: MessageSource::Unix,
            resolved_hostname: "h".into(),
        }
    }

    #[tokio::test]
    async fn forward_counts_drop_when_channel_full() {
        let (tx, _rx) = mpsc::channel::<ReceivedMessage>(1);
        let stats = IngestStats::default();
        let rl = SharedRateLimiter::new(None, 1, Instant::now());
        // Fills the capacity-1 channel.
        assert!(forward_parsed(received("a"), &tx, &stats, &rl, OverflowPolicy::DropNewest).await);
        // Second send finds it full → dropped + counted, but keep going.
        assert!(forward_parsed(received("b"), &tx, &stats, &rl, OverflowPolicy::DropNewest).await);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn forward_reports_closed_channel() {
        let (tx, rx) = mpsc::channel::<ReceivedMessage>(1);
        drop(rx);
        let stats = IngestStats::default();
        let rl = SharedRateLimiter::new(None, 1, Instant::now());
        // Closed channel → false (caller should stop).
        assert!(!forward_parsed(received("x"), &tx, &stats, &rl, OverflowPolicy::DropNewest).await);
    }

    #[tokio::test]
    async fn forward_counts_rate_limited_drop() {
        let (tx, mut rx) = mpsc::channel::<ReceivedMessage>(8);
        let stats = IngestStats::default();
        let now = Instant::now();
        let rl = SharedRateLimiter::new(Some(1), 1_000_000, now);
        // First within budget → sent.
        assert!(forward_parsed(received("a"), &tx, &stats, &rl, OverflowPolicy::DropNewest).await);
        // Second over budget (huge sample ratio) → shed + counted as dropped.
        assert!(forward_parsed(received("b"), &tx, &stats, &rl, OverflowPolicy::DropNewest).await);
        assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);
        // Only the first made it onto the channel.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}
