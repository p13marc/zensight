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
use std::collections::{HashMap, VecDeque};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::collector::SeriesKey;

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
/// mirroring what a scrape at that instant would yield. Text series become
/// value `1` under an `<name>_info` family with the text in a label named for
/// the subject leaf — byte-identical to `/metrics` (#752), because two
/// spellings of the same series is a bug waiting to be found in production.
pub fn build_write_request(metrics: &[StoredMetric], timestamp_ms: i64) -> WriteRequest {
    build_write_request_since(metrics, timestamp_ms, &HashMap::new()).request
}

/// What one build produced: the request to send, and the watermarks that
/// become true **only if it is delivered** (#1143).
pub struct PendingPush {
    pub request: WriteRequest,
    /// `(series, timestamp)` for every series in `request`, to be committed
    /// after a 2xx and not before.
    pub advanced: Vec<(SeriesKey, i64)>,
    /// The series the snapshot still holds, so the watermark map can be
    /// pruned to them. Independent of delivery: a series the collector has
    /// aged out is gone whether or not this push lands.
    pub live: std::collections::HashSet<SeriesKey>,
}

/// Build a `WriteRequest`, skipping series whose point timestamp has not
/// advanced since the last push.
///
/// # Why the point's timestamp and not the push time (#759)
///
/// Every sample used to be stamped with `current_timestamp_millis()`, so for up
/// to `stale_timeout_secs` after a sensor died the push path MANUFACTURED a
/// fresh datapoint from the last known value every interval — up to ten
/// synthetic samples per dead series at the 30s default. Grafana drew a flat
/// line where there should have been a gap. A sensor polling slower than the
/// push interval produced a staircase of values that never happened.
///
/// # Why the skip is not optional
///
/// Using the point's timestamp alone introduces a *different* bug: an unchanged
/// series would be re-pushed with an identical `(series, timestamp)` every
/// interval, which receivers reject as a duplicate sample. Tracking the last
/// pushed timestamp per series and skipping the ones that have not moved is
/// what actually makes a dead sensor gap.
///
/// `/metrics` deliberately stays UNtimestamped — see this module's note on the
/// asymmetry.
///
/// # Why this does not advance the watermark itself (#1143)
///
/// It used to: the skip test read `last_pushed` and wrote it back in the same
/// `filter_map`, while *building* the request — before a byte had been sent.
/// A push that then failed (connection refused, a 503 from Mimir, an auth
/// blip) left every series in it marked as delivered, so the next tick skipped
/// each one whose point timestamp had not moved since. A series slower than
/// the push interval — sysinfo at 60 s against the 30 s default — lost that
/// datapoint **permanently**. `run`'s own doc promises "push failures are
/// logged and retried on the next tick"; the tick retried, the samples did
/// not.
///
/// So this reads the map and returns what the watermarks *would* be. The
/// caller commits them on a 2xx.
pub fn build_write_request_since(
    metrics: &[StoredMetric],
    fallback_ms: i64,
    last_pushed: &HashMap<SeriesKey, i64>,
) -> PendingPush {
    // `filter_map` takes `&mut self` through a closure, so the watermarks it
    // would set go into a cell rather than into the map it is reading.
    let advanced = std::cell::RefCell::new(Vec::new());
    let mut timeseries: Vec<TimeSeries> = metrics
        .iter()
        .filter_map(|m| {
            let mut advanced = advanced.borrow_mut();
            let (value, extra_label) = match m.metric_type {
                PrometheusType::Text => {
                    let text = m.text_value.as_ref()?;
                    let label_name = m.text_label.clone().unwrap_or_else(|| "value".to_string());
                    // Same duplicate-name guard as `/metrics`: a structural
                    // label always wins over the text (#753).
                    if m.key.labels.iter().any(|(k, _)| k == &label_name) {
                        (1.0, None)
                    } else {
                        (1.0, Some((label_name, text.clone())))
                    }
                }
                _ => (m.value?, None),
            };

            let mut labels: Vec<Label> = Vec::with_capacity(m.key.labels.len() + 2);
            labels.push(Label {
                name: "__name__".to_string(),
                value: m.emitted_name(),
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

            // The point's own timestamp, falling back to the push time only
            // when the sensor supplied none.
            let ts = if m.timestamp_ms > 0 {
                m.timestamp_ms
            } else {
                fallback_ms
            };
            if let Some(&prev) = last_pushed.get(&m.key)
                && prev >= ts
            {
                return None;
            }
            advanced.push((m.key.clone(), ts));

            Some(TimeSeries {
                labels,
                samples: vec![Sample {
                    value,
                    timestamp: ts,
                }],
            })
        })
        .collect();

    // A series the collector has aged out stops appearing in the snapshot;
    // its watermark goes with it, or the map is bounded by lifetime label
    // churn rather than by `max_series` — a slow, monotonic leak on a fleet
    // with per-container or per-target labels. Pruning is a fact about the
    // COLLECTOR and not about this push, so it is not held back by delivery.
    let live: std::collections::HashSet<SeriesKey> =
        metrics.iter().map(|m| m.key.clone()).collect();

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

    PendingPush {
        request: WriteRequest { timeseries },
        advanced: advanced.into_inner(),
        live,
    }
}

/// What makes two samples the same sample to a receiver: the label set and
/// the timestamp. Two of these in one request is the duplicate #759 is about.
fn sample_identity(ts: &TimeSeries) -> (String, i64) {
    let labels = ts
        .labels
        .iter()
        .map(|l| format!("{}={}", l.name, l.value))
        .collect::<Vec<_>>()
        .join("\0");
    (labels, ts.samples.first().map(|s| s.timestamp).unwrap_or(0))
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
    /// Last timestamp **delivered** per series, so an unchanged series is
    /// skipped rather than re-sent as a duplicate sample (#759).
    ///
    /// Written after a 2xx and never before (#1143): a watermark advanced
    /// while *building* a request is a claim that the receiver has the sample,
    /// and a failed push makes it a false one that nothing later corrects.
    ///
    /// Bounded by the collector's own `max_series` in practice, and pruned
    /// alongside it: a series the collector has aged out stops appearing in
    /// the snapshot, so its watermark is dropped on the next sweep.
    last_pushed: parking_lot::Mutex<HashMap<SeriesKey, i64>>,
    /// Series a failed push still owes the receiver, oldest first (#1143).
    ///
    /// The watermark fix alone replays a series whose value has not moved —
    /// the next snapshot still carries the same point, and it is no longer
    /// skipped. It cannot replay one that HAS moved: the collector keeps only
    /// the latest value per series, so the datapoint the failed push was
    /// carrying is gone by the next tick. This holds it.
    ///
    /// Bounded and drop-oldest, because a backlog that grows without limit
    /// turns a receiver outage into an exporter OOM — the failure mode this
    /// whole crate exists to notice in other processes.
    backlog: parking_lot::Mutex<VecDeque<TimeSeries>>,
}

/// How many series a failed push may hold for replay.
///
/// At roughly 200 bytes of `TimeSeries` this is a few MB — an hour of
/// 30-second pushes for a 1 000-series fleet, which is the outage shape worth
/// surviving. Beyond it the oldest go, because the newest sample of a series
/// is the one a dashboard is about to ask for.
pub const MAX_BACKLOG_SERIES: usize = 20_000;

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
            last_pushed: parking_lot::Mutex::new(HashMap::new()),
            backlog: parking_lot::Mutex::new(VecDeque::new()),
        })
    }

    /// Snapshot the collector and push one `WriteRequest`. Returns the number
    /// of series pushed (0 = nothing to send, no request made).
    pub async fn push_once(&self) -> anyhow::Result<usize> {
        let metrics = self.collector.snapshot_metrics();
        let mut pending = {
            let seen = self.last_pushed.lock();
            build_write_request_since(&metrics, current_timestamp_millis(), &seen)
        };

        // Prune the watermarks to what the collector still holds. Not held
        // back by delivery: a series it has aged out is gone either way.
        self.last_pushed
            .lock()
            .retain(|k, _| pending.live.contains(k));

        // What a previous push failed to deliver goes first, so each series'
        // samples stay in ascending timestamp order within the request.
        //
        // Deduplicated on `(labels, timestamp)`: when the value has NOT moved
        // since the failed push, the collector re-offers the very same point
        // and the backlog is holding a copy of it. Sending both would be an
        // identical `(series, timestamp)` twice in one request, which is
        // exactly the duplicate sample #759 exists to avoid.
        let replayed = {
            let backlog = self.backlog.lock();
            let fresh: std::collections::HashSet<(String, i64)> = pending
                .request
                .timeseries
                .iter()
                .map(sample_identity)
                .collect();
            let mut all: Vec<TimeSeries> = backlog
                .iter()
                .filter(|t| !fresh.contains(&sample_identity(t)))
                .cloned()
                .collect();
            let n = all.len();
            all.append(&mut pending.request.timeseries);
            pending.request.timeseries = all;
            n
        };

        if pending.request.timeseries.is_empty() {
            debug!("remote-write: no series to push");
            return Ok(0);
        }
        let series = pending.request.timeseries.len();
        let body = encode_write_request(&pending.request)?;

        let sent = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .body(body)
            .send()
            .await;

        // EVERY failure path below retries. The watermark is not advanced, so
        // the next snapshot re-offers each series whose value has not moved;
        // the backlog holds the ones whose value has.
        let response = match sent {
            Ok(r) => r,
            Err(e) => {
                self.hold_for_retry(std::mem::take(&mut pending.request.timeseries));
                return Err(anyhow::anyhow!("remote-write POST failed: {e}"));
            }
        };
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            self.hold_for_retry(std::mem::take(&mut pending.request.timeseries));
            anyhow::bail!("remote-write endpoint returned {status}: {}", detail.trim());
        }

        // Delivered. Only now is the watermark true, and only now is the
        // backlog owed nothing.
        {
            let mut seen = self.last_pushed.lock();
            for (key, ts) in pending.advanced {
                seen.insert(key, ts);
            }
        }
        self.backlog.lock().clear();

        debug!(series, replayed, %status, "remote-write push ok");
        Ok(series)
    }

    /// Keep a failed push's series for the next tick, newest wins.
    fn hold_for_retry(&self, series: Vec<TimeSeries>) {
        let mut backlog = self.backlog.lock();
        backlog.clear();
        backlog.extend(series);
        let over = backlog.len().saturating_sub(MAX_BACKLOG_SERIES);
        if over > 0 {
            backlog.drain(..over);
            warn!(
                dropped = over,
                cap = MAX_BACKLOG_SERIES,
                "remote-write: retry backlog full; dropped the oldest series"
            );
        }
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
    use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};

    fn make_collector() -> SharedCollector {
        Arc::new(MetricCollector::new(
            PrometheusConfig::default(),
            AggregationConfig::default(),
            FilterConfig::default(),
        ))
    }

    /// Record one SNMP point under a wire-legal key.
    ///
    /// Naming flows from the key through the registry (#764), and SNMP is a
    /// rest-var producer: its subject is `<device>/<metric...>`, so the device
    /// rides in the key and becomes a label while the rest names the family.
    ///
    /// Metric names here are lowercase because the WIRE is lowercase — a key
    /// chunk must be `[a-z0-9]`-bounded, which is why the SNMP poller slugs at
    /// the publish boundary (#559). A test using `sysDescr` would be testing a
    /// key no sensor can publish.
    fn record(collector: &MetricCollector, source: &str, metric: &str, value: TelemetryValue) {
        record_at(collector, source, metric, value, 1_700_000_000_000);
    }

    /// The same, with the sensor's own clock — what makes two observations of
    /// one series two SAMPLES rather than one restated.
    fn record_at(
        collector: &MetricCollector,
        source: &str,
        metric: &str,
        value: TelemetryValue,
        timestamp: i64,
    ) {
        let key = format!("v1/h-0123456789ab/telemetry/snmp/{source}/{metric}");
        collector.record(
            &key,
            &TelemetryPoint {
                timestamp,
                source: source.to_string(),
                metric: metric.to_string(),
                value,
                labels: HashMap::new(),
                unit: None,
            },
        );
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
                    value: "zensight_snmp_sysuptime_total".into(),
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

    /// A sample carries the POINT's timestamp, not the push time (#759).
    ///
    /// Stamping push time meant that for up to `stale_timeout_secs` after a
    /// sensor died, every interval manufactured a fresh datapoint from the last
    /// known value — Grafana drew a flat line where there should have been a gap.
    #[test]
    fn build_from_collector_state_has_name_label_and_point_timestamp() {
        let collector = make_collector();
        record(
            &collector,
            "router01",
            "sysuptime",
            TelemetryValue::Counter(12345),
        );
        record(
            &collector,
            "router01",
            "cpu/load",
            TelemetryValue::Gauge(0.75),
        );

        // The `record` helper stamps its points at this instant.
        const POINT_MS: i64 = 1_700_000_000_000;
        let push_ms = 1_720_000_000_000;
        let req = build_write_request(&collector.snapshot_metrics(), push_ms);
        assert_eq!(req.timeseries.len(), 2);

        for ts in &req.timeseries {
            // Exactly one sample per series, stamped when the SENSOR observed
            // it — not when we happened to push.
            assert_eq!(ts.samples.len(), 1);
            assert_eq!(ts.samples[0].timestamp, POINT_MS);
            assert_ne!(ts.samples[0].timestamp, push_ms);
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
            .find(|ts| label(ts, "__name__") == Some("zensight_snmp_sysuptime_total"))
            .expect("counter series present");
        assert_eq!(counter.samples[0].value, 12345.0);

        let gauge = req
            .timeseries
            .iter()
            .find(|ts| label(ts, "__name__") == Some("zensight_snmp_cpu_load"))
            .expect("gauge series present");
        assert_eq!(gauge.samples[0].value, 0.75);
    }

    /// A text point rides an `_info` family under a label named for the subject
    /// leaf — not the old literal `value` label, and not the bare metric name.
    ///
    /// The `_info` suffix is what keeps a text family from colliding with a
    /// numeric family of the same name, and remote-write must spell the series
    /// exactly as `/metrics` does (#752).
    #[test]
    fn text_series_becomes_value_one_under_an_info_family() {
        let collector = make_collector();
        record(
            &collector,
            "router01",
            "sysdescr",
            TelemetryValue::Text("Cisco IOS".into()),
        );

        let req = build_write_request(&collector.snapshot_metrics(), 1);
        assert_eq!(req.timeseries.len(), 1);
        let ts = &req.timeseries[0];
        assert_eq!(ts.samples[0].value, 1.0);
        assert_eq!(
            label(ts, "__name__"),
            Some("zensight_snmp_sysdescr_info"),
            "a text family must carry the _info suffix"
        );
        assert_eq!(
            label(ts, "sysdescr"),
            Some("Cisco IOS"),
            "the text rides under the subject leaf, not a literal `value` label"
        );
        assert_eq!(label(ts, "value"), None, "the old literal label is gone");
    }

    /// A control character in a device-supplied string must never reach the
    /// wire: a raw newline would terminate the sample line in the exposition
    /// format and corrupt every byte after it.
    #[test]
    fn text_values_are_clamped_and_stripped_of_control_characters() {
        let collector = make_collector();
        record(
            &collector,
            "router01",
            "sysdescr",
            TelemetryValue::Text(format!("bad\nline{}", "x".repeat(400))),
        );

        let req = build_write_request(&collector.snapshot_metrics(), 1);
        let ts = &req.timeseries[0];
        let v = label(ts, "sysdescr").expect("text label present");
        assert!(!v.contains('\n'), "control characters are stripped: {v:?}");
        assert!(
            v.chars().count() <= crate::mapping::MAX_TEXT_LEN,
            "text is clamped to MAX_TEXT_LEN, got {}",
            v.chars().count()
        );
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
            "sysuptime",
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
        assert_eq!(label(ts, "__name__"), Some("zensight_snmp_sysuptime_total"));
        assert_eq!(label(ts, "source"), Some("router01"));
        assert_eq!(ts.samples[0].value, 7.0);
        assert!(ts.samples[0].timestamp > 0);
    }

    /// **#1143, the acceptance.** A sink that fails once must have the sample
    /// on the second push.
    ///
    /// It did not. The watermark was written while *building* the request, so
    /// the failed push had already marked the series delivered and the next
    /// tick skipped it — the sample was lost for good. A series slower than
    /// the push interval (sysinfo at 60 s against the 30 s default) loses
    /// every datapoint that lands on a failed push, permanently, while
    /// `run`'s own doc promises "push failures are logged and retried on the
    /// next tick". The tick retried. The samples did not.
    #[tokio::test]
    async fn a_sink_that_fails_once_gets_the_sample_on_the_second_push() {
        use axum::Router;
        use axum::body::Bytes;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Fail the first POST with a 503 — an overloaded Mimir, the commonest
        // shape of this — then accept.
        let attempts = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(parking_lot::Mutex::new(Vec::<WriteRequest>::new()));
        let app = Router::new().route(
            "/api/v1/write",
            post({
                let attempts = attempts.clone();
                let delivered = delivered.clone();
                move |body: Bytes| {
                    let attempts = attempts.clone();
                    let delivered = delivered.clone();
                    async move {
                        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                            return StatusCode::SERVICE_UNAVAILABLE;
                        }
                        let raw = snap::raw::Decoder::new().decompress_vec(&body).unwrap();
                        delivered
                            .lock()
                            .push(WriteRequest::decode(&raw[..]).unwrap());
                        StatusCode::NO_CONTENT
                    }
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
            "cpu/load",
            TelemetryValue::Gauge(0.5),
        );
        let cfg = RemoteWriteConfig {
            enabled: true,
            url: format!("http://{addr}/api/v1/write"),
            interval_secs: 30,
            headers: HashMap::new(),
        };
        let client = RemoteWriteClient::new(collector.clone(), &cfg).unwrap();

        assert!(
            client.push_once().await.is_err(),
            "the first push must surface the 503"
        );
        client.push_once().await.expect("the second push lands");

        let got = delivered.lock();
        assert_eq!(got.len(), 1, "exactly one delivery");
        assert_eq!(
            got[0].timeseries.len(),
            1,
            "the sample the failed push was carrying must be in the second"
        );
        assert_eq!(
            label(&got[0].timeseries[0], "__name__"),
            Some("zensight_snmp_cpu_load")
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    /// The other half of the same fix: a value that MOVED between the failed
    /// push and the retry is not simply re-offered by the collector — it only
    /// keeps the latest — so the failed push's sample is replayed from the
    /// backlog, and both land in timestamp order.
    #[tokio::test]
    async fn a_value_that_moved_during_an_outage_is_replayed_not_dropped() {
        use axum::Router;
        use axum::body::Bytes;
        use axum::http::StatusCode;
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(parking_lot::Mutex::new(Vec::<WriteRequest>::new()));
        let app = Router::new().route(
            "/api/v1/write",
            post({
                let attempts = attempts.clone();
                let delivered = delivered.clone();
                move |body: Bytes| {
                    let attempts = attempts.clone();
                    let delivered = delivered.clone();
                    async move {
                        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                            return StatusCode::SERVICE_UNAVAILABLE;
                        }
                        let raw = snap::raw::Decoder::new().decompress_vec(&body).unwrap();
                        delivered
                            .lock()
                            .push(WriteRequest::decode(&raw[..]).unwrap());
                        StatusCode::NO_CONTENT
                    }
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
            "cpu/load",
            TelemetryValue::Gauge(0.5),
        );
        let cfg = RemoteWriteConfig {
            enabled: true,
            url: format!("http://{addr}/api/v1/write"),
            interval_secs: 30,
            headers: HashMap::new(),
        };
        let client = RemoteWriteClient::new(collector.clone(), &cfg).unwrap();
        assert!(client.push_once().await.is_err());

        // The sensor publishes again during the outage, with its own newer
        // clock. The collector keeps only this value; the 0.5 exists nowhere
        // but the backlog.
        record_at(
            &collector,
            "router01",
            "cpu/load",
            TelemetryValue::Gauge(0.9),
            1_700_000_060_000,
        );
        client.push_once().await.expect("the second push lands");

        let got = delivered.lock();
        let values: Vec<f64> = got[0]
            .timeseries
            .iter()
            .flat_map(|t| t.samples.iter().map(|s| s.value))
            .collect();
        assert!(
            values.contains(&0.5) && values.contains(&0.9),
            "both the replayed and the fresh sample must arrive, got {values:?}"
        );
        let stamps: Vec<i64> = got[0]
            .timeseries
            .iter()
            .flat_map(|t| t.samples.iter().map(|s| s.timestamp))
            .collect();
        assert!(
            stamps.windows(2).all(|w| w[0] <= w[1]),
            "a series' samples must be in ascending timestamp order: {stamps:?}"
        );
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

    /// A series whose timestamp has not advanced is SKIPPED on the next push.
    ///
    /// This is the second half of #759, and the reason the fix is not simply
    /// "use the point's timestamp": re-pushing an unchanged series would send
    /// an identical `(series, timestamp)` every interval, which receivers
    /// reject as a duplicate sample. Skipping is what actually makes a dead
    /// sensor gap.
    #[test]
    fn an_unchanged_series_is_not_pushed_twice() {
        let collector = make_collector();
        record(
            &collector,
            "router01",
            "cpu/load",
            TelemetryValue::Gauge(0.5),
        );

        // The watermarks are DELIVERED ones (#1143), so the test commits them
        // by hand where `push_once` would commit them on a 2xx.
        let mut seen = HashMap::new();
        let first = build_write_request_since(&collector.snapshot_metrics(), 1, &seen);
        assert_eq!(
            first.request.timeseries.len(),
            1,
            "first push sends the series"
        );
        seen.extend(first.advanced);

        let second = build_write_request_since(&collector.snapshot_metrics(), 2, &seen);
        assert!(
            second.request.timeseries.is_empty(),
            "an unchanged series must not be re-pushed as a duplicate sample"
        );

        // A newer observation moves the watermark and is pushed again.
        let mut newer = collector.snapshot_metrics();
        for m in &mut newer {
            m.timestamp_ms += 1_000;
        }
        let third = build_write_request_since(&newer, 3, &seen);
        assert_eq!(
            third.request.timeseries.len(),
            1,
            "a fresh observation is pushed"
        );

        // AND THE OTHER HALF (#1143): that third build was never delivered —
        // nothing committed `third.advanced` — so the same observation must
        // still be offered. Before the fix the build itself had already
        // written the watermark, and this series was skipped forever.
        let retry = build_write_request_since(&newer, 4, &seen);
        assert_eq!(
            retry.request.timeseries.len(),
            1,
            "building a request must not be what marks a series delivered"
        );
    }
}
