//! The adapter's Rerun-free event model.
//!
//! Everything that is *discrete* on the bus — `Text` telemetry, `events/…`
//! control-plane telemetry, alert transitions, health-status transitions —
//! normalizes into one [`NormalizedEvent`] shape before it reaches the sink,
//! so the mapping (docs/plans/rerun/02-mapping.md §4–§7) is testable without
//! the `rerun` dependency.

use std::collections::BTreeMap;

use zensight_common::telemetry::Protocol;

/// Severity of a normalized event (maps onto Rerun's `TextLogLevel`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum EventSeverity {
    Debug,
    #[default]
    Info,
    Warning,
    Error,
    Critical,
}

impl EventSeverity {
    /// Stable lowercase name.
    pub fn as_str(&self) -> &'static str {
        match self {
            EventSeverity::Debug => "debug",
            EventSeverity::Info => "info",
            EventSeverity::Warning => "warning",
            EventSeverity::Error => "error",
            EventSeverity::Critical => "critical",
        }
    }
}

/// What happened. The catalogue covers the discrete occurrences ZenSight
/// sensors put on the bus today (netlink control-plane timeline, netring
/// detectors, alert/health lifecycles); anything unrecognized becomes
/// `Other(tag)` rather than being dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    TcpOpen,
    TcpReset,
    TcpTimeout,
    IcmpUnreachable,
    PacketLoss,
    LinkDegraded,
    PeerUp,
    PeerDown,
    RouteChange,
    Restart,
    Anomaly,
    AlertFiring,
    AlertResolved,
    HealthChange,
    Other(String),
}

impl EventKind {
    /// Stable snake_case tag (used as the `kind` attribute).
    pub fn as_str(&self) -> &str {
        match self {
            EventKind::TcpOpen => "tcp_open",
            EventKind::TcpReset => "tcp_reset",
            EventKind::TcpTimeout => "tcp_timeout",
            EventKind::IcmpUnreachable => "icmp_unreachable",
            EventKind::PacketLoss => "packet_loss",
            EventKind::LinkDegraded => "link_degraded",
            EventKind::PeerUp => "peer_up",
            EventKind::PeerDown => "peer_down",
            EventKind::RouteChange => "route_change",
            EventKind::Restart => "restart",
            EventKind::Anomaly => "anomaly",
            EventKind::AlertFiring => "alert_firing",
            EventKind::AlertResolved => "alert_resolved",
            EventKind::HealthChange => "health_change",
            EventKind::Other(tag) => tag,
        }
    }
}

/// One discrete occurrence, normalized. This is what
/// [`VisualizationSink::publish_event`](crate::sink::VisualizationSink) receives.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedEvent {
    /// Unix epoch millis — always the *domain* timestamp.
    pub timestamp: i64,
    pub kind: EventKind,
    pub severity: EventSeverity,
    /// Host / sensor identifier (telemetry `source`).
    pub source: String,
    pub protocol: Option<Protocol>,
    /// What the event is about (peer address, unit name, rule, ...).
    pub target: Option<String>,
    /// Network interface, when the event is interface-scoped.
    pub interface: Option<String>,
    /// Correlated host entity, when the source is resolved.
    pub entity_id: Option<String>,
    /// Free-form correlation handle (e.g. `inc-<base_ts>`).
    pub correlation_id: Option<String>,
    /// Human-readable one-liner (the `TextLog` body).
    pub message: String,
    /// Structured context (sorted for deterministic emission).
    pub attributes: BTreeMap<String, String>,
}

impl NormalizedEvent {
    /// Minimal constructor; enrich with the builder-style setters.
    pub fn new(
        timestamp: i64,
        kind: EventKind,
        severity: EventSeverity,
        source: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            timestamp,
            kind,
            severity,
            source: source.into(),
            protocol: None,
            target: None,
            interface: None,
            entity_id: None,
            correlation_id: None,
            message: message.into(),
            attributes: BTreeMap::new(),
        }
    }

    pub fn with_protocol(mut self, protocol: Protocol) -> Self {
        self.protocol = Some(protocol);
        self
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }
}

/// Normalize an event-shaped telemetry point (`Text` value or `events/…`
/// path). Labels become attributes; well-known labels (`peer`, `iface`,
/// `correlation_id`) are lifted into the typed fields.
pub fn normalize_point(
    point: &zensight_common::telemetry::TelemetryPoint,
    kind: EventKind,
    entity_id: Option<String>,
) -> NormalizedEvent {
    use zensight_common::telemetry::TelemetryValue;

    let message = match &point.value {
        TelemetryValue::Text(text) => text.clone(),
        _ => format!("{} ({})", point.metric, kind.as_str()),
    };
    let mut event = NormalizedEvent::new(
        point.timestamp,
        kind,
        EventSeverity::Info,
        point.source.clone(),
        message,
    )
    .with_protocol(point.protocol);
    event.entity_id = entity_id;
    event
        .attributes
        .insert("metric".into(), point.metric.clone());
    for (key, value) in &point.labels {
        match key.as_str() {
            "peer" | "target" => event.target = Some(value.clone()),
            "iface" | "interface" => event.interface = Some(value.clone()),
            "correlation_id" => event.correlation_id = Some(value.clone()),
            _ => {
                event.attributes.insert(key.clone(), value.clone());
            }
        }
    }
    event
}

/// Normalize a health snapshot into a transition event — `None` when the
/// status did not change (steady-state snapshots are noise, 02-mapping.md §7).
pub fn normalize_health(
    snapshot: &zensight_common::health::HealthSnapshot,
    previous: Option<zensight_common::health::HealthStatus>,
) -> Option<NormalizedEvent> {
    use zensight_common::health::HealthStatus;

    if previous == Some(snapshot.status) {
        return None;
    }
    let severity = match snapshot.status {
        HealthStatus::Healthy | HealthStatus::Starting => EventSeverity::Info,
        HealthStatus::Degraded => EventSeverity::Warning,
        HealthStatus::Unhealthy | HealthStatus::Error | HealthStatus::Offline => {
            EventSeverity::Error
        }
    };
    let source = snapshot.source.clone().unwrap_or_else(|| "unknown".into());
    let message = match previous {
        Some(prev) => format!("{} health: {prev} -> {}", snapshot.sensor, snapshot.status),
        None => format!("{} health: {}", snapshot.sensor, snapshot.status),
    };
    // HealthSnapshot carries no wire timestamp — transition-observation time
    // is the best available domain time (documented exception, 02-mapping §7).
    let mut event = NormalizedEvent::new(
        zensight_common::telemetry::current_timestamp_millis(),
        EventKind::HealthChange,
        severity,
        source,
        message,
    );
    event
        .attributes
        .insert("sensor".into(), snapshot.sensor.clone());
    event
        .attributes
        .insert("status".into(), snapshot.status.to_string());
    if let Some(prev) = previous {
        event.attributes.insert("previous".into(), prev.to_string());
    }
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::health::{HealthSnapshot, HealthStatus};
    use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};

    #[test]
    fn text_point_becomes_message() {
        let point = TelemetryPoint::new(
            "host1",
            Protocol::Netlink,
            "events/route/replace",
            TelemetryValue::Text("default via 10.0.0.1".into()),
        )
        .with_label("iface", "eth0")
        .with_label("correlation_id", "inc-1");
        let event = normalize_point(&point, EventKind::RouteChange, Some("h_abc".into()));
        assert_eq!(event.message, "default via 10.0.0.1");
        assert_eq!(event.kind, EventKind::RouteChange);
        assert_eq!(event.interface.as_deref(), Some("eth0"));
        assert_eq!(event.correlation_id.as_deref(), Some("inc-1"));
        assert_eq!(event.entity_id.as_deref(), Some("h_abc"));
        assert_eq!(event.attributes["metric"], "events/route/replace");
        // lifted labels don't duplicate into attributes
        assert!(!event.attributes.contains_key("iface"));
    }

    fn snapshot(status: HealthStatus) -> HealthSnapshot {
        HealthSnapshot {
            sensor: "netlink".into(),
            status,
            uptime_secs: 1,
            devices_total: 0,
            devices_responding: 0,
            devices_failed: 0,
            last_poll_duration_ms: 0,
            errors_last_hour: 0,
            metrics_published: 0,
            host_id: None,
            source: Some("host1".into()),
        }
    }

    #[test]
    fn health_transitions_only() {
        // First sight emits (baseline visibility)...
        let first = normalize_health(&snapshot(HealthStatus::Healthy), None).unwrap();
        assert_eq!(first.kind, EventKind::HealthChange);
        assert_eq!(first.severity, EventSeverity::Info);
        // ...steady state does not...
        assert!(
            normalize_health(
                &snapshot(HealthStatus::Healthy),
                Some(HealthStatus::Healthy)
            )
            .is_none()
        );
        // ...a degradation does, at Warning+.
        let degraded = normalize_health(
            &snapshot(HealthStatus::Degraded),
            Some(HealthStatus::Healthy),
        )
        .unwrap();
        assert_eq!(degraded.severity, EventSeverity::Warning);
        assert!(degraded.message.contains("healthy -> degraded"));
    }
}
