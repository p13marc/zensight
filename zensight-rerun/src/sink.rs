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

use crate::events::NormalizedEvent;
use crate::mapping::{Class, EntityIndex, classify, event_entity_path, metric_entity_path};

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
    stats: WorkerStats,
}

impl SinkWorker {
    pub fn new(
        rx_telemetry: mpsc::Receiver<TelemetryPoint>,
        rx_control: mpsc::Receiver<ControlItem>,
        sink: Box<dyn VisualizationSink>,
    ) -> Self {
        Self {
            rx_telemetry,
            rx_control,
            sink,
            index: EntityIndex::default(),
            health: HashMap::new(),
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
                // NOTE: counter policy (rate conversion) and sampling land in
                // #420; until then counters plot raw.
                let value = match &point.value {
                    TelemetryValue::Gauge(v) => *v,
                    TelemetryValue::Counter(v) => *v as f64,
                    TelemetryValue::Boolean(b) => {
                        if *b {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    // classify() never routes Text/Binary here.
                    TelemetryValue::Text(_) | TelemetryValue::Binary(_) => return,
                };
                self.publish_metric(&point, &path, value);
            }
            Class::Event(kind) => {
                let path = event_entity_path(point.protocol.as_str(), &point.source, &self.index);
                let entity_id = self
                    .index
                    .resolve(point.protocol.as_str(), &point.source)
                    .map(str::to_string);
                let event = crate::events::normalize_point(&point, kind, entity_id);
                self.publish_event(&event, &path);
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

    fn flush(&mut self) -> anyhow::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}
