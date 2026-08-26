//! OTLP exporter setup and management.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use opentelemetry::logs::{LogRecord as _, Logger, LoggerProvider as _, Severity};
use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry::trace::{
    SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
};
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_otlp::{
    LogExporter, MetricExporter, SpanExporter, WithExportConfig, WithHttpConfig, WithTonicConfig,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, SpanData, SpanEvents, SpanLinks, SpanProcessor,
};
use parking_lot::{Mutex, RwLock};
use tracing::{debug, error, info, trace, warn};
use zensight_common::alert::{Alert, AlertSeverity, AlertState};
use zensight_common::exposition::{MetricKind, identify};
use zensight_common::telemetry::TelemetryPoint;

use crate::config::{FilterConfig, OtelConfig, OtlpProtocol, ResourceMode};
use crate::logs::LogRecord;
use crate::metrics::{
    ObservedHost, build_resource_attributes, extract_value, is_log_exportable, is_metric_exportable,
};
use crate::traces::{AlertSpan, AlertSpanTracker};

/// Filter for telemetry points.
pub struct TelemetryFilter {
    include_protocols: Vec<String>,
    exclude_protocols: Vec<String>,
    include_sources: Vec<String>,
    exclude_sources: Vec<String>,
}

impl TelemetryFilter {
    /// Create a new filter from configuration.
    pub fn new(config: &FilterConfig) -> Self {
        Self {
            include_protocols: config.include_protocols.clone(),
            exclude_protocols: config.exclude_protocols.clone(),
            include_sources: config.include_sources.clone(),
            exclude_sources: config.exclude_sources.clone(),
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

        true
    }
}

impl Default for TelemetryFilter {
    fn default() -> Self {
        Self::new(&FilterConfig::default())
    }
}

/// Statistics for the exporter.
#[derive(Debug, Clone, Default)]
pub struct ExporterStats {
    pub points_received: u64,
    pub points_filtered: u64,
    pub metrics_exported: u64,
    pub metrics_failed: u64,
    pub logs_exported: u64,
    pub alerts_exported: u64,
    pub spans_exported: u64,
    pub export_errors: u64,
}

/// Build a collision-resistant gauge key from metric name and attributes.
///
/// Attributes are sorted and separated by null bytes to prevent collisions.
fn build_series_key(metric_name: &str, attributes: &[opentelemetry::KeyValue]) -> String {
    let mut sorted_attrs: Vec<_> = attributes
        .iter()
        .map(|kv| format!("{}={}", kv.key, kv.value.as_str()))
        .collect();
    sorted_attrs.sort();
    format!("{}\x00{}", metric_name, sorted_attrs.join("\x00"))
}

/// Convert a Unix-epoch-millis timestamp to a [`SystemTime`] (clamped at the
/// epoch for the — not expected — negative case).
pub(crate) fn ms_to_system_time(ms: i64) -> SystemTime {
    if ms >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        SystemTime::UNIX_EPOCH
    }
}

/// Whether a resolved endpoint needs TLS.
fn is_tls(endpoint: &str) -> bool {
    endpoint.starts_with("https://")
}

/// gRPC metadata from the configured headers.
///
/// Header names are lower-cased because gRPC metadata keys must be; a name or
/// value that cannot be represented is a config error, surfaced at startup
/// rather than as a silent 401 (#756). `remote_write.rs` already does exactly
/// this for the Prometheus push path.
fn grpc_metadata(
    headers: &HashMap<String, String>,
) -> anyhow::Result<opentelemetry_otlp::tonic_types::metadata::MetadataMap> {
    use opentelemetry_otlp::tonic_types::metadata::MetadataMap;

    // `MetadataMap` is built from an http::HeaderMap, which is the shape the
    // Prometheus remote-write path already validates headers into.
    let mut http_headers = http::HeaderMap::with_capacity(headers.len());
    for (k, v) in headers {
        let name: http::header::HeaderName = k
            .to_ascii_lowercase()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid OTLP header name {k:?}: {e}"))?;
        let value: http::header::HeaderValue = v
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid OTLP header value for {k:?}: {e}"))?;
        http_headers.insert(name, value);
    }
    Ok(MetadataMap::from_headers(http_headers))
}

/// Build OTel [`SpanData`] from a synthesized alert span.
///
/// The span is a leaf (`Internal`, no parent, no remote context): the ids are
/// deterministic (alert key + firing time), the flags are `SAMPLED` so the
/// backend keeps it, and `Status::Unset` — a fired-then-resolved alert is an
/// observation, not a failed operation. Pure (no exporter state) so span
/// construction is unit-testable without a live OTLP pipeline.
fn build_span_data(span: AlertSpan, scope: &InstrumentationScope) -> SpanData {
    let span_context = SpanContext::new(
        TraceId::from_bytes(span.trace_id),
        SpanId::from_bytes(span.span_id),
        TraceFlags::SAMPLED,
        false,
        TraceState::NONE,
    );
    let attributes = span
        .attributes
        .into_iter()
        .map(|(k, v)| KeyValue::new(k, v))
        .collect();

    SpanData {
        span_context,
        parent_span_id: SpanId::INVALID,
        parent_span_is_remote: false,
        span_kind: SpanKind::Internal,
        name: span.name.into(),
        start_time: ms_to_system_time(span.start_ms),
        end_time: ms_to_system_time(span.end_ms),
        attributes,
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links: SpanLinks::default(),
        status: Status::Unset,
        instrumentation_scope: scope.clone(),
    }
}

/// Map a ZenSight alert severity onto an OTel log severity.
fn alert_severity_to_otel(severity: AlertSeverity) -> Severity {
    match severity {
        AlertSeverity::Info => Severity::Info,
        AlertSeverity::Warning => Severity::Warn,
        AlertSeverity::Critical => Severity::Error,
    }
}

/// Which asynchronous instrument a series is observed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObsKind {
    /// `TelemetryValue::Counter` — a cumulative total, observed as a monotonic Sum.
    Counter,
    /// `TelemetryValue::Gauge` / `Boolean` — a current level.
    Gauge,
}

/// One observed series: its latest value, its attributes, and when it last
/// moved.
///
/// This replaces the old write-only `GaugeEntry` map, whose `value` field was
/// `#[allow(dead_code)]` because nothing ever read it (#754). The store is now
/// the single source the asynchronous callbacks read on every collection, which
/// is what makes both counter semantics correct and staleness observable: an
/// entry evicted here stops being observed, producing a real **gap** rather
/// than a value that flat-lines forever.
#[derive(Debug, Clone)]
struct Observation {
    value: f64,
    attrs: Vec<opentelemetry::KeyValue>,
    last_updated: Instant,
}

/// metric name -> (series key -> observation)
/// `(origin, metric name) -> (series key -> observation)`.
///
/// Keyed by origin as well as name because each host has its own meter and
/// therefore its own instruments (#755); a callback must observe only its own
/// host's series or every resource would report the whole fleet.
type ObservationStore = Arc<RwLock<HashMap<(String, String), HashMap<String, Observation>>>>;

/// One host's signal providers, each pinned to that host's `Resource`.
///
/// OTel binds a `Resource` to a **provider**, not to a record, so "one resource
/// per observed host" necessarily means one provider set per host (#755). They
/// are built lazily on the first sample from an origin and capped by
/// `max_resources`.
struct SignalStack {
    meter_provider: Option<SdkMeterProvider>,
    meter: Option<Meter>,
    logger_provider: Option<SdkLoggerProvider>,
    logger: Option<SdkLogger>,
    alert_logger: Option<SdkLogger>,
    event_logger: Option<SdkLogger>,
}

/// OpenTelemetry exporter that receives telemetry and exports via OTLP.
pub struct OtelExporter {
    /// Meter provider for metrics.
    meter_provider: Option<SdkMeterProvider>,
    /// Cached meter instance (avoids re-creating on every metric).
    meter: Option<Meter>,
    /// Logger provider for logs.
    logger_provider: Option<SdkLoggerProvider>,
    /// Cached logger instance (avoids re-creating on every log).
    logger: Option<SdkLogger>,
    /// Cached logger for sensor alerts (scope `zensight.alerts`).
    alert_logger: Option<SdkLogger>,
    /// Span processor for synthesized alert-lifecycle spans (traces signal).
    ///
    /// Spans carry deterministic ids derived from the alert lifecycle, which
    /// the SDK's tracer/`IdGenerator` path cannot express — so completed
    /// [`AlertSpan`]s are converted to [`SpanData`] directly and handed to the
    /// batch processor.
    span_processor: Option<BatchSpanProcessor>,
    /// Instrumentation scope stamped on synthesized spans.
    span_scope: InstrumentationScope,
    /// Alert lifecycle tracker feeding the traces signal.
    alert_spans: Option<Mutex<AlertSpanTracker>>,
    /// Whether metrics export is enabled.
    export_metrics: bool,
    /// Whether logs export is enabled.
    export_logs: bool,
    /// Whether alert export is enabled.
    export_alerts: bool,
    /// Whether events-class export is enabled.
    export_events: bool,
    /// Cached logger for the events class (scope `zensight.events`).
    event_logger: Option<SdkLogger>,
    /// Telemetry filter.
    filter: TelemetryFilter,
    /// Export statistics.
    stats: RwLock<ExporterStats>,
    /// Every observed series, read by the asynchronous instrument callbacks.
    observations: ObservationStore,
    /// Which `(origin, metric name)` pairs already have an instrument
    /// registered, and of which kind. Registering twice would create a
    /// duplicate stream; changing kind mid-flight is a sensor bug we report
    /// rather than paper over.
    ///
    /// Keyed by origin as well as name because each host has its own meter
    /// (#755) — the same metric name on two hosts is two instruments.
    registered: RwLock<HashMap<(String, String), ObsKind>>,
    /// Per-host signal providers, built lazily. Empty in `Flat` mode.
    per_origin: RwLock<HashMap<String, Arc<SignalStack>>>,
    /// The exporter config, kept so a stack can be built on first sight of a
    /// host rather than only at startup.
    config: OtelConfig,
    /// Origins refused because `max_resources` was reached.
    dropped_resources: RwLock<u64>,
    /// Instrument handles, kept alive for the lifetime of the exporter.
    _counters: RwLock<Vec<opentelemetry::metrics::ObservableCounter<u64>>>,
    _gauges: RwLock<Vec<opentelemetry::metrics::ObservableGauge<f64>>>,
    /// Maximum number of series to store, across all metric names.
    max_gauge_series: usize,
}

impl OtelExporter {
    /// Build an exporter around an already-constructed meter provider.
    ///
    /// Test-only seam. It exists so the metrics path can be asserted **on the
    /// wire** with `InMemoryMetricExporter` rather than through a log line —
    /// which is exactly the coverage whose absence let the counter bug (#754)
    /// ship: every existing test was a pure conversion test, and none of them
    /// could tell `add(absolute)` from `observe(absolute)`.
    #[cfg(test)]
    fn with_meter_provider(meter_provider: SdkMeterProvider) -> Self {
        Self::with_providers(Some(meter_provider), None)
    }

    /// Build an exporter around already-constructed providers.
    ///
    /// Test-only seam, so the logs path can be asserted **on the wire** with
    /// `InMemoryLogExporter` rather than through a log line. The log-timestamp
    /// bug (#760) shipped precisely because nothing inspected an emitted
    /// record.
    #[cfg(test)]
    fn with_providers(
        meter_provider: Option<SdkMeterProvider>,
        logger_provider: Option<SdkLoggerProvider>,
    ) -> Self {
        let meter = meter_provider.as_ref().map(|mp| mp.meter("zensight"));
        let logger = logger_provider
            .as_ref()
            .map(|lp| lp.logger("zensight.syslog"));
        let alert_logger = logger_provider
            .as_ref()
            .map(|lp| lp.logger("zensight.alerts"));
        Self {
            meter_provider,
            meter,
            logger_provider,
            logger,
            alert_logger,
            event_logger: None,
            export_events: false,
            span_processor: None,
            span_scope: InstrumentationScope::builder("zensight.alerts").build(),
            alert_spans: None,
            export_metrics: true,
            export_logs: true,
            export_alerts: true,
            filter: TelemetryFilter::new(&FilterConfig::default()),
            stats: RwLock::new(ExporterStats::default()),
            observations: Arc::new(RwLock::new(HashMap::new())),
            registered: RwLock::new(HashMap::new()),
            per_origin: RwLock::new(HashMap::new()),
            // The seam injects providers directly, so the pool stays empty and
            // every record uses them — `Flat` says exactly that.
            config: OtelConfig {
                resource_mode: ResourceMode::Flat,
                ..Default::default()
            },
            dropped_resources: RwLock::new(0),
            _counters: RwLock::new(Vec::new()),
            _gauges: RwLock::new(Vec::new()),
            max_gauge_series: 100_000,
        }
    }

    /// Create a new OTLP exporter.
    pub async fn new(
        otel_config: &OtelConfig,
        filter_config: &FilterConfig,
    ) -> anyhow::Result<Self> {
        info!(
            endpoint = %otel_config.endpoint,
            protocol = ?otel_config.protocol,
            "Initializing OpenTelemetry exporter"
        );

        // Build resource attributes
        // The SHARED resource, used in `Flat` mode and as the fallback past
        // `max_resources`. It deliberately carries no host identity — there is
        // no single host it could honestly name (#755).
        let resource_attrs = build_resource_attributes(
            &otel_config.service_name,
            otel_config.service_version.as_deref(),
            &otel_config.resource,
            None,
        );
        let resource = Resource::builder().with_attributes(resource_attrs).build();

        // Initialize meter provider if metrics enabled
        let meter_provider = if otel_config.export_metrics {
            Some(Self::init_meter_provider_sync(
                otel_config,
                resource.clone(),
            )?)
        } else {
            None
        };

        // Initialize logger provider if logs enabled
        // The logger pipeline backs both syslog logs and alert events, so
        // initialize it if either is enabled.
        let logger_provider =
            if otel_config.export_logs || otel_config.export_alerts || otel_config.export_events {
                Some(Self::init_logger_provider_sync(
                    otel_config,
                    resource.clone(),
                )?)
            } else {
                None
            };

        // Initialize the span pipeline if the traces signal is enabled.
        let span_processor = if otel_config.traces.enabled {
            Some(Self::init_span_processor(otel_config, &resource)?)
        } else {
            None
        };

        let meter = meter_provider.as_ref().map(|mp| mp.meter("zensight"));
        let logger = if otel_config.export_logs {
            logger_provider
                .as_ref()
                .map(|lp| lp.logger("zensight.syslog"))
        } else {
            None
        };
        let alert_logger = if otel_config.export_alerts {
            logger_provider
                .as_ref()
                .map(|lp| lp.logger("zensight.alerts"))
        } else {
            None
        };
        let event_logger = if otel_config.export_events {
            logger_provider
                .as_ref()
                .map(|lp| lp.logger("zensight.events"))
        } else {
            None
        };

        let alert_spans = otel_config
            .traces
            .enabled
            .then(|| Mutex::new(AlertSpanTracker::new()));

        Ok(Self {
            meter_provider,
            meter,
            logger_provider,
            logger,
            alert_logger,
            span_processor,
            span_scope: InstrumentationScope::builder("zensight.alerts").build(),
            alert_spans,
            export_metrics: otel_config.export_metrics,
            export_logs: otel_config.export_logs,
            export_alerts: otel_config.export_alerts,
            export_events: otel_config.export_events,
            event_logger,
            filter: TelemetryFilter::new(filter_config),
            stats: RwLock::new(ExporterStats::default()),
            observations: Arc::new(RwLock::new(HashMap::new())),
            registered: RwLock::new(HashMap::new()),
            per_origin: RwLock::new(HashMap::new()),
            config: otel_config.clone(),
            dropped_resources: RwLock::new(0),
            _counters: RwLock::new(Vec::new()),
            _gauges: RwLock::new(Vec::new()),
            max_gauge_series: 100_000,
        })
    }

    /// Not `async`: the builders are synchronous — the original signature only
    /// needed a live tokio runtime for the gRPC exporter's lazy connect, which
    /// the caller already provides. Making it sync is what lets a per-host
    /// stack be built on first sight of a host rather than only at startup.
    fn init_meter_provider_sync(
        config: &OtelConfig,
        resource: Resource,
    ) -> anyhow::Result<SdkMeterProvider> {
        // Per-signal endpoint (#756): under HTTP the signal path is appended to
        // the base, which opentelemetry-otlp does not do for a programmatic
        // endpoint — only for one read from the environment. Under gRPC the
        // base passes through, because gRPC routes by service name.
        let endpoint = config.signal_endpoint(crate::config::Signal::Metrics);

        let exporter = match config.protocol {
            OtlpProtocol::Grpc => {
                let mut b = MetricExporter::builder()
                    .with_tonic()
                    .with_endpoint(&endpoint)
                    .with_timeout(config.timeout())
                    .with_metadata(grpc_metadata(&config.headers)?);
                if is_tls(&endpoint) {
                    b = b.with_tls_config(
                        opentelemetry_otlp::tonic_types::transport::ClientTlsConfig::new()
                            .with_enabled_roots(),
                    );
                }
                b.build()
                    .map_err(|e| anyhow::anyhow!("Failed to create gRPC metric exporter: {}", e))?
            }
            OtlpProtocol::Http => MetricExporter::builder()
                .with_http()
                .with_endpoint(&endpoint)
                .with_timeout(config.timeout())
                .with_headers(config.headers.clone())
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to create HTTP metric exporter: {}", e))?,
        };

        let reader = PeriodicReader::builder(exporter)
            .with_interval(config.export_interval())
            .build();

        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(reader)
            .build();

        info!("Meter provider initialized");
        Ok(provider)
    }

    fn init_logger_provider_sync(
        config: &OtelConfig,
        resource: Resource,
    ) -> anyhow::Result<SdkLoggerProvider> {
        // Per-signal endpoint (#756): under HTTP the signal path is appended to
        // the base, which opentelemetry-otlp does not do for a programmatic
        // endpoint — only for one read from the environment. Under gRPC the
        // base passes through, because gRPC routes by service name.
        let endpoint = config.signal_endpoint(crate::config::Signal::Logs);

        let exporter = match config.protocol {
            OtlpProtocol::Grpc => {
                let mut b = LogExporter::builder()
                    .with_tonic()
                    .with_endpoint(&endpoint)
                    .with_timeout(config.timeout())
                    .with_metadata(grpc_metadata(&config.headers)?);
                if is_tls(&endpoint) {
                    b = b.with_tls_config(
                        opentelemetry_otlp::tonic_types::transport::ClientTlsConfig::new()
                            .with_enabled_roots(),
                    );
                }
                b.build()
                    .map_err(|e| anyhow::anyhow!("Failed to create gRPC log exporter: {}", e))?
            }
            OtlpProtocol::Http => LogExporter::builder()
                .with_http()
                .with_endpoint(&endpoint)
                .with_timeout(config.timeout())
                .with_headers(config.headers.clone())
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to create HTTP log exporter: {}", e))?,
        };

        let provider = SdkLoggerProvider::builder()
            .with_resource(resource)
            .with_batch_exporter(exporter)
            .build();

        info!("Logger provider initialized");
        Ok(provider)
    }

    /// Initialize the OTLP span pipeline for the (synthesized) traces signal.
    ///
    /// We do not use an `SdkTracerProvider`/`SdkTracer`: those own trace/span id
    /// generation via an `IdGenerator`, but our spans carry *deterministic* ids
    /// derived from the alert lifecycle (see [`crate::traces`]). Instead we drive
    /// a [`BatchSpanProcessor`] directly — building [`SpanData`] ourselves and
    /// handing finished spans to [`SpanProcessor::on_end`]. The resource is set
    /// on the processor explicitly (normally the provider would do this).
    ///
    /// Called from within the async `new` (so a tokio runtime is live for the
    /// gRPC exporter); the builders themselves are synchronous.
    fn init_span_processor(
        config: &OtelConfig,
        resource: &Resource,
    ) -> anyhow::Result<BatchSpanProcessor> {
        // Per-signal endpoint (#756): under HTTP the signal path is appended to
        // the base, which opentelemetry-otlp does not do for a programmatic
        // endpoint — only for one read from the environment. Under gRPC the
        // base passes through, because gRPC routes by service name.
        let endpoint = config.signal_endpoint(crate::config::Signal::Traces);

        let exporter = match config.protocol {
            OtlpProtocol::Grpc => {
                let mut b = SpanExporter::builder()
                    .with_tonic()
                    .with_endpoint(&endpoint)
                    .with_timeout(config.timeout())
                    .with_metadata(grpc_metadata(&config.headers)?);
                if is_tls(&endpoint) {
                    b = b.with_tls_config(
                        opentelemetry_otlp::tonic_types::transport::ClientTlsConfig::new()
                            .with_enabled_roots(),
                    );
                }
                b.build()
                    .map_err(|e| anyhow::anyhow!("Failed to create gRPC span exporter: {}", e))?
            }
            OtlpProtocol::Http => SpanExporter::builder()
                .with_http()
                .with_endpoint(&endpoint)
                .with_timeout(config.timeout())
                .with_headers(config.headers.clone())
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to create HTTP span exporter: {}", e))?,
        };

        let mut processor = BatchSpanProcessor::builder(exporter).build();
        processor.set_resource(resource);

        info!("Span processor initialized (traces signal)");
        Ok(processor)
    }

    /// Record a telemetry point.
    /// Record one telemetry sample.
    ///
    /// `key` is the sample's own base-relative key expression — the registry
    /// can refine it, and the payload cannot supply the origin (#764, #475).
    pub fn record(&self, key: &str, point: &TelemetryPoint) {
        {
            let mut stats = self.stats.write();
            stats.points_received += 1;
        }

        // Apply filter
        if !self.filter.should_include(point) {
            let mut stats = self.stats.write();
            stats.points_filtered += 1;
            trace!(
                source = %point.source,
                protocol = %point.protocol,
                "Point filtered"
            );
            return;
        }

        // Export as metric if applicable
        if self.export_metrics && is_metric_exportable(&point.value) {
            self.record_metric(key, point);
        }

        // Export as log if applicable.
        //
        // Logs are the signal that MOST needs a per-host resource (#755): a
        // backend derives stream identity from the resource, so a shared one
        // collapses the whole fleet into a single log stream.
        if self.export_logs && is_log_exportable(&point.value, point.protocol) {
            self.record_log(key, point);
        }
    }

    /// The host a refined sample came from.
    ///
    /// `origin` comes off the KEY (via `identify`'s structural labels), which
    /// is the only place it exists — a payload cannot assert its own RFC 06
    /// host id, and a hostname is not one.
    fn host_of(
        identity: &zensight_common::exposition::MetricIdentity,
        point: &TelemetryPoint,
    ) -> Option<ObservedHost> {
        let label = |n: &str| {
            identity
                .labels
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.clone())
        };
        let origin = label("origin")?;
        let producer = label("protocol")?;
        let producer = match label("producer_instance") {
            Some(i) => format!("{producer}-{i}"),
            None => producer,
        };
        Some(ObservedHost {
            origin,
            host_name: point.source.clone(),
            producer,
        })
    }

    fn record_metric(&self, key: &str, point: &TelemetryPoint) {
        // Naming flows from the KEY through the registry (#764). OTLP attribute
        // keys are unconstrained, so the sanitizer is identity — the collision
        // guarantee comes from the merge, not the normalisation.
        let identity = match identify(key, point, &Default::default(), |n: &str| n.to_string()) {
            Ok(i) => i,
            Err(reason) => {
                let mut stats = self.stats.write();
                stats.metrics_failed += 1;
                trace!(key = %key, reason = reason.reason(), "Key not refined by the registry");
                return;
            }
        };

        // OTel names are dotted; a semconv identity is already a complete
        // dotted name, everything else is `zensight.<producer>.<family>`.
        let metric_name = if identity.semconv {
            identity.name.join(".")
        } else {
            format!("zensight.{}", identity.name.join("."))
        };
        let attributes: Vec<KeyValue> = identity
            .labels
            .iter()
            .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
            .collect();

        let kind = match identity.kind {
            MetricKind::Counter => ObsKind::Counter,
            MetricKind::Gauge => ObsKind::Gauge,
            MetricKind::Text | MetricKind::Unsupported => return,
        };

        // One resource per observed host (#755). The stack owns the meter, so
        // the instrument and its callback belong to that host's provider.
        let host = Self::host_of(&identity, point);
        let stack = host.as_ref().and_then(|h| self.stack_for(h));
        let meter = match stack.as_ref().and_then(|s| s.meter.as_ref()) {
            Some(m) => m.clone(),
            None => match &self.meter {
                Some(m) => m.clone(),
                None => return,
            },
        };
        // Instruments and observations are scoped to the host whose meter owns
        // them; in Flat mode every host shares one scope, which is what "flat"
        // means.
        let scope = match (&self.config.resource_mode, &host) {
            (ResourceMode::PerOrigin, Some(h)) if stack.is_some() => h.origin.clone(),
            _ => String::new(),
        };
        let store_key = (scope.clone(), metric_name.clone());

        let Some(value) = extract_value(&point.value) else {
            warn!(
                metric = %metric_name,
                source = %point.source,
                "Value marked as exportable but extraction failed"
            );
            self.stats.write().metrics_failed += 1;
            return;
        };

        // Store the observation. The asynchronous callback registered below
        // reads this on every collection cycle.
        //
        // This is the fix for #754. The old counter path called
        // `counter.add(value)` with the **absolute** device reading, so under
        // cumulative temporality the exported Sum became a running total of
        // absolute readings: an interface sitting at 1_000_000 octets reported
        // 1e6, then 2e6, then 3e6, forever. `rate()` returned
        // `reading / interval` and the series never decreased even across a
        // counter reset — silently wrong, which is worse than broken.
        //
        // `TelemetryValue::Counter` is already the cumulative total
        // (`zensight-common/src/telemetry.rs`), so the right instrument is an
        // asynchronous one that *reports* that total, not a synchronous one
        // that adds to it. A delta cache would be the wrong shape too: it would
        // have to invent reset semantics it cannot observe.
        let series_key = build_series_key(&metric_name, &attributes);
        {
            let mut store = self.observations.write();
            let series = store.entry(store_key.clone()).or_default();
            if !series.contains_key(&series_key) {
                let total: usize = store.values().map(HashMap::len).sum();
                if total >= self.max_gauge_series {
                    warn!(
                        max = self.max_gauge_series,
                        "Max series limit reached, dropping new series"
                    );
                    self.stats.write().metrics_failed += 1;
                    return;
                }
                let series = store.entry(store_key.clone()).or_default();
                series.insert(
                    series_key,
                    Observation {
                        value,
                        attrs: attributes,
                        last_updated: Instant::now(),
                    },
                );
            } else {
                let obs = series
                    .get_mut(&series_key)
                    .expect("checked present immediately above");
                obs.value = value;
                obs.attrs = attributes;
                obs.last_updated = Instant::now();
            }
        }

        // Register the instrument once per metric name. The SDK resolves an
        // instrument to a stream, so rebuilding it per point (as the old code
        // did on every single sample) was a lock, an allocation and a pipeline
        // lookup on the Zenoh receive path for no gain.
        let already = self.registered.read().get(&store_key).copied();
        match already {
            Some(existing) if existing == kind => {}
            Some(existing) => {
                // Two value variants under one metric name. Reporting a level
                // as a monotonic Sum is a contract violation, so say so rather
                // than silently picking one.
                warn!(
                    metric = %metric_name,
                    ?existing,
                    attempted = ?kind,
                    "Metric changed value kind mid-flight; keeping the first"
                );
                self.stats.write().metrics_failed += 1;
                return;
            }
            None => {
                self.register_instrument(
                    &meter,
                    &scope,
                    &metric_name,
                    kind,
                    identity.unit.as_deref(),
                );
                self.registered.write().insert(store_key.clone(), kind);
            }
        }

        trace!(metric = %metric_name, value, ?kind, "Recorded observation");
        self.stats.write().metrics_exported += 1;
    }

    /// The signal providers for one observed host.
    ///
    /// In `Flat` mode this is always the shared stack. In `PerOrigin` it is
    /// built on first sight and cached; past `max_resources` the host falls
    /// back to the shared stack and is counted, which is a degraded but honest
    /// answer rather than unbounded growth.
    fn stack_for(&self, host: &ObservedHost) -> Option<Arc<SignalStack>> {
        if self.config.resource_mode == ResourceMode::Flat {
            return None;
        }
        if let Some(hit) = self.per_origin.read().get(&host.origin) {
            return Some(Arc::clone(hit));
        }

        let mut pool = self.per_origin.write();
        // Re-check: another task may have built it while we waited.
        if let Some(hit) = pool.get(&host.origin) {
            return Some(Arc::clone(hit));
        }
        if pool.len() >= self.config.max_resources {
            let mut dropped = self.dropped_resources.write();
            if *dropped == 0 {
                warn!(
                    max = self.config.max_resources,
                    "max_resources reached; further hosts share the flat resource"
                );
            }
            *dropped += 1;
            return None;
        }

        match self.build_stack(host) {
            Ok(stack) => {
                let stack = Arc::new(stack);
                pool.insert(host.origin.clone(), Arc::clone(&stack));
                debug!(origin = %host.origin, host = %host.host_name, "Built a per-host resource");
                Some(stack)
            }
            Err(e) => {
                warn!(origin = %host.origin, error = %e, "Could not build per-host providers; using the flat resource");
                None
            }
        }
    }

    /// Build one host's providers, pinned to its own `Resource`.
    fn build_stack(&self, host: &ObservedHost) -> anyhow::Result<SignalStack> {
        let attrs = build_resource_attributes(
            &self.config.service_name,
            self.config.service_version.as_deref(),
            &self.config.resource,
            Some(host),
        );
        let resource = Resource::builder().with_attributes(attrs).build();

        let meter_provider = if self.config.export_metrics {
            Some(Self::init_meter_provider_sync(
                &self.config,
                resource.clone(),
            )?)
        } else {
            None
        };
        let logger_provider =
            if self.config.export_logs || self.config.export_alerts || self.config.export_events {
                Some(Self::init_logger_provider_sync(&self.config, resource)?)
            } else {
                None
            };

        let meter = meter_provider.as_ref().map(|mp| mp.meter("zensight"));
        let logger = self
            .config
            .export_logs
            .then(|| {
                logger_provider
                    .as_ref()
                    .map(|lp| lp.logger("zensight.syslog"))
            })
            .flatten();
        let alert_logger = self
            .config
            .export_alerts
            .then(|| {
                logger_provider
                    .as_ref()
                    .map(|lp| lp.logger("zensight.alerts"))
            })
            .flatten();
        let event_logger = self
            .config
            .export_events
            .then(|| {
                logger_provider
                    .as_ref()
                    .map(|lp| lp.logger("zensight.events"))
            })
            .flatten();

        Ok(SignalStack {
            meter_provider,
            meter,
            logger_provider,
            logger,
            alert_logger,
            event_logger,
        })
    }

    /// Register the asynchronous instrument for one metric name.
    ///
    /// The callback closes over a clone of the observation store, so a series
    /// removed by [`Self::cleanup_stale_observations`] simply stops being observed on
    /// the next collection — a gap, which is the honest rendering of "this host
    /// stopped reporting".
    fn register_instrument(
        &self,
        meter: &Meter,
        scope: &str,
        metric_name: &str,
        kind: ObsKind,
        unit: Option<&str>,
    ) {
        let store = Arc::clone(&self.observations);
        // The callback observes only ITS host's series. Without the scope in
        // the key, every per-host resource would report the whole fleet (#755).
        let name_for_cb = (scope.to_string(), metric_name.to_string());

        match kind {
            ObsKind::Counter => {
                let mut b = meter.u64_observable_counter(metric_name.to_string());
                if let Some(u) = unit {
                    b = b.with_unit(u.to_string());
                }
                let inst = b
                    .with_callback(move |observer| {
                        if let Some(series) = store.read().get(&name_for_cb) {
                            for obs in series.values() {
                                observer.observe(obs.value as u64, &obs.attrs);
                            }
                        }
                    })
                    .build();
                self._counters.write().push(inst);
            }
            ObsKind::Gauge => {
                let mut b = meter.f64_observable_gauge(metric_name.to_string());
                if let Some(u) = unit {
                    b = b.with_unit(u.to_string());
                }
                let inst = b
                    .with_callback(move |observer| {
                        if let Some(series) = store.read().get(&name_for_cb) {
                            for obs in series.values() {
                                observer.observe(obs.value, &obs.attrs);
                            }
                        }
                    })
                    .build();
                self._gauges.write().push(inst);
            }
        }
    }

    fn record_log(&self, key: &str, point: &TelemetryPoint) {
        // Resolve the host from the KEY so the record lands on that host's
        // logger, and therefore that host's resource.
        let stack = identify(key, point, &Default::default(), |n: &str| n.to_string())
            .ok()
            .and_then(|id| Self::host_of(&id, point))
            .and_then(|h| self.stack_for(&h));
        let logger = match stack.as_ref().and_then(|s| s.logger.as_ref()) {
            Some(l) => l,
            None => match &self.logger {
                Some(l) => l,
                None => return,
            },
        };

        let Some(record) = LogRecord::from_telemetry(point) else {
            return;
        };

        // Create a new log record
        let mut log_record = logger.create_log_record();

        // Set body
        // The event's own time, and separately when we saw it (#760). Without
        // these an OTLP record ships `time_unix_nano = 0` and every line is
        // timestamped at ingestion.
        log_record.set_timestamp(record.timestamp);
        log_record.set_observed_timestamp(SystemTime::now());
        log_record.set_body(record.body.clone().into());

        // Set severity
        log_record.set_severity_number(record.otel_severity());
        log_record.set_severity_text(crate::logs::severity_text(record.severity));

        // Add attributes
        log_record.add_attribute("hostname", record.hostname.clone());
        log_record.add_attribute(
            "syslog.severity",
            crate::logs::severity_text(record.severity).to_string(),
        );

        if let Some(facility) = &record.facility {
            log_record.add_attribute("syslog.facility", facility.as_str().to_string());
        }

        if let Some(appname) = &record.appname {
            log_record.add_attribute("syslog.appname", appname.clone());
        }

        // OTel logs data model (#104): per-line record uid + verbatim original.
        if let Some(uid) = &record.uid {
            log_record.add_attribute("log.record.uid", uid.clone());
        }
        if let Some(original) = &record.original {
            log_record.add_attribute("log.record.original", original.clone());
        }

        // Emit the log
        logger.emit(log_record);

        trace!(
            hostname = %record.hostname,
            severity = %crate::logs::severity_text(record.severity),
            "Recorded log"
        );

        let mut stats = self.stats.write();
        stats.logs_exported += 1;
    }

    /// Record one `events`-class record as an OTLP log (#762).
    ///
    /// `key` is the record's own key (`…/events/<producer>/<subject…>/<ulid>`),
    /// which carries the origin, so the record lands on that host's resource
    /// like every other signal.
    ///
    /// The ULID rides as `event.id`: it is the record's identity on the bus
    /// (one key per record, nothing overwrites), which makes it the natural
    /// de-duplication key for a consumer replaying a storage.
    pub fn record_event(&self, key: &str, event: &zensight_common::event::EventRecord) {
        if !self.export_events {
            return;
        }

        let stack = zensight_common::keyexpr::parse_key(key)
            .and_then(|parsed| {
                let producer = parsed.producer.as_ref()?.name().to_string();
                Some(ObservedHost {
                    origin: parsed.origin.to_string(),
                    host_name: event.source.clone(),
                    producer,
                })
            })
            .and_then(|h| self.stack_for(&h));

        let logger = match stack.as_ref().and_then(|s| s.event_logger.as_ref()) {
            Some(l) => l,
            None => match &self.event_logger {
                Some(l) => l,
                None => return,
            },
        };

        let mut rec = logger.create_log_record();
        rec.set_event_name("zensight.event");
        rec.set_timestamp(ms_to_system_time(event.timestamp));
        rec.set_observed_timestamp(SystemTime::now());
        rec.set_body(event.summary.clone().into());
        rec.set_severity_number(alert_severity_to_otel(event.severity));
        rec.set_severity_text(event.severity.as_str());

        rec.add_attribute("event.id", event.id.clone());
        rec.add_attribute("event.kind", event.kind.clone());
        rec.add_attribute("event.source", event.source.clone());
        rec.add_attribute("event.protocol", event.protocol.as_str().to_string());
        // #651: when the producer mapped this record to an alert transition,
        // carrying the key is what lets a consumer join the two.
        if let Some(alert_key) = &event.alert_key {
            rec.add_attribute("alert.key", alert_key.clone());
        }
        for (k, v) in &event.fields {
            rec.add_attribute(format!("event.field.{k}"), v.clone());
        }

        logger.emit(rec);
        self.stats.write().logs_exported += 1;
    }

    /// Whether event export is enabled (drives whether the subscriber declares
    /// the events subscriber at all).
    pub fn export_events(&self) -> bool {
        self.export_events
    }

    /// Whether alert export is enabled (drives whether the subscriber decodes
    /// the `state/*/alert/*` channel).
    pub fn export_alerts(&self) -> bool {
        self.export_alerts
    }

    /// Whether the exporter needs the `state/*/alert/*` stream at all — true if
    /// either alert log export or the traces signal (which synthesizes spans
    /// from the alert lifecycle) is enabled. The subscriber uses this to decide
    /// whether to declare the alert subscriber.
    pub fn wants_alert_stream(&self) -> bool {
        self.export_alerts || self.alert_spans.is_some()
    }

    /// Record a sensor alert.
    ///
    /// Two independent sinks, each gated on its own config:
    /// - **logs** (`export_alerts`): every transition (firing/resolved) is one
    ///   OTLP log event on the `zensight.alerts` scope — an append-only stream,
    ///   so unlike the Prometheus gauge there is no per-alert state to clear;
    ///   the `alert.state` attribute carries firing vs resolved.
    /// - **traces** (`traces.enabled`): the firing→resolved pair is folded into
    ///   a single synthesized span whose duration is how long the alert fired
    ///   (see [`crate::traces`]). Only the resolve transition emits a span.
    ///
    /// `key` is the alert's own key (`…/state/<producer>/alert/<alert_key>`),
    /// which is where the origin lives. An `Alert` payload carries `source`
    /// (a hostname) and a `host.id` label holding the FULL host id — not the
    /// `h-<12hex>` origin chunk — so routing from the payload would mean
    /// re-deriving the chunk outside the grammar crate, which is the
    /// hand-rolling #475 exists to prevent. The key already has it.
    pub fn record_alert(&self, key: &str, alert: &Alert) {
        // Traces signal: fold the lifecycle into a span (independent of logs).
        if let Some(tracker) = &self.alert_spans {
            let completed = tracker.lock().on_alert(alert);
            if let Some(span) = completed {
                self.emit_alert_span(span);
            }
        }

        // Route to the host's own logger, so the record lands on that host's
        // resource (#755).
        let stack = zensight_common::keyexpr::parse_key(key)
            .and_then(|parsed| {
                let producer = parsed.producer.as_ref()?.name().to_string();
                Some(ObservedHost {
                    origin: parsed.origin.to_string(),
                    host_name: alert.source.clone(),
                    producer,
                })
            })
            .and_then(|h| self.stack_for(&h));
        let logger = match stack.as_ref().and_then(|s| s.alert_logger.as_ref()) {
            Some(l) => l,
            None => match &self.alert_logger {
                Some(l) => l,
                None => return,
            },
        };

        let mut rec = logger.create_log_record();
        rec.set_event_name("zensight.alert");
        rec.set_timestamp(ms_to_system_time(alert.timestamp));
        rec.set_observed_timestamp(SystemTime::now());
        rec.set_body(alert.summary.clone().into());
        rec.set_severity_number(alert_severity_to_otel(alert.severity));
        rec.set_severity_text(alert.severity.as_str());

        rec.add_attribute("alert.key", alert.alert_key());
        rec.add_attribute(
            "alert.state",
            match alert.state {
                AlertState::Firing => "firing",
                AlertState::Resolved => "resolved",
            },
        );
        rec.add_attribute("alert.source", alert.source.clone());
        rec.add_attribute("alert.protocol", alert.protocol.to_string());
        rec.add_attribute("alert.rule", alert.rule.clone());
        rec.add_attribute("alert.kind", alert.kind.as_str());
        rec.add_attribute("alert.severity", alert.severity.as_str());
        for (k, v) in &alert.labels {
            rec.add_attribute(format!("alert.label.{k}"), v.clone());
        }

        logger.emit(rec);

        trace!(source = %alert.source, rule = %alert.rule, state = ?alert.state, "Recorded alert");

        let mut stats = self.stats.write();
        stats.alerts_exported += 1;
    }

    /// Convert a completed [`AlertSpan`] to OTel [`SpanData`] and hand it to the
    /// batch span processor for OTLP export.
    fn emit_alert_span(&self, span: AlertSpan) {
        let Some(processor) = &self.span_processor else {
            return;
        };
        let data = build_span_data(span, &self.span_scope);
        // `on_end` enqueues onto the batch processor's channel — the OTLP export
        // itself happens on the processor's worker, off this thread.
        processor.on_end(data);

        let mut stats = self.stats.write();
        stats.spans_exported += 1;
    }

    /// Remove stale gauge entries that haven't been updated within the given duration.
    /// Drop series not updated within `max_age`, and return how many went.
    ///
    /// This has teeth now. It previously had **zero callers** and its store was
    /// write-only, so a host that went quiet kept flat-lining its last value
    /// forever (#754). The asynchronous callbacks read the same store, so an
    /// evicted series simply stops being observed on the next collection —
    /// which renders as a gap, the honest answer.
    pub fn cleanup_stale_observations(&self, max_age: Duration) -> usize {
        let mut store = self.observations.write();
        let before: usize = store.values().map(HashMap::len).sum();
        for series in store.values_mut() {
            series.retain(|_, obs| obs.last_updated.elapsed() < max_age);
        }
        // Drop metric names with no series left, so the map does not grow
        // without bound on a fleet with churn. The instrument stays registered
        // (its callback just observes nothing), which is correct: the metric
        // still exists, nothing is currently reporting it.
        store.retain(|_, series| !series.is_empty());
        let after: usize = store.values().map(HashMap::len).sum();
        let removed = before - after;
        if removed > 0 {
            info!(removed, remaining = after, "Cleaned up stale series");
        }
        removed
    }

    /// Get the number of stored gauge series.
    /// Total observed series across all metric names.
    pub fn series_count(&self) -> usize {
        self.observations.read().values().map(HashMap::len).sum()
    }

    /// Get current statistics.
    pub fn stats(&self) -> ExporterStats {
        self.stats.read().clone()
    }

    /// Shutdown the exporter gracefully.
    pub fn shutdown(&self) -> anyhow::Result<()> {
        info!("Shutting down OpenTelemetry exporter");

        if let Some(meter_provider) = &self.meter_provider
            && let Err(e) = meter_provider.shutdown()
        {
            error!("Error shutting down meter provider: {:?}", e);
        }

        if let Some(logger_provider) = &self.logger_provider
            && let Err(e) = logger_provider.shutdown()
        {
            error!("Error shutting down logger provider: {:?}", e);
        }

        // Every PER-HOST provider too (#755). Each owns its own batch queues,
        // so shutting down only the shared pair would silently drop whatever
        // the per-host pipelines still held.
        for (origin, stack) in self.per_origin.read().iter() {
            if let Some(mp) = &stack.meter_provider
                && let Err(e) = mp.shutdown()
            {
                error!(origin = %origin, "Error shutting down host meter provider: {:?}", e);
            }
            if let Some(lp) = &stack.logger_provider
                && let Err(e) = lp.shutdown()
            {
                error!(origin = %origin, "Error shutting down host logger provider: {:?}", e);
            }
        }

        // Flush + stop the span processor so any spans still batched in memory
        // are exported before we exit.
        if let Some(processor) = &self.span_processor {
            if let Err(e) = processor.force_flush() {
                error!("Error flushing span processor: {:?}", e);
            }
            if let Err(e) = processor.shutdown() {
                error!("Error shutting down span processor: {:?}", e);
            }
        }

        info!("OpenTelemetry exporter shutdown complete");
        Ok(())
    }
}

/// Shareable exporter handle.
pub type SharedExporter = Arc<OtelExporter>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traces::AlertSpan;
    use zensight_common::telemetry::{Protocol, TelemetryValue};

    #[test]
    fn build_span_data_maps_alert_span_faithfully() {
        let scope = InstrumentationScope::builder("zensight.alerts").build();
        let span = AlertSpan {
            name: "alert:ssh-listening".to_string(),
            trace_id: [7u8; 16],
            span_id: [3u8; 8],
            start_ms: 1_000,
            end_ms: 5_000,
            attributes: vec![
                ("alert.source".to_string(), "host01".to_string()),
                ("alert.severity".to_string(), "critical".to_string()),
            ],
        };

        let data = build_span_data(span, &scope);

        // Deterministic ids flow through verbatim.
        assert_eq!(data.span_context.trace_id(), TraceId::from_bytes([7u8; 16]));
        assert_eq!(data.span_context.span_id(), SpanId::from_bytes([3u8; 8]));
        // Sampled leaf span, no parent.
        assert!(data.span_context.trace_flags().is_sampled());
        assert!(data.span_context.is_valid());
        assert_eq!(data.parent_span_id, SpanId::INVALID);
        assert!(matches!(data.span_kind, SpanKind::Internal));
        assert!(matches!(data.status, Status::Unset));
        assert_eq!(data.name, "alert:ssh-listening");

        // Start/end map to wall-clock; end - start == firing duration.
        assert_eq!(
            data.start_time,
            SystemTime::UNIX_EPOCH + Duration::from_millis(1_000)
        );
        assert_eq!(
            data.end_time
                .duration_since(data.start_time)
                .unwrap()
                .as_millis(),
            4_000
        );

        // Attributes carried across as KeyValues.
        let get = |k: &str| {
            data.attributes
                .iter()
                .find(|kv| kv.key.as_str() == k)
                .map(|kv| kv.value.as_str().to_string())
        };
        assert_eq!(get("alert.source").as_deref(), Some("host01"));
        assert_eq!(get("alert.severity").as_deref(), Some("critical"));
        assert_eq!(data.instrumentation_scope.name(), "zensight.alerts");
    }

    #[test]
    fn alert_severity_maps_to_otel() {
        // Info/Warning/Critical -> Info/Warn/Error, distinct and ordered.
        assert!(matches!(
            alert_severity_to_otel(AlertSeverity::Info),
            Severity::Info
        ));
        assert!(matches!(
            alert_severity_to_otel(AlertSeverity::Warning),
            Severity::Warn
        ));
        assert!(matches!(
            alert_severity_to_otel(AlertSeverity::Critical),
            Severity::Error
        ));
    }

    #[test]
    fn test_telemetry_filter_include_protocols() {
        let config = FilterConfig {
            include_protocols: vec!["snmp".to_string()],
            ..Default::default()
        };
        let filter = TelemetryFilter::new(&config);

        let snmp_point =
            TelemetryPoint::new("r1", Protocol::Snmp, "metric", TelemetryValue::Gauge(1.0));
        let sysinfo_point = TelemetryPoint::new(
            "s1",
            Protocol::Sysinfo,
            "metric",
            TelemetryValue::Gauge(1.0),
        );

        assert!(filter.should_include(&snmp_point));
        assert!(!filter.should_include(&sysinfo_point));
    }

    #[test]
    fn test_telemetry_filter_exclude_sources() {
        let config = FilterConfig {
            exclude_sources: vec!["test-device".to_string()],
            ..Default::default()
        };
        let filter = TelemetryFilter::new(&config);

        let point1 = TelemetryPoint::new(
            "test-device",
            Protocol::Snmp,
            "m",
            TelemetryValue::Gauge(1.0),
        );
        let point2 = TelemetryPoint::new(
            "prod-device",
            Protocol::Snmp,
            "m",
            TelemetryValue::Gauge(1.0),
        );

        assert!(!filter.should_include(&point1));
        assert!(filter.should_include(&point2));
    }

    // ---- OTLP wire assertions (#754) --------------------------------------
    //
    // These assert what actually leaves the process, not what a conversion
    // helper returns. Everything above this line could pass with the counter
    // bug intact, which is how it shipped.

    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

    /// The wire key for a test point. SNMP registers a rest-var catch-all
    /// `<device>/<metric...>`, so its key carries the device chunk that
    /// `point.metric` does not — and naming now flows from the key (#764).
    fn key_of(p: &TelemetryPoint) -> String {
        format!("v1/h-0123456789ab/telemetry/snmp/{}/{}", p.source, p.metric)
    }

    fn point(source: &str, metric: &str, value: TelemetryValue) -> TelemetryPoint {
        TelemetryPoint {
            timestamp: 1_700_000_000_000,
            source: source.to_string(),
            protocol: Protocol::Snmp,
            metric: metric.to_string(),
            value,
            labels: std::collections::HashMap::new(),
            unit: None,
        }
    }

    fn harness() -> (OtelExporter, InMemoryMetricExporter) {
        let sink = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(sink.clone()).build();
        let mp = SdkMeterProvider::builder().with_reader(reader).build();
        (OtelExporter::with_meter_provider(mp), sink)
    }

    /// A cumulative counter must report the reading, not accumulate readings.
    ///
    /// Feeding 100, 150, 150 must export **150** — the device's current total.
    /// The old `counter.add(absolute)` exported 400, so `rate()` returned
    /// `reading / interval` and the series never decreased even on a reset.
    #[test]
    fn a_counter_reports_its_reading_not_the_sum_of_readings() {
        let (exporter, sink) = harness();

        for v in [100u64, 150, 150] {
            {
                let p = point("sw1", "if/1/in_octets", TelemetryValue::Counter(v));
                exporter.record_metric(&key_of(&p), &p);
            }
        }
        exporter
            .meter_provider
            .as_ref()
            .expect("meter provider")
            .force_flush()
            .expect("flush");

        let metrics = sink.get_finished_metrics().expect("finished metrics");
        let mut seen = None;
        for rm in &metrics {
            for sm in rm.scope_metrics() {
                for m in sm.metrics() {
                    if !m.name().contains("in_octets") {
                        continue;
                    }
                    let AggregatedMetrics::U64(MetricData::Sum(sum)) = m.data() else {
                        panic!("a Counter must export as a Sum, got {:?}", m.name());
                    };
                    assert!(sum.is_monotonic(), "a counter Sum must be monotonic");
                    for dp in sum.data_points() {
                        seen = Some(dp.value());
                    }
                }
            }
        }
        assert_eq!(
            seen,
            Some(150),
            "the exported Sum must be the last reading (150), not the running \
             total of readings (400)"
        );
    }

    /// A series nobody has reported since the cutoff stops being observed, so
    /// the metric gaps instead of flat-lining its last value forever.
    #[test]
    fn an_evicted_series_stops_being_observed() {
        let (exporter, sink) = harness();
        {
            let p = point("sw1", "cpu/load", TelemetryValue::Gauge(0.5));
            exporter.record_metric(&key_of(&p), &p);
        }

        let removed = exporter.cleanup_stale_observations(Duration::from_secs(0));
        assert_eq!(removed, 1, "the series is evicted");
        assert_eq!(exporter.series_count(), 0);

        exporter
            .meter_provider
            .as_ref()
            .expect("meter provider")
            .force_flush()
            .expect("flush");

        let metrics = sink.get_finished_metrics().expect("finished metrics");
        let points: usize = metrics
            .iter()
            .flat_map(|rm| rm.scope_metrics())
            .flat_map(|sm| sm.metrics())
            .filter(|m| m.name().contains("cpu_load") || m.name().contains("cpu/load"))
            .map(|m| match m.data() {
                AggregatedMetrics::F64(MetricData::Gauge(g)) => g.data_points().count(),
                _ => 0,
            })
            .sum();
        assert_eq!(points, 0, "an evicted series must produce no data points");
    }

    /// One instrument per metric name, however many points arrive. The old path
    /// rebuilt the instrument on every single sample.
    #[test]
    fn an_instrument_is_registered_once_per_metric_name() {
        let (exporter, _sink) = harness();
        for i in 0..10u64 {
            {
                let p = point("sw1", "if/1/in_octets", TelemetryValue::Counter(i));
                exporter.record_metric(&key_of(&p), &p);
            }
        }
        assert_eq!(exporter.registered.read().len(), 1);
        assert_eq!(exporter._counters.read().len(), 1);
        assert_eq!(exporter.series_count(), 1, "one series, ten updates");
    }

    /// A log record must carry the SENSOR's event time, not the time we saw it.
    ///
    /// Without `set_timestamp` an OTLP record ships `time_unix_nano = 0`, and
    /// Loki/Grafana fall back to ingestion time — so a backlog after a
    /// reconnect plots as a single spike and a line from thirty seconds ago
    /// plots as "now". Nothing inspected an emitted record before (#760), which
    /// is how a computed-then-discarded field survived.
    #[test]
    fn a_log_record_carries_the_sensor_event_time() {
        use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};

        let sink = InMemoryLogExporter::default();
        let lp = SdkLoggerProvider::builder()
            .with_simple_exporter(sink.clone())
            .build();
        let exporter = OtelExporter::with_providers(None, Some(lp));

        const EVENT_MS: i64 = 1_700_000_123_000;
        let mut p = point(
            "host01",
            "syslog",
            TelemetryValue::Text("sshd: accepted".into()),
        );
        p.protocol = Protocol::Logs;
        p.timestamp = EVENT_MS;
        exporter.record_log(
            &format!("v1/h-0123456789ab/telemetry/logs/{}", p.metric),
            &p,
        );

        exporter
            .logger_provider
            .as_ref()
            .expect("logger provider")
            .force_flush()
            .expect("flush");

        let logs = sink.get_emitted_logs().expect("emitted logs");
        assert_eq!(logs.len(), 1, "one record");
        let rec = &logs[0].record;

        let expected = SystemTime::UNIX_EPOCH + Duration::from_millis(EVENT_MS as u64);
        assert_eq!(
            rec.timestamp(),
            Some(expected),
            "the record must carry the sensor's event time"
        );
        assert!(
            rec.observed_timestamp().is_some_and(|o| o >= expected),
            "observed_timestamp is when WE saw it, and is distinct from the event time"
        );
    }

    /// Resource attributes carry the observed host, not just a service name.
    ///
    /// Before #755 this emitted only `service.name`, an optional
    /// `service.version` and hand-written extras — so every host on the bus
    /// shared one `Resource`: `instance=""` in Prometheus, and one Loki stream
    /// for the entire fleet.
    #[test]
    fn a_resource_names_the_host_it_describes() {
        let host = ObservedHost {
            origin: "h-0ead7da13eea".into(),
            host_name: "vm-dev-01".into(),
            producer: "netlink-2".into(),
        };
        let attrs = build_resource_attributes("zensight", None, &HashMap::new(), Some(&host));
        let get = |k: &str| {
            attrs
                .iter()
                .find(|kv| kv.key.as_str() == k)
                .map(|kv| kv.value.to_string())
        };

        assert_eq!(get("host.id").as_deref(), Some("h-0ead7da13eea"));
        assert_eq!(get("host.name").as_deref(), Some("vm-dev-01"));
        assert_eq!(
            get("service.instance.id").as_deref(),
            Some("h-0ead7da13eea/netlink-2"),
            "the producer instance is part of the instance id, so netring-2 is \
             distinguishable from netring"
        );
        assert_eq!(
            get("service.name").as_deref(),
            Some("zensight.netlink-2"),
            "service.name is per-producer, so a service map is meaningful \
             rather than one node called zensight"
        );
    }

    /// Operator config never overrides observed truth.
    #[test]
    fn config_attributes_do_not_override_the_observed_host() {
        let host = ObservedHost {
            origin: "h-real".into(),
            host_name: "real-host".into(),
            producer: "sysinfo".into(),
        };
        let mut extra = HashMap::new();
        extra.insert("host.name".to_string(), "impostor".to_string());
        extra.insert("deployment".to_string(), "prod".to_string());

        let attrs = build_resource_attributes("zensight", None, &extra, Some(&host));
        // Later entries win in an OTel Resource, and observed truth is pushed last.
        let host_names: Vec<String> = attrs
            .iter()
            .filter(|kv| kv.key.as_str() == "host.name")
            .map(|kv| kv.value.to_string())
            .collect();
        assert_eq!(
            host_names.last().map(String::as_str),
            Some("real-host"),
            "the wire wins over config"
        );
        assert!(
            attrs.iter().any(|kv| kv.key.as_str() == "deployment"),
            "unrelated operator attributes still ride along"
        );
    }

    /// The flat resource names no host, because there is no single host it
    /// could honestly name.
    #[test]
    fn the_flat_resource_claims_no_host() {
        let attrs = build_resource_attributes("zensight", None, &HashMap::new(), None);
        assert!(
            !attrs.iter().any(|kv| kv.key.as_str().starts_with("host.")),
            "a shared resource must not claim to be some particular host"
        );
        assert!(attrs.iter().any(|kv| kv.key.as_str() == "service.name"));
    }

    /// An events-class record reaches OTLP as a log, with its ULID as
    /// `event.id` (#762).
    ///
    /// The events plane reached the GUI and a Zenoh storage and **never
    /// reached OTLP at all** — an SNMP trap is arguably the most alert-worthy
    /// thing on the bus.
    #[test]
    fn an_events_record_reaches_otlp_as_a_log() {
        use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
        use zensight_common::event::EventRecord;

        let sink = InMemoryLogExporter::default();
        let lp = SdkLoggerProvider::builder()
            .with_simple_exporter(sink.clone())
            .build();
        let mut exporter = OtelExporter::with_providers(None, Some(lp));
        exporter.export_events = true;
        exporter.event_logger = exporter
            .logger_provider
            .as_ref()
            .map(|lp| lp.logger("zensight.events"));

        let mut fields = std::collections::HashMap::new();
        fields.insert("ifIndex".to_string(), "2".to_string());
        let event = EventRecord {
            id: "01hqzz000000000000000000ab".into(),
            timestamp: 1_700_000_321_000,
            source: "sw1".into(),
            protocol: Protocol::Snmp,
            kind: "trap/link_down".into(),
            severity: AlertSeverity::Warning,
            summary: "link down on eth2".into(),
            alert_key: Some("deadbeefdeadbeef".into()),
            fields,
        };
        exporter.record_event("v1/h-0123456789ab/events/snmp/trap/01hq", &event);

        exporter
            .logger_provider
            .as_ref()
            .expect("logger provider")
            .force_flush()
            .expect("flush");

        let logs = sink.get_emitted_logs().expect("emitted logs");
        assert_eq!(logs.len(), 1, "one record");
        let rec = &logs[0].record;

        assert_eq!(
            rec.timestamp(),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_321_000)),
            "events carry the observation time, not ingestion time"
        );

        let attr = |k: &str| {
            rec.attributes_iter()
                .find(|(key, _)| key.as_str() == k)
                .map(|(_, v)| format!("{v:?}"))
        };
        let is = |k: &str, want: &str| attr(k).is_some_and(|v| v.contains(want));
        assert!(
            is("event.id", "01hqzz000000000000000000ab"),
            "the ULID is the record's identity on the bus, so it is the natural \
             de-duplication key for a replaying consumer: {:?}",
            attr("event.id")
        );
        assert!(
            is("event.kind", "trap/link_down"),
            "{:?}",
            attr("event.kind")
        );
        assert!(
            is("alert.key", "deadbeefdeadbeef"),
            "#651: carrying the key is what lets a consumer join a record to \
             the alert transition it drove: {:?}",
            attr("alert.key")
        );
        assert!(
            is("event.field.ifIndex", "2"),
            "{:?}",
            attr("event.field.ifIndex")
        );
    }
}
