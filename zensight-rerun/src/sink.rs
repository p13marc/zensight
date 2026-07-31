//! The `VisualizationSink` seam and the worker that feeds it.
//!
//! Everything upstream of [`VisualizationSink`] is Rerun-free: the
//! [`SinkWorker`] does all conversion (classification, entity paths, counter
//! policy, sampling) *before* the trait, so [`TestSink`] observes exactly
//! what the real Rerun sink would receive (docs/plans/rerun/03-sink-design.md).

use std::collections::HashMap;

use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};
use zensight_common::alert::Alert;
use zensight_common::entity::HostEntity;
use zensight_common::health::{HealthSnapshot, HealthStatus};
use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};

use crate::config::{CounterPolicy, SamplingConfig};
use crate::events::NormalizedEvent;
use crate::mapping::{
    Class, EntityIndex, RateConverter, Sampler, alert_entity_path, classify,
    event_entity_path_resolved, metric_entity_path,
};
use crate::topology::{Topology, TopologyBuilder};

/// Telemetry channel capacity (drop-newest when full — a lost sample is
/// superseded by the next one; the subscriber must never block on the sink).
pub const TELEMETRY_QUEUE: usize = 4096;
/// Control channel capacity (alerts/health/entities block when full —
/// must-arrive, same reasoning as `QosClass::Alert`).
pub const CONTROL_QUEUE: usize = 1024;

/// A must-arrive item from the control-plane subscriptions.
#[derive(Debug, Clone)]
pub enum ControlItem {
    Alert(Alert),
    Health(HealthSnapshot),
    Entity(Box<HostEntity>),
}

/// The seam between the Rerun-free pipeline and the one Rerun-aware module.
///
/// All inputs are final: `path` is the complete Rerun entity path and the
/// metric `value` is the post-policy plot value.
pub trait VisualizationSink: Send {
    /// One point on a numeric time series.
    fn publish_metric(
        &mut self,
        point: &TelemetryPoint,
        path: &str,
        value: f64,
    ) -> anyhow::Result<()>;

    /// One discrete occurrence.
    fn publish_event(&mut self, event: &NormalizedEvent, path: &str) -> anyhow::Result<()>;

    /// One alert transition (firing/resolved).
    fn publish_alert(&mut self, alert: &Alert, path: &str) -> anyhow::Result<()>;

    /// One (re)computed host entity.
    fn publish_entity(&mut self, entity: &HostEntity) -> anyhow::Result<()>;

    /// The topology graph after a change, stamped at the causing timestamp.
    fn publish_topology(&mut self, topology: &Topology, timestamp: i64) -> anyhow::Result<()>;

    /// Flush any buffered data (blocking is acceptable; called on shutdown).
    fn flush(&mut self) -> anyhow::Result<()>;
}

/// Counters the worker maintains (logged on shutdown; asserted in tests).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkerStats {
    pub metrics_published: u64,
    pub events_published: u64,
    pub alerts_published: u64,
    pub entities_published: u64,
    /// `Binary` values (unrenderable) — counted, never logged.
    pub ignored_binary: u64,
    /// Points suppressed by the per-series sampler.
    pub sampled_out: u64,
    /// Counter samples absorbed by the rate converter (first sample / reset).
    pub rate_absorbed: u64,
    /// Topology graph re-publications (entity/link changes).
    pub topology_published: u64,
    /// Sink calls that returned an error (logged, not fatal).
    pub sink_errors: u64,
}

/// Drains the two bounded channels and drives a [`VisualizationSink`].
pub struct SinkWorker {
    rx_telemetry: mpsc::Receiver<TelemetryPoint>,
    rx_control: mpsc::Receiver<ControlItem>,
    sink: Box<dyn VisualizationSink>,
    index: EntityIndex,
    /// Last seen health status per (sensor, source) — transition detection.
    health: HashMap<(String, String), HealthStatus>,
    counters: CounterPolicy,
    rates: RateConverter,
    sampler: Sampler,
    topology: TopologyBuilder,
    stats: WorkerStats,
}

impl SinkWorker {
    pub fn new(
        rx_telemetry: mpsc::Receiver<TelemetryPoint>,
        rx_control: mpsc::Receiver<ControlItem>,
        sink: Box<dyn VisualizationSink>,
        counters: CounterPolicy,
        sampling: SamplingConfig,
    ) -> Self {
        Self {
            rx_telemetry,
            rx_control,
            sink,
            index: EntityIndex::default(),
            health: HashMap::new(),
            counters,
            rates: RateConverter::default(),
            sampler: Sampler::new(sampling),
            topology: TopologyBuilder::default(),
            stats: WorkerStats::default(),
        }
    }

    /// Run until shutdown flips (or both channels close); flush; return stats.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> WorkerStats {
        loop {
            tokio::select! {
                // Control items (rare, must-arrive) drain ahead of telemetry.
                biased;

                changed = shutdown.changed() => match changed {
                    // A dropped sender (Err) is a shutdown too: treating it as
                    // a no-op would leave this permanently-ready arm starving
                    // the channel arms at 100% CPU.
                    Err(_) => break,
                    Ok(()) if *shutdown.borrow() => break,
                    Ok(()) => {}
                },

                item = self.rx_control.recv() => match item {
                    Some(item) => self.handle_control(item),
                    None => break,
                },

                point = self.rx_telemetry.recv() => match point {
                    Some(point) => self.handle_point(point),
                    None => break,
                },
            }
        }

        // Shutdown drain: keep *receiving* (not try_recv) until both channels
        // return None. The subscriber observes the same shutdown signal,
        // finishes forwarding, and drops its senders — so this terminates,
        // and a control item still in flight at the shutdown flip (e.g. the
        // final Resolved transition) is never lost. Control first: mpsc
        // receivers keep yielding queued items after senders drop, and
        // re-polling a closed+empty receiver just returns None again.
        while let Some(item) = self.rx_control.recv().await {
            self.handle_control(item);
        }
        while let Some(point) = self.rx_telemetry.recv().await {
            self.handle_point(point);
        }

        if let Err(e) = self.sink.flush() {
            warn!("sink flush failed: {e}");
            self.stats.sink_errors += 1;
        }
        debug!(?self.stats, "sink worker stopped");
        self.stats
    }

    fn handle_point(&mut self, point: TelemetryPoint) {
        match classify(&point) {
            Class::Ignore => self.stats.ignored_binary += 1,
            Class::Metric => {
                let path = metric_entity_path(&point, &self.index);
                // Sampler first, rate converter after: a sub-sampled counter
                // series still differentiates the samples that pass, so rates
                // stay correct over the longer window (04-live-metrics.md).
                if !self.sampler.allow(&path, &point.metric, point.timestamp) {
                    self.stats.sampled_out += 1;
                    return;
                }
                match &point.value {
                    TelemetryValue::Gauge(v) => self.publish_metric(&point, &path, *v),
                    TelemetryValue::Boolean(b) => {
                        let v = if *b { 1.0 } else { 0.0 };
                        self.publish_metric(&point, &path, v);
                    }
                    TelemetryValue::Counter(v) => {
                        let v = *v;
                        if matches!(self.counters, CounterPolicy::Rate | CounterPolicy::Both) {
                            match self.rates.rate(&path, point.timestamp, v) {
                                Some(rate) => self.publish_metric(&point, &path, rate),
                                // First sample / reset / stale clock: absorbed.
                                None => self.stats.rate_absorbed += 1,
                            }
                        }
                        if matches!(self.counters, CounterPolicy::Raw | CounterPolicy::Both) {
                            let raw_path = match self.counters {
                                CounterPolicy::Both => format!("{path}/raw"),
                                _ => path.clone(),
                            };
                            self.publish_metric(&point, &raw_path, v as f64);
                        }
                    }
                    // classify() never routes Text/Binary here.
                    TelemetryValue::Text(_) | TelemetryValue::Binary(_) => {}
                }
            }
            Class::Event(kind) => {
                let protocol = point.protocol.as_str();
                let entity_id = self
                    .index
                    .resolve(protocol, &point.source)
                    .map(str::to_string);
                let path =
                    event_entity_path_resolved(protocol, &point.source, entity_id.as_deref());
                // Telemetry-derived events (Text values, `events/...` paths)
                // can arrive at full poll rate per host, so they honor the
                // sampling config too. The lane is shared per source, so the
                // sampler keys on path+metric to keep distinct event kinds
                // independent. Alerts/health transitions are NOT sampled —
                // they arrive on the control channel, not through here.
                let series = format!("{path}/{}", point.metric);
                if !self.sampler.allow(&series, &point.metric, point.timestamp) {
                    self.stats.sampled_out += 1;
                    return;
                }
                let event = crate::events::normalize_point(&point, kind, entity_id);
                self.publish_event(&event, &path);
                if self.topology.apply_event(&event) {
                    self.publish_topology(event.timestamp);
                }
            }
        }
    }

    fn handle_control(&mut self, item: ControlItem) {
        match item {
            ControlItem::Entity(entity) => {
                self.index.upsert(&entity);
                if let Err(e) = self.sink.publish_entity(&entity) {
                    warn!(entity = %entity.entity_id, "sink entity publish failed: {e}");
                    self.stats.sink_errors += 1;
                } else {
                    self.stats.entities_published += 1;
                }
                if self.topology.upsert_entity(&entity) {
                    self.publish_topology(entity.last_updated);
                }
            }
            ControlItem::Alert(alert) => {
                // Identity-scoped path: two hosts firing the same rule must
                // not share a lane (see mapping::alert_entity_path).
                let path = alert_entity_path(&alert);
                if let Err(e) = self.sink.publish_alert(&alert, &path) {
                    warn!(rule = %alert.rule, "sink alert publish failed: {e}");
                    self.stats.sink_errors += 1;
                } else {
                    self.stats.alerts_published += 1;
                }
            }
            ControlItem::Health(snapshot) => {
                // Same "unknown" fallback for the transition-map key and the
                // path (and events::normalize_health) — a source-less snapshot
                // must not track under "" but render under "unknown".
                let source = snapshot
                    .source
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                let key = (snapshot.sensor.clone(), source.clone());
                let previous = self.health.insert(key, snapshot.status);
                if let Some(event) = crate::events::normalize_health(&snapshot, previous) {
                    let path = format!("health/{}/{source}", snapshot.sensor);
                    self.publish_event(&event, &path);
                }
            }
        }
    }

    fn publish_metric(&mut self, point: &TelemetryPoint, path: &str, value: f64) {
        if let Err(e) = self.sink.publish_metric(point, path, value) {
            warn!(path, "sink metric publish failed: {e}");
            self.stats.sink_errors += 1;
        } else {
            self.stats.metrics_published += 1;
        }
    }

    fn publish_topology(&mut self, timestamp: i64) {
        let topology = self.topology.build();
        if let Err(e) = self.sink.publish_topology(&topology, timestamp) {
            warn!("sink topology publish failed: {e}");
            self.stats.sink_errors += 1;
        } else {
            self.stats.topology_published += 1;
        }
    }

    fn publish_event(&mut self, event: &NormalizedEvent, path: &str) {
        if let Err(e) = self.sink.publish_event(event, path) {
            warn!(path, "sink event publish failed: {e}");
            self.stats.sink_errors += 1;
        } else {
            self.stats.events_published += 1;
        }
    }
}

/// A recording sink for tests: captures exactly what Rerun would receive.
#[derive(Debug, Default)]
pub struct TestSink {
    pub metrics: Vec<(String, i64, f64)>,
    pub events: Vec<(String, NormalizedEvent)>,
    pub alerts: Vec<(String, Alert)>,
    pub entities: Vec<HostEntity>,
    pub topologies: Vec<(i64, Topology)>,
    pub flushes: u32,
}

impl VisualizationSink for TestSink {
    fn publish_metric(
        &mut self,
        point: &TelemetryPoint,
        path: &str,
        value: f64,
    ) -> anyhow::Result<()> {
        self.metrics
            .push((path.to_string(), point.timestamp, value));
        Ok(())
    }

    fn publish_event(&mut self, event: &NormalizedEvent, path: &str) -> anyhow::Result<()> {
        self.events.push((path.to_string(), event.clone()));
        Ok(())
    }

    fn publish_alert(&mut self, alert: &Alert, path: &str) -> anyhow::Result<()> {
        self.alerts.push((path.to_string(), alert.clone()));
        Ok(())
    }

    fn publish_entity(&mut self, entity: &HostEntity) -> anyhow::Result<()> {
        self.entities.push(entity.clone());
        Ok(())
    }

    fn publish_topology(&mut self, topology: &Topology, timestamp: i64) -> anyhow::Result<()> {
        self.topologies.push((timestamp, topology.clone()));
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

    use super::*;

    /// A [`TestSink`] that stays inspectable after the worker consumed it.
    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<TestSink>>);

    impl VisualizationSink for SharedSink {
        fn publish_metric(
            &mut self,
            point: &TelemetryPoint,
            path: &str,
            value: f64,
        ) -> anyhow::Result<()> {
            self.0.lock().unwrap().publish_metric(point, path, value)
        }
        fn publish_event(&mut self, event: &NormalizedEvent, path: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().publish_event(event, path)
        }
        fn publish_alert(&mut self, alert: &Alert, path: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().publish_alert(alert, path)
        }
        fn publish_entity(&mut self, entity: &HostEntity) -> anyhow::Result<()> {
            self.0.lock().unwrap().publish_entity(entity)
        }
        fn publish_topology(&mut self, topology: &Topology, timestamp: i64) -> anyhow::Result<()> {
            self.0.lock().unwrap().publish_topology(topology, timestamp)
        }
        fn flush(&mut self) -> anyhow::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }

    async fn run_worker(
        points: Vec<TelemetryPoint>,
        control: Vec<ControlItem>,
        counters: CounterPolicy,
        sampling: SamplingConfig,
    ) -> (Arc<Mutex<TestSink>>, WorkerStats) {
        let sink = SharedSink::default();
        let captured = sink.0.clone();
        let (tx_t, rx_t) = mpsc::channel(64);
        let (tx_c, rx_c) = mpsc::channel(64);
        // Control first so entity docs land before the telemetry that joins
        // on them (mirrors the seed-before-subscribe ordering).
        for item in control {
            tx_c.send(item).await.unwrap();
        }
        for point in points {
            tx_t.send(point).await.unwrap();
        }
        drop(tx_t);
        drop(tx_c);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = SinkWorker::new(rx_t, rx_c, Box::new(sink), counters, sampling);
        let stats = worker.run(shutdown_rx).await;
        (captured, stats)
    }

    fn gauge(metric: &str, ts: i64, v: f64) -> TelemetryPoint {
        let mut p =
            TelemetryPoint::new("host1", Protocol::Sysinfo, metric, TelemetryValue::Gauge(v));
        p.timestamp = ts;
        p
    }

    fn counter(metric: &str, ts: i64, v: u64) -> TelemetryPoint {
        let mut p = TelemetryPoint::new(
            "host1",
            Protocol::Netlink,
            metric,
            TelemetryValue::Counter(v),
        );
        p.timestamp = ts;
        p
    }

    #[tokio::test]
    async fn counter_rate_policy_end_to_end() {
        let points = vec![
            counter("iface/eth0/rx_bytes", 1_000, 0),
            counter("iface/eth0/rx_bytes", 2_000, 1_000),
            counter("iface/eth0/rx_bytes", 3_000, 3_000),
            counter("iface/eth0/rx_bytes", 4_000, 100), // reset
            counter("iface/eth0/rx_bytes", 5_000, 600),
        ];
        let (sink, stats) = run_worker(
            points,
            vec![],
            CounterPolicy::Rate,
            SamplingConfig::default(),
        )
        .await;
        let sink = sink.lock().unwrap();
        let series: Vec<_> = sink.metrics.iter().map(|(_, ts, v)| (*ts, *v)).collect();
        // First sample and the reset are absorbed; three rates emitted.
        assert_eq!(
            series,
            vec![(2_000, 1_000.0), (3_000, 2_000.0), (5_000, 500.0)]
        );
        assert_eq!(stats.rate_absorbed, 2);
        assert_eq!(stats.metrics_published, 3);
        assert_eq!(
            sink.metrics[0].0,
            "sensors/netlink/host1/iface/eth0/rx_bytes"
        );
    }

    #[tokio::test]
    async fn counter_both_policy_emits_two_lanes() {
        let points = vec![
            counter("iface/eth0/rx_bytes", 1_000, 0),
            counter("iface/eth0/rx_bytes", 2_000, 500),
        ];
        let (sink, _) = run_worker(
            points,
            vec![],
            CounterPolicy::Both,
            SamplingConfig::default(),
        )
        .await;
        let sink = sink.lock().unwrap();
        let paths: Vec<_> = sink.metrics.iter().map(|(p, _, _)| p.as_str()).collect();
        // t=1000: raw only (rate absorbed); t=2000: rate + raw.
        assert_eq!(
            paths,
            vec![
                "sensors/netlink/host1/iface/eth0/rx_bytes/raw",
                "sensors/netlink/host1/iface/eth0/rx_bytes",
                "sensors/netlink/host1/iface/eth0/rx_bytes/raw",
            ]
        );
    }

    #[tokio::test]
    async fn sampler_suppresses_and_counts() {
        let points = (0..10)
            .map(|i| gauge("cpu/usage", i * 100, i as f64))
            .collect();
        let (sink, stats) = run_worker(
            points,
            vec![],
            CounterPolicy::Rate,
            SamplingConfig {
                max_hz_per_series: Some(2.0), // min interval 500 ms
                per_prefix: std::collections::HashMap::new(),
            },
        )
        .await;
        let sink = sink.lock().unwrap();
        let times: Vec<_> = sink.metrics.iter().map(|(_, ts, _)| *ts).collect();
        assert_eq!(times, vec![0, 500]);
        assert_eq!(stats.sampled_out, 8);
    }

    fn event_text(metric: &str, ts: i64) -> TelemetryPoint {
        let mut p = TelemetryPoint::new(
            "host1",
            Protocol::Netlink,
            metric,
            TelemetryValue::Text("something happened".into()),
        );
        p.timestamp = ts;
        p
    }

    fn warning_alert(source: &str) -> zensight_common::alert::Alert {
        use zensight_common::alert::{Alert, AlertKind, AlertSeverity};
        Alert::new(
            source,
            Protocol::Netlink,
            AlertKind::Anomaly,
            "port-scan",
            AlertSeverity::Warning,
            "scan detected",
        )
    }

    /// Finding: dropped watch sender must read as shutdown, not busy-spin —
    /// and items queued (or still being sent) at shutdown must all arrive.
    #[tokio::test]
    async fn dropped_shutdown_sender_terminates_worker() {
        let sink = SharedSink::default();
        let captured = sink.0.clone();
        let (tx_t, rx_t) = mpsc::channel(64);
        let (tx_c, rx_c) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = SinkWorker::new(
            rx_t,
            rx_c,
            Box::new(sink),
            CounterPolicy::Rate,
            SamplingConfig::default(),
        );
        let handle = tokio::spawn(worker.run(shutdown_rx));

        // Drop the sender WITHOUT sending true: the worker must treat the
        // Err from changed() as shutdown and enter the drain, not spin.
        drop(shutdown_tx);

        // Everything sent before the senders drop must still be handled.
        tx_c.send(ControlItem::Alert(warning_alert("host1")))
            .await
            .unwrap();
        tx_t.send(gauge("cpu/usage", 1_000, 1.0)).await.unwrap();
        drop(tx_t);
        drop(tx_c);

        let stats = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("worker must terminate after the watch sender drops")
            .unwrap();
        assert_eq!(stats.alerts_published, 1);
        assert_eq!(stats.metrics_published, 1);
        assert_eq!(captured.lock().unwrap().flushes, 1);
    }

    /// Finding: the shutdown flip must not race must-arrive control items —
    /// a Resolved transition forwarded after the flip still lands.
    #[tokio::test]
    async fn control_items_in_flight_at_shutdown_are_not_lost() {
        let sink = SharedSink::default();
        let captured = sink.0.clone();
        let (tx_t, rx_t) = mpsc::channel(64);
        let (tx_c, rx_c) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let worker = SinkWorker::new(
            rx_t,
            rx_c,
            Box::new(sink),
            CounterPolicy::Rate,
            SamplingConfig::default(),
        );
        let handle = tokio::spawn(worker.run(shutdown_rx));

        let firing = warning_alert("host1");
        tx_c.send(ControlItem::Alert(firing.clone())).await.unwrap();

        // Flip shutdown, give the worker time to observe it and enter the
        // drain, THEN forward the final Resolved (the subscriber may still be
        // doing exactly this when the flip lands).
        shutdown_tx.send(true).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx_c.send(ControlItem::Alert(firing.resolved()))
            .await
            .unwrap();
        drop(tx_t);
        drop(tx_c);

        let stats = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("worker must terminate once the senders drop")
            .unwrap();
        assert_eq!(
            stats.alerts_published, 2,
            "the Resolved transition is must-arrive"
        );
        let sink = captured.lock().unwrap();
        assert_eq!(sink.alerts.len(), 2);
        // Firing and Resolved share one identity-scoped lane.
        assert_eq!(sink.alerts[0].0, sink.alerts[1].0);
    }

    /// Finding: alert paths must carry the alert's identity — two hosts
    /// firing the same rule get distinct lanes.
    #[tokio::test]
    async fn alert_paths_are_identity_scoped() {
        let a = warning_alert("host-a");
        let b = warning_alert("host-b");
        let (sink, stats) = run_worker(
            vec![],
            vec![ControlItem::Alert(a.clone()), ControlItem::Alert(b.clone())],
            CounterPolicy::Rate,
            SamplingConfig::default(),
        )
        .await;
        assert_eq!(stats.alerts_published, 2);
        let sink = sink.lock().unwrap();
        assert_eq!(
            sink.alerts[0].0,
            format!("alerts/netlink/host-a/{}", a.alert_key())
        );
        assert_eq!(
            sink.alerts[1].0,
            format!("alerts/netlink/host-b/{}", b.alert_key())
        );
        assert_ne!(sink.alerts[0].0, sink.alerts[1].0);
    }

    /// Finding: telemetry-derived events honor the sampler; control-plane
    /// alerts never do.
    #[tokio::test]
    async fn events_are_sampled_but_alerts_are_not() {
        let points = (0..10)
            .map(|i| event_text("events/tcp_reset/burst", i * 100))
            .collect();
        let control = (0..3)
            .map(|_| ControlItem::Alert(warning_alert("host1")))
            .collect();
        let (sink, stats) = run_worker(
            points,
            control,
            CounterPolicy::Rate,
            SamplingConfig {
                max_hz_per_series: Some(2.0), // min interval 500 ms
                per_prefix: std::collections::HashMap::new(),
            },
        )
        .await;
        // 10 events at 10 Hz through a 2 Hz sampler: t=0 and t=500 pass.
        assert_eq!(stats.events_published, 2, "{stats:?}");
        assert_eq!(stats.sampled_out, 8, "{stats:?}");
        // Alerts bypass the sampler entirely (must-arrive).
        assert_eq!(stats.alerts_published, 3, "{stats:?}");
        assert_eq!(sink.lock().unwrap().events.len(), 2);
    }

    #[tokio::test]
    async fn entity_arrival_moves_series_to_host_path() {
        use zensight_common::entity::{HostEntity, MemberClaim};
        let entity = HostEntity {
            entity_id: "h_0123456789ab".into(),
            aliases: vec![],
            host_id: None,
            boot_id: None,
            ips: vec![],
            macs: vec![],
            container_ids: vec![],
            hostname: Some("host1".into()),
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            os_name: None,
            os_version: None,
            kernel: None,
            arch: None,
            members: vec![MemberClaim {
                sensor: "sysinfo".into(),
                source: "host1".into(),
                rule: "host_id".into(),
                confidence: 1.0,
                last_seen: 1,
            }],
            status: None,
            last_updated: 1,
        };
        let (sink, stats) = run_worker(
            vec![gauge("cpu/usage", 1_000, 42.0)],
            vec![ControlItem::Entity(Box::new(entity))],
            CounterPolicy::Rate,
            SamplingConfig::default(),
        )
        .await;
        let sink = sink.lock().unwrap();
        assert_eq!(sink.metrics[0].0, "hosts/h_0123456789ab/sysinfo/cpu/usage");
        assert_eq!(stats.entities_published, 1);
        assert_eq!(sink.flushes, 1);
    }
}
