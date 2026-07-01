//! Drain tasks: move telemetry/anomalies off the capture path onto Zenoh.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use zensight_common::Format;
use zensight_sensor_core::{AdvancedPublisherConfig, AdvancedPublisherRegistry, AlertReporter};

use crate::map;
use crate::monitor::{MonitorChannels, dns_snapshot, to_view};

/// Drain telemetry points and publish them. Also emits periodic flow aggregates
/// from the shared counters.
#[allow(clippy::too_many_arguments)]
pub async fn run_drains(
    mut channels: MonitorChannels,
    session: Arc<zenoh::Session>,
    key_prefix: String,
    sensor_id: String,
    format: Format,
    reporter: Arc<AlertReporter>,
    flow_period_secs: u64,
    health: Arc<zensight_sensor_core::SensorHealth>,
) {
    // This sensor monitors one capture host (itself).
    health.set_devices_total(1);
    health.record_device_success(&sensor_id);
    let started = channels.flow_started.clone();
    let ended = channels.flow_ended.clone();
    let flow_bytes = channels.flow_bytes.clone();
    let flow_packets = channels.flow_packets.clone();
    let flow_retransmits = channels.flow_retransmits.clone();
    let flow_durations = channels.flow_durations_ms.clone();
    let tcp_resets = channels.tcp_resets.clone();
    let tcp_refused = channels.tcp_refused.clone();
    let tls_handshakes = channels.tls_handshakes.clone();
    let tls_inventory = channels.tls_inventory.clone();
    let l4 = channels.l4.clone();
    let icmp = channels.icmp.clone();
    let dns = channels.dns.clone();
    let http = channels.http.clone();
    let quic = channels.quic.clone();
    let ssh = channels.ssh.clone();
    let assets = channels.assets.clone();

    // Take (and clear) the current window's flow durations for percentile points.
    let drain_window = |buf: &std::sync::Mutex<Vec<u64>>| -> Vec<u64> {
        buf.lock()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default()
    };

    // Cached publishers so late-joining consumers get current values on connect.
    let registry = AdvancedPublisherRegistry::new(
        session,
        key_prefix,
        format,
        AdvancedPublisherConfig::default(),
    );

    let mut flow_tick = tokio::time::interval(Duration::from_secs(flow_period_secs.max(1)));

    // Assemble the full set of periodic aggregate points from the shared state.
    // `final_flush` controls whether duration/RTT/latency percentiles are taken
    // (always true — at both tick and EOF we want the windowed distribution).
    let build_aggregate = |sensor_id: &str| -> Vec<zensight_common::TelemetryPoint> {
        let s = started.load(Ordering::Relaxed);
        let e = ended.load(Ordering::Relaxed);
        let active = s.saturating_sub(e);
        let resets = tcp_resets.load(Ordering::Relaxed);
        let refused = tcp_refused.load(Ordering::Relaxed);
        let bytes = flow_bytes.load(Ordering::Relaxed);
        let pkts = flow_packets.load(Ordering::Relaxed);
        let retx = flow_retransmits.load(Ordering::Relaxed);
        let mut durs = drain_window(&flow_durations);

        let mut points: Vec<_> = map::flow_points(sensor_id, s, e, active)
            .into_iter()
            .chain(map::flow_volume_points(sensor_id, bytes, pkts, retx))
            .chain(map::flow_latency_points(sensor_id, &mut durs))
            .chain(map::tcp_reset_points(sensor_id, resets, refused))
            // Per-L4 composition + connection-state breakdown (issue #16).
            .chain(map::flow_by_l4_points(
                sensor_id,
                l4.tcp_bytes.load(Ordering::Relaxed),
                l4.tcp_flows.load(Ordering::Relaxed),
                l4.udp_bytes.load(Ordering::Relaxed),
                l4.udp_flows.load(Ordering::Relaxed),
                l4.icmp_bytes.load(Ordering::Relaxed),
                l4.icmp_flows.load(Ordering::Relaxed),
            ))
            .chain(map::tcp_closed_points(
                sensor_id,
                l4.closed_fin.load(Ordering::Relaxed),
                l4.closed_rst.load(Ordering::Relaxed),
                l4.closed_idle.load(Ordering::Relaxed),
            ))
            .collect();

        // TLS handshake aggregates (passive asset inventory size).
        let tls_n = tls_handshakes.load(Ordering::Relaxed);
        let tls_distinct = tls_inventory.lock().map(|i| i.len() as u64).unwrap_or(0);
        points.extend(map::tls_points(sensor_id, tls_n, tls_distinct));

        // L7 QUIC / SSH inventory sizes (issue #72) — only published once the
        // inventory is non-empty, so the cached gauge isn't clobbered to 0 on a
        // build without the collector armed.
        let quic_distinct = quic.lock().map(|i| i.len() as u64).unwrap_or(0);
        if quic_distinct > 0 {
            points.push(map::quic_count_point(sensor_id, quic_distinct));
        }
        let ssh_distinct = ssh.lock().map(|i| i.len() as u64).unwrap_or(0);
        if ssh_distinct > 0 {
            points.push(map::ssh_count_point(sensor_id, ssh_distinct));
        }
        // Passive asset inventory size (issue #70) — only when any asset has
        // been discovered, so the cached gauge isn't clobbered to 0 on a build
        // without the asset collector armed.
        let asset_count = assets.lock().map(|a| a.len() as u64).unwrap_or(0);
        if asset_count > 0 {
            points.push(map::asset_count_point(sensor_id, asset_count));
        }

        // ICMP error aggregates (issue #15).
        let by_kind: Vec<(String, u64)> = icmp
            .by_kind
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();
        points.extend(map::icmp_points(
            sensor_id,
            icmp.unreachable.load(Ordering::Relaxed),
            icmp.time_exceeded.load(Ordering::Relaxed),
            icmp.mtu_signal.load(Ordering::Relaxed),
            &by_kind,
        ));

        // DNS RED aggregates (issue #19).
        let (dns_queries, by_rcode, dns_unanswered) = dns_snapshot(&dns);
        let mut rtt = drain_window(&dns.rtt_ms);
        points.extend(map::dns_points(
            sensor_id,
            dns_queries,
            &by_rcode,
            dns_unanswered,
            &mut rtt,
        ));

        // HTTP RED aggregates (issue #20).
        let by_method: Vec<(String, u64)> = http
            .methods
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default();
        let mut lat = drain_window(&http.latency_ms);
        points.extend(map::http_points(
            sensor_id,
            http.requests.load(Ordering::Relaxed),
            http.status_2xx.load(Ordering::Relaxed),
            http.status_3xx.load(Ordering::Relaxed),
            http.status_4xx.load(Ordering::Relaxed),
            http.status_5xx.load(Ordering::Relaxed),
            &by_method,
            &mut lat,
        ));

        points
    };

    // Per-detector anomaly counts (#254): monotonic totals keyed by detector slug
    // (the alert `rule` / `AnomalyView::kind`), re-emitted each aggregate tick as
    // `anomaly/<kind>/total` counters so the GUI can roll up per-detector activity.
    let mut anomaly_counts: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();

    loop {
        tokio::select! {
            // Telemetry points from monitor callbacks.
            point = channels.telemetry.recv() => {
                match point {
                    Some(point) => { health.record_metrics_published(1);
                        let suffix = format!("{}/{}", point.source, point.metric);
                        if let Err(e) = registry.publish(&suffix, &point).await { tracing::warn!(error=%e, "publish failed"); } }
                    None => {
                        // Monitor finished (e.g. pcap EOF / shutdown): flush a final
                        // aggregate so short replays still emit their counts.
                        for point in build_aggregate(&sensor_id) {
                            let suffix = format!("{}/{}", point.source, point.metric);
                            let _ = registry.publish(&suffix, &point).await;
                        }
                        // Drain any detector anomalies / sensor alerts still queued so a
                        // trailing alert isn't lost when the telemetry channel closes first
                        // (fast pcap EOF, or a clean live shutdown).
                        while let Ok(a) = channels.anomalies.try_recv() {
                            let view = to_view(&a);
                            *anomaly_counts.entry(view.kind.clone()).or_default() += 1;
                            let alert = map::anomaly_alert(&sensor_id, &view);
                            if let Err(e) = reporter.observe(alert, Some(Duration::ZERO)).await {
                                tracing::warn!(error = %e, "failed to publish anomaly alert");
                            }
                        }
                        while let Ok(alert) = channels.alerts.try_recv() {
                            count_anomaly_alert(&mut anomaly_counts, &alert);
                            if let Err(e) = drain_sensor_alert(&reporter, alert).await {
                                tracing::warn!(error = %e, "failed to publish sensor alert");
                            }
                        }
                        // Flush a final per-detector count so short replays surface totals.
                        for (kind, count) in &anomaly_counts {
                            let point = map::anomaly_count_point(&sensor_id, kind, *count);
                            let suffix = format!("{}/{}", point.source, point.metric);
                            let _ = registry.publish(&suffix, &point).await;
                        }
                        // Give late subscribers a moment to pull from the cache.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        break;
                    }
                }
            }
            // Detector anomalies → alerts.
            anomaly = channels.anomalies.recv() => {
                if let Some(a) = anomaly {
                    let view = to_view(&a);
                    *anomaly_counts.entry(view.kind.clone()).or_default() += 1;
                    let alert = map::anomaly_alert(&sensor_id, &view);
                    if let Err(e) = reporter.observe(alert, Some(Duration::ZERO)).await {
                        tracing::warn!(error = %e, "failed to publish anomaly alert");
                    }
                }
            }
            // Typed sensor alerts (ICMP flow-killed, capture-overload) →
            // AlertReporter (never lossy). Firing alerts are observed; resolved
            // alerts (capture recovered) reconcile the firing set away.
            alert = channels.alerts.recv() => {
                if let Some(alert) = alert {
                    count_anomaly_alert(&mut anomaly_counts, &alert);
                    if let Err(e) = drain_sensor_alert(&reporter, alert).await {
                        tracing::warn!(error = %e, "failed to publish sensor alert");
                    }
                }
            }
            // Periodic aggregates.
            _ = flow_tick.tick() => {
                for point in build_aggregate(&sensor_id) {
                    health.record_metrics_published(1);
                    let suffix = format!("{}/{}", point.source, point.metric);
                    if let Err(e) = registry.publish(&suffix, &point).await { tracing::warn!(error=%e, "publish failed"); }
                }
                // Per-detector anomaly counters (#254): re-emit the running totals.
                for (kind, count) in &anomaly_counts {
                    let point = map::anomaly_count_point(&sensor_id, kind, *count);
                    health.record_metrics_published(1);
                    let suffix = format!("{}/{}", point.source, point.metric);
                    if let Err(e) = registry.publish(&suffix, &point).await { tracing::warn!(error=%e, "publish failed"); }
                }
                // The capture host responded this window.
                health.record_device_success(&sensor_id);
            }
        }
    }
}

/// Tally a firing **anomaly** alert into the per-detector counts (#254), keyed by
/// its rule (detector slug). DNS-tunnel / NOD detectors arrive here as pre-built
/// `Alert`s on the alerts channel (unlike flowscope anomalies), so this is where
/// they get counted. Operational alerts (capture-overload, ICMP flow-killed) carry
/// a non-`Anomaly` kind and are skipped, as are resolve transitions.
fn count_anomaly_alert(
    counts: &mut std::collections::HashMap<String, u64>,
    alert: &zensight_common::Alert,
) {
    use zensight_common::AlertKind;
    if alert.kind == AlertKind::Anomaly && alert.is_firing() {
        *counts.entry(alert.rule.clone()).or_default() += 1;
    }
}

/// Route a capture-path sensor alert to the reporter: a firing alert is observed
/// (published immediately — these are edge-triggered, not debounced), while a
/// resolved alert (e.g. capture-overload recovery) reconciles its rule's firing
/// set away so the GUI clears the badge.
async fn drain_sensor_alert(
    reporter: &AlertReporter,
    alert: zensight_common::Alert,
) -> zensight_sensor_core::Result<()> {
    if alert.is_firing() {
        reporter.observe(alert, Some(Duration::ZERO)).await
    } else {
        reporter.reconcile(&alert.rule, &[]).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

    use super::count_anomaly_alert;

    fn anomaly(rule: &str) -> Alert {
        Alert::new(
            "host01",
            Protocol::Netring,
            AlertKind::Anomaly,
            rule,
            AlertSeverity::Warning,
            "test",
        )
    }

    #[test]
    fn counts_firing_anomalies_by_rule() {
        let mut counts: HashMap<String, u64> = HashMap::new();
        count_anomaly_alert(&mut counts, &anomaly("DnsTunnel"));
        count_anomaly_alert(&mut counts, &anomaly("DnsTunnel"));
        count_anomaly_alert(&mut counts, &anomaly("NewlyObservedDomain"));
        assert_eq!(counts.get("DnsTunnel"), Some(&2));
        assert_eq!(counts.get("NewlyObservedDomain"), Some(&1));
    }

    #[test]
    fn skips_non_anomaly_and_resolved() {
        let mut counts: HashMap<String, u64> = HashMap::new();
        // Operational (non-anomaly) alerts must not inflate detector counts.
        let health = Alert::new(
            "host01",
            Protocol::Netring,
            AlertKind::SensorHealth,
            "capture-overload",
            AlertSeverity::Warning,
            "test",
        );
        count_anomaly_alert(&mut counts, &health);
        // A resolved anomaly (reconcile transition) must not be counted either.
        count_anomaly_alert(&mut counts, &anomaly("PortScanTRW").resolved());
        assert!(counts.is_empty());
    }
}
