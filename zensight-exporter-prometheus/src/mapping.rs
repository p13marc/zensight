//! Mapping from ZenSight TelemetryPoint to Prometheus metrics.

use zensight_common::telemetry::{Protocol, TelemetryValue};

/// Sanitize a metric name to be Prometheus-compatible.
///
/// Prometheus metric names must match `[a-zA-Z_:][a-zA-Z0-9_:]*`.
/// This function:
/// - Replaces invalid characters with underscores
/// - Ensures the name starts with a letter or underscore
/// - Collapses multiple underscores into one
pub fn sanitize_metric_name(name: &str) -> String {
    let mut result = String::with_capacity(name.len() + 1);
    let mut last_was_underscore = false;
    let mut chars = name.chars().peekable();

    // Handle first character specially - must be letter or underscore
    // If it's a digit, prefix with underscore and keep the digit
    if let Some(&first) = chars.peek()
        && first.is_ascii_digit() {
            result.push('_');
            last_was_underscore = true;
        }

    for c in chars {
        // After first char handling, all alphanumeric, underscore, and colon are valid
        let is_valid_char = c.is_ascii_alphanumeric() || c == '_' || c == ':';

        if is_valid_char {
            // For underscores, only add if last char wasn't an underscore (collapse)
            if c == '_' {
                if !last_was_underscore {
                    result.push(c);
                    last_was_underscore = true;
                }
            } else {
                result.push(c);
                last_was_underscore = false;
            }
        } else if !last_was_underscore {
            // Replace invalid char with underscore (but don't add consecutive)
            result.push('_');
            last_was_underscore = true;
        }
    }

    // Remove trailing underscores
    while result.ends_with('_') {
        result.pop();
    }

    // Handle empty result
    if result.is_empty() {
        result.push_str("unnamed");
    }

    result
}

/// Sanitize a label name to be Prometheus-compatible.
///
/// Prometheus label names must match `[a-zA-Z_][a-zA-Z0-9_]*`.
/// Labels starting with `__` are reserved for internal use.
pub fn sanitize_label_name(name: &str) -> String {
    let mut result = String::with_capacity(name.len());
    let mut last_was_underscore = false;

    for (i, c) in name.chars().enumerate() {
        let valid = if i == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        };

        if valid {
            result.push(c);
            last_was_underscore = c == '_';
        } else if !last_was_underscore {
            result.push('_');
            last_was_underscore = true;
        }
    }

    // Remove trailing underscores
    while result.ends_with('_') {
        result.pop();
    }

    // Handle empty or reserved labels
    if result.is_empty() {
        return "label".to_string();
    }

    // Prefix with underscore if starts with double underscore (reserved)
    if result.starts_with("__") {
        result.insert(0, 'z');
    }

    result
}

/// Build a full Prometheus metric name from components.
///
/// Format: `{prefix}_{protocol}_{metric_path}`
pub fn build_metric_name(prefix: &str, protocol: Protocol, metric: &str) -> String {
    let sanitized_metric = sanitize_metric_name(metric);
    let protocol_str = protocol.as_str();

    if prefix.is_empty() {
        format!("{}_{}", protocol_str, sanitized_metric)
    } else {
        format!("{}_{}_{}", prefix, protocol_str, sanitized_metric)
    }
}

/// Determine the Prometheus metric type from a TelemetryValue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrometheusType {
    Counter,
    Gauge,
    Info,
    Untyped,
}

impl PrometheusType {
    /// Determine the type from a TelemetryValue.
    pub fn from_value(value: &TelemetryValue) -> Self {
        match value {
            TelemetryValue::Counter(_) => PrometheusType::Counter,
            TelemetryValue::Gauge(_) => PrometheusType::Gauge,
            TelemetryValue::Boolean(_) => PrometheusType::Gauge,
            TelemetryValue::Text(_) => PrometheusType::Info,
            TelemetryValue::Binary(_) => PrometheusType::Untyped,
        }
    }

    /// Get the TYPE comment string for Prometheus exposition format.
    pub fn as_str(&self) -> &'static str {
        match self {
            PrometheusType::Counter => "counter",
            PrometheusType::Gauge => "gauge",
            PrometheusType::Info => "info",
            PrometheusType::Untyped => "untyped",
        }
    }
}

/// Extract a numeric value from TelemetryValue for Prometheus.
///
/// Returns None for values that can't be represented as numbers (Text, Binary).
pub fn extract_numeric_value(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Counter(v) => Some(*v as f64),
        TelemetryValue::Gauge(v) => Some(*v),
        TelemetryValue::Boolean(v) => Some(if *v { 1.0 } else { 0.0 }),
        TelemetryValue::Text(_) => None,
        TelemetryValue::Binary(_) => None,
    }
}

/// Check if a TelemetryValue can be exported as a Prometheus metric.
pub fn is_exportable(value: &TelemetryValue) -> bool {
    !matches!(value, TelemetryValue::Binary(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_metric_name_simple() {
        assert_eq!(sanitize_metric_name("cpu_usage"), "cpu_usage");
        assert_eq!(sanitize_metric_name("memory_total"), "memory_total");
    }

    #[test]
    fn test_sanitize_metric_name_slashes() {
        assert_eq!(sanitize_metric_name("system/sysUpTime"), "system_sysUpTime");
        assert_eq!(sanitize_metric_name("if/1/ifInOctets"), "if_1_ifInOctets");
    }

    #[test]
    fn test_sanitize_metric_name_special_chars() {
        assert_eq!(sanitize_metric_name("cpu.usage%"), "cpu_usage");
        assert_eq!(sanitize_metric_name("memory-used"), "memory_used");
        assert_eq!(sanitize_metric_name("disk[sda]"), "disk_sda");
    }

    #[test]
    fn test_sanitize_metric_name_collapse_underscores() {
        assert_eq!(sanitize_metric_name("cpu___usage"), "cpu_usage");
        assert_eq!(sanitize_metric_name("a//b//c"), "a_b_c");
    }

    #[test]
    fn test_sanitize_metric_name_leading_number() {
        assert_eq!(sanitize_metric_name("1cpu"), "_1cpu");
    }

    #[test]
    fn test_sanitize_metric_name_empty() {
        assert_eq!(sanitize_metric_name(""), "unnamed");
        assert_eq!(sanitize_metric_name("///"), "unnamed");
    }

    #[test]
    fn test_sanitize_metric_name_colons() {
        // Colons are allowed in Prometheus metric names
        assert_eq!(sanitize_metric_name("foo:bar:baz"), "foo:bar:baz");
    }

    #[test]
    fn test_sanitize_label_name() {
        assert_eq!(sanitize_label_name("source"), "source");
        assert_eq!(sanitize_label_name("device-id"), "device_id");
        assert_eq!(sanitize_label_name("interface.name"), "interface_name");
    }

    #[test]
    fn test_sanitize_label_name_reserved() {
        // Labels starting with __ are reserved
        assert_eq!(sanitize_label_name("__meta"), "z__meta");
    }

    #[test]
    fn test_build_metric_name() {
        assert_eq!(
            build_metric_name("zensight", Protocol::Snmp, "sysUpTime"),
            "zensight_snmp_sysUpTime"
        );
        assert_eq!(
            build_metric_name("zensight", Protocol::Sysinfo, "cpu/usage"),
            "zensight_sysinfo_cpu_usage"
        );
        assert_eq!(
            build_metric_name("", Protocol::Netflow, "bytes"),
            "netflow_bytes"
        );
    }

    #[test]
    fn test_prometheus_type_from_value() {
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Counter(100)),
            PrometheusType::Counter
        );
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Gauge(3.14)),
            PrometheusType::Gauge
        );
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Boolean(true)),
            PrometheusType::Gauge
        );
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Text("hello".into())),
            PrometheusType::Info
        );
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Binary(vec![1, 2, 3])),
            PrometheusType::Untyped
        );
    }

    #[test]
    fn test_extract_numeric_value() {
        assert_eq!(
            extract_numeric_value(&TelemetryValue::Counter(100)),
            Some(100.0)
        );
        assert_eq!(
            extract_numeric_value(&TelemetryValue::Gauge(3.14)),
            Some(3.14)
        );
        assert_eq!(
            extract_numeric_value(&TelemetryValue::Boolean(true)),
            Some(1.0)
        );
        assert_eq!(
            extract_numeric_value(&TelemetryValue::Boolean(false)),
            Some(0.0)
        );
        assert_eq!(
            extract_numeric_value(&TelemetryValue::Text("hello".into())),
            None
        );
        assert_eq!(
            extract_numeric_value(&TelemetryValue::Binary(vec![1, 2, 3])),
            None
        );
    }

    #[test]
    fn test_is_exportable() {
        assert!(is_exportable(&TelemetryValue::Counter(100)));
        assert!(is_exportable(&TelemetryValue::Gauge(3.14)));
        assert!(is_exportable(&TelemetryValue::Boolean(true)));
        assert!(is_exportable(&TelemetryValue::Text("hello".into())));
        assert!(!is_exportable(&TelemetryValue::Binary(vec![1, 2, 3])));
    }
}
