//! Mapping from ZenSight TelemetryPoint to OpenTelemetry metrics.

use opentelemetry::KeyValue;
use zensight_common::telemetry::{Protocol, TelemetryValue};

/// The host an observed signal came from, for its OTLP `Resource` (#755).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedHost {
    /// The RFC 06 minted origin chunk, e.g. `h-0ead7da13eea`. Stable across
    /// hostname changes, which is why it and not `host.name` is the identity.
    pub origin: String,
    /// The sensor's self-reported hostname.
    pub host_name: String,
    /// The registry producer that emitted this, with its instance suffix when
    /// it has one (`netring-2`).
    pub producer: String,
}

/// Build resource attributes.
///
/// # Why the observed host belongs here and not on the data point (#755)
///
/// This used to emit `service.name`, an optional `service.version` and whatever
/// the operator hand-wrote — and nothing else. Every host on the bus therefore
/// shared ONE `Resource`, despite the keyspace origin being literally
/// `h-<12hex>`.
///
/// For metrics that shows up as `job="zensight", instance=""`. For **logs** it
/// is worse: a backend derives stream identity from the resource, so the whole
/// fleet collapsed into a single log stream with the host demoted to structured
/// metadata. For traces the spans carried no host at all.
///
/// `service.name` is per-producer (`zensight.netlink`), because that is what a
/// service *is* here — the thing emitting the signal — and it is what makes a
/// service map meaningful rather than one node called "zensight".
///
/// Operator-supplied `resource` attributes and `service.version` merge
/// underneath and never override these: they are configuration, and this is
/// observed truth off the wire.
pub fn build_resource_attributes(
    service_name: &str,
    service_version: Option<&str>,
    extra_attrs: &std::collections::HashMap<String, String>,
    host: Option<&ObservedHost>,
) -> Vec<KeyValue> {
    let mut attrs = Vec::with_capacity(5 + extra_attrs.len());

    // Weakest first, so the loop below cannot clobber observed truth.
    for (k, v) in extra_attrs {
        attrs.push(KeyValue::new(k.clone(), v.clone()));
    }
    if let Some(version) = service_version {
        attrs.push(KeyValue::new("service.version", version.to_string()));
    }

    match host {
        Some(h) => {
            attrs.push(KeyValue::new(
                "service.name",
                format!("{service_name}.{}", h.producer),
            ));
            attrs.push(KeyValue::new("host.id", h.origin.clone()));
            attrs.push(KeyValue::new("host.name", h.host_name.clone()));
            attrs.push(KeyValue::new(
                "service.instance.id",
                format!("{}/{}", h.origin, h.producer),
            ));
        }
        None => attrs.push(KeyValue::new("service.name", service_name.to_string())),
    }

    attrs
}

/// OTel host-metrics semconv (#100): keys with a standard mapping export under
/// their `system.*` name (e.g. `memory/used` → `system.memory.usage`); everything
/// else falls back to `zensight.{protocol}.{metric_path}`.
pub fn build_metric_name(protocol: Protocol, metric: &str) -> String {
    if let Some(sc) = zensight_common::semconv::metric_semconv(protocol, metric) {
        return sc.name.to_string();
    }
    // Replace slashes with dots for OTEL convention
    let sanitized = metric.replace('/', ".");

    format!("zensight.{}.{}", protocol.as_str(), sanitized)
}

/// Determine the OTEL metric type from a TelemetryValue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtelMetricType {
    /// Monotonically increasing counter.
    Counter,
    /// Point-in-time gauge value.
    Gauge,
    /// Not exportable as a metric.
    NotExportable,
}

impl OtelMetricType {
    /// Determine the type from a TelemetryValue.
    pub fn from_value(value: &TelemetryValue) -> Self {
        match value {
            TelemetryValue::Counter(_) => OtelMetricType::Counter,
            TelemetryValue::Gauge(_) => OtelMetricType::Gauge,
            TelemetryValue::Boolean(_) => OtelMetricType::Gauge,
            TelemetryValue::Text(_) => OtelMetricType::NotExportable,
            TelemetryValue::Binary(_) => OtelMetricType::NotExportable,
        }
    }
}

/// Extract a numeric value from TelemetryValue.
pub fn extract_value(value: &TelemetryValue) -> Option<f64> {
    match value {
        TelemetryValue::Counter(v) => Some(*v as f64),
        TelemetryValue::Gauge(v) => Some(*v),
        TelemetryValue::Boolean(v) => Some(if *v { 1.0 } else { 0.0 }),
        TelemetryValue::Text(_) => None,
        TelemetryValue::Binary(_) => None,
    }
}

/// Check if a TelemetryValue can be exported as an OTEL metric.
pub fn is_metric_exportable(value: &TelemetryValue) -> bool {
    !matches!(value, TelemetryValue::Text(_) | TelemetryValue::Binary(_))
}

/// Check if a TelemetryValue can be exported as an OTEL log.
pub fn is_log_exportable(value: &TelemetryValue, protocol: Protocol) -> bool {
    // Only syslog text messages are exported as logs
    matches!(protocol, Protocol::Logs) && matches!(value, TelemetryValue::Text(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_build_resource_attributes() {
        let mut extra = HashMap::new();
        extra.insert("env".to_string(), "prod".to_string());

        let attrs = build_resource_attributes("zensight", Some("1.0.0"), &extra, None);

        assert!(attrs.iter().any(|kv| kv.key.as_str() == "service.name"));
        assert!(attrs.iter().any(|kv| kv.key.as_str() == "service.version"));
        assert!(attrs.iter().any(|kv| kv.key.as_str() == "env"));
    }

    // `build_metric_attributes` / `build_metric_name` are gone (#764): naming
    // and attributes now come from `zensight_common::exposition::identify`,
    // which resolves the KEY through the registry instead of guessing from the
    // payload. Their coverage moved with them —
    // `zensight-common/src/exposition.rs` tests the merge precedence and
    // `zensight-common/tests/exposition_naming.rs` walks every registry
    // pattern through the family rule. The end-to-end shape is asserted on the
    // OTLP wire in `exporter.rs`'s tests.

    #[test]
    fn test_otel_metric_type() {
        assert_eq!(
            OtelMetricType::from_value(&TelemetryValue::Counter(100)),
            OtelMetricType::Counter
        );
        assert_eq!(
            OtelMetricType::from_value(&TelemetryValue::Gauge(2.5)),
            OtelMetricType::Gauge
        );
        assert_eq!(
            OtelMetricType::from_value(&TelemetryValue::Boolean(true)),
            OtelMetricType::Gauge
        );
        assert_eq!(
            OtelMetricType::from_value(&TelemetryValue::Text("hello".into())),
            OtelMetricType::NotExportable
        );
        assert_eq!(
            OtelMetricType::from_value(&TelemetryValue::Binary(vec![1, 2, 3])),
            OtelMetricType::NotExportable
        );
    }

    #[test]
    fn test_extract_value() {
        assert_eq!(extract_value(&TelemetryValue::Counter(100)), Some(100.0));
        assert_eq!(extract_value(&TelemetryValue::Gauge(2.5)), Some(2.5));
        assert_eq!(extract_value(&TelemetryValue::Boolean(true)), Some(1.0));
        assert_eq!(extract_value(&TelemetryValue::Boolean(false)), Some(0.0));
        assert_eq!(extract_value(&TelemetryValue::Text("hello".into())), None);
    }

    #[test]
    fn test_is_metric_exportable() {
        assert!(is_metric_exportable(&TelemetryValue::Counter(100)));
        assert!(is_metric_exportable(&TelemetryValue::Gauge(2.5)));
        assert!(is_metric_exportable(&TelemetryValue::Boolean(true)));
        assert!(!is_metric_exportable(&TelemetryValue::Text("hello".into())));
        assert!(!is_metric_exportable(&TelemetryValue::Binary(vec![1])));
    }

    #[test]
    fn test_is_log_exportable() {
        let text = TelemetryValue::Text("log message".into());
        let gauge = TelemetryValue::Gauge(1.0);

        // Only syslog text is exportable as log
        assert!(is_log_exportable(&text, Protocol::Logs));
        assert!(!is_log_exportable(&text, Protocol::Snmp));
        assert!(!is_log_exportable(&gauge, Protocol::Logs));
    }
}
