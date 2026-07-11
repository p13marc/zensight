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
use crate::topology::{NodeStatus, Topology};

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

    /// Bundle string fields into an [`rerun::AnyValues`] (each field becomes a
    /// `Text`-typed component column named after the key). Logged on the same
    /// path/timepoint as the `TextLog`, so the selection panel and dataframe
    /// queries see structured columns next to the prose lane.
    fn any_values<'a>(fields: impl IntoIterator<Item = (&'a str, &'a str)>) -> rerun::AnyValues {
        fields
            .into_iter()
            .fold(rerun::AnyValues::default(), |acc, (key, value)| {
                acc.with_component::<rerun::components::Text>(key, [value.to_string()])
            })
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

        // Structured lane: typed fields + attributes as queryable columns.
        // Two log calls per event is a Rerun-modeling wart, not a choice —
        // see docs/plans/rerun/05-events.md §2.
        let protocol = event.protocol.map(|p| p.as_str().to_string());
        let mut fields: Vec<(&str, &str)> = vec![
            ("kind", event.kind.as_str()),
            ("severity", event.severity.as_str()),
            ("event_source", event.source.as_str()),
        ];
        if let Some(protocol) = protocol.as_deref() {
            fields.push(("protocol", protocol));
        }
        if let Some(target) = event.target.as_deref() {
            fields.push(("target", target));
        }
        if let Some(interface) = event.interface.as_deref() {
            fields.push(("interface", interface));
        }
        if let Some(entity_id) = event.entity_id.as_deref() {
            fields.push(("entity_id", entity_id));
        }
        if let Some(correlation_id) = event.correlation_id.as_deref() {
            fields.push(("correlation_id", correlation_id));
        }
        fields.extend(
            event
                .attributes
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        );
        self.rec.log(path, &Self::any_values(fields))?;
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

        // Structured lane (alert identity + context labels).
        let alert_key = alert.alert_key();
        let state = match alert.state {
            AlertState::Firing => "firing",
            AlertState::Resolved => "resolved",
        };
        let protocol = alert.protocol.as_str();
        let mut fields: Vec<(&str, &str)> = vec![
            ("alert_key", alert_key.as_str()),
            ("rule", alert.rule.as_str()),
            ("state", state),
            ("severity", alert.severity.as_str()),
            ("kind", alert.kind.as_str()),
            ("alert_source", alert.source.as_str()),
            ("protocol", protocol),
        ];
        fields.extend(alert.labels.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        self.rec.log(path, &Self::any_values(fields))?;
        Ok(())
    }

    fn publish_entity(&mut self, entity: &HostEntity) -> anyhow::Result<()> {
        // Static identity card at the host root: selecting the host in the
        // viewer shows who it is next to its metric/event lanes.
        let path = format!("hosts/{}", entity.entity_id);
        let ips = entity.ips.join(",");
        let macs = entity.macs.join(",");
        let members = entity.members.len().to_string();
        let mut fields: Vec<(&str, &str)> = vec![
            ("entity_id", entity.entity_id.as_str()),
            ("ips", ips.as_str()),
            ("macs", macs.as_str()),
            ("member_count", members.as_str()),
        ];
        if let Some(hostname) = entity.hostname.as_deref() {
            fields.push(("hostname", hostname));
        }
        if let Some(fqdn) = entity.fqdn.as_deref() {
            fields.push(("fqdn", fqdn));
        }
        if let Some(status) = entity.status.as_deref() {
            fields.push(("status", status));
        }
        self.rec.log_static(path, &Self::any_values(fields))?;
        Ok(())
    }

    fn publish_topology(&mut self, topology: &Topology, timestamp: i64) -> anyhow::Result<()> {
        self.set_time(timestamp);

        let ids: Vec<&str> = topology.nodes.iter().map(|n| n.id.as_str()).collect();
        let labels: Vec<&str> = topology.nodes.iter().map(|n| n.label.as_str()).collect();
        // Status colors (severity palette shared with the alert lanes). The
        // frontend's D2 color guard scopes to zensight/src — data colors for
        // the Rerun graph live here by design.
        let colors: Vec<rerun::Color> = topology
            .nodes
            .iter()
            .map(|n| match n.status {
                NodeStatus::Up => rerun::Color::from_rgb(0x4c, 0xaf, 0x50),
                NodeStatus::Degraded => rerun::Color::from_rgb(0xff, 0xb3, 0x00),
                NodeStatus::Down => rerun::Color::from_rgb(0xef, 0x53, 0x50),
                NodeStatus::Unknown => rerun::Color::from_rgb(0x9e, 0x9e, 0x9e),
            })
            .collect();
        let nodes = rerun::archetypes::GraphNodes::new(ids)
            .with_labels(labels)
            .with_colors(colors);

        // GraphEdges in 0.34 has no per-edge styling — a downed link is
        // rendered by *omitting* the edge (the peer node stays, colored Down),
        // so scrubbing over a link-down event makes the edge disappear.
        // Recorded as an expressiveness gap in 09-topology.md.
        let up_edges: Vec<(&str, &str)> = topology
            .edges
            .iter()
            .filter(|e| e.up)
            .map(|e| (e.from.as_str(), e.to.as_str()))
            .collect();
        let edges = rerun::archetypes::GraphEdges::new(up_edges).with_directed_edges();

        let both: [&dyn rerun::AsComponents; 2] = [&nodes, &edges];
        self.rec.log("topology/hosts", &both)?;
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        self.rec.flush_blocking()?;
        Ok(())
    }
}
