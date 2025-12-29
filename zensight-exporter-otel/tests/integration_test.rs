//! Integration tests for the OpenTelemetry exporter.
//!
//! These tests verify the data conversion and filtering logic.
//! Note: Full OTLP endpoint tests require external services.

use std::collections::HashMap;

use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};
use zensight_exporter_otel::config::FilterConfig;
use zensight_exporter_otel::logs::{LogRecord, SyslogSeverity};
use zensight_exporter_otel::metrics::{
    OtelMetricType, build_metric_attributes, build_metric_name, extract_value, is_log_exportable,
    is_metric_exportable,
};

/// Helper to create a telemetry point with labels.
fn make_point(
    source: &str,
    protocol: Protocol,
    metric: &str,
    value: TelemetryValue,
    labels: HashMap<String, String>,
) -> TelemetryPoint {
    let mut point = TelemetryPoint::new(source, protocol, metric, value);
    point.labels = labels;
    point
}

// =============================================================================
// Metric Conversion Tests
// =============================================================================

#[test]
fn test_counter_to_otel_metric() {
    let point = make_point(
        "router01",
        Protocol::Snmp,
        "if/1/ifInOctets",
        TelemetryValue::Counter(1_000_000),
        HashMap::new(),
    );

    assert!(is_metric_exportable(&point.value));
    assert_eq!(
        OtelMetricType::from_value(&point.value),
        OtelMetricType::Counter
    );
    assert_eq!(extract_value(&point.value), Some(1_000_000.0));
}

#[test]
fn test_gauge_to_otel_metric() {
    let point = make_point(
        "server01",
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(75.5),
        HashMap::new(),
    );

    assert!(is_metric_exportable(&point.value));
    assert_eq!(
        OtelMetricType::from_value(&point.value),
        OtelMetricType::Gauge
    );
    assert_eq!(extract_value(&point.value), Some(75.5));
}

#[test]
fn test_boolean_to_otel_metric() {
    let point_true = make_point(
        "router01",
        Protocol::Snmp,
        "if/1/ifOperStatus",
        TelemetryValue::Boolean(true),
        HashMap::new(),
    );
    let point_false = make_point(
        "router01",
        Protocol::Snmp,
        "if/1/ifOperStatus",
        TelemetryValue::Boolean(false),
        HashMap::new(),
    );

    assert!(is_metric_exportable(&point_true.value));
    assert!(is_metric_exportable(&point_false.value));
    assert_eq!(
        OtelMetricType::from_value(&point_true.value),
        OtelMetricType::Gauge
    );
    assert_eq!(extract_value(&point_true.value), Some(1.0));
    assert_eq!(extract_value(&point_false.value), Some(0.0));
}

#[test]
fn test_text_not_metric_exportable() {
    let point = make_point(
        "router01",
        Protocol::Snmp,
        "sysDescr",
        TelemetryValue::Text("Cisco IOS".to_string()),
        HashMap::new(),
    );

    assert!(!is_metric_exportable(&point.value));
    assert_eq!(
        OtelMetricType::from_value(&point.value),
        OtelMetricType::NotExportable
    );
}

#[test]
fn test_binary_not_metric_exportable() {
    let point = make_point(
        "server01",
        Protocol::Snmp,
        "data",
        TelemetryValue::Binary(vec![1, 2, 3, 4]),
        HashMap::new(),
    );

    assert!(!is_metric_exportable(&point.value));
    assert_eq!(
        OtelMetricType::from_value(&point.value),
        OtelMetricType::NotExportable
    );
}

#[test]
fn test_metric_name_building() {
    // Metric names include zensight prefix and use dots as separators
    assert_eq!(
        build_metric_name(Protocol::Snmp, "sysUpTime"),
        "zensight.snmp.sysUpTime"
    );
    assert_eq!(
        build_metric_name(Protocol::Sysinfo, "cpu/usage"),
        "zensight.sysinfo.cpu.usage"
    );
    assert_eq!(
        build_metric_name(Protocol::Modbus, "holding/temperature"),
        "zensight.modbus.holding.temperature"
    );
}

#[test]
fn test_metric_attributes() {
    let mut labels = HashMap::new();
    labels.insert("interface".to_string(), "eth0".to_string());

    let point = make_point(
        "router01",
        Protocol::Snmp,
        "if/1/ifInOctets",
        TelemetryValue::Counter(1000),
        labels,
    );

    let attrs = build_metric_attributes(&point);

    // Should have source, protocol, and custom labels
    assert!(
        attrs
            .iter()
            .any(|kv| kv.key.as_str() == "source" && kv.value.as_str() == "router01")
    );
    assert!(
        attrs
            .iter()
            .any(|kv| kv.key.as_str() == "protocol" && kv.value.as_str() == "snmp")
    );
    assert!(
        attrs
            .iter()
            .any(|kv| kv.key.as_str() == "interface" && kv.value.as_str() == "eth0")
    );
}

// =============================================================================
// Log Conversion Tests
// =============================================================================

#[test]
fn test_syslog_to_otel_log() {
    let point = make_point(
        "server01",
        Protocol::Syslog,
        "daemon/warning",
        TelemetryValue::Text("Connection timeout".to_string()),
        [
            ("severity".to_string(), "warning".to_string()),
            ("facility".to_string(), "daemon".to_string()),
            ("appname".to_string(), "myapp".to_string()),
        ]
        .into_iter()
        .collect(),
    );

    assert!(is_log_exportable(&point.value, point.protocol));

    let log_record = LogRecord::from_telemetry(&point);
    assert!(log_record.is_some());

    let record = log_record.unwrap();
    assert_eq!(record.body, "Connection timeout");
    assert_eq!(record.hostname, "server01");
    assert_eq!(record.severity, SyslogSeverity::Warning);
    assert_eq!(record.facility.unwrap().as_str(), "daemon");
    assert_eq!(record.appname.unwrap(), "myapp");
}

#[test]
fn test_non_syslog_not_log_exportable() {
    let point = make_point(
        "router01",
        Protocol::Snmp,
        "sysDescr",
        TelemetryValue::Text("Cisco IOS".to_string()),
        HashMap::new(),
    );

    // SNMP text is not a log
    assert!(!is_log_exportable(&point.value, point.protocol));
}

#[test]
fn test_syslog_severity_mapping() {
    use opentelemetry::logs::Severity;

    // Test all severity mappings
    let test_cases = [
        (SyslogSeverity::Emergency, Severity::Fatal),
        (SyslogSeverity::Alert, Severity::Fatal),
        (SyslogSeverity::Critical, Severity::Error),
        (SyslogSeverity::Error, Severity::Error),
        (SyslogSeverity::Warning, Severity::Warn),
        (SyslogSeverity::Notice, Severity::Info),
        (SyslogSeverity::Informational, Severity::Info),
        (SyslogSeverity::Debug, Severity::Debug),
    ];

    for (syslog_sev, expected_otel_sev) in test_cases {
        assert_eq!(
            syslog_sev.to_otel_severity(),
            expected_otel_sev,
            "Syslog {:?} should map to OTEL {:?}",
            syslog_sev,
            expected_otel_sev
        );
    }
}

#[test]
fn test_syslog_severity_parsing() {
    assert_eq!(
        SyslogSeverity::from_str("emergency"),
        Some(SyslogSeverity::Emergency)
    );
    assert_eq!(
        SyslogSeverity::from_str("alert"),
        Some(SyslogSeverity::Alert)
    );
    assert_eq!(
        SyslogSeverity::from_str("critical"),
        Some(SyslogSeverity::Critical)
    );
    assert_eq!(
        SyslogSeverity::from_str("error"),
        Some(SyslogSeverity::Error)
    );
    assert_eq!(
        SyslogSeverity::from_str("warning"),
        Some(SyslogSeverity::Warning)
    );
    assert_eq!(
        SyslogSeverity::from_str("notice"),
        Some(SyslogSeverity::Notice)
    );
    assert_eq!(
        SyslogSeverity::from_str("info"),
        Some(SyslogSeverity::Informational)
    );
    assert_eq!(
        SyslogSeverity::from_str("debug"),
        Some(SyslogSeverity::Debug)
    );

    // Case insensitive
    assert_eq!(
        SyslogSeverity::from_str("WARNING"),
        Some(SyslogSeverity::Warning)
    );
    assert_eq!(
        SyslogSeverity::from_str("Error"),
        Some(SyslogSeverity::Error)
    );

    // Unknown returns None
    assert_eq!(SyslogSeverity::from_str("unknown"), None);
}

// =============================================================================
// Filter Tests
// =============================================================================

#[test]
fn test_filter_include_protocols() {
    use zensight_exporter_otel::exporter::TelemetryFilter;

    let config = FilterConfig {
        include_protocols: vec!["snmp".to_string(), "sysinfo".to_string()],
        ..Default::default()
    };
    let filter = TelemetryFilter::new(&config);

    let snmp_point = make_point(
        "router01",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );
    let sysinfo_point = make_point(
        "server01",
        Protocol::Sysinfo,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );
    let modbus_point = make_point(
        "plc01",
        Protocol::Modbus,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );

    assert!(filter.should_include(&snmp_point));
    assert!(filter.should_include(&sysinfo_point));
    assert!(!filter.should_include(&modbus_point));
}

#[test]
fn test_filter_exclude_protocols() {
    use zensight_exporter_otel::exporter::TelemetryFilter;

    let config = FilterConfig {
        exclude_protocols: vec!["syslog".to_string()],
        ..Default::default()
    };
    let filter = TelemetryFilter::new(&config);

    let snmp_point = make_point(
        "router01",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );
    let syslog_point = make_point(
        "server01",
        Protocol::Syslog,
        "message",
        TelemetryValue::Text("log".to_string()),
        HashMap::new(),
    );

    assert!(filter.should_include(&snmp_point));
    assert!(!filter.should_include(&syslog_point));
}

#[test]
fn test_filter_include_sources() {
    use zensight_exporter_otel::exporter::TelemetryFilter;

    let config = FilterConfig {
        include_sources: vec!["router01".to_string(), "router02".to_string()],
        ..Default::default()
    };
    let filter = TelemetryFilter::new(&config);

    let point1 = make_point(
        "router01",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );
    let point2 = make_point(
        "router02",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );
    let point3 = make_point(
        "router03",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );

    assert!(filter.should_include(&point1));
    assert!(filter.should_include(&point2));
    assert!(!filter.should_include(&point3));
}

#[test]
fn test_filter_exclude_sources() {
    use zensight_exporter_otel::exporter::TelemetryFilter;

    let config = FilterConfig {
        exclude_sources: vec!["test-device".to_string()],
        ..Default::default()
    };
    let filter = TelemetryFilter::new(&config);

    let point1 = make_point(
        "test-device",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );
    let point2 = make_point(
        "prod-device",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );

    assert!(!filter.should_include(&point1));
    assert!(filter.should_include(&point2));
}

#[test]
fn test_filter_combined() {
    use zensight_exporter_otel::exporter::TelemetryFilter;

    let config = FilterConfig {
        include_protocols: vec!["snmp".to_string()],
        exclude_sources: vec!["test-router".to_string()],
        ..Default::default()
    };
    let filter = TelemetryFilter::new(&config);

    // SNMP from prod-router: should pass
    let point1 = make_point(
        "prod-router",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );

    // SNMP from test-router: should fail (excluded source)
    let point2 = make_point(
        "test-router",
        Protocol::Snmp,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );

    // Sysinfo from prod-server: should fail (wrong protocol)
    let point3 = make_point(
        "prod-server",
        Protocol::Sysinfo,
        "metric",
        TelemetryValue::Gauge(1.0),
        HashMap::new(),
    );

    assert!(filter.should_include(&point1));
    assert!(!filter.should_include(&point2));
    assert!(!filter.should_include(&point3));
}

#[test]
fn test_filter_empty_allows_all() {
    use zensight_exporter_otel::exporter::TelemetryFilter;

    let config = FilterConfig::default();
    let filter = TelemetryFilter::new(&config);

    let points = vec![
        make_point(
            "router01",
            Protocol::Snmp,
            "metric",
            TelemetryValue::Gauge(1.0),
            HashMap::new(),
        ),
        make_point(
            "server01",
            Protocol::Sysinfo,
            "metric",
            TelemetryValue::Gauge(1.0),
            HashMap::new(),
        ),
        make_point(
            "server01",
            Protocol::Syslog,
            "message",
            TelemetryValue::Text("log".to_string()),
            HashMap::new(),
        ),
    ];

    for point in points {
        assert!(
            filter.should_include(&point),
            "Empty filter should allow {:?}",
            point.protocol
        );
    }
}

// =============================================================================
// Multiple Protocol Flow Tests
// =============================================================================

#[test]
fn test_multiple_protocols_classification() {
    let test_cases = vec![
        // (protocol, value, is_metric, is_log)
        (Protocol::Snmp, TelemetryValue::Counter(100), true, false),
        (Protocol::Snmp, TelemetryValue::Gauge(50.5), true, false),
        (
            Protocol::Snmp,
            TelemetryValue::Text("desc".into()),
            false,
            false,
        ),
        (Protocol::Sysinfo, TelemetryValue::Gauge(75.0), true, false),
        (
            Protocol::Syslog,
            TelemetryValue::Text("log message".into()),
            false,
            true,
        ),
        (Protocol::Modbus, TelemetryValue::Gauge(23.5), true, false),
        (
            Protocol::Netflow,
            TelemetryValue::Counter(1000),
            true,
            false,
        ),
    ];

    for (protocol, value, expected_metric, expected_log) in test_cases {
        assert_eq!(
            is_metric_exportable(&value),
            expected_metric,
            "Protocol {:?} with value {:?} metric exportable mismatch",
            protocol,
            value
        );
        assert_eq!(
            is_log_exportable(&value, protocol),
            expected_log,
            "Protocol {:?} with value {:?} log exportable mismatch",
            protocol,
            value
        );
    }
}

#[test]
fn test_full_syslog_flow() {
    // Simulate a complete syslog message flow
    let mut labels = HashMap::new();
    labels.insert("severity".to_string(), "error".to_string());
    labels.insert("facility".to_string(), "auth".to_string());
    labels.insert("appname".to_string(), "sshd".to_string());
    labels.insert("procid".to_string(), "12345".to_string());

    let point = make_point(
        "server01",
        Protocol::Syslog,
        "auth/error",
        TelemetryValue::Text("Failed password for invalid user admin".to_string()),
        labels,
    );

    // Should be log exportable
    assert!(is_log_exportable(&point.value, point.protocol));
    assert!(!is_metric_exportable(&point.value));

    // Should produce valid log record
    let record = LogRecord::from_telemetry(&point).expect("Should create log record");

    assert_eq!(record.hostname, "server01");
    assert_eq!(record.body, "Failed password for invalid user admin");
    assert_eq!(record.severity, SyslogSeverity::Error);
    assert_eq!(record.appname.as_deref(), Some("sshd"));

    // OTEL severity should be Error
    assert_eq!(record.otel_severity(), opentelemetry::logs::Severity::Error);
}
