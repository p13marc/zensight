//! Prometheus remote-write (push) support.
//!
//! The exporter is normally pull-based (`/metrics` served by [`crate::http`]).
//! For push-based / agent topologies (no inbound connectivity to the exporter,
//! e.g. Grafana Cloud, Mimir, Thanos Receive, VictoriaMetrics), this module
//! adds an optional periodic push implementing the
//! [Prometheus remote-write 1.0 protocol]: a snappy-compressed (raw block
//! format) protobuf `WriteRequest` POSTed with `Content-Encoding: snappy`,
//! `Content-Type: application/x-protobuf` and
//! `X-Prometheus-Remote-Write-Version: 0.1.0`.
//!
//! Each push snapshots the collector's current metric state (the same state
//! the `/metrics` endpoint renders) and sends one sample per live series,
//! stamped with the push time — exactly what a scrape at that instant would
//! have produced. Info (text) series are sent as value `1` with the text in a
//! `value` label, mirroring the exposition format. Alert series and the
//! exporter's self-metrics stay on the pull endpoint only.
//!
//! The protobuf types are hand-written with `prost` derive (the remote-write
//! proto is four tiny messages), so no `protoc` is needed at build time.
//!
//! [Prometheus remote-write 1.0 protocol]: https://prometheus.io/docs/specs/remote_write_spec/

use std::time::Duration;

use prost::Message;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tokio::sync::watch;
use tracing::{debug, info, warn};
use zensight_common::telemetry::current_timestamp_millis;

use crate::collector::{SharedCollector, StoredMetric};
use crate::config::RemoteWriteConfig;
use crate::mapping::PrometheusType;

/// Value of the `X-Prometheus-Remote-Write-Version` header.
pub const REMOTE_WRITE_VERSION: &str = "0.1.0";

// --- Wire types (io.prometheus.write.v1) -----------------------------------
//
// Field tags match `prompb/remote.proto` / `prompb/types.proto`. Only the
// fields a 1.0 sender must produce are modelled; receivers ignore the absent
// optional ones (metadata, exemplars, histograms).
//
// Exemplars assessment (#167 successor): `TimeSeries` has an `exemplars` field
// (tag 3, `repeated Exemplar`) in the proto that we deliberately omit. Sending
// exemplars would require (a) a histogram-shaped `TelemetryValue` so a bucketed
// sample can carry a trace-linked exemplar — today `TelemetryValue` is only
// Counter/Gauge/Text/Boolean/Binary, all scalar — and (b) a trace id to point
// at, which the bus does not propagate (see the OTLP traces module: ids are
// *synthesized*, not real spans an exemplar could link to). Both are the same
// blockers as OTel exemplars; adding the proto field alone would emit empty
// exemplars. Deferred to the successor issue behind a histogram value type.

/// Top-level remote-write payload: a batch of time series.
#[derive(Clone, PartialEq, Message)]
pub struct WriteRequest {
    #[prost(message, repeated, tag = "1")]
    pub timeseries: Vec<TimeSeries>,
}

/// One time series: a full label set plus its samples.
#[derive(Clone, PartialEq, Message)]
pub struct TimeSeries {
    /// Label set, including `__name__`. Per spec: unique names, sorted
    /// lexicographically, no empty values.
    #[prost(message, repeated, tag = "1")]
    pub labels: Vec<Label>,
    #[prost(message, repeated, tag = "2")]
    pub samples: Vec<Sample>,
}

/// A single label name/value pair.
#[derive(Clone, PartialEq, Message)]
pub struct Label {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

/// A single (value, timestamp-ms) sample.
#[derive(Clone, PartialEq, Message)]
pub struct Sample {
    #[prost(double, tag = "1")]
    pub value: f64,
    #[prost(int64, tag = "2")]
    pub timestamp: i64,
}

/// Build a [`WriteRequest`] from a collector snapshot (pure — unit-testable
/// without any I/O).
///
/// One sample per series, all stamped with `timestamp_ms` (the push time),
/// mirroring what a scrape at that instant would yield. Info series become
/// value `1` with the text in a `value` label, matching `/metrics`.
pub fn build_write_request(metrics: &[StoredMetric], timestamp_ms: i64) -> WriteRequest {
    let mut timeseries: Vec<TimeSeries> = metrics
        .iter()
        .filter_map(|m| {
            let (value, extra_label) = match m.metric_type {
                PrometheusType::Info => {
                    let text = m.text_value.as_ref()?;
                    (1.0, Some(("value".to_string(), text.clone())))
                }
                _ => (m.value?, None),
            };

            let mut labels: Vec<Label> = Vec::with_capacity(m.key.labels.len() + 2);
            labels.push(Label {
                name: "__name__".to_string(),
                value: m.key.name.clone(),
            });
            for (k, v) in &m.key.labels {
                if !v.is_empty() {
                    labels.push(Label {
                        name: k.clone(),
                        value: v.clone(),
                    });
                }
            }
            if let Some((k, v)) = extra_label {
                labels.push(Label { name: k, value: v });
            }
            // Spec: labels MUST be sorted lexicographically by name.
            labels.sort_by(|a, b| a.name.cmp(&b.name));

            Some(TimeSeries {
                labels,
                samples: vec![Sample {
                    value,
                    timestamp: timestamp_ms,
                }],
            })
        })
        .collect();

    // Deterministic batch order (stable pushes, stable tests).
    timeseries.sort_by(|a, b| {
        let key = |ts: &TimeSeries| {
            ts.labels
                .iter()
                .map(|l| format!("{}={}", l.name, l.value))
                .collect::<Vec<_>>()
                .join("\0")
        };
        key(a).cmp(&key(b))
    });

    WriteRequest { timeseries }
}

/// Encode a [`WriteRequest`] to the wire form: protobuf, then snappy raw
/// (block) compression as required by the remote-write spec.
pub fn encode_write_request(request: &WriteRequest) -> anyhow::Result<Vec<u8>> {
    let raw = request.encode_to_vec();
    snap::raw::Encoder::new()
        .compress_vec(&raw)
        .map_err(|e| anyhow::anyhow!("snappy compression failed: {e}"))
}

/// Periodic remote-write pusher.
pub struct RemoteWriteClient {
    collector: SharedCollector,
    url: String,
    interval: Duration,
    headers: HeaderMap,
    client: reqwest::Client,
}

impl RemoteWriteClient {
    /// Create a new client from configuration. Fails on malformed custom
    /// header names/values.
    pub fn new(collector: SharedCollector, config: &RemoteWriteConfig) -> anyhow::Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("snappy"));
        headers.insert(
            "content-type",
            HeaderValue::from_static("application/x-protobuf"),
        );
        headers.insert(
            "x-prometheus-remote-write-version",
            HeaderValue::from_static(REMOTE_WRITE_VERSION),
        );
        for (name, value) in &config.headers {
            let name: HeaderName = name
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid remote_write header name {name:?}: {e}"))?;
            let value = HeaderValue::from_str(value)
                .map_err(|e| anyhow::anyhow!("invalid remote_write header value: {e}"))?;
            headers.insert(name, value);
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))?;

        Ok(Self {
            collector,
            url: config.url.clone(),
            interval: Duration::from_secs(config.interval_secs),
            headers,
            client,
        })
    }

    /// Snapshot the collector and push one `WriteRequest`. Returns the number
    /// of series pushed (0 = nothing to send, no request made).
    pub async fn push_once(&self) -> anyhow::Result<usize> {
        let metrics = self.collector.snapshot_metrics();
        let request = build_write_request(&metrics, current_timestamp_millis());
        if request.timeseries.is_empty() {
            debug!("remote-write: no series to push");
            return Ok(0);
        }
        let series = request.timeseries.len();
        let body = encode_write_request(&request)?;

        let response = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .body(body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("remote-write POST failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            anyhow::bail!("remote-write endpoint returned {status}: {}", detail.trim());
        }

        debug!(series, %status, "remote-write push ok");
        Ok(series)
    }

    /// Run the periodic push loop until the shutdown signal fires. Push
    /// failures are logged and retried on the next tick — they never take the
    /// exporter down.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        info!(url = %self.url, interval_secs = self.interval.as_secs(), "Remote-write push enabled");
        let mut interval = tokio::time::interval(self.interval);
        // The first tick fires immediately; skip it so the collector has one
        // interval to fill before the first push.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = self.push_once().await {
                        warn!(url = %self.url, "Remote-write push failed: {e:#}");
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Remote-write push stopped");
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::MetricCollector;
    use crate::config::{AggregationConfig, FilterConfig, PrometheusConfig};
    use std::collections::HashMap;
    use std::sync::Arc;
    use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

    fn make_collector() -> SharedCollector {
        Arc::new(MetricCollector::new(
            PrometheusConfig::default(),
            AggregationConfig::default(),
            FilterConfig::default(),
        ))
    }

    fn record(collector: &MetricCollector, source: &str, metric: &str, value: TelemetryValue) {
        collector.record(&TelemetryPoint {
            timestamp: 1_700_000_000_000,
            source: source.to_string(),
            protocol: Protocol::Snmp,
            metric: metric.to_string(),
            value,
            labels: HashMap::new(),
        });
    }

    fn label<'a>(ts: &'a TimeSeries, name: &str) -> Option<&'a str> {
        ts.labels
            .iter()
            .find(|l| l.name == name)
            .map(|l| l.value.as_str())
    }

    #[test]
    fn proto_roundtrip() {
        let req = WriteRequest {
            timeseries: vec![TimeSeries {
                labels: vec![Label {
                    name: "__name__".into(),
                    value: "up".into(),
                }],
                samples: vec![Sample {
                    value: 1.0,
                    timestamp: 42,
                }],
            }],
        };
        let bytes = req.encode_to_vec();
        let back = WriteRequest::decode(&bytes[..]).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn snappy_encoding_roundtrip() {
        let req = WriteRequest {
            timeseries: vec![TimeSeries {
                labels: vec![Label {
                    name: "__name__".into(),
                    value: "zensight_snmp_sysUpTime".into(),
                }],
                samples: vec![Sample {
                    value: 3.5,
                    timestamp: 1_700_000_000_000,
                }],
            }],
        };
        let compressed = encode_write_request(&req).unwrap();
        let raw = snap::raw::Decoder::new()
            .decompress_vec(&compressed)
            .unwrap();
        assert_eq!(WriteRequest::decode(&raw[..]).unwrap(), req);
    }

    #[test]
    fn build_from_collector_state_has_name_label_and_push_timestamp() {
        let collector = make_collector();
        record(
            &collector,
            "router01",
            "sysUpTime",
            TelemetryValue::Counter(12345),
        );
        record(
            &collector,
            "router01",
            "cpu/load",
            TelemetryValue::Gauge(0.75),
        );

        let now_ms = 1_720_000_000_000;
        let req = build_write_request(&collector.snapshot_metrics(), now_ms);
        assert_eq!(req.timeseries.len(), 2);

        for ts in &req.timeseries {
            // Exactly one sample per series, stamped with the push time.
            assert_eq!(ts.samples.len(), 1);
            assert_eq!(ts.samples[0].timestamp, now_ms);
            assert!(label(ts, "__name__").is_some());
            assert_eq!(label(ts, "source"), Some("router01"));
            assert_eq!(label(ts, "protocol"), Some("snmp"));
            // Spec: labels sorted lexicographically by name.
            let names: Vec<&str> = ts.labels.iter().map(|l| l.name.as_str()).collect();
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(names, sorted, "labels must be sorted by name");
        }

        let counter = req
            .timeseries
            .iter()
            .find(|ts| label(ts, "__name__") == Some("zensight_snmp_sysUpTime"))
            .expect("counter series present");
        assert_eq!(counter.samples[0].value, 12345.0);

        let gauge = req
            .timeseries
            .iter()
            .find(|ts| label(ts, "__name__") == Some("zensight_snmp_cpu_load"))
            .expect("gauge series present");
        assert_eq!(gauge.samples[0].value, 0.75);
    }

    #[test]
    fn info_series_becomes_value_one_with_value_label() {
        let collector = make_collector();
        record(
            &collector,
            "router01",
            "sysDescr",
            TelemetryValue::Text("Cisco IOS".into()),
        );

        let req = build_write_request(&collector.snapshot_metrics(), 1);
        assert_eq!(req.timeseries.len(), 1);
        let ts = &req.timeseries[0];
        assert_eq!(ts.samples[0].value, 1.0);
        assert_eq!(label(ts, "value"), Some("Cisco IOS"));
    }

    #[test]
    fn empty_collector_builds_empty_request() {
        let collector = make_collector();
        let req = build_write_request(&collector.snapshot_metrics(), 1);
        assert!(req.timeseries.is_empty());
    }

    #[test]
    fn invalid_custom_header_is_rejected() {
        let cfg = RemoteWriteConfig {
            enabled: true,
            url: "http://localhost:9009/api/v1/push".into(),
            interval_secs: 30,
            headers: HashMap::from([("bad header name".to_string(), "x".to_string())]),
        };
        assert!(RemoteWriteClient::new(make_collector(), &cfg).is_err());
    }

    /// End-to-end against an in-process mock receiver: the POST body must be a
    /// snappy-compressed protobuf `WriteRequest` with the spec headers set.
    #[tokio::test]
    async fn push_once_delivers_snappy_protobuf_to_receiver() {
        use axum::Router;
        use axum::body::Bytes;
        use axum::http::{HeaderMap as AxumHeaderMap, StatusCode};
        use axum::routing::post;

        let (tx, rx) = tokio::sync::oneshot::channel::<(AxumHeaderMap, Bytes)>();
        let tx = Arc::new(parking_lot::Mutex::new(Some(tx)));
        let app = Router::new().route(
            "/api/v1/write",
            post(move |headers: AxumHeaderMap, body: Bytes| {
                let tx = tx.clone();
                async move {
                    if let Some(tx) = tx.lock().take() {
                        let _ = tx.send((headers, body));
                    }
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let collector = make_collector();
        record(
            &collector,
            "router01",
            "sysUpTime",
            TelemetryValue::Counter(7),
        );

        let cfg = RemoteWriteConfig {
            enabled: true,
            url: format!("http://{addr}/api/v1/write"),
            interval_secs: 30,
            headers: HashMap::from([(
                "authorization".to_string(),
                "Bearer secret-token".to_string(),
            )]),
        };
        let client = RemoteWriteClient::new(collector, &cfg).unwrap();
        let pushed = client.push_once().await.unwrap();
        assert_eq!(pushed, 1);

        let (headers, body) = tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .expect("receiver saw the push")
            .unwrap();

        // Spec headers + custom auth header.
        assert_eq!(headers.get("content-encoding").unwrap(), "snappy");
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/x-protobuf"
        );
        assert_eq!(
            headers.get("x-prometheus-remote-write-version").unwrap(),
            REMOTE_WRITE_VERSION
        );
        assert_eq!(headers.get("authorization").unwrap(), "Bearer secret-token");

        // Body: snappy raw block -> protobuf WriteRequest.
        let raw = snap::raw::Decoder::new().decompress_vec(&body).unwrap();
        let req = WriteRequest::decode(&raw[..]).unwrap();
        assert_eq!(req.timeseries.len(), 1);
        let ts = &req.timeseries[0];
        assert_eq!(label(ts, "__name__"), Some("zensight_snmp_sysUpTime"));
        assert_eq!(label(ts, "source"), Some("router01"));
        assert_eq!(ts.samples[0].value, 7.0);
        assert!(ts.samples[0].timestamp > 0);
    }

    /// Nothing in the collector -> no HTTP request is made at all.
    #[tokio::test]
    async fn push_once_skips_when_empty() {
        let cfg = RemoteWriteConfig {
            enabled: true,
            // Nothing listens here — the test only passes because no request
            // is attempted for an empty snapshot.
            url: "http://127.0.0.1:9/api/v1/write".into(),
            interval_secs: 30,
            headers: HashMap::new(),
        };
        let client = RemoteWriteClient::new(make_collector(), &cfg).unwrap();
        assert_eq!(client.push_once().await.unwrap(), 0);
    }
}
