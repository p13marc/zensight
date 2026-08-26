//! Metric collector that stores and manages Prometheus metrics.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tracing::{debug, trace, warn};
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use crate::config::{AggregationConfig, FilterConfig, PrometheusConfig};
use zensight_common::exposition::{MetricIdentity, MetricKind, identify};

use crate::mapping::{
    PrometheusType, extract_numeric_value, sanitize_label_name, sanitize_metric_name,
};

/// A unique identifier for a metric time series.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SeriesKey {
    /// The full Prometheus metric name.
    pub name: String,
    /// Sorted label key-value pairs.
    pub labels: Vec<(String, String)>,
}

impl SeriesKey {
    /// Build a series key from a registry-derived identity.
    ///
    /// The name comes from the *registered pattern*, not from the payload
    /// (#764). A semconv-mapped identity already carries a complete dotted
    /// name and takes no producer chunk; everything else is
    /// `<producer>_<pattern literals>`.
    pub fn from_identity(identity: &MetricIdentity, prefix: &str) -> Self {
        let joined = identity.name.join("_");
        let sanitized = sanitize_metric_name(&joined);
        let base = if prefix.is_empty() {
            sanitized
        } else {
            format!("{prefix}_{sanitized}")
        };
        // `_total` for counters, `_bytes`/`_seconds`/… from the unit (#767).
        // Values are never rescaled — see `mapping::unit_suffix`.
        let kind = match identity.kind {
            MetricKind::Counter => PrometheusType::Counter,
            MetricKind::Text => PrometheusType::Text,
            _ => PrometheusType::Gauge,
        };
        let name = crate::mapping::apply_conventions(&base, kind, identity.unit.as_deref());

        Self {
            name,
            // Already merged, sorted and unique by `exposition::identify`.
            labels: identity.labels.clone(),
        }
    }

    /// Format labels for Prometheus exposition format.
    pub fn format_labels(&self) -> String {
        if self.labels.is_empty() {
            return String::new();
        }

        let parts: Vec<String> = self
            .labels
            .iter()
            .map(|(k, v)| format!("{}=\"{}\"", k, escape_label_value(v)))
            .collect();

        format!("{{{}}}", parts.join(","))
    }
}

/// A stored metric value with metadata.
#[derive(Debug, Clone)]
pub struct StoredMetric {
    /// The series identifier.
    pub key: SeriesKey,
    /// The metric type.
    pub metric_type: PrometheusType,
    /// The current value (for numeric metrics).
    pub value: Option<f64>,
    /// Text value (for text/info metrics).
    pub text_value: Option<String>,
    /// Label name the text value rides under, derived from the subject leaf
    /// (#752). `None` for numeric metrics.
    pub text_label: Option<String>,
    /// The registry's sentence for this subject, rendered as `# HELP` (#768).
    pub help: Option<String>,
    /// The resolved unit, appended to `# HELP` when it did not become a name
    /// suffix.
    pub unit: Option<String>,
    /// When this metric was last updated.
    pub last_updated: Instant,
    /// Original timestamp from the telemetry point.
    pub timestamp_ms: i64,
}

impl StoredMetric {
    /// Build a stored metric from a registry-derived identity and its point.
    ///
    /// `None` for values no exporter can represent (binary), and for the
    /// per-line log events at `events/<uid>` — one `info` series per log line
    /// would explode cardinality (#104); logs belong on a logs pipeline.
    pub fn from_identity(
        identity: &MetricIdentity,
        point: &TelemetryPoint,
        prefix: &str,
    ) -> Option<Self> {
        if identity.kind == MetricKind::Unsupported {
            return None;
        }
        if point.protocol == Protocol::Logs && point.metric.starts_with("events/") {
            return None;
        }

        let key = SeriesKey::from_identity(identity, prefix);
        let metric_type = match identity.kind {
            MetricKind::Counter => PrometheusType::Counter,
            MetricKind::Gauge => PrometheusType::Gauge,
            MetricKind::Text => PrometheusType::Text,
            MetricKind::Unsupported => return None,
        };
        let value = extract_numeric_value(&point.value);
        let text_value = match &point.value {
            TelemetryValue::Text(s) => Some(crate::mapping::clamp_text(s)),
            _ => None,
        };
        let text_label = text_value
            .is_some()
            .then(|| crate::mapping::text_label_name(&point.metric));

        Some(Self {
            key,
            metric_type,
            value,
            text_value,
            text_label,
            help: identity.description.clone(),
            unit: identity.unit.clone(),
            last_updated: Instant::now(),
            timestamp_ms: point.timestamp,
        })
    }

    /// The family name this metric is exposed under.
    ///
    /// Text points get an `_info` suffix (#752) so they can never share a
    /// `# TYPE` block with a numeric family of the same name. Everything else
    /// is exposed under its stored name.
    pub fn emitted_name(&self) -> String {
        match self.metric_type {
            // Idempotent, like the `_total` and unit suffixes (#767): a subject
            // whose leaf is already `info` (netlink's `iface/{iface}/info`)
            // would otherwise render `..._iface_info_info`.
            PrometheusType::Text if self.key.name.ends_with(crate::mapping::INFO_SUFFIX) => {
                self.key.name.clone()
            }
            PrometheusType::Text => {
                format!("{}{}", self.key.name, crate::mapping::INFO_SUFFIX)
            }
            _ => self.key.name.clone(),
        }
    }

    /// Check if this metric is stale based on the timeout.
    pub fn is_stale(&self, timeout: Duration) -> bool {
        self.last_updated.elapsed() > timeout
    }
}

/// Filter for telemetry points.
pub struct MetricFilter {
    include_protocols: Vec<String>,
    exclude_protocols: Vec<String>,
    include_sources: Vec<String>,
    exclude_sources: Vec<String>,
    include_metrics: Vec<glob::Pattern>,
    exclude_metrics: Vec<glob::Pattern>,
}

impl MetricFilter {
    /// Create a new filter from configuration.
    pub fn new(config: &FilterConfig) -> Self {
        let include_metrics = config
            .include_metrics
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();

        let exclude_metrics = config
            .exclude_metrics
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();

        Self {
            include_protocols: config.include_protocols.clone(),
            exclude_protocols: config.exclude_protocols.clone(),
            include_sources: config.include_sources.clone(),
            exclude_sources: config.exclude_sources.clone(),
            include_metrics,
            exclude_metrics,
        }
    }

    /// Check if a telemetry point should be included.
    pub fn should_include(&self, point: &TelemetryPoint) -> bool {
        let protocol = point.protocol.as_str();

        // Check protocol filters
        if !self.include_protocols.is_empty()
            && !self.include_protocols.iter().any(|p| p == protocol)
        {
            return false;
        }
        if self.exclude_protocols.iter().any(|p| p == protocol) {
            return false;
        }

        // Check source filters
        if !self.include_sources.is_empty()
            && !self.include_sources.iter().any(|s| s == &point.source)
        {
            return false;
        }
        if self.exclude_sources.iter().any(|s| s == &point.source) {
            return false;
        }

        // Check metric name filters
        if !self.include_metrics.is_empty()
            && !self
                .include_metrics
                .iter()
                .any(|p| p.matches(&point.metric))
        {
            return false;
        }
        if self
            .exclude_metrics
            .iter()
            .any(|p| p.matches(&point.metric))
        {
            return false;
        }

        true
    }
}

impl Default for MetricFilter {
    fn default() -> Self {
        Self::new(&FilterConfig::default())
    }
}

/// Thread-safe metric collector.
pub struct MetricCollector {
    /// Stored metrics indexed by series key.
    metrics: RwLock<HashMap<SeriesKey, StoredMetric>>,
    /// Prometheus configuration.
    prometheus_config: PrometheusConfig,
    /// Aggregation configuration.
    aggregation_config: AggregationConfig,
    /// Metric filter.
    filter: MetricFilter,
    /// Currently-firing sensor alerts (rendered as a `<prefix>_alert` gauge).
    alerts: crate::alerts::AlertStore,
    /// Statistics.
    stats: RwLock<CollectorStats>,
}

/// Collector statistics.
#[derive(Debug, Clone, Default)]
pub struct CollectorStats {
    /// Total telemetry points received.
    pub points_received: u64,
    /// Points that passed the filter.
    pub points_accepted: u64,
    /// Points rejected by filter.
    pub points_filtered: u64,
    /// Points rejected because they're not exportable (Binary).
    pub points_not_exportable: u64,
    /// Points rejected because max_series was reached.
    pub points_dropped_max_series: u64,
    /// Number of stale metrics removed.
    pub stale_metrics_removed: u64,
    /// Number of render errors (write failures during exposition).
    pub render_errors: u64,
    /// Points whose key could not be refined through the registry, by reason.
    ///
    /// "A subject that is not registered does not exist" is worth nothing if
    /// the exporter quietly invents a name for it anyway (#764), so this is
    /// counted and exposed rather than swallowed.
    pub points_unrefined: BTreeMap<&'static str, u64>,
    /// Label candidates dropped because a stronger stage held the name (#753).
    pub labels_shadowed: u64,
}

impl MetricCollector {
    /// Create a new metric collector.
    pub fn new(
        prometheus_config: PrometheusConfig,
        aggregation_config: AggregationConfig,
        filter_config: FilterConfig,
    ) -> Self {
        Self {
            metrics: RwLock::new(HashMap::new()),
            prometheus_config,
            aggregation_config,
            filter: MetricFilter::new(&filter_config),
            alerts: crate::alerts::AlertStore::new(),
            stats: RwLock::new(CollectorStats::default()),
        }
    }

    /// Record a sensor alert (from the `state/*/alert/*` channel). Firing
    /// alerts are stored; resolved alerts clear their series. No-op unless
    /// alert export is enabled.
    pub fn record_alert(&self, alert: zensight_common::alert::Alert) {
        if self.prometheus_config.export_alerts {
            self.alerts.apply(alert);
        }
    }

    /// Clear a firing alert by its `alert_key` (a Zenoh `Delete` tombstone).
    pub fn remove_alert(&self, alert_key: &str) {
        if self.prometheus_config.export_alerts {
            self.alerts.remove(alert_key);
        }
    }

    /// Whether alert export is enabled (drives whether the subscriber bothers
    /// decoding the `state/*/alert/*` channel).
    pub fn export_alerts(&self) -> bool {
        self.prometheus_config.export_alerts
    }

    /// Number of currently-firing alerts.
    pub fn alert_count(&self) -> usize {
        self.alerts.len()
    }

    /// Record a telemetry point.
    /// Record one telemetry sample.
    ///
    /// `key` is the sample's own base-relative key expression. The exporters
    /// used to name metrics from `point.protocol` + `point.metric`, which baked
    /// per-entity subjects into metric NAMES and made `sum by (iface)`
    /// impossible for every producer the semconv table did not hand-map. The
    /// key is what the registry can refine (#475, #764).
    pub fn record(&self, key: &str, point: &TelemetryPoint) {
        {
            let mut stats = self.stats.write();
            stats.points_received += 1;
        }

        // Check filter
        if !self.filter.should_include(point) {
            let mut stats = self.stats.write();
            stats.points_filtered += 1;
            trace!(
                source = %point.source,
                metric = %point.metric,
                "Telemetry point filtered out"
            );
            return;
        }

        // Resolve the identity through the registry. An unregistered subject is
        // COUNTED, not silently renamed.
        let identity = match identify(
            key,
            point,
            &self.prometheus_config.default_labels,
            sanitize_label_name,
        ) {
            Ok(i) => i,
            Err(reason) => {
                let mut stats = self.stats.write();
                *stats.points_unrefined.entry(reason.reason()).or_insert(0) += 1;
                trace!(key = %key, reason = reason.reason(), "Key not refined by the registry");
                return;
            }
        };
        if identity.shadowed > 0 {
            let mut stats = self.stats.write();
            stats.labels_shadowed += u64::from(identity.shadowed);
        }

        let stored =
            match StoredMetric::from_identity(&identity, point, &self.prometheus_config.prefix) {
                Some(m) => m,
                None => {
                    let mut stats = self.stats.write();
                    stats.points_not_exportable += 1;
                    trace!(
                        source = %point.source,
                        metric = %point.metric,
                        "Telemetry point not exportable"
                    );
                    return;
                }
            };

        let key = stored.key.clone();

        // Update or insert the metric
        let mut metrics = self.metrics.write();

        // Check if we're at max capacity and this is a new series
        if !metrics.contains_key(&key) && metrics.len() >= self.aggregation_config.max_series {
            drop(metrics);
            let mut stats = self.stats.write();
            stats.points_dropped_max_series += 1;
            warn!(
                max_series = self.aggregation_config.max_series,
                "Max series limit reached, dropping new metric"
            );
            return;
        }

        metrics.insert(key, stored);
        drop(metrics);

        let mut stats = self.stats.write();
        stats.points_accepted += 1;
    }

    /// Remove stale metrics.
    pub fn cleanup_stale(&self) -> usize {
        let timeout = Duration::from_secs(self.aggregation_config.stale_timeout_secs);
        let mut metrics = self.metrics.write();
        let before = metrics.len();

        metrics.retain(|_, m| !m.is_stale(timeout));

        let removed = before - metrics.len();

        if removed > 0 {
            debug!(
                removed,
                remaining = metrics.len(),
                "Cleaned up stale metrics"
            );
            let mut stats = self.stats.write();
            stats.stale_metrics_removed += removed as u64;
        }
        drop(metrics);

        // Alerts are deliberately NOT swept on a timer (#758). Sensors publish
        // them edge-triggered, so "not re-received in 300s" means "still firing
        // and nothing changed" far more often than it means "gone" — and since
        // absence is the resolve signal, a sweep here silently closed live
        // incidents. A sensor that dies is caught by its liveliness token
        // disappearing instead; see `AlertStore::drop_source`.
        removed
    }

    /// Drop every firing alert from a source whose liveliness token vanished.
    pub fn drop_source_alerts(&self, source: &str) -> usize {
        let removed = self.alerts.drop_source(source);
        if removed > 0 {
            debug!(source, removed, "Dropped alerts for a departed sensor");
        }
        removed
    }

    /// Get the current number of stored series.
    pub fn series_count(&self) -> usize {
        self.metrics.read().len()
    }

    /// Snapshot the current metric state (used by remote-write pushes to build
    /// a `WriteRequest` from the same state `/metrics` renders).
    pub fn snapshot_metrics(&self) -> Vec<StoredMetric> {
        self.metrics.read().values().cloned().collect()
    }

    /// Get collector statistics.
    pub fn stats(&self) -> CollectorStats {
        self.stats.read().clone()
    }

    /// Render metrics in Prometheus exposition format.
    pub fn render(&self) -> String {
        let metrics = self.metrics.read();
        let mut output = Vec::with_capacity(metrics.len() * 100);
        let mut render_errors = 0u64;

        // Helper macro to handle write errors
        macro_rules! write_or_count {
            ($dst:expr, $($arg:tt)*) => {
                if let Err(e) = writeln!($dst, $($arg)*) {
                    render_errors += 1;
                    warn!(error = %e, "Failed to write metric line");
                }
            };
        }

        // Group by the name we will actually EMIT, not by the stored name.
        //
        // A text point is emitted as `<name>_info` (#752). Grouping by the raw
        // name would let a text family and a numeric family of the same name
        // share one `# TYPE` block, whose token is then taken from whichever
        // series a HashMap iteration happened to yield first — a body that is
        // valid or invalid depending on hash order.
        let mut by_name: HashMap<String, Vec<&StoredMetric>> = HashMap::new();
        for metric in metrics.values() {
            by_name
                .entry(metric.emitted_name())
                .or_default()
                .push(metric);
        }

        // Sort by metric name for consistent output
        let mut names: Vec<_> = by_name.keys().cloned().collect();
        names.sort();

        for name in &names {
            let series = &by_name[name];
            if series.is_empty() {
                continue;
            }

            // Get type from first series
            let metric_type = series[0].metric_type;

            // `# HELP` from the registry's own sentence for this subject
            // (#768). The registry has carried a description per subject all
            // along; it was simply unreachable at runtime, which is why HELP
            // was emitted only for alerts while the docs claimed otherwise.
            //
            // The unit is appended when it did NOT become a name suffix, so a
            // `ms` metric still tells the reader its unit even though renaming
            // it `_seconds` without rescaling would be a lie.
            if let Some(help) = series[0].help.as_deref() {
                let unit_note = series[0]
                    .unit
                    .as_deref()
                    .filter(|u| crate::mapping::unit_suffix(u).is_none())
                    .map(|u| format!(" ({u})"))
                    .unwrap_or_default();
                write_or_count!(output, "# HELP {} {}{}", name, escape_help(help), unit_note);
            }

            // Write TYPE comment
            write_or_count!(output, "# TYPE {} {}", name, metric_type.as_str());

            // Write each series
            for metric in series {
                match metric.metric_type {
                    PrometheusType::Text => {
                        // Text points become an info-style gauge: value 1, with
                        // the text carried in a label named for the subject leaf.
                        if let Some(text) = &metric.text_value {
                            let label_name = metric
                                .text_label
                                .clone()
                                .unwrap_or_else(|| "value".to_string());
                            let mut labels = metric.key.labels.clone();
                            // Never emit a duplicate label name: if the merged
                            // set already carries this name, the structural
                            // label wins and the text is dropped rather than
                            // producing an invalid series (#753).
                            if !labels.iter().any(|(k, _)| k == &label_name) {
                                labels.push((label_name, text.clone()));
                            }
                            labels.sort_by(|a, b| a.0.cmp(&b.0));

                            let label_str = format_labels(&labels);
                            write_or_count!(output, "{}{} 1", name, label_str);
                        }
                    }
                    _ => {
                        if let Some(value) = metric.value {
                            write_or_count!(
                                output,
                                "{}{} {}",
                                metric.key.name,
                                metric.key.format_labels(),
                                format_value(value)
                            );
                        }
                    }
                }
            }
        }

        // Add collector stats as metrics
        let stats = self.stats.read();
        let _ = writeln!(output);
        write_or_count!(
            output,
            "# TYPE {}_exporter_series gauge",
            self.prometheus_config.prefix
        );
        write_or_count!(
            output,
            "{}_exporter_series {}",
            self.prometheus_config.prefix,
            metrics.len()
        );

        write_or_count!(
            output,
            "# TYPE {}_exporter_points_received_total counter",
            self.prometheus_config.prefix
        );
        write_or_count!(
            output,
            "{}_exporter_points_received_total {}",
            self.prometheus_config.prefix,
            stats.points_received
        );

        write_or_count!(
            output,
            "# TYPE {}_exporter_points_accepted_total counter",
            self.prometheus_config.prefix
        );
        write_or_count!(
            output,
            "{}_exporter_points_accepted_total {}",
            self.prometheus_config.prefix,
            stats.points_accepted
        );

        write_or_count!(
            output,
            "# TYPE {}_exporter_points_filtered_total counter",
            self.prometheus_config.prefix
        );
        write_or_count!(
            output,
            "{}_exporter_points_filtered_total {}",
            self.prometheus_config.prefix,
            stats.points_filtered
        );

        // Record render errors in stats
        if render_errors > 0 {
            drop(stats);
            let mut stats = self.stats.write();
            stats.render_errors += render_errors;
        }

        // Append firing sensor alerts as a `<prefix>_alert` gauge.
        if self.prometheus_config.export_alerts {
            let _ = writeln!(output);
            self.alerts
                .render(&self.prometheus_config.prefix, &mut output);
        }

        String::from_utf8(output).unwrap_or_default()
    }
}

/// Create a shareable collector handle.
pub type SharedCollector = Arc<MetricCollector>;

/// Escape special characters in label values.
pub(crate) fn escape_label_value(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            _ => result.push(c),
        }
    }
    result
}

/// Format a floating point value for Prometheus.
fn format_value(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            "+Inf".to_string()
        } else {
            "-Inf".to_string()
        }
    } else if value.fract() == 0.0 {
        format!("{:.0}", value)
    } else {
        format!("{}", value)
    }
}

/// Escape a `# HELP` text per the exposition format: backslash and newline
/// only (a comment is not a label value, so quotes ride through unescaped).
fn escape_help(help: &str) -> String {
    help.replace('\\', "\\\\").replace('\n', "\\n")
}

/// Format labels for Prometheus exposition format.
fn format_labels(labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return String::new();
    }

    let parts: Vec<String> = labels
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, escape_label_value(v)))
        .collect();

    format!("{{{}}}", parts.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zensight_common::telemetry::Protocol;

    /// A base-relative telemetry key for a point, as the wire carries it.
    ///
    /// Naming now flows from the KEY through the registry (#764), so a test
    /// that only builds a `TelemetryPoint` is testing nothing the exporter
    /// does. This mints the matching key.
    fn key_for(point: &TelemetryPoint) -> String {
        let producer = point.protocol.as_str();
        // snmp/modbus/gnmi/netflow register a rest-var catch-all
        // `<device>/<metric...>`, so their key carries the device chunk that
        // `point.metric` does not.
        let rest_var = matches!(producer, "snmp" | "modbus" | "gnmi" | "netflow");
        if rest_var {
            format!(
                "v1/h-0123456789ab/telemetry/{producer}/{}/{}",
                point.source, point.metric
            )
        } else {
            format!("v1/h-0123456789ab/telemetry/{producer}/{}", point.metric)
        }
    }

    /// Resolve a point to its identity exactly as `record` does.
    fn identity_of(point: &TelemetryPoint) -> MetricIdentity {
        identify(
            &key_for(point),
            point,
            &HashMap::new(),
            crate::mapping::sanitize_label_name,
        )
        .unwrap_or_else(|e| panic!("{} did not refine: {:?}", point.metric, e.reason()))
    }

    fn make_point(
        source: &str,
        protocol: Protocol,
        metric: &str,
        value: TelemetryValue,
    ) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: 1234567890000,
            source: source.to_string(),
            protocol,
            metric: metric.to_string(),
            value,
            labels: HashMap::new(),
            unit: None,
        }
    }

    /// #104: `events/<uid>` log lines must never become Prometheus series —
    /// one info series per log line is a cardinality explosion.
    ///
    /// Registry-driven naming now catches this a step earlier and more
    /// generally: `events/{uid}` is deliberately NOT a registered subject (it
    /// is never published, it only feeds the bounded `@rpc/logs/events` ring —
    /// see registry/logs.toml), so the key does not refine at all. The
    /// hand-rolled guard in `from_identity` is kept as a belt-and-braces
    /// backstop, but this is the assertion that matters.
    #[test]
    fn per_line_log_events_do_not_refine() {
        let event = make_point(
            "host01",
            Protocol::Logs,
            "events/0000000000009000000000042",
            TelemetryValue::Text("login failed".into()),
        );
        let err = identify(
            &key_for(&event),
            &event,
            &HashMap::new(),
            crate::mapping::sanitize_label_name,
        )
        .expect_err("an unregistered subject must not refine");
        assert_eq!(err.reason(), "subject_not_registered");

        // A registered Logs metric still exports.
        let real = make_point(
            "host01",
            Protocol::Logs,
            "errors_total",
            TelemetryValue::Counter(2),
        );
        assert!(StoredMetric::from_identity(&identity_of(&real), &real, "zensight").is_some());
    }

    #[test]
    fn test_series_key_from_telemetry() {
        // Lowercase because the WIRE is lowercase: a key chunk must be
        // `[a-z0-9]`-bounded, which is why the SNMP poller slugs at the publish
        // boundary (#559).
        let point = make_point(
            "router01",
            Protocol::Snmp,
            "sysuptime",
            TelemetryValue::Counter(100),
        );
        let key = SeriesKey::from_identity(&identity_of(&point), "zensight");

        // `_total` is the counter convention (#767), applied idempotently.
        assert_eq!(key.name, "zensight_snmp_sysuptime_total");
        // The device is now a real label, lifted out of the key's `{device}`
        // chunk — it used to be reachable only as `source` (#764).
        assert!(
            key.labels
                .iter()
                .any(|(k, v)| k == "device" && v == "router01"),
            "labels: {:?}",
            key.labels
        );
        assert!(
            key.labels
                .iter()
                .any(|(k, v)| k == "source" && v == "router01")
        );
        assert!(
            key.labels
                .iter()
                .any(|(k, v)| k == "protocol" && v == "snmp")
        );
    }

    #[test]
    fn systemd_per_unit_point_has_single_unit_label() {
        // #282: per-unit systemd series must export as one clean metric name with
        // the unit as a SINGLE label — semconv adds no attrs, so the point's own
        // `unit` label isn't duplicated.
        let mut point = make_point(
            "host01",
            Protocol::Systemd,
            "unit/sshd.service/active",
            TelemetryValue::Boolean(true),
        );
        point
            .labels
            .insert("unit".to_string(), "sshd.service".to_string());
        let key = SeriesKey::from_identity(&identity_of(&point), "zensight");

        assert_eq!(key.name, "zensight_systemd_unit_active");
        let unit_labels: Vec<_> = key.labels.iter().filter(|(k, _)| k == "unit").collect();
        assert_eq!(unit_labels.len(), 1, "unit label must not be duplicated");
        assert_eq!(unit_labels[0].1, "sshd.service");
    }

    #[test]
    fn test_series_key_with_default_labels() {
        let point = make_point(
            "server01",
            Protocol::Sysinfo,
            "cpu/usage",
            TelemetryValue::Gauge(45.5),
        );
        let mut defaults = HashMap::new();
        defaults.insert("env".to_string(), "prod".to_string());

        let identity = identify(
            &key_for(&point),
            &point,
            &defaults,
            crate::mapping::sanitize_label_name,
        )
        .expect("refines");
        let key = SeriesKey::from_identity(&identity, "zensight");

        assert!(key.labels.iter().any(|(k, v)| k == "env" && v == "prod"));
    }

    #[test]
    fn test_series_key_format_labels() {
        let key = SeriesKey {
            name: "test_metric".to_string(),
            labels: vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string()),
            ],
        };

        assert_eq!(key.format_labels(), "{a=\"1\",b=\"2\"}");
    }

    #[test]
    fn test_stored_metric_from_telemetry() {
        let point = make_point(
            "router01",
            Protocol::Snmp,
            "if/1/in_octets",
            TelemetryValue::Counter(1000),
        );
        let stored = StoredMetric::from_identity(&identity_of(&point), &point, "zensight");

        assert!(stored.is_some());
        let stored = stored.unwrap();
        assert_eq!(stored.metric_type, PrometheusType::Counter);
        assert_eq!(stored.value, Some(1000.0));
    }

    /// #779: two interfaces of one device are ONE family, told apart by a
    /// label — which is the whole point of registering the interface table.
    ///
    /// Before the registry change, `snmp` matched only its rest-var catch-all
    /// (`{device}/{metric...}`), so the family name came from the rest
    /// variable's *value* and the table index rode in the metric NAME:
    /// `zensight_snmp_if_1_in_octets` and `zensight_snmp_if_2_in_octets` were
    /// two unrelated families and `sum by (index)` could not be written.
    /// #769 attached the index as a label; this asserts the other half.
    #[test]
    fn interface_columns_aggregate_across_indices() {
        let if1 = {
            let mut p = make_point(
                "router01",
                Protocol::Snmp,
                "if/1/in_octets",
                TelemetryValue::Counter(1000),
            );
            p.labels.insert("index".to_string(), "1".to_string());
            p
        };
        let if2 = {
            let mut p = make_point(
                "router01",
                Protocol::Snmp,
                "if/2/in_octets",
                TelemetryValue::Counter(2000),
            );
            p.labels.insert("index".to_string(), "2".to_string());
            p
        };

        let k1 = SeriesKey::from_identity(&identity_of(&if1), "zensight");
        let k2 = SeriesKey::from_identity(&identity_of(&if2), "zensight");

        assert_eq!(
            k1.name, "zensight_snmp_if_in_octets_total",
            "the index must not be in the metric name"
        );
        assert_eq!(k1.name, k2.name, "two interfaces, one family");

        let index_of = |k: &SeriesKey| {
            k.labels
                .iter()
                .find(|(n, _)| n == "index")
                .map(|(_, v)| v.clone())
                .expect("the table index rides as a label (#769)")
        };
        assert_eq!(index_of(&k1), "1");
        assert_eq!(index_of(&k2), "2");
        assert_ne!(k1.labels, k2.labels, "the label is what tells them apart");
    }

    /// Columns of the same table stay in *separate* families.
    ///
    /// This is why the registry spells one subject per column rather than the
    /// `{device}/if/{index}/{column}` shape #779 sketched: a `{column}`
    /// variable would be dropped from the name with every other variable,
    /// collapsing counters, gauges and strings into one `zensight_snmp_if`
    /// family and emitting two `# TYPE` lines for one name — the scrape-killer
    /// class #752 fixed.
    #[test]
    fn interface_columns_are_not_collapsed_into_one_family() {
        let octets = make_point(
            "router01",
            Protocol::Snmp,
            "if/1/in_octets",
            TelemetryValue::Counter(1000),
        );
        let mtu = make_point(
            "router01",
            Protocol::Snmp,
            "if/1/mtu",
            TelemetryValue::Gauge(1500.0),
        );

        let k_octets = SeriesKey::from_identity(&identity_of(&octets), "zensight");
        let k_mtu = SeriesKey::from_identity(&identity_of(&mtu), "zensight");

        assert_eq!(k_octets.name, "zensight_snmp_if_in_octets_total");
        assert_eq!(k_mtu.name, "zensight_snmp_if_mtu");
        assert_ne!(
            k_octets.name, k_mtu.name,
            "a counter and a gauge must never share a name"
        );
    }

    /// The derived per-second sibling the poller publishes for every counter
    /// is registered too — an unregistered `.rate` would fall back to the
    /// catch-all and keep the index in its name.
    #[test]
    fn the_derived_rate_sibling_aggregates_too() {
        let mut p = make_point(
            "router01",
            Protocol::Snmp,
            "if/1/in_octets.rate",
            TelemetryValue::Gauge(12.5),
        );
        p.labels.insert("index".to_string(), "1".to_string());

        let key = SeriesKey::from_identity(&identity_of(&p), "zensight");
        assert!(
            !key.name.contains("_1_"),
            "the index must not be in the name: {}",
            key.name
        );
        assert!(
            key.name.starts_with("zensight_snmp_if_in_octets_rate"),
            "unexpected name: {}",
            key.name
        );
    }

    #[test]
    fn test_stored_metric_binary_not_exportable() {
        let point = make_point(
            "server",
            Protocol::Snmp,
            "data",
            TelemetryValue::Binary(vec![1, 2, 3]),
        );
        let stored = StoredMetric::from_identity(&identity_of(&point), &point, "zensight");

        assert!(stored.is_none());
    }

    #[test]
    fn test_metric_filter_include_protocols() {
        let config = FilterConfig {
            include_protocols: vec!["snmp".to_string()],
            ..Default::default()
        };
        let filter = MetricFilter::new(&config);

        let snmp_point = make_point("r1", Protocol::Snmp, "m", TelemetryValue::Gauge(1.0));
        let sysinfo_point = make_point("s1", Protocol::Sysinfo, "m", TelemetryValue::Gauge(1.0));

        assert!(filter.should_include(&snmp_point));
        assert!(!filter.should_include(&sysinfo_point));
    }

    #[test]
    fn test_metric_filter_exclude_sources() {
        let config = FilterConfig {
            exclude_sources: vec!["test-device".to_string()],
            ..Default::default()
        };
        let filter = MetricFilter::new(&config);

        let point1 = make_point(
            "test-device",
            Protocol::Snmp,
            "m",
            TelemetryValue::Gauge(1.0),
        );
        let point2 = make_point(
            "prod-device",
            Protocol::Snmp,
            "m",
            TelemetryValue::Gauge(1.0),
        );

        assert!(!filter.should_include(&point1));
        assert!(filter.should_include(&point2));
    }

    #[test]
    fn test_metric_filter_glob_patterns() {
        let config = FilterConfig {
            exclude_metrics: vec!["**/debug/**".to_string()],
            ..Default::default()
        };
        let filter = MetricFilter::new(&config);

        let point1 = make_point(
            "s",
            Protocol::Snmp,
            "system/debug/trace",
            TelemetryValue::Gauge(1.0),
        );
        let point2 = make_point(
            "s",
            Protocol::Snmp,
            "system/uptime",
            TelemetryValue::Gauge(1.0),
        );

        assert!(!filter.should_include(&point1));
        assert!(filter.should_include(&point2));
    }

    #[test]
    fn test_collector_record_and_render() {
        let collector = MetricCollector::new(
            PrometheusConfig::default(),
            AggregationConfig::default(),
            FilterConfig::default(),
        );

        let point = make_point(
            "router01",
            Protocol::Snmp,
            "sysuptime",
            TelemetryValue::Counter(12345),
        );
        collector.record(&key_for(&point), &point);

        assert_eq!(collector.series_count(), 1);

        let output = collector.render();
        assert!(output.contains("# TYPE zensight_snmp_sysuptime_total counter"));
        assert!(output.contains("zensight_snmp_sysuptime_total{"));
        assert!(output.contains("source=\"router01\""));
        assert!(output.contains("12345"));
    }

    #[test]
    fn test_collector_max_series_limit() {
        let collector = MetricCollector::new(
            PrometheusConfig::default(),
            AggregationConfig {
                max_series: 2,
                ..Default::default()
            },
            FilterConfig::default(),
        );

        for i in 0..5 {
            let point = make_point(
                &format!("device{}", i),
                Protocol::Snmp,
                "metric",
                TelemetryValue::Gauge(i as f64),
            );
            collector.record(&key_for(&point), &point);
        }

        assert_eq!(collector.series_count(), 2);
        assert_eq!(collector.stats().points_dropped_max_series, 3);
    }

    #[test]
    fn test_escape_label_value() {
        assert_eq!(escape_label_value("simple"), "simple");
        assert_eq!(escape_label_value("with\"quote"), "with\\\"quote");
        assert_eq!(escape_label_value("with\\backslash"), "with\\\\backslash");
        assert_eq!(escape_label_value("with\nnewline"), "with\\nnewline");
    }

    #[test]
    fn test_format_value() {
        assert_eq!(format_value(42.0), "42");
        assert_eq!(format_value(2.5), "2.5");
        assert_eq!(format_value(f64::NAN), "NaN");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
    }
}
