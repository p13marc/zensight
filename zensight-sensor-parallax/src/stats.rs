//! Per-stream stats: lock-free counters fed by the pipeline/egress tasks,
//! flushed to ordinary telemetry by a ticker.
//!
//! The registry's `Mutex<HashMap>` is touched only on stream open/close and
//! on ticker snapshots; the hot paths (egress per frame, encoder per frame)
//! bump `AtomicU64`s on a shared [`StreamStats`]. Stats are **per stream**,
//! aggregated over its open profiles (kbps is the stream's total media
//! bandwidth; fps counts every published frame, video + preview).
//!
//! Telemetry rides `zensight/@v1/<origin>/telemetry/parallax/<stream>/stats/<metric>`
//! (fps / kbps / drops / viewers / encode_ms), so existing charts light up
//! for free; `streams/advertised` is published every tick so a parallax host
//! shows up on the dashboard even before any stream is opened.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zensight_common::TelemetryValue;
use zensight_sensor_core::Publisher;

/// Lock-free counters for one stream (shared by its open profiles).
#[derive(Debug, Default)]
pub struct StreamStats {
    /// Frames published on the media plane (cumulative).
    pub frames: AtomicU64,
    /// Payload bytes published (cumulative).
    pub bytes: AtomicU64,
    /// Frames lost between encoder and egress — sequence gaps observed on
    /// the video profile (cumulative). Intentional preview throttling is
    /// never counted.
    pub drops: AtomicU64,
    /// Cumulative wall time spent inside encoder `process()` calls.
    pub encode_ns: AtomicU64,
    /// Frames that went through a timed encoder (denominator for encode_ms).
    pub encoded_frames: AtomicU64,
    /// Current matching-viewer count (gauge, set by the session actor).
    pub viewers: AtomicU64,
    /// Strictest per-frame encode budget among open profiles (ns);
    /// 0 = no encoder (e.g. RTSP passthrough) → no overrun evaluation.
    pub budget_ns: AtomicU64,
}

impl StreamStats {
    /// Record one published frame of `bytes` payload.
    pub fn record_frame(&self, bytes: usize) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Record `n` frames lost before egress (sequence gap).
    pub fn record_drops(&self, n: u64) {
        self.drops.fetch_add(n, Ordering::Relaxed);
    }

    /// Record one encoder `process()` call's wall time.
    pub fn record_encode(&self, ns: u64) {
        self.encode_ns.fetch_add(ns, Ordering::Relaxed);
        self.encoded_frames.fetch_add(1, Ordering::Relaxed);
    }

    /// Tighten the per-frame budget (keeps the strictest non-zero value).
    pub fn tighten_budget(&self, budget_ns: u64) {
        if budget_ns == 0 {
            return;
        }
        let mut current = self.budget_ns.load(Ordering::Relaxed);
        while current == 0 || budget_ns < current {
            match self.budget_ns.compare_exchange_weak(
                current,
                budget_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}

/// Shared registry: one [`StreamStats`] per open stream.
#[derive(Debug, Clone, Default)]
pub struct StatsRegistry {
    inner: Arc<Mutex<HashMap<String, Arc<StreamStats>>>>,
}

impl StatsRegistry {
    /// Get (or create) the stats handle for `stream` — called at open.
    pub fn handle(&self, stream: &str) -> Arc<StreamStats> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(stream.to_string()).or_default().clone()
    }

    /// Drop a stream's stats — called when its last profile closes.
    pub fn remove(&self, stream: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(stream);
    }

    /// Snapshot the open streams' stats handles.
    pub fn snapshot(&self) -> Vec<(String, Arc<StreamStats>)> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
}

/// Counter values remembered between ticks, for rate derivation.
#[derive(Debug, Default, Clone, Copy)]
struct PrevCounters {
    frames: u64,
    bytes: u64,
    encode_ns: u64,
    encoded_frames: u64,
}

/// One tick's derived numbers for a stream (pure — unit-tested).
#[derive(Debug, PartialEq)]
struct TickDerived {
    fps: f64,
    kbps: f64,
    encode_ms: Option<f64>,
}

fn derive(prev: PrevCounters, now: PrevCounters, interval_secs: f64) -> TickDerived {
    let dframes = now.frames.saturating_sub(prev.frames);
    let dbytes = now.bytes.saturating_sub(prev.bytes);
    let denc_ns = now.encode_ns.saturating_sub(prev.encode_ns);
    let denc_frames = now.encoded_frames.saturating_sub(prev.encoded_frames);
    TickDerived {
        fps: dframes as f64 / interval_secs,
        kbps: (dbytes as f64 * 8.0) / 1000.0 / interval_secs,
        encode_ms: (denc_frames > 0).then(|| denc_ns as f64 / denc_frames as f64 / 1e6),
    }
}

/// Run the stats ticker: every `interval`, publish per-open-stream telemetry
/// plus the always-on `streams/advertised` gauge, and (when an alert sink is
/// wired) evaluate the encoder-overrun rule.
pub async fn run_ticker(
    publisher: Publisher,
    source: String,
    registry: StatsRegistry,
    advertised_streams: usize,
    interval: Duration,
    alerts: Option<Arc<crate::alerts::ParallaxAlerts>>,
) {
    let interval_secs = interval.as_secs_f64();
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prev: HashMap<String, PrevCounters> = HashMap::new();

    loop {
        tick.tick().await;

        // Baseline presence gauge: the parallax host exists on the dashboard
        // even with zero open streams.
        publish(
            &publisher,
            &source,
            "streams/advertised",
            TelemetryValue::Gauge(advertised_streams as f64),
        )
        .await;

        let open = registry.snapshot();
        // Forget closed streams' rate baselines.
        prev.retain(|k, _| open.iter().any(|(name, _)| name == k));

        for (stream, stats) in open {
            let now = PrevCounters {
                frames: stats.frames.load(Ordering::Relaxed),
                bytes: stats.bytes.load(Ordering::Relaxed),
                encode_ns: stats.encode_ns.load(Ordering::Relaxed),
                encoded_frames: stats.encoded_frames.load(Ordering::Relaxed),
            };
            let baseline = prev.insert(stream.clone(), now).unwrap_or_default();
            let derived = derive(baseline, now, interval_secs);

            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/fps"),
                TelemetryValue::Gauge(derived.fps),
            )
            .await;
            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/kbps"),
                TelemetryValue::Gauge(derived.kbps),
            )
            .await;
            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/drops"),
                TelemetryValue::Counter(stats.drops.load(Ordering::Relaxed)),
            )
            .await;
            publish(
                &publisher,
                &source,
                &format!("{stream}/stats/viewers"),
                TelemetryValue::Gauge(stats.viewers.load(Ordering::Relaxed) as f64),
            )
            .await;
            if let Some(encode_ms) = derived.encode_ms {
                publish(
                    &publisher,
                    &source,
                    &format!("{stream}/stats/encode_ms"),
                    TelemetryValue::Gauge(encode_ms),
                )
                .await;

                // Encoder overrun: average encode time above the strictest
                // per-frame budget means the encoder can't keep up live.
                if let Some(alerts) = &alerts {
                    let budget_ns = stats.budget_ns.load(Ordering::Relaxed);
                    if budget_ns > 0 {
                        let budget_ms = budget_ns as f64 / 1e6;
                        alerts
                            .encoder_overrun(&stream, encode_ms, budget_ms, encode_ms > budget_ms)
                            .await;
                    }
                }
            }
        }
    }
}

async fn publish(publisher: &Publisher, source: &str, metric: &str, value: TelemetryValue) {
    let point = crate::telemetry_guard::checked_point(source, metric, value);
    if let Err(e) = publisher.publish(metric, &point).await {
        tracing::warn!(error = %e, metric = %metric, "failed to publish stream stats");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_computes_rates() {
        let prev = PrevCounters {
            frames: 100,
            bytes: 1_000_000,
            encode_ns: 1_000_000_000,
            encoded_frames: 100,
        };
        let now = PrevCounters {
            frames: 150,
            bytes: 2_000_000,
            encode_ns: 1_500_000_000,
            encoded_frames: 150,
        };
        let d = derive(prev, now, 5.0);
        assert_eq!(d.fps, 10.0);
        assert_eq!(d.kbps, 1600.0);
        assert_eq!(d.encode_ms, Some(10.0));
    }

    #[test]
    fn derive_handles_no_encoded_frames() {
        let d = derive(PrevCounters::default(), PrevCounters::default(), 5.0);
        assert_eq!(d.fps, 0.0);
        assert_eq!(d.encode_ms, None, "no encoder frames → no encode_ms point");
    }

    #[test]
    fn registry_open_close_lifecycle() {
        let registry = StatsRegistry::default();
        let a = registry.handle("cam0");
        let a2 = registry.handle("cam0");
        a.record_frame(100);
        assert_eq!(a2.frames.load(Ordering::Relaxed), 1, "same shared handle");
        assert_eq!(registry.snapshot().len(), 1);
        registry.remove("cam0");
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn budget_keeps_strictest() {
        let stats = StreamStats::default();
        stats.tighten_budget(0);
        assert_eq!(stats.budget_ns.load(Ordering::Relaxed), 0);
        stats.tighten_budget(500_000_000);
        stats.tighten_budget(66_666_666);
        stats.tighten_budget(500_000_000); // looser: ignored
        assert_eq!(stats.budget_ns.load(Ordering::Relaxed), 66_666_666);
    }
}
