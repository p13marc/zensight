//! `zensight-rerun-demo incident` — the deterministic correlated-incident
//! scenario (docs/plans/rerun/06-incident.md).
//!
//! A wireless backhaul degrades: RSSI drops, loss ramps, TCP retransmits
//! accelerate, latency explodes, the route fails over (event), a Critical
//! alert fires with a shared `correlation_id`, then everything recovers and
//! the alert resolves. Every timestamp derives from `base_ts` (`--base-ts`
//! pins it), so two runs produce identical `.rrd` timelines.

use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
use zensight_common::entity::{HostEntity, MemberClaim};
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use super::DemoContext;

/// The demo host's `source` key segment (shared with the other scenarios).
pub const SOURCE: &str = super::metrics::SOURCE;

/// The incident's fixed entity id (a correlator-shaped `h_<12hex>`).
pub const ENTITY_ID: &str = "h_deadbeef0042";

/// Total scripted duration, seconds.
pub const DURATION_SECS: u64 = 65;

/// A discrete scripted step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The gateway peer over wlan0 stops answering (topology edge drops).
    LinkDown,
    /// The kernel fails the default route over to the LTE backup.
    RouteChange,
    /// The sentinel decides the path is down: Critical, correlated.
    AlertFiring,
    /// The gateway peer is reachable again (topology edge returns).
    LinkUp,
    /// Recovery confirmed; the alert clears.
    AlertResolved,
}

/// The discrete script: `(offset_secs, step)`. The continuous series ramp
/// around these (see [`series_at`]).
pub const SCRIPT: &[(u64, Step)] = &[
    (26, Step::LinkDown),
    (28, Step::RouteChange),
    (30, Step::AlertFiring),
    (55, Step::LinkUp),
    (60, Step::AlertResolved),
];

/// The shared correlation handle: `inc-<base_ts>`.
pub fn correlation_id(base_ts: i64) -> String {
    format!("inc-{base_ts}")
}

/// Linear ramp helper: value moves `from`→`to` as `t` crosses `[start, end]`.
fn ramp(t: f64, start: f64, end: f64, from: f64, to: f64) -> f64 {
    if t <= start {
        from
    } else if t >= end {
        to
    } else {
        from + (to - from) * (t - start) / (end - start)
    }
}

/// The continuous series at `sec` seconds into the script.
///
/// Every metric here is one a **real sensor actually emits** — the registry
/// (RFC 08, issue #468) now enforces that, and it caught this file publishing
/// four series that no sensor produces (`wifi/wlan0/rssi_dbm`,
/// `path/gateway/{rtt_ms,loss_percent}`, and `sockets/tcp/retransmits` with the
/// `_total` suffix missing). A demo that invents keys teaches the viewer — and
/// any blueprint built on it — a keyspace that does not exist.
///
/// The story is unchanged, told on the real keys: a wifi link degrades, so its
/// negotiated rate collapses, flows start erroring, TCP retransmits spike, and
/// flow lifetimes stretch out.
///
/// - Link rate: 300 Mb/s, collapses to 6 over t+10..t+20, recovers t+50..t+60.
/// - Flow error ratio: 0, ramps to 0.08 over t+15..t+22, recovers t+50..t+55.
/// - Retransmits (counter): 1/s baseline, 50/s from t+18, 1/s after t+50.
/// - Flow-lifetime p95: 20 ms, ramps to 400 over t+22..t+30, recovers t+50..t+60.
pub fn series_at(base_ts: i64, sec: u64) -> Vec<TelemetryPoint> {
    let ts = base_ts + (sec * 1000) as i64;
    let t = sec as f64;
    let point = |protocol: Protocol, metric: &str, value: TelemetryValue| {
        let mut p = TelemetryPoint::new(SOURCE, protocol, metric, value);
        p.timestamp = ts;
        p
    };

    let recovering = |v: f64, healthy: f64, rec_end: f64| ramp(t, 50.0, rec_end, v, healthy);

    let link_mbps = recovering(ramp(t, 10.0, 20.0, 300.0, 6.0), 300.0, 60.0);
    let error_ratio = recovering(ramp(t, 15.0, 22.0, 0.0, 0.08), 0.0, 55.0);
    let rtt = recovering(ramp(t, 22.0, 30.0, 20.0, 400.0), 20.0, 60.0);

    // Retransmit counter: integrate the scripted per-second rate.
    let retransmits: u64 = (0..sec)
        .map(|s| if (18..50).contains(&s) { 50 } else { 1 })
        .sum();

    vec![
        point(
            Protocol::Netlink,
            "ethtool/wlan0/speed_mbps",
            TelemetryValue::Gauge(link_mbps),
        ),
        point(
            Protocol::Netring,
            "flow/red/error_ratio",
            TelemetryValue::Gauge(error_ratio),
        ),
        point(
            Protocol::Netlink,
            "sockets/tcp/retransmits_total",
            TelemetryValue::Counter(retransmits),
        ),
        point(
            Protocol::Netring,
            "flow/red/p95_ms",
            TelemetryValue::Gauge(rtt),
        ),
    ]
}

/// The scripted gateway link transition (peer_up/peer_down event — drives
/// the topology graph's edge state, docs/plans/rerun/09-topology.md).
pub fn link_event(base_ts: i64, offset_secs: u64, up: bool) -> TelemetryPoint {
    let (kind, message) = if up {
        ("peer_up", "gateway 10.0.0.1 reachable again via wlan0")
    } else {
        (
            "peer_down",
            "gateway 10.0.0.1 unreachable via wlan0 (3 probes lost)",
        )
    };
    let mut p = TelemetryPoint::new(
        SOURCE,
        Protocol::Netlink,
        format!("events/{kind}/gateway"),
        TelemetryValue::Text(message.into()),
    );
    p.timestamp = base_ts + (offset_secs * 1000) as i64;
    p.with_label("peer", "10.0.0.1")
        .with_label("iface", "wlan0")
        .with_label("correlation_id", correlation_id(base_ts))
}

/// The scripted route-change event.
pub fn route_change_event(base_ts: i64, offset_secs: u64) -> TelemetryPoint {
    let mut p = TelemetryPoint::new(
        SOURCE,
        Protocol::Netlink,
        "events/route/replace",
        TelemetryValue::Text("default via 192.168.8.1 dev lte0 (was 10.0.0.1 dev wlan0)".into()),
    );
    p.timestamp = base_ts + (offset_secs * 1000) as i64;
    p.with_label("iface", "lte0")
        .with_label("correlation_id", correlation_id(base_ts))
}

/// The scripted Critical alert (same `alert_key` for firing and resolved).
pub fn incident_alert(base_ts: i64, offset_secs: u64) -> Alert {
    let mut alert = Alert::new(
        SOURCE,
        Protocol::Netlink,
        AlertKind::Expectation,
        "wan-path-degraded",
        AlertSeverity::Critical,
        "Primary WAN path degraded: loss 8%, RTT 400ms, failed over to lte0",
    )
    .with_label("path", "wlan0->gateway")
    .with_label("correlation_id", correlation_id(base_ts));
    alert.timestamp = base_ts + (offset_secs * 1000) as i64;
    alert
}

/// The demo host entity binding all three protocols' sources to one host.
pub fn incident_entity(base_ts: i64) -> HostEntity {
    let member = |sensor: &str| MemberClaim {
        sensor: sensor.to_string(),
        source: SOURCE.to_string(),
        rule: "host_id".to_string(),
        confidence: 1.0,
        last_seen: base_ts,
    };
    HostEntity {
        entity_id: ENTITY_ID.into(),
        aliases: vec![],
        host_id: Some("42".repeat(32)),
        boot_id: None,
        ips: vec!["10.0.0.5".into()],
        macs: vec!["aa:bb:cc:dd:ee:ff".into()],
        container_ids: vec![],
        hostname: Some(SOURCE.into()),
        fqdn: None,
        names: vec![],
        vendor: None,
        platform: None,
        members: vec![member("netlink"), member("netring"), member("sysinfo")],
        status: Some("online".into()),
        last_updated: base_ts,
    }
}

/// Run the incident. `pace_ms` is the wall-clock delay per scripted second
/// (1000 for live viewing; small values fast-forward a recording — domain
/// timestamps are scripted either way). Returns (points, events, alerts).
pub async fn run(
    ctx: &DemoContext,
    base_ts: Option<i64>,
    pace_ms: u64,
) -> anyhow::Result<(u64, u64, u64)> {
    let base_ts = base_ts.unwrap_or_else(zensight_common::telemetry::current_timestamp_millis);
    let mut points = 0u64;
    let mut events = 0u64;
    let mut alerts = 0u64;

    ctx.publish_entity(&incident_entity(base_ts)).await?;

    for sec in 0..=DURATION_SECS {
        for point in series_at(base_ts, sec) {
            ctx.publish_point(&point).await?;
            points += 1;
        }
        for (offset, step) in SCRIPT {
            if *offset == sec {
                match step {
                    Step::LinkDown => {
                        ctx.publish_point(&link_event(base_ts, sec, false)).await?;
                        events += 1;
                    }
                    Step::LinkUp => {
                        ctx.publish_point(&link_event(base_ts, sec, true)).await?;
                        events += 1;
                    }
                    Step::RouteChange => {
                        ctx.publish_point(&route_change_event(base_ts, sec)).await?;
                        events += 1;
                    }
                    Step::AlertFiring => {
                        ctx.publish_alert(&incident_alert(base_ts, sec)).await?;
                        alerts += 1;
                    }
                    Step::AlertResolved => {
                        let mut alert = incident_alert(base_ts, sec).resolved();
                        alert.timestamp = base_ts + (sec * 1000) as i64;
                        ctx.publish_alert(&alert).await?;
                        alerts += 1;
                    }
                }
            }
        }
        if pace_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(pace_ms)).await;
        }
    }

    Ok((points, events, alerts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_is_deterministic_in_base_ts() {
        let a: Vec<_> = (0..=DURATION_SECS)
            .flat_map(|s| series_at(1_000_000, s))
            .collect();
        let b: Vec<_> = (0..=DURATION_SECS)
            .flat_map(|s| series_at(1_000_000, s))
            .collect();
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(
                (x.timestamp, &x.metric, &x.value),
                (y.timestamp, &y.metric, &y.value)
            );
        }
    }

    #[test]
    fn ramps_hit_the_scripted_plateaus() {
        let g = |sec: u64, idx: usize| match &series_at(0, sec)[idx].value {
            TelemetryValue::Gauge(v) => *v,
            TelemetryValue::Counter(v) => *v as f64,
            other => panic!("unexpected value {other:?}"),
        };
        // Baseline.
        assert_eq!(g(5, 0), 300.0); // ethtool/wlan0/speed_mbps
        assert_eq!(g(5, 1), 0.0); // flow/red/error_ratio
        assert_eq!(g(5, 3), 20.0); // flow/red/p95_ms
        // Degraded plateau (t=40 is inside every ramp's plateau).
        assert_eq!(g(40, 0), 6.0);
        assert_eq!(g(40, 1), 0.08);
        assert_eq!(g(40, 3), 400.0);
        // Recovered.
        assert_eq!(g(65, 0), 300.0);
        assert_eq!(g(65, 1), 0.0);
        assert_eq!(g(65, 3), 20.0);
        // Retransmit counter accelerates at t=18 (50/s vs 1/s).
        let r = |sec| g(sec, 2);
        assert_eq!(r(18) - r(17), 1.0);
        assert_eq!(r(19) - r(18), 50.0);
        assert_eq!(r(51) - r(50), 1.0);
    }

    #[test]
    fn alert_pair_shares_key_and_correlation_id() {
        let firing = incident_alert(123, 30);
        let resolved = incident_alert(123, 60).resolved();
        assert_eq!(firing.alert_key(), resolved.alert_key());
        assert_eq!(firing.labels["correlation_id"], "inc-123");
        assert_eq!(
            route_change_event(123, 28).labels["correlation_id"],
            "inc-123"
        );
    }
}
