//! Metric collector that stores and manages Prometheus metrics.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tracing::{debug, trace, warn};
use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};

use crate::config::{AggregationConfig, FilterConfig, PrometheusConfig};
use crate::mapping::{
    PrometheusType, build_metric_name, extract_numeric_value, is_exportable, sanitize_label_name,
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
    /// Create a new series key from a telemetry point.
    pub fn from_telemetry(
        point: &TelemetryPoint,
        prefix: &str,
        default_labels: &HashMap<String, String>,
    ) -> Self {
        let name = build_metric_name(prefix, point.protocol, &point.metric);

        // Build labels: source + protocol + user labels + default labels
        let mut labels = Vec::with_capacity(2 + point.labels.len() + default_labels.len());

        // Always include source and protocol
        labels.push(("source".to_string(), point.source.clone()));
        labels.push(("protocol".to_string(), point.protocol.as_str().to_string()));

        // Add telemetry labels (sanitized)
        for (k, v) in &point.labels {
            let key = sanitize_label_name(k);
            // Skip if it would conflict with built-in labels
            if key != "source" && key != "protocol" {
                labels.push((key, v.clone()));
            }
        }

        // Add default labels (don't override existing)
        for (k, v) in default_labels {
            let key = sanitize_label_name(k);
            if !labels.iter().any(|(lk, _)| lk == &key) {
                labels.push((key, v.clone()));
            }
        }

        // Sort for consistent hashing
        labels.sort_by(|a, b| a.0.cmp(&b.0));

        Self { name, labels }
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
    /// Text value (for info metrics).
    pub text_value: Option<String>,
    /// When this metric was last updated.
    pub last_updated: Instant,
    /// Original timestamp from the telemetry point.
    pub timestamp_ms: i64,
}

impl StoredMetric {
    /// Create a new stored metric from a telemetry point.
    pub fn from_telemetry(
        point: &TelemetryPoint,
        prefix: &str,
        default_labels: &HashMap<String, String>,
    ) -> Option<Self> {
        if !is_exportable(&point.value) {
            return None;
        }

        let key = SeriesKey::from_telemetry(point, prefix, default_labels);
        let metric_type = PrometheusType::from_value(&point.value);
        let value = extract_numeric_value(&point.value);
        let text_value = match &point.value {
            TelemetryValue::Text(s) => Some(s.clone()),
            _ => None,
        };

        Some(Self {
            key,
            metric_type,
            value,
            text_value,
            last_updated: Instant::now(),
            timestamp_ms: point.timestamp,
        })
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
            stats: RwLock::new(CollectorStats::default()),
        }
    }

    /// Record a telemetry point.
    pub fn record(&self, point: &TelemetryPoint) {
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

        // Try to convert to stored metric
        let stored = match StoredMetric::from_telemetry(
            point,
            &self.prometheus_config.prefix,
            &self.prometheus_config.default_labels,
        ) {
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

        removed
    }

    /// Get the current number of stored series.
    pub fn series_count(&self) -> usize {
        self.metrics.read().len()
    }

    /// Get collector statistics.
    pub fn stats(&self) -> CollectorStats {
        self.stats.read().clone()
    }

    /// Render metrics in Prometheus exposition format.
    pub fn render(&self) -> String {
        let metrics = self.metrics.read();
        let mut output = Vec::with_capacity(metrics.len() * 100);

        // Group metrics by name for TYPE/HELP comments
        let mut by_name: HashMap<&str, Vec<&StoredMetric>> = HashMap::new();
        for metric in metrics.values() {
            by_name.entry(&metric.key.name).or_default().push(metric);
        }

        // Sort by metric name for consistent output
        let mut names: Vec<_> = by_name.keys().collect();
        names.sort();

        for name in names {
            let series = &by_name[name];
            if series.is_empty() {
                continue;
            }

            // Get type from first series
            let metric_type = series[0].metric_type;

            // Write TYPE comment
            writeln!(output, "# TYPE {} {}", name, metric_type.as_str()).ok();

            // Write each series
            for metric in series {
                match metric.metric_type {
                    PrometheusType::Info => {
                        // Info metrics get value=1 with the text as a label
                        if let Some(text) = &metric.text_value {
                            let mut labels = metric.key.labels.clone();
                            labels.push(("value".to_string(), text.clone()));
                            labels.sort_by(|a, b| a.0.cmp(&b.0));

                            let label_str = format_labels(&labels);
                            writeln!(output, "{}{} 1", metric.key.name, label_str).ok();
                        }
                    }
                    _ => {
                        if let Some(value) = metric.value {
                            writeln!(
                                output,
                                "{}{} {}",
                                metric.key.name,
                                metric.key.format_labels(),
                                format_value(value)
                            )
                            .ok();
                        }
                    }
                }
            }
        }

        // Add collector stats as metrics
        let stats = self.stats.read();
        writeln!(output).ok();
        writeln!(
            output,
            "# TYPE {}_exporter_series_total gauge",
            self.prometheus_config.prefix
        )
        .ok();
        writeln!(
            output,
            "{}_exporter_series_total {}",
            self.prometheus_config.prefix,
            metrics.len()
        )
        .ok();

        writeln!(
            output,
            "# TYPE {}_exporter_points_received_total counter",
            self.prometheus_config.prefix
        )
        .ok();
        writeln!(
            output,
            "{}_exporter_points_received_total {}",
            self.prometheus_config.prefix, stats.points_received
        )
        .ok();

        writeln!(
            output,
            "# TYPE {}_exporter_points_accepted_total counter",
            self.prometheus_config.prefix
        )
        .ok();
        writeln!(
            output,
            "{}_exporter_points_accepted_total {}",
            self.prometheus_config.prefix, stats.points_accepted
        )
        .ok();

        writeln!(
            output,
            "# TYPE {}_exporter_points_filtered_total counter",
            self.prometheus_config.prefix
        )
        .ok();
        writeln!(
            output,
            "{}_exporter_points_filtered_total {}",
            self.prometheus_config.prefix, stats.points_filtered
        )
        .ok();

        String::from_utf8(output).unwrap_or_default()
    }
}

/// Create a shareable collector handle.
pub type SharedCollector = Arc<MetricCollector>;

/// Escape special characters in label values.
fn escape_label_value(value: &str) -> String {
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
        }
    }

    #[test]
    fn test_series_key_from_telemetry() {
        let point = make_point(
            "router01",
            Protocol::Snmp,
            "sysUpTime",
            TelemetryValue::Counter(100),
        );
        let key = SeriesKey::from_telemetry(&point, "zensight", &HashMap::new());

        assert_eq!(key.name, "zensight_snmp_sysUpTime");
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
    fn test_series_key_with_default_labels() {
        let point = make_point(
            "server01",
            Protocol::Sysinfo,
            "cpu/usage",
            TelemetryValue::Gauge(45.5),
        );
        let mut defaults = HashMap::new();
        defaults.insert("env".to_string(), "prod".to_string());

        let key = SeriesKey::from_telemetry(&point, "zensight", &defaults);

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
            "ifInOctets",
            TelemetryValue::Counter(1000),
        );
        let stored = StoredMetric::from_telemetry(&point, "zensight", &HashMap::new());

        assert!(stored.is_some());
        let stored = stored.unwrap();
        assert_eq!(stored.metric_type, PrometheusType::Counter);
        assert_eq!(stored.value, Some(1000.0));
    }

    #[test]
    fn test_stored_metric_binary_not_exportable() {
        let point = make_point(
            "server",
            Protocol::Snmp,
            "data",
            TelemetryValue::Binary(vec![1, 2, 3]),
        );
        let stored = StoredMetric::from_telemetry(&point, "zensight", &HashMap::new());

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
            "sysUpTime",
            TelemetryValue::Counter(12345),
        );
        collector.record(&point);

        assert_eq!(collector.series_count(), 1);

        let output = collector.render();
        assert!(output.contains("# TYPE zensight_snmp_sysUpTime counter"));
        assert!(output.contains("zensight_snmp_sysUpTime{"));
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
            collector.record(&point);
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
        assert_eq!(format_value(3.14), "3.14");
        assert_eq!(format_value(f64::NAN), "NaN");
        assert_eq!(format_value(f64::INFINITY), "+Inf");
        assert_eq!(format_value(f64::NEG_INFINITY), "-Inf");
    }
}
