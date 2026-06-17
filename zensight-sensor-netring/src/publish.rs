//! Drain tasks: move telemetry/anomalies off the capture path onto Zenoh.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use zensight_common::Format;
use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry, AlertReporter};

use crate::map;
use crate::monitor::{MonitorChannels, to_view};

/// Drain telemetry points and publish them. Also emits periodic flow aggregates
/// from the shared counters.
pub async fn run_drains(
    mut channels: MonitorChannels,
    session: Arc<zenoh::Session>,
    key_prefix: String,
    sensor_id: String,
    format: Format,
    reporter: Arc<AlertReporter>,
    flow_period_secs: u64,
) {
    let started = channels.flow_started.clone();
    let ended = channels.flow_ended.clone();
    let flow_bytes = channels.flow_bytes.clone();
    let flow_packets = channels.flow_packets.clone();
    let flow_retransmits = channels.flow_retransmits.clone();
    let flow_durations = channels.flow_durations_ms.clone();
    let tcp_resets = channels.tcp_resets.clone();
    let tcp_refused = channels.tcp_refused.clone();

    // Take (and clear) the current window's flow durations for percentile points.
    let drain_durations = |buf: &std::sync::Mutex<Vec<u64>>| -> Vec<u64> {
        buf.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
    };

    // Cached publishers so late-joining consumers get current values on connect.
    let registry = AdvancedPublisherRegistry::new(
        session,
        key_prefix,
        format,
        AdvancedPublisherConfig::default(),
    );

    let mut flow_tick = tokio::time::interval(Duration::from_secs(flow_period_secs.max(1)));

    loop {
        tokio::select! {
            // Telemetry points from monitor callbacks.
            point = channels.telemetry.recv() => {
                match point {
                    Some(point) => { let suffix = format!("{}/{}", point.source, point.metric);
                        if let Err(e) = registry.publish(&suffix, &point).await { tracing::warn!(error=%e, "publish failed"); } }
                    None => {
                        // Monitor finished (e.g. pcap EOF / shutdown): flush a final
                        // aggregate so short replays still emit flow + TCP counts.
                        let s = started.load(Ordering::Relaxed);
                        let e = ended.load(Ordering::Relaxed);
                        let resets = tcp_resets.load(Ordering::Relaxed);
                        let refused = tcp_refused.load(Ordering::Relaxed);
                        let bytes = flow_bytes.load(Ordering::Relaxed);
                        let pkts = flow_packets.load(Ordering::Relaxed);
                        let retx = flow_retransmits.load(Ordering::Relaxed);
                        let mut durs = drain_durations(&flow_durations);
                        let points = map::flow_points(&sensor_id, s, e, s.saturating_sub(e))
                            .into_iter()
                            .chain(map::flow_volume_points(&sensor_id, bytes, pkts, retx))
                            .chain(map::flow_latency_points(&sensor_id, &mut durs))
                            .chain(map::tcp_reset_points(&sensor_id, resets, refused));
                        for point in points {
                            let suffix = format!("{}/{}", point.source, point.metric);
                            let _ = registry.publish(&suffix, &point).await;
                        }
                        // Give late subscribers a moment to pull from the cache.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        break;
                    }
                }
            }
            // Anomalies → alerts.
            anomaly = channels.anomalies.recv() => {
                if let Some(a) = anomaly {
                    let view = to_view(&a);
                    let alert = map::anomaly_alert(&sensor_id, &view);
                    if let Err(e) = reporter.observe(alert, Some(Duration::ZERO)).await {
                        tracing::warn!(error = %e, "failed to publish anomaly alert");
                    }
                }
            }
            // Periodic flow + TCP-reset aggregates.
            _ = flow_tick.tick() => {
                let s = started.load(Ordering::Relaxed);
                let e = ended.load(Ordering::Relaxed);
                let active = s.saturating_sub(e);
                let resets = tcp_resets.load(Ordering::Relaxed);
                let refused = tcp_refused.load(Ordering::Relaxed);
                let bytes = flow_bytes.load(Ordering::Relaxed);
                let pkts = flow_packets.load(Ordering::Relaxed);
                let retx = flow_retransmits.load(Ordering::Relaxed);
                let mut durs = drain_durations(&flow_durations);
                let points = map::flow_points(&sensor_id, s, e, active)
                    .into_iter()
                    .chain(map::flow_volume_points(&sensor_id, bytes, pkts, retx))
                    .chain(map::flow_latency_points(&sensor_id, &mut durs))
                    .chain(map::tcp_reset_points(&sensor_id, resets, refused));
                for point in points {
                    let suffix = format!("{}/{}", point.source, point.metric);
                    if let Err(e) = registry.publish(&suffix, &point).await { tracing::warn!(error=%e, "publish failed"); }
                }
            }
        }
    }
}
