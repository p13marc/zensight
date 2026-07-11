//! The ONLY module allowed to `use rerun` (grep-gated in CI/docs).
//!
//! Implements [`VisualizationSink`] over a [`rerun::RecordingStream`]. All
//! mapping decisions were made upstream (mapping.rs/events.rs); this module
//! only translates the already-final inputs into Rerun archetypes and
//! timeline calls (API names pinned in docs/plans/rerun/01-capabilities.md).

use std::collections::HashMap;

use zensight_common::alert::{Alert, AlertState};
use zensight_common::entity::HostEntity;
use zensight_common::telemetry::TelemetryPoint;

use crate::config::{RerunMode, RerunSinkConfig};
use crate::events::{EventSeverity, NormalizedEvent};
use crate::mapping::{severity_from_alert, severity_weight};
use crate::sink::VisualizationSink;

/// The domain timeline: ZenSight epoch-millisecond timestamps. Every log call
/// stamps this; Rerun's auto `log_time` (receive time) stays diagnostic-only.
pub const TIMELINE: &str = "zensight_time";

/// [`VisualizationSink`] over a Rerun `RecordingStream`.
pub struct RerunSink {
    rec: rerun::RecordingStream,
    /// Entity paths whose `SeriesLines` styling was already logged.
    styled: HashMap<String, ()>,
}

impl RerunSink {
    /// Build the recording stream for the configured mode.
    pub fn new(config: &RerunSinkConfig) -> anyhow::Result<Self> {
        let mut builder = rerun::RecordingStreamBuilder::new(config.application_id.as_str());
        if let Some(recording_id) = &config.recording_id {
            builder = builder.recording_id(recording_id.clone());
        }

        let rec = match config.mode {
            RerunMode::Live => builder.connect_grpc_opts(config.viewer_url.clone())?,
            RerunMode::Record => {
                let path = config
                    .rrd_path
                    .as_ref()
                    .expect("validated: record mode has rrd_path");
                builder.save(path)?
            }
            RerunMode::Both => {
                let path = config
                    .rrd_path
                    .as_ref()
                    .expect("validated: both mode has rrd_path");
                builder.set_sinks((
                    rerun::sink::GrpcSink::new(config.viewer_url.as_str().parse()?),
                    rerun::sink::FileSink::new(path)?,
                ))?
            }
        };

        Ok(Self {
            rec,
            styled: HashMap::new(),
        })
    }

    /// Stamp the domain timeline from an epoch-milliseconds timestamp.
    fn set_time(&self, timestamp_ms: i64) {
        self.rec
            .set_timestamp_nanos_since_epoch(TIMELINE, timestamp_ms.saturating_mul(1_000_000));
    }

    fn level(severity: EventSeverity) -> rerun::TextLogLevel {
        // The TextLogLevel associated constants are `&'static str` in 0.34.
        match severity {
            EventSeverity::Debug => rerun::TextLogLevel::DEBUG.into(),
            EventSeverity::Info => rerun::TextLogLevel::INFO.into(),
            EventSeverity::Warning => rerun::TextLogLevel::WARN.into(),
            EventSeverity::Error => rerun::TextLogLevel::ERROR.into(),
            EventSeverity::Critical => rerun::TextLogLevel::CRITICAL.into(),
        }
    }
}

impl VisualizationSink for RerunSink {
    fn publish_metric(
        &mut self,
        point: &TelemetryPoint,
        path: &str,
        value: f64,
    ) -> anyhow::Result<()> {
        // Style the series once, statically, on first sight: named after the
        // metric leaf so the plot legend reads e.g. "usage", not a full path.
        if !self.styled.contains_key(path) {
            self.styled.insert(path.to_string(), ());
            let name = point.metric.rsplit('/').next().unwrap_or(&point.metric);
            self.rec.log_static(
                path,
                &rerun::archetypes::SeriesLines::new().with_names([name]),
            )?;
        }
        self.set_time(point.timestamp);
        self.rec
            .log(path, &rerun::archetypes::Scalars::single(value))?;
        Ok(())
    }

    fn publish_event(&mut self, event: &NormalizedEvent, path: &str) -> anyhow::Result<()> {
        self.set_time(event.timestamp);
        self.rec.log(
            path,
            &rerun::archetypes::TextLog::new(event.message.clone())
                .with_level(Self::level(event.severity)),
        )?;
        Ok(())
    }

    fn publish_alert(&mut self, alert: &Alert, path: &str) -> anyhow::Result<()> {
        self.set_time(alert.timestamp);
        let (text, level, weight) = match alert.state {
            AlertState::Firing => (
                format!("[FIRING] {}", alert.summary),
                Self::level(severity_from_alert(alert.severity)),
                severity_weight(alert.severity),
            ),
            AlertState::Resolved => (
                format!("[RESOLVED] {}", alert.summary),
                rerun::TextLogLevel::INFO.into(),
                0.0,
            ),
        };
        self.rec.log(
            path,
            &rerun::archetypes::TextLog::new(text).with_level(level),
        )?;
        // The firing-window lane: severity weight while firing, 0 on resolve.
        self.rec.log(
            format!("{path}/state"),
            &rerun::archetypes::Scalars::single(weight),
        )?;
        Ok(())
    }

    fn publish_entity(&mut self, _entity: &HostEntity) -> anyhow::Result<()> {
        // #421 adds the static AnyValues identity card at hosts/<entity_id>;
        // until then entities only feed the (Rerun-free) EntityIndex.
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.rec.flush_blocking()?;
        Ok(())
    }
}
