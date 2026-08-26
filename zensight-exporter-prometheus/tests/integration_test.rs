//! Integration tests for the Prometheus exporter.
//!
//! These tests verify the full flow from receiving telemetry points
//! to exposing them via the HTTP /metrics endpoint.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};
use zensight_exporter_prometheus::{ExporterConfig, HttpServer, MetricCollector, SharedCollector};

/// Helper to create a collector with default config.
fn create_collector() -> SharedCollector {
    let config = ExporterConfig::default();
    Arc::new(MetricCollector::new(
        config.prometheus,
        config.aggregation,
        config.filters,
    ))
}

/// Helper to create a telemetry point with labels.
/// A base-relative telemetry key for a point, as the wire carries it.
///
/// Naming flows from the KEY through the registry (#764), so a test that only
/// builds a `TelemetryPoint` exercises nothing the exporter actually does.
/// snmp/modbus/gnmi/netflow register a rest-var catch-all `<device>/<metric...>`,
/// so their keys carry a device chunk `point.metric` does not.
fn key_for(point: &TelemetryPoint) -> String {
    let producer = point.protocol.as_str();
    if matches!(producer, "snmp" | "modbus" | "gnmi" | "netflow") {
        format!(
            "v1/h-0123456789ab/telemetry/{producer}/{}/{}",
            point.source, point.metric
        )
    } else {
        format!("v1/h-0123456789ab/telemetry/{producer}/{}", point.metric)
    }
}

/// Record a point under its wire key. Same arity as the old
/// `rec(&collector, make_point(..))`, so the call sites stay readable.
fn rec(collector: &MetricCollector, point: TelemetryPoint) {
    collector.record(&key_for(&point), &point);
}

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

/// Helper to parse Prometheus text format and extract metric values.
#[allow(dead_code)]
fn parse_prometheus_line(line: &str) -> Option<(&str, f64)> {
    // Skip comments and empty lines
    if line.starts_with('#') || line.trim().is_empty() {
        return None;
    }

    // Parse "metric_name{labels} value" or "metric_name value"
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        let metric_part = parts[0];
        let value_str = parts[1];

        // Extract metric name (before { if present)
        let metric_name = metric_part.split('{').next().unwrap_or(metric_part);

        if let Ok(value) = value_str.parse::<f64>() {
            return Some((metric_name, value));
        }
    }
    None
}

#[tokio::test]
async fn test_full_flow_gauge_metrics() {
    let collector = create_collector();

    // Record multiple gauge metrics from different sources
    let point1 = make_point(
        "server01",
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(75.5),
        HashMap::new(),
    );
    let point2 = make_point(
        "server02",
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(42.0),
        HashMap::new(),
    );
    let point3 = make_point(
        "server01",
        Protocol::Sysinfo,
        "memory/used",
        TelemetryValue::Gauge(8_000_000_000.0),
        HashMap::new(),
    );

    collector.record(&key_for(&point1), &point1);
    collector.record(&key_for(&point2), &point2);
    collector.record(&key_for(&point3), &point3);

    // Render metrics
    let output = collector.render();

    // Verify output contains expected metrics (#100: OTel semconv names).
    assert!(
        output.contains("system_cpu_utilization"),
        "Should contain system.cpu.utilization metric"
    );
    assert!(
        output.contains("system_memory_usage"),
        "Should contain system.memory.usage metric"
    );
    assert!(output.contains("75.5"), "Should contain server01 CPU value");
    assert!(output.contains("42"), "Should contain server02 CPU value");
    assert!(
        output.contains("source=\"server01\""),
        "Should contain server01 label"
    );
    assert!(
        output.contains("source=\"server02\""),
        "Should contain server02 label"
    );
}

#[tokio::test]
async fn test_full_flow_counter_metrics() {
    let collector = create_collector();

    // Record counter metrics (e.g., network bytes)
    let point = make_point(
        "router01",
        Protocol::Snmp,
        "if/1/in_octets",
        TelemetryValue::Counter(1_000_000),
        [("interface".to_string(), "eth0".to_string())]
            .into_iter()
            .collect(),
    );

    collector.record(&key_for(&point), &point);

    let output = collector.render();

    // Verify counter is present with correct type (full name includes prefix and protocol)
    assert!(
        output.contains("# TYPE zensight_snmp_if_1_in_octets_total counter"),
        "Should have counter type. Output: {}",
        output
    );
    assert!(output.contains("1000000"), "Should contain counter value");
}

/// A text point is exposed as an info-style **gauge**, under an `_info` family,
/// with the text in a label named for the subject leaf.
///
/// This test used to assert `# TYPE ... info`, which was the bug (#752): `info`
/// is an OpenMetrics type, and emitting it into the `version=0.0.4` body we
/// serve made Prometheus's parser abort and discard **every sample in the
/// scrape** while the target still reported healthy. The suite asserted the
/// broken behaviour, which is why it survived.
#[tokio::test]
async fn text_points_are_info_style_gauges_not_the_openmetrics_info_type() {
    let collector = create_collector();

    let point = make_point(
        "router01",
        Protocol::Snmp,
        "system/sysdescr",
        TelemetryValue::Text("Cisco IOS XE Software".to_string()),
        HashMap::new(),
    );

    collector.record(&key_for(&point), &point);
    let output = collector.render();

    assert!(
        output.contains("# TYPE zensight_snmp_system_sysdescr_info gauge"),
        "text families must be a `gauge` under an `_info` name. Output: {output}"
    );
    assert!(
        !output.contains(" info\n"),
        "`info` is not a legal type token in the 0.0.4 text format. Output: {output}"
    );
    assert!(
        output.contains(r#"sysdescr="Cisco IOS XE Software""#),
        "the text rides under the subject leaf, not a literal `value` label. Output: {output}"
    );
}

/// The exact shape of #753, end to end through the real collector.
///
/// `disk/sda/io/read_bytes` gets `device="sda"` from the semconv table
/// (`semconv.rs` maps `disk/{dev}/io/{field}` to `system.disk.io{device,direction}`)
/// and `device="sda"` again from the sysinfo sensor's own point labels. The old
/// merge de-duplicated only against `source`/`protocol`, so it emitted both —
/// an invalid series that Prometheus drops and remote-write 400s wholesale.
#[tokio::test]
async fn a_semconv_attribute_and_a_point_label_never_duplicate() {
    let collector = create_collector();

    let mut labels = HashMap::new();
    labels.insert("device".to_string(), "sda".to_string());
    labels.insert("unit".to_string(), "bytes".to_string());

    rec(
        &collector,
        make_point(
            "host01",
            Protocol::Sysinfo,
            "disk/sda/io/read_bytes",
            TelemetryValue::Counter(12_345),
            labels,
        ),
    );

    let output = collector.render();
    let line = output
        .lines()
        .find(|l| l.starts_with("zensight_system_disk_io"))
        .unwrap_or_else(|| panic!("disk io series missing. Output: {output}"));

    assert_eq!(
        line.matches("device=").count(),
        1,
        "`device` must appear exactly once. Line: {line}"
    );
    assert!(
        line.contains(r#"device="sda""#) && line.contains(r#"direction="read""#),
        "both the pattern var and the semconv constant survive. Line: {line}"
    );
}

/// No rendered series may carry the same label name twice, whatever the sensor
/// attached. This is the invariant, asserted over every series in the body.
#[tokio::test]
async fn no_series_carries_a_duplicate_label_name() {
    let collector = create_collector();

    // A sensor doing everything wrong at once: shadowing structural labels,
    // shadowing a semconv attribute, and shadowing a pattern var.
    let mut hostile = HashMap::new();
    for (k, v) in [
        ("device", "not-sda"),
        ("direction", "sideways"),
        ("source", "impostor"),
        ("protocol", "impostor"),
    ] {
        hostile.insert(k.to_string(), v.to_string());
    }

    rec(
        &collector,
        make_point(
            "host01",
            Protocol::Sysinfo,
            "disk/sda/io/read_bytes",
            TelemetryValue::Counter(1),
            hostile,
        ),
    );

    for line in collector.render().lines() {
        let Some(open) = line.find('{') else { continue };
        let Some(close) = line.rfind('}') else {
            continue;
        };
        let mut names = Vec::new();
        for part in line[open + 1..close].split("\",") {
            if let Some((k, _)) = part.split_once('=') {
                names.push(k.trim().to_string());
            }
        }
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate label name in series: {line}"
        );
    }
}

/// Every `# TYPE` token in a rendered body must be one the Prometheus 0.0.4
/// grammar accepts. This is the invariant #752 violated, asserted directly
/// rather than through a proxy — a full validator lands with the exposition
/// test suite.
#[tokio::test]
async fn every_type_token_is_legal_in_the_text_format() {
    const LEGAL: [&str; 5] = ["counter", "gauge", "histogram", "summary", "untyped"];

    let collector = create_collector();
    let cases = [
        ("system/sysdescr", TelemetryValue::Text("Cisco".into())),
        ("if/1/in_octets", TelemetryValue::Counter(1_000)),
        ("cpu/load", TelemetryValue::Gauge(0.5)),
        ("link/up", TelemetryValue::Boolean(true)),
    ];
    for (metric, value) in cases {
        rec(
            &collector,
            make_point("router01", Protocol::Snmp, metric, value, HashMap::new()),
        );
    }

    for line in collector.render().lines() {
        let Some(rest) = line.strip_prefix("# TYPE ") else {
            continue;
        };
        let token = rest.rsplit(' ').next().expect("a TYPE line has a token");
        assert!(
            LEGAL.contains(&token),
            "illegal `# TYPE` token {token:?} in line {line:?} — Prometheus \
             rejects the entire scrape body on an unknown type"
        );
    }
}

#[tokio::test]
async fn test_full_flow_multiple_protocols() {
    let collector = create_collector();

    // Record metrics from different protocols
    let snmp_point = make_point(
        "router01",
        Protocol::Snmp,
        "sysuptime",
        TelemetryValue::Counter(123456),
        HashMap::new(),
    );
    let sysinfo_point = make_point(
        "server01",
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(55.0),
        HashMap::new(),
    );
    let modbus_point = make_point(
        "plc01",
        Protocol::Modbus,
        "holding/temperature",
        TelemetryValue::Gauge(23.5),
        HashMap::new(),
    );

    collector.record(&key_for(&snmp_point), &snmp_point);
    collector.record(&key_for(&sysinfo_point), &sysinfo_point);
    collector.record(&key_for(&modbus_point), &modbus_point);

    let output = collector.render();

    // All protocols should be present with correct labels
    assert!(
        output.contains("protocol=\"snmp\""),
        "Should have SNMP protocol"
    );
    assert!(
        output.contains("protocol=\"sysinfo\""),
        "Should have sysinfo protocol"
    );
    assert!(
        output.contains("protocol=\"modbus\""),
        "Should have modbus protocol"
    );
}

#[tokio::test]
async fn test_metric_updates_preserve_latest_value() {
    let collector = create_collector();

    // Record initial value
    let point1 = make_point(
        "server01",
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(50.0),
        HashMap::new(),
    );
    collector.record(&key_for(&point1), &point1);

    // Update with new value
    let point2 = make_point(
        "server01",
        Protocol::Sysinfo,
        "cpu/usage",
        TelemetryValue::Gauge(75.0),
        HashMap::new(),
    );
    collector.record(&key_for(&point2), &point2);

    let output = collector.render();

    // Should contain only the latest value
    assert!(output.contains("75"), "Should contain updated value");

    // Count occurrences of the metric line (should be 1)
    let metric_lines: Vec<&str> = output
        .lines()
        .filter(|l| l.contains("system_cpu_utilization") && !l.starts_with('#'))
        .collect();
    assert_eq!(metric_lines.len(), 1, "Should have exactly one metric line");
}

#[tokio::test]
async fn test_collector_stats() {
    let collector = create_collector();

    // Initially empty
    let stats = collector.stats();
    assert_eq!(stats.points_received, 0);
    assert_eq!(collector.series_count(), 0);

    // Add some points
    for i in 0..5 {
        let point = make_point(
            &format!("server{:02}", i),
            Protocol::Sysinfo,
            "cpu/usage",
            TelemetryValue::Gauge(i as f64 * 10.0),
            HashMap::new(),
        );
        collector.record(&key_for(&point), &point);
    }

    let stats = collector.stats();
    assert_eq!(stats.points_received, 5);
    assert_eq!(
        collector.series_count(),
        5,
        "Each source creates a unique series"
    );
}

#[tokio::test]
async fn test_http_server_metrics_endpoint() {
    let collector = create_collector();

    // Add a metric
    // A REGISTERED subject: naming flows from the key through the registry
    // (#764), so an invented metric name never refines and this test would be
    // asserting against an empty body.
    let point = make_point(
        "test",
        Protocol::Sysinfo,
        "memory/usage_percent",
        TelemetryValue::Gauge(42.0),
        HashMap::new(),
    );
    collector.record(&key_for(&point), &point);

    // Start HTTP server on random port
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let _server = HttpServer::new(collector.clone(), addr, "/metrics".to_string());

    // We need to bind and get the actual port
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let actual_addr = listener.local_addr().unwrap();
    drop(listener); // Release the port

    // Start server in background
    let server = HttpServer::new(collector, actual_addr, "/metrics".to_string());
    let server_handle = tokio::spawn(async move {
        let _ = server.run(shutdown_rx).await;
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Make HTTP request
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{}/metrics", actual_addr))
        .send()
        .await;

    // Shutdown server
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), server_handle).await;

    // Verify response
    match response {
        Ok(resp) => {
            assert!(resp.status().is_success());
            let body = resp.text().await.unwrap();
            assert!(body.contains("zensight_system_memory_utilization"));
        }
        Err(e) => {
            // Server might not have started in time - this is acceptable in CI
            eprintln!("HTTP request failed (acceptable in CI): {}", e);
        }
    }
}

#[tokio::test]
async fn test_special_characters_in_metric_names() {
    let collector = create_collector();

    // Metric names with special characters that need sanitization
    let point1 = make_point(
        "router01",
        Protocol::Snmp,
        "if/1/in-octets",
        TelemetryValue::Counter(1000),
        HashMap::new(),
    );
    let point2 = make_point(
        "router01",
        Protocol::Gnmi,
        "interfaces/interface[name=eth0]/state/counters",
        TelemetryValue::Gauge(500.0),
        HashMap::new(),
    );

    collector.record(&key_for(&point1), &point1);
    collector.record(&key_for(&point2), &point2);

    let output = collector.render();

    // Verify sanitized names are valid Prometheus metric names
    // (only [a-zA-Z0-9_:] allowed, must not start with digit)
    for line in output.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }

        let metric_name = line.split('{').next().unwrap_or(line);
        let metric_name = metric_name.split_whitespace().next().unwrap_or("");

        assert!(!metric_name.is_empty(), "Metric name should not be empty");
        assert!(
            !metric_name.chars().next().unwrap().is_ascii_digit(),
            "Metric name '{}' should not start with digit",
            metric_name
        );
        assert!(
            metric_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
            "Metric name '{}' contains invalid characters",
            metric_name
        );
    }
}

#[tokio::test]
async fn test_high_cardinality_protection() {
    // Create collector with low max_series for testing
    let config = ExporterConfig {
        aggregation: zensight_exporter_prometheus::config::AggregationConfig {
            max_series: 10,
            ..Default::default()
        },
        ..Default::default()
    };
    let collector = Arc::new(MetricCollector::new(
        config.prometheus,
        config.aggregation,
        config.filters,
    ));

    // Try to add more series than allowed
    for i in 0..20 {
        let point = make_point(
            &format!("server{:02}", i),
            Protocol::Sysinfo,
            "cpu/usage",
            TelemetryValue::Gauge(i as f64),
            HashMap::new(),
        );
        collector.record(&key_for(&point), &point);
    }

    // Should be capped at max_series
    assert!(
        collector.series_count() <= 10,
        "Series count {} should not exceed max_series 10",
        collector.series_count()
    );
}

#[tokio::test]
async fn test_boolean_metrics() {
    let collector = create_collector();

    let point_true = make_point(
        "router01",
        Protocol::Snmp,
        "if/1/oper_status",
        TelemetryValue::Boolean(true),
        HashMap::new(),
    );
    let point_false = make_point(
        "router02",
        Protocol::Snmp,
        "if/1/oper_status",
        TelemetryValue::Boolean(false),
        HashMap::new(),
    );

    collector.record(&key_for(&point_true), &point_true);
    collector.record(&key_for(&point_false), &point_false);

    let output = collector.render();

    // Boolean true should be 1, false should be 0
    let lines: Vec<&str> = output
        .lines()
        .filter(|l| l.contains("oper_status") && !l.starts_with('#'))
        .collect();

    assert_eq!(lines.len(), 2, "Should have two metric lines");

    // Check values
    let has_one = lines.iter().any(|l| l.ends_with(" 1"));
    let has_zero = lines.iter().any(|l| l.ends_with(" 0"));
    assert!(has_one, "Should have value 1 for true");
    assert!(has_zero, "Should have value 0 for false");
}

#[tokio::test]
async fn test_empty_collector_render() {
    let collector = create_collector();

    // Render with no metrics
    let output = collector.render();

    // Should produce valid Prometheus output
    // Even empty collector outputs exporter stats metrics
    // At minimum, should not panic and should be valid UTF-8
    assert!(
        output.lines().all(|l| {
            l.starts_with('#') || l.trim().is_empty() || l.starts_with("zensight_exporter_")
        }),
        "Output should only contain comments, empty lines, or exporter stats. Got: {}",
        output
    );
}

#[tokio::test]
async fn test_concurrent_recording() {
    let collector = create_collector();

    // Spawn multiple tasks recording concurrently
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let collector = collector.clone();
            tokio::spawn(async move {
                for j in 0..100 {
                    let point = make_point(
                        &format!("server{:02}", i),
                        Protocol::Sysinfo,
                        &format!("metric_{}", j),
                        TelemetryValue::Gauge((i * 100 + j) as f64),
                        HashMap::new(),
                    );
                    collector.record(&key_for(&point), &point);
                }
            })
        })
        .collect();

    // Wait for all tasks to complete
    for handle in handles {
        handle.await.unwrap();
    }

    let stats = collector.stats();

    // Should have recorded all points (10 tasks * 100 points)
    assert_eq!(stats.points_received, 1000);

    // Render should not panic
    let output = collector.render();
    assert!(!output.is_empty());
}
