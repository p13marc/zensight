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
        && first.is_ascii_digit()
    {
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
    // OTel host-metrics semconv (#100): a mapped key exports under its `system.*`
    // name (sanitized, no protocol segment); state/direction/device become labels.
    if let Some(sc) = zensight_common::semconv::metric_semconv(protocol, metric) {
        let name = sanitize_metric_name(sc.name);
        return if prefix.is_empty() {
            name
        } else {
            format!("{prefix}_{name}")
        };
    }

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
    /// A `TelemetryValue::Text` point, rendered as an info-style **gauge**.
    ///
    /// This is deliberately NOT called `Info` any more, and [`Self::as_str`]
    /// deliberately does not return `"info"` (#752). `info` is an *OpenMetrics*
    /// type; the Prometheus text exposition format we serve
    /// (`text/plain; version=0.0.4`, see `http.rs`) admits only
    /// `counter | gauge | histogram | summary | untyped`. On an unknown type
    /// token Prometheus's parser errors and **rolls the whole scrape back** —
    /// every sample in the body, not just the offending family — while the
    /// target still reports healthy. One netlink MAC address or one SNMP
    /// `sysDescr` was enough to empty the entire endpoint.
    ///
    /// The series is emitted as `<name>_info{..., <leaf>="<text>"} 1`. The
    /// `_info` suffix is load-bearing: it keeps a text family from ever sharing
    /// a `# TYPE` block with a numeric family of the same name.
    Text,
    Untyped,
}

impl PrometheusType {
    /// Determine the type from a TelemetryValue.
    pub fn from_value(value: &TelemetryValue) -> Self {
        match value {
            TelemetryValue::Counter(_) => PrometheusType::Counter,
            TelemetryValue::Gauge(_) => PrometheusType::Gauge,
            TelemetryValue::Boolean(_) => PrometheusType::Gauge,
            TelemetryValue::Text(_) => PrometheusType::Text,
            TelemetryValue::Binary(_) => PrometheusType::Untyped,
        }
    }

    /// Get the TYPE comment string for Prometheus exposition format.
    ///
    /// Every arm must return a token the 0.0.4 grammar accepts — see
    /// [`PrometheusType::Text`] for what happens when one does not.
    pub fn as_str(&self) -> &'static str {
        match self {
            PrometheusType::Counter => "counter",
            PrometheusType::Gauge => "gauge",
            PrometheusType::Text => "gauge",
            PrometheusType::Untyped => "untyped",
        }
    }
}

/// The suffix appended to a text point's family name (#752).
pub const INFO_SUFFIX: &str = "_info";

/// Longest text value we will put in a label. A gnmi `Text` value is
/// device-defined and unbounded; an unbounded label value is a cardinality and
/// a body-size problem at once.
pub const MAX_TEXT_LEN: usize = 128;

/// The label name a text point's value rides under.
///
/// Derived from the **leaf** of the subject path, so
/// `iface/{iface}/oper_state` yields `oper_state="up"` rather than the
/// meaningless `value="up"` this used to emit. Falls back to `value` when the
/// leaf is itself `info` (or sanitizes away to nothing), which is the only case
/// where the old spelling was ever the right one.
pub fn text_label_name(metric: &str) -> String {
    let leaf = metric.rsplit('/').next().unwrap_or(metric);
    // A `.rate` style dot-suffix is part of the leaf name, not a path chunk.
    let sanitized = sanitize_label_name(leaf);
    if sanitized.is_empty() || sanitized == "info" {
        "value".to_string()
    } else {
        sanitized
    }
}

/// Clamp a text value to something safe to put in a label: no control
/// characters (a raw newline would terminate the sample line and corrupt every
/// byte after it), and bounded length.
pub fn clamp_text(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT_LEN)
        .collect();
    cleaned
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

/// The Prometheus name suffix a UCUM-ish unit implies.
///
/// Convention only — the VALUE is never rescaled. A `ms` unit gets no suffix
/// because renaming it `_seconds` without dividing by 1000 would be a lie, and
/// dividing would silently change what every existing dashboard reads. When
/// there is no suffix the unit still reaches the reader, in `# HELP`.
pub fn unit_suffix(unit: &str) -> Option<&'static str> {
    match unit {
        "By" | "bytes" => Some("_bytes"),
        "s" | "seconds" => Some("_seconds"),
        "By/s" => Some("_bytes_per_second"),
        "1/s" => Some("_per_second"),
        "%" | "percent" => Some("_percent"),
        "Cel" => Some("_celsius"),
        _ => None,
    }
}

/// Apply the Prometheus naming conventions to a family name.
///
/// A counter gains `_total`; a unit with a conventional suffix gains it. Both
/// are idempotent — a name that already ends the right way is left alone,
/// because `..._bytes_bytes` helps nobody.
pub fn apply_conventions(name: &str, kind: PrometheusType, unit: Option<&str>) -> String {
    let mut out = name.to_string();
    if let Some(suffix) = unit.and_then(unit_suffix)
        && !out.ends_with(suffix)
    {
        out.push_str(suffix);
    }
    if kind == PrometheusType::Counter && !out.ends_with("_total") {
        out.push_str("_total");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The conventions are idempotent, so a name that already ends the right
    /// way is left alone — `..._bytes_bytes` helps nobody.
    #[test]
    fn conventions_are_idempotent() {
        assert_eq!(
            apply_conventions(
                "zensight_netlink_iface_rx_bytes",
                PrometheusType::Counter,
                Some("By")
            ),
            "zensight_netlink_iface_rx_bytes_total",
            "the name already ends _bytes, so only _total is appended"
        );
        assert_eq!(
            apply_conventions("x_total", PrometheusType::Counter, None),
            "x_total"
        );
        assert_eq!(
            apply_conventions("x", PrometheusType::Counter, Some("By")),
            "x_bytes_total"
        );
        assert_eq!(
            apply_conventions("x", PrometheusType::Gauge, Some("By")),
            "x_bytes",
            "a gauge never gains _total"
        );
    }

    /// A unit with no conventional suffix must NOT be renamed. Calling a
    /// millisecond metric `_seconds` without dividing by 1000 is a lie, and
    /// dividing would silently change what every dashboard reads — so the unit
    /// rides in `# HELP` instead.
    #[test]
    fn an_unconvertible_unit_gets_no_suffix() {
        assert_eq!(unit_suffix("ms"), None);
        assert_eq!(
            apply_conventions("latency", PrometheusType::Gauge, Some("ms")),
            "latency"
        );
    }

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
        // #100: mapped sysinfo keys export under the sanitized system.* name (no
        // protocol segment); unmapped keys keep the protocol-qualified name.
        assert_eq!(
            build_metric_name("zensight", Protocol::Sysinfo, "cpu/usage"),
            "zensight_system_cpu_utilization"
        );
        assert_eq!(
            build_metric_name("", Protocol::Sysinfo, "memory/used"),
            "system_memory_usage"
        );
        assert_eq!(
            build_metric_name("zensight", Protocol::Sysinfo, "network/conntrack/count"),
            "zensight_sysinfo_network_conntrack_count"
        );
        assert_eq!(
            build_metric_name("", Protocol::Netflow, "bytes"),
            "netflow_bytes"
        );
        // #282: systemd per-unit series collapse to a clean name (unit rides as a
        // label); aggregates keep the protocol-qualified raw name.
        assert_eq!(
            build_metric_name("zensight", Protocol::Systemd, "unit/sshd.service/active"),
            "zensight_systemd_unit_active"
        );
        assert_eq!(
            build_metric_name("zensight", Protocol::Systemd, "units/failed"),
            "zensight_systemd_units_failed"
        );
    }

    #[test]
    fn test_prometheus_type_from_value() {
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Counter(100)),
            PrometheusType::Counter
        );
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Gauge(2.5)),
            PrometheusType::Gauge
        );
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Boolean(true)),
            PrometheusType::Gauge
        );
        assert_eq!(
            PrometheusType::from_value(&TelemetryValue::Text("hello".into())),
            PrometheusType::Text
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
            extract_numeric_value(&TelemetryValue::Gauge(2.5)),
            Some(2.5)
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
        assert!(is_exportable(&TelemetryValue::Gauge(2.5)));
        assert!(is_exportable(&TelemetryValue::Boolean(true)));
        assert!(is_exportable(&TelemetryValue::Text("hello".into())));
        assert!(!is_exportable(&TelemetryValue::Binary(vec![1, 2, 3])));
    }

    /// #470: the logs sensor no longer prefixes its metric names with its own
    /// producer name, and the exported series name follows — because it derives
    /// from `point.metric`, not from the key.
    ///
    /// This is the breaking half of #470 and the reason it is pinned: the KEY
    /// change is invisible to consumers (everything subscribes by class
    /// wildcard), but the SERIES change is not — it renames every logs metric in
    /// every Prometheus dashboard and alert rule built on them.
    #[test]
    fn logs_series_lost_their_doubled_name() {
        let name = |m| build_metric_name("zensight", Protocol::Logs, m);

        // was: zensight_logs_logs_errors_total
        assert_eq!(name("errors_total"), "zensight_logs_errors_total");
        assert_eq!(name("units_in_failure"), "zensight_logs_units_in_failure");
        assert_eq!(
            name("journald/read_total"),
            "zensight_logs_journald_read_total"
        );
        assert_eq!(
            name("by_unit/nginx.service/messages_total"),
            "zensight_logs_by_unit_nginx_service_messages_total"
        );

        // No logs series contains the producer name twice any more.
        for m in ["errors_total", "warnings_total", "units_in_failure"] {
            assert!(
                !name(m).contains("logs_logs"),
                "the doubled producer name survived: {}",
                name(m)
            );
        }
    }
}
