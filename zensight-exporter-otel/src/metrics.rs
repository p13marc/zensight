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
    /// A fixed-bucket distribution (#1151), exported as an OTLP explicit-bucket
    /// Histogram.
    Histogram,
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
            TelemetryValue::Histogram(_) => OtelMetricType::Histogram,
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
        // A distribution is not one number; it has its own path
        // (`histogram_replay`).
        TelemetryValue::Histogram(_) => None,
    }
}

/// How one histogram delta is fed into an SDK histogram instrument (#1151).
///
/// # Why a replay at all
///
/// A sensor publishes an **already aggregated** distribution, and the OTel
/// Rust SDK (0.32) has no way to accept one: there is no asynchronous
/// histogram in the API, no `MetricProducer`, and the SDK's
/// `HistogramDataPoint` cannot be built outside the crate. The one door is a
/// synchronous `Histogram::record(value)`. So the exporter records, per
/// declared bucket, the number of observations that arrived in it since the
/// last point — each at a value **inside** that bucket — and the SDK
/// aggregates them back into exactly the counts the sensor sent.
///
/// # What is exact and what is not
///
/// - The **bucket counts** and the **count** are exact by construction: every
///   recorded value lies in its own bucket.
/// - The **sum** is exact whenever it can be: the values are placed at a
///   common fraction `t` of each finite bucket's width, and `t` is solved so
///   the recorded values add up to the delta's own `sum`; an overflow bucket
///   takes whatever lies beyond the last bound. When no placement inside the
///   buckets can reach the true sum (a sum that contradicts its own counts),
///   the closest one is used and [`Replay::sum_exact`] says so — counted by
///   the exporter, never silent.
/// - **min/max** would be the replayed values, not observations, so the
///   exporter's view turns `record_min_max` off for these instruments.
#[derive(Debug, Clone, PartialEq)]
pub struct Replay {
    /// `(value, times)`: record `value` this many times.
    pub values: Vec<(f64, u64)>,
    /// Whether the recorded values sum to the delta's `sum`.
    pub sum_exact: bool,
}

/// The replay for one delta. Empty when nothing was observed.
pub fn histogram_replay(delta: &zensight_common::HistogramValue) -> Replay {
    let b = &delta.buckets;
    let finite: Vec<(f64, f64, u64)> = b
        .iter()
        .enumerate()
        .filter_map(|(i, hi)| {
            let d = delta.counts.get(i).copied().unwrap_or(0);
            (d > 0).then(|| {
                // `(lo, hi]`. The first bucket's lower edge: 0 for a positive
                // first bound (latencies, sizes), else one bucket-width below.
                let lo = if i == 0 {
                    if *hi > 0.0 {
                        0.0
                    } else {
                        hi - b.get(1).map(|n| n - hi).unwrap_or(1.0)
                    }
                } else {
                    b[i - 1]
                };
                (lo, *hi, d)
            })
        })
        .collect();
    let overflow = delta.counts.get(b.len()).copied().unwrap_or(0);
    if finite.is_empty() && overflow == 0 {
        return Replay {
            values: Vec::new(),
            sum_exact: true,
        };
    }
    let at = |t: f64| -> f64 {
        finite
            .iter()
            .map(|(lo, hi, d)| (*d as f64) * (lo + t * (hi - lo)))
            .sum()
    };
    let place = |t: f64| -> Vec<(f64, u64)> {
        finite
            .iter()
            .map(|(lo, hi, d)| {
                // Strictly above `lo`: `t` is clamped away from 0 so a value
                // never lands on the previous bucket's upper edge.
                let t = t.max(1e-9);
                ((lo + t * (hi - lo)).min(*hi), *d)
            })
            .collect()
    };
    const TOL: f64 = 1e-9;
    let target = delta.sum;
    if overflow == 0 {
        let (f0, f1) = (at(0.0), at(1.0));
        let t = if f1 > f0 {
            (target - f0) / (f1 - f0)
        } else {
            1.0
        };
        let clamped = t.clamp(0.0, 1.0);
        let values = place(clamped);
        let recorded: f64 = values.iter().map(|(v, n)| v * *n as f64).sum();
        return Replay {
            sum_exact: (recorded - target).abs() <= TOL * target.abs().max(1.0),
            values,
        };
    }
    // An overflow bucket absorbs what the finite buckets cannot: its values
    // may be anything above the last bound. Midpoints for the finite ones,
    // then the overflow value that makes the sum exact — or, if that value
    // would not clear the last bound, the finite placement lowered to make
    // room for an overflow value just above it.
    let last = b.last().copied().unwrap_or(0.0);
    let just_above = |x: f64| {
        if x == 0.0 {
            f64::MIN_POSITIVE
        } else {
            x + x.abs() * 1e-9
        }
    };
    let mut t = 0.5;
    let mut ov = (target - at(t)) / overflow as f64;
    if ov <= last {
        ov = just_above(last);
        let (f0, f1) = (at(0.0), at(1.0));
        let want = target - ov * overflow as f64;
        t = if f1 > f0 {
            ((want - f0) / (f1 - f0)).clamp(0.0, 1.0)
        } else {
            0.0
        };
    }
    let mut values = place(t);
    values.push((ov, overflow));
    let recorded: f64 = values.iter().map(|(v, n)| v * *n as f64).sum();
    Replay {
        sum_exact: (recorded - target).abs() <= TOL * target.abs().max(1.0),
        values,
    }
}

/// Check if a TelemetryValue can be exported as an OTEL metric.
pub fn is_metric_exportable(value: &TelemetryValue) -> bool {
    !matches!(value, TelemetryValue::Text(_) | TelemetryValue::Binary(_))
}

/// Check if a TelemetryValue can be exported as an OTEL log. `producer` is the
/// key's chunk 4 (`keyexpr::producer_name`) — the point no longer carries it
/// (#1255).
pub fn is_log_exportable(value: &TelemetryValue, producer: &str) -> bool {
    // Only syslog text messages are exported as logs
    producer == "logs" && matches!(value, TelemetryValue::Text(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a replay into bucket counts the way the SDK aggregates it, and
    /// return (counts, sum) — the round trip the exporter relies on.
    fn aggregate(bounds: &[f64], r: &Replay) -> (Vec<u64>, f64) {
        let mut h = zensight_common::HistogramValue::new(bounds);
        let mut sum = 0.0;
        for (v, n) in &r.values {
            for _ in 0..*n {
                assert!(h.observe(*v));
            }
            sum += v * *n as f64;
        }
        (h.counts, sum)
    }

    /// #1151: the replay reproduces the bucket counts exactly, and the sum
    /// exactly whenever the counts admit it — with and without an overflow.
    #[test]
    fn a_replay_reproduces_the_counts_and_the_sum() {
        let bounds = [0.01, 0.1, 1.0];
        for (counts, sum) in [
            (vec![2, 3, 1, 0], 0.9),
            (vec![0, 0, 4, 0], 2.0),
            (vec![1, 0, 0, 2], 60.0),
            (vec![0, 0, 0, 1], 1.5),
            (vec![5, 0, 0, 0], 0.001),
        ] {
            let delta = zensight_common::HistogramValue {
                buckets: bounds.to_vec(),
                count: counts.iter().sum(),
                counts: counts.clone(),
                sum,
            };
            let r = histogram_replay(&delta);
            let (got, recorded) = aggregate(&bounds, &r);
            assert_eq!(got, counts, "counts for {counts:?}");
            assert!(r.sum_exact, "sum for {counts:?} / {sum}: {r:?}");
            assert!((recorded - sum).abs() < 1e-6, "{recorded} vs {sum}");
        }
    }

    /// A sum its own counts contradict (four observations in (0.1, 1] cannot
    /// add up to 50) is replayed as close as the buckets allow, and said.
    #[test]
    fn an_impossible_sum_is_flagged_not_hidden() {
        let delta = zensight_common::HistogramValue {
            buckets: vec![0.01, 0.1, 1.0],
            counts: vec![0, 0, 4, 0],
            count: 4,
            sum: 50.0,
        };
        let r = histogram_replay(&delta);
        assert!(!r.sum_exact);
        let (got, _) = aggregate(&[0.01, 0.1, 1.0], &r);
        assert_eq!(got, vec![0, 0, 4, 0], "the counts stay exact regardless");
    }

    #[test]
    fn an_empty_delta_records_nothing() {
        let r = histogram_replay(&zensight_common::HistogramValue::new(&[1.0]));
        assert!(r.values.is_empty() && r.sum_exact);
    }
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
        assert!(is_log_exportable(&text, "logs"));
        assert!(!is_log_exportable(&text, "snmp"));
        assert!(!is_log_exportable(&gauge, "logs"));
    }
}
