//! OTLP exporter setup and management.

use std::collections::HashMap;
use std::sync::Arc;

use opentelemetry::logs::{LogRecord as _, Logger, LoggerProvider as _};
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_otlp::{LogExporter, MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use parking_lot::RwLock;
use tracing::{error, info, trace};
use zensight_common::telemetry::TelemetryPoint;

use crate::config::{FilterConfig, OtelConfig, OtlpProtocol};
use crate::logs::LogRecord;
use crate::metrics::{
    OtelMetricType, build_metric_attributes, build_metric_name, build_resource_attributes,
    extract_value, is_log_exportable, is_metric_exportable,
};

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
    pub logs_exported: u64,
    pub export_errors: u64,
}

/// OpenTelemetry exporter that receives telemetry and exports via OTLP.
pub struct OtelExporter {
    /// Meter provider for metrics.
    meter_provider: Option<SdkMeterProvider>,
    /// Logger provider for logs.
    logger_provider: Option<SdkLoggerProvider>,
    /// Whether metrics export is enabled.
    export_metrics: bool,
    /// Whether logs export is enabled.
    export_logs: bool,
    /// Telemetry filter.
    filter: TelemetryFilter,
    /// Export statistics.
    stats: RwLock<ExporterStats>,
    /// Registered gauges for updating.
    gauges: RwLock<HashMap<String, f64>>,
}

impl OtelExporter {
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
        let resource_attrs = build_resource_attributes(
            &otel_config.service_name,
            otel_config.service_version.as_deref(),
            &otel_config.resource,
        );
        let resource = Resource::builder().with_attributes(resource_attrs).build();

        // Initialize meter provider if metrics enabled
        let meter_provider = if otel_config.export_metrics {
            Some(Self::init_meter_provider(otel_config, resource.clone()).await?)
        } else {
            None
        };

        // Initialize logger provider if logs enabled
        let logger_provider = if otel_config.export_logs {
            Some(Self::init_logger_provider(otel_config, resource).await?)
        } else {
            None
        };

        Ok(Self {
            meter_provider,
            logger_provider,
            export_metrics: otel_config.export_metrics,
            export_logs: otel_config.export_logs,
            filter: TelemetryFilter::new(filter_config),
            stats: RwLock::new(ExporterStats::default()),
            gauges: RwLock::new(HashMap::new()),
        })
    }

    async fn init_meter_provider(
        config: &OtelConfig,
        resource: Resource,
    ) -> anyhow::Result<SdkMeterProvider> {
        let exporter = match config.protocol {
            OtlpProtocol::Grpc => MetricExporter::builder()
                .with_tonic()
                .with_endpoint(&config.endpoint)
                .with_timeout(config.timeout())
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to create gRPC metric exporter: {}", e))?,
            OtlpProtocol::Http => MetricExporter::builder()
                .with_http()
                .with_endpoint(&config.endpoint)
                .with_timeout(config.timeout())
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

    async fn init_logger_provider(
        config: &OtelConfig,
        resource: Resource,
    ) -> anyhow::Result<SdkLoggerProvider> {
        let exporter = match config.protocol {
            OtlpProtocol::Grpc => LogExporter::builder()
                .with_tonic()
                .with_endpoint(&config.endpoint)
                .with_timeout(config.timeout())
                .build()
                .map_err(|e| anyhow::anyhow!("Failed to create gRPC log exporter: {}", e))?,
            OtlpProtocol::Http => LogExporter::builder()
                .with_http()
                .with_endpoint(&config.endpoint)
                .with_timeout(config.timeout())
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

    /// Record a telemetry point.
    pub fn record(&self, point: &TelemetryPoint) {
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
            self.record_metric(point);
        }

        // Export as log if applicable
        if self.export_logs && is_log_exportable(&point.value, point.protocol) {
            self.record_log(point);
        }
    }

    fn record_metric(&self, point: &TelemetryPoint) {
        let Some(meter_provider) = &self.meter_provider else {
            return;
        };

        let metric_name = build_metric_name(point.protocol, &point.metric);
        let attributes = build_metric_attributes(point);

        let meter = meter_provider.meter("zensight");

        match OtelMetricType::from_value(&point.value) {
            OtelMetricType::Counter => {
                if let Some(value) = extract_value(&point.value) {
                    let counter = meter.u64_counter(metric_name.clone()).build();
                    counter.add(value as u64, &attributes);

                    trace!(
                        metric = %metric_name,
                        value = value,
                        "Recorded counter"
                    );

                    let mut stats = self.stats.write();
                    stats.metrics_exported += 1;
                }
            }
            OtelMetricType::Gauge => {
                if let Some(value) = extract_value(&point.value) {
                    // For gauges, we use an observable gauge pattern
                    // Store the value and let the SDK read it periodically
                    let key = format!(
                        "{}:{}",
                        metric_name,
                        attributes
                            .iter()
                            .map(|kv| format!("{}={}", kv.key, kv.value.as_str()))
                            .collect::<Vec<_>>()
                            .join(",")
                    );

                    self.gauges.write().insert(key, value);

                    // Create/update gauge
                    let gauge = meter.f64_gauge(metric_name.clone()).build();
                    gauge.record(value, &attributes);

                    trace!(
                        metric = %metric_name,
                        value = value,
                        "Recorded gauge"
                    );

                    let mut stats = self.stats.write();
                    stats.metrics_exported += 1;
                }
            }
            OtelMetricType::NotExportable => {}
        }
    }

    fn record_log(&self, point: &TelemetryPoint) {
        let Some(logger_provider) = &self.logger_provider else {
            return;
        };

        let Some(record) = LogRecord::from_telemetry(point) else {
            return;
        };

        let logger = logger_provider.logger("zensight.syslog");

        // Create a new log record
        let mut log_record = logger.create_log_record();

        // Set body
        log_record.set_body(record.body.clone().into());

        // Set severity
        log_record.set_severity_number(record.otel_severity());
        log_record.set_severity_text(record.severity.as_str());

        // Add attributes
        log_record.add_attribute("hostname", record.hostname.clone());
        log_record.add_attribute("syslog.severity", record.severity.as_str().to_string());

        if let Some(facility) = &record.facility {
            log_record.add_attribute("syslog.facility", facility.as_str().to_string());
        }

        if let Some(appname) = &record.appname {
            log_record.add_attribute("syslog.appname", appname.clone());
        }

        // Emit the log
        logger.emit(log_record);

        trace!(
            hostname = %record.hostname,
            severity = %record.severity.as_str(),
            "Recorded log"
        );

        let mut stats = self.stats.write();
        stats.logs_exported += 1;
    }

    /// Get current statistics.
    pub fn stats(&self) -> ExporterStats {
        self.stats.read().clone()
    }

    /// Shutdown the exporter gracefully.
    pub fn shutdown(&self) -> anyhow::Result<()> {
        info!("Shutting down OpenTelemetry exporter");

        if let Some(meter_provider) = &self.meter_provider
            && let Err(e) = meter_provider.shutdown() {
                error!("Error shutting down meter provider: {:?}", e);
            }

        if let Some(logger_provider) = &self.logger_provider
            && let Err(e) = logger_provider.shutdown() {
                error!("Error shutting down logger provider: {:?}", e);
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
    use zensight_common::telemetry::{Protocol, TelemetryValue};

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
}
