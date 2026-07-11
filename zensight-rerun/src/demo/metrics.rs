//! `zensight-rerun-demo metrics` — six live series on real-shaped keys
//! (docs/plans/rerun/04-live-metrics.md §1).

use std::time::Duration;

use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use super::DemoContext;

/// The demo host's `source` key segment.
pub const SOURCE: &str = "demo-host";

/// Build the six points for one tick. Deterministic in `tick` (`base_ts` +
/// `tick * interval` drives both the waveforms and the timestamps), so tests
/// and replays are reproducible.
pub fn points_for_tick(base_ts: i64, interval_ms: u64, tick: u64) -> Vec<TelemetryPoint> {
    let ts = base_ts + (tick * interval_ms) as i64;
    let t = tick as f64 * (interval_ms as f64 / 1000.0); // seconds since start
    let point = |protocol: Protocol, metric: &str, value: TelemetryValue| {
        let mut p = TelemetryPoint::new(SOURCE, protocol, metric, value);
        p.timestamp = ts;
        p
    };

    // Counter with a deliberate mid-run reset (tick 20 wraps to zero) —
    // exercises the rate converter's re-arm path. ~1.2 MB/s nominal.
    let rx_bytes = (tick % 20) * (1_200_000 * interval_ms / 1000);

    vec![
        point(
            Protocol::Sysinfo,
            "cpu/usage",
            TelemetryValue::Gauge(35.0 + 15.0 * (t / 5.0).sin()),
        ),
        point(
            Protocol::Sysinfo,
            "memory/usage_percent",
            TelemetryValue::Gauge(40.0 + t * 0.3),
        ),
        point(
            Protocol::Netlink,
            "iface/eth0/rx_bytes",
            TelemetryValue::Counter(rx_bytes),
        ),
        point(
            Protocol::Netring,
            "path/gateway/rtt_ms",
            TelemetryValue::Gauge(20.0 + 3.0 * (t * 1.7).sin()),
        ),
        point(
            Protocol::Netring,
            "path/gateway/loss_percent",
            TelemetryValue::Gauge(if tick.is_multiple_of(13) { 1.5 } else { 0.0 }),
        ),
        point(
            Protocol::Netlink,
            "wifi/wlan0/rssi_dbm",
            TelemetryValue::Gauge(-60.0 + 2.0 * (t * 0.9).cos()),
        ),
    ]
}

/// Publish the metric scenario: `duration_secs` of ticks every `interval_ms`.
/// Returns the number of points published.
pub async fn run(ctx: &DemoContext, duration_secs: u64, interval_ms: u64) -> anyhow::Result<u64> {
    let base_ts = zensight_common::telemetry::current_timestamp_millis();
    let ticks = duration_secs * 1000 / interval_ms;
    let mut published = 0u64;
    for tick in 0..ticks {
        for p in points_for_tick(base_ts, interval_ms, tick) {
            ctx.publish_point(&p).await?;
            published += 1;
        }
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }
    Ok(published)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_points_are_deterministic_and_real_shaped() {
        let a = points_for_tick(1_000_000, 500, 3);
        let b = points_for_tick(1_000_000, 500, 3);
        assert_eq!(a.len(), 6);
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.timestamp, y.timestamp);
            assert_eq!(x.metric, y.metric);
            assert_eq!(x.value, y.value);
        }
        assert_eq!(a[0].timestamp, 1_001_500);
        // Real sensor metric names (verified against sysinfo/netlink sources).
        assert_eq!(a[0].metric, "cpu/usage");
        assert_eq!(a[1].metric, "memory/usage_percent");
        assert_eq!(a[2].metric, "iface/eth0/rx_bytes");
    }

    #[test]
    fn rx_bytes_counter_resets_every_20_ticks() {
        let v = |tick| match points_for_tick(0, 500, tick)[2].value {
            TelemetryValue::Counter(v) => v,
            _ => panic!("rx_bytes must be a counter"),
        };
        assert!(v(19) > v(1));
        assert_eq!(v(20), 0); // the reset the rate converter must absorb
        assert!(v(21) > 0);
    }
}
