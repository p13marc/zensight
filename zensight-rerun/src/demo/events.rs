//! `zensight-rerun-demo events` — a realistic discrete-event sequence plus a
//! burst, one alert firing→resolved pair, and a health degradation
//! (docs/plans/rerun/05-events.md).

use std::time::Duration;

use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
use zensight_common::health::{HealthSnapshot, HealthStatus};
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use super::DemoContext;

/// The demo host's `source` key segment (shared with the metrics scenario).
pub const SOURCE: &str = super::metrics::SOURCE;

fn event_point(metric: &str, message: &str, ts: i64) -> TelemetryPoint {
    let mut p = TelemetryPoint::new(
        SOURCE,
        Protocol::Netlink,
        metric,
        TelemetryValue::Text(message.to_string()),
    );
    p.timestamp = ts;
    p
}

/// The scripted steady sequence: `(offset_ms, metric path, message, labels)`.
/// Kinds derive from the `events/<kind>/…` path chunk (mapping::classify).
pub fn steady_sequence(base_ts: i64) -> Vec<TelemetryPoint> {
    vec![
        event_point(
            "events/route/replace",
            "default via 10.0.0.1 dev eth0",
            base_ts,
        )
        .with_label("iface", "eth0"),
        event_point(
            "events/peer_up/10.0.0.7",
            "peer 10.0.0.7 reachable",
            base_ts + 1_000,
        )
        .with_label("peer", "10.0.0.7"),
        event_point(
            "events/tcp_reset/10.0.0.9",
            "connection to 10.0.0.9:443 reset by peer",
            base_ts + 2_500,
        )
        .with_label("peer", "10.0.0.9")
        .with_label("port", "443"),
        event_point(
            "events/peer_down/10.0.0.7",
            "peer 10.0.0.7 unreachable (3 probes lost)",
            base_ts + 4_000,
        )
        .with_label("peer", "10.0.0.7"),
        event_point(
            "events/anomaly/scan",
            "port scan pattern from 10.0.0.66 (37 ports / 5 s)",
            base_ts + 5_500,
        )
        .with_label("src", "10.0.0.66"),
    ]
}

/// The demo alert (netring anomaly) used for the firing→resolved pair.
pub fn demo_alert() -> Alert {
    Alert::new(
        SOURCE,
        Protocol::Netring,
        AlertKind::Anomaly,
        "PortScanDetector",
        AlertSeverity::Warning,
        "Port scan from 10.0.0.66 (37 ports)",
    )
    .with_label("src", "10.0.0.66")
}

fn health(status: HealthStatus) -> HealthSnapshot {
    HealthSnapshot {
        sensor: "netlink".into(),
        status,
        uptime_secs: 60,
        devices_total: 1,
        devices_responding: 1,
        devices_failed: 0,
        last_poll_duration_ms: 5,
        errors_last_hour: 0,
        metrics_published: 100,
        host_id: None,
        source: Some(SOURCE.to_string()),
    }
}

/// Run the scenario. Returns (events, alerts, health) publication counts.
pub async fn run(ctx: &DemoContext, burst: u64) -> anyhow::Result<(u64, u64, u64)> {
    let base_ts = zensight_common::telemetry::current_timestamp_millis();
    let mut events = 0u64;

    // Steady sequence, paced (real wall-clock pacing keeps the live viewer
    // readable; timestamps are the scripted domain times regardless).
    for point in steady_sequence(base_ts) {
        ctx.publish_point(&point).await?;
        events += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Health: healthy baseline, then a degradation (adapter emits one
    // HealthChange transition event for each status change).
    ctx.publish_health(&health(HealthStatus::Healthy)).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    ctx.publish_health(&health(HealthStatus::Degraded)).await?;

    // Alert lifecycle: firing → resolved on the same alert_key.
    let alert = demo_alert();
    ctx.publish_alert(&alert).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    ctx.publish_alert(&alert.resolved()).await?;

    // Burst: N events inside ~1 s, same-ms collisions likely — this is the
    // AnyValues per-timepoint-overwrite probe (05-events.md §2).
    let burst_ts = zensight_common::telemetry::current_timestamp_millis();
    for i in 0..burst {
        let point = event_point(
            "events/tcp_reset/burst",
            &format!("burst reset #{i}"),
            burst_ts + (i as i64 * 1000 / burst.max(1) as i64),
        )
        .with_label("peer", "10.0.0.99")
        .with_label("seq", i.to_string());
        ctx.publish_point(&point).await?;
        events += 1;
    }

    Ok((events, 2, 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventKind;
    use crate::mapping::{Class, classify};

    #[test]
    fn steady_sequence_classifies_as_events() {
        let seq = steady_sequence(1_000);
        let kinds: Vec<_> = seq
            .iter()
            .map(|p| match classify(p) {
                Class::Event(kind) => kind,
                other => panic!("expected event, got {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::RouteChange,
                EventKind::PeerUp,
                EventKind::TcpReset,
                EventKind::PeerDown,
                EventKind::Anomaly,
            ]
        );
    }

    #[test]
    fn demo_alert_key_is_stable_across_transitions() {
        let firing = demo_alert();
        let resolved = demo_alert().resolved();
        assert_eq!(firing.alert_key(), resolved.alert_key());
    }
}
