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
    Class, EntityIndex, RateConverter, Sampler, classify, event_entity_path, metric_entity_path,
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

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }

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

        // Drain whatever is already queued before flushing.
        while let Ok(item) = self.rx_control.try_recv() {
            self.handle_control(item);
        }
        while let Ok(point) = self.rx_telemetry.try_recv() {
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
                let path = event_entity_path(point.protocol.as_str(), &point.source, &self.index);
                let entity_id = self
                    .index
                    .resolve(point.protocol.as_str(), &point.source)
                    .map(str::to_string);
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
                let path = format!("alerts/{}/{}", alert.protocol.as_str(), alert.rule);
                if let Err(e) = self.sink.publish_alert(&alert, &path) {
                    warn!(rule = %alert.rule, "sink alert publish failed: {e}");
                    self.stats.sink_errors += 1;
                } else {
                    self.stats.alerts_published += 1;
                }
            }
            ControlItem::Health(snapshot) => {
                let source = snapshot.source.clone().unwrap_or_default();
                let key = (snapshot.sensor.clone(), source);
                let previous = self.health.insert(key, snapshot.status);
                if let Some(event) = crate::events::normalize_health(&snapshot, previous) {
                    let path = format!(
                        "health/{}/{}",
                        snapshot.sensor,
                        snapshot.source.as_deref().unwrap_or("unknown")
                    );
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
