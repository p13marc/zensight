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
                    None => break, // monitor finished (e.g. pcap EOF)
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
            // Periodic flow aggregates.
            _ = flow_tick.tick() => {
                let s = started.load(Ordering::Relaxed);
                let e = ended.load(Ordering::Relaxed);
                let active = s.saturating_sub(e);
                for point in map::flow_points(&sensor_id, s, e, active) {
                    let suffix = format!("{}/{}", point.source, point.metric);
                    if let Err(e) = registry.publish(&suffix, &point).await { tracing::warn!(error=%e, "publish failed"); }
                }
            }
        }
    }
}
