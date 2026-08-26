//! Configuration for the OpenTelemetry exporter.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;
use zensight_common::config::ZenohConfig;

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse config: {0}")]
    Parse(#[from] json5::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

/// Complete exporter configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExporterConfig {
    /// Zenoh connection settings.
    #[serde(default)]
    pub zenoh: ZenohConfig,

    /// OpenTelemetry exporter settings.
    #[serde(default)]
    pub opentelemetry: OtelConfig,

    /// Metric filtering settings.
    #[serde(default)]
    pub filters: FilterConfig,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// OpenTelemetry OTLP configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OtelConfig {
    /// Base OTLP endpoint (e.g. `http://localhost:4317` for gRPC).
    ///
    /// Under `protocol: "http"` the per-signal path is appended to this base —
    /// `/v1/metrics`, `/v1/logs`, `/v1/traces` — unless overridden below. That
    /// is the fix for #756: `opentelemetry-otlp` takes a *programmatic*
    /// endpoint VERBATIM and only appends a signal path when falling back to
    /// `OTEL_EXPORTER_OTLP_ENDPOINT` from the environment. So all three signals
    /// POSTed to `/` and the collector 404'd everything — and because one field
    /// served three signals, appending `/v1/metrics` by hand fixed metrics and
    /// broke logs and traces.
    ///
    /// Under `grpc` the base is passed through unchanged; gRPC routes by
    /// service name, not path.
    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    /// Override the metrics endpoint. Defaults to the base (+ `/v1/metrics`
    /// under HTTP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics_endpoint: Option<String>,

    /// Override the logs endpoint. Defaults to the base (+ `/v1/logs` under HTTP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs_endpoint: Option<String>,

    /// Override the traces endpoint. Defaults to the base (+ `/v1/traces` under HTTP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traces_endpoint: Option<String>,

    /// Protocol: "grpc" or "http".
    #[serde(default = "default_protocol")]
    pub protocol: OtlpProtocol,

    /// Headers to include in OTLP requests (e.g. for authentication).
    ///
    /// These were parsed and then **never used** (#756) — `with_headers` /
    /// `with_metadata` appeared nowhere in the crate — so every authenticated
    /// backend the README advertises (Grafana Cloud, Honeycomb, Datadog, New
    /// Relic) got a 401 with no hint why.
    #[serde(default)]
    pub headers: HashMap<String, String>,

    /// Export interval in seconds.
    #[serde(default = "default_export_interval")]
    pub export_interval_secs: u64,

    /// Export timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,

    /// Whether to export metrics.
    #[serde(default = "default_true")]
    pub export_metrics: bool,

    /// Whether to export logs (syslog messages).
    #[serde(default = "default_true")]
    pub export_logs: bool,

    /// Whether to export sensor alerts (`state/*/alert/*`) as OTLP log records.
    ///
    /// Each alert transition (firing/resolved) is emitted as a log record on the
    /// `zensight.alerts` scope with severity mapped from the alert severity and
    /// `alert.*` attributes. Needs the logger pipeline, which is initialized
    /// whenever logs or alerts are enabled.
    #[serde(default = "default_true")]
    pub export_alerts: bool,

    /// Traces signal: synthesized spans (default: disabled).
    ///
    /// ZenSight has no distributed-tracing context propagation; spans are
    /// *synthesized* from request/response-shaped events the exporter already
    /// observes on the bus. Today that is the alert lifecycle: each
    /// firing → resolved transition becomes one span (`alert:<rule>`) whose
    /// duration is the time the alert was firing. Trace/span ids are derived
    /// deterministically from the alert key + firing timestamp.
    #[serde(default)]
    pub traces: TracesConfig,

    /// Resource attributes to add to all telemetry.
    #[serde(default)]
    pub resource: HashMap<String, String>,

    /// How host identity reaches the OTLP `Resource` (#755).
    #[serde(default)]
    pub resource_mode: ResourceMode,

    /// Cap on distinct per-origin resources held at once.
    ///
    /// Each one owns its own signal providers, so this bounds both memory and
    /// the number of exporter pipelines. Past it, further origins fall back to
    /// the shared flat resource and `dropped_resources` counts them — a
    /// degraded but honest answer, rather than unbounded growth.
    #[serde(default = "default_max_resources")]
    pub max_resources: usize,

    /// Service name for OTEL resource.
    #[serde(default = "default_service_name")]
    pub service_name: String,

    /// Service version for OTEL resource.
    #[serde(default)]
    pub service_version: Option<String>,
}

/// How host identity reaches the OTLP `Resource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceMode {
    /// One `Resource` per observed host, carrying `host.id`, `host.name` and
    /// `service.instance.id`.
    ///
    /// This is the shape OTel semantic conventions expect, and the only one
    /// that works for logs: a backend derives stream identity from the
    /// **resource**, so a single shared resource collapses every host in the
    /// fleet into one log stream.
    #[default]
    PerOrigin,
    /// One shared `Resource`, with host identity carried as data-point and
    /// log-record **attributes** instead.
    ///
    /// For backends that cope badly with many resources. Metrics stay
    /// queryable (the attributes are labels), but logs lose per-host streams.
    Flat,
}

fn default_max_resources() -> usize {
    512
}

fn default_endpoint() -> String {
    "http://localhost:4317".to_string()
}

fn default_protocol() -> OtlpProtocol {
    OtlpProtocol::Grpc
}

fn default_export_interval() -> u64 {
    10
}

fn default_timeout() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

fn default_service_name() -> String {
    "zensight".to_string()
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            metrics_endpoint: None,
            logs_endpoint: None,
            traces_endpoint: None,
            protocol: default_protocol(),
            headers: HashMap::new(),
            export_interval_secs: default_export_interval(),
            timeout_secs: default_timeout(),
            export_metrics: true,
            export_logs: true,
            export_alerts: true,
            traces: TracesConfig::default(),
            resource: HashMap::new(),
            resource_mode: ResourceMode::default(),
            max_resources: default_max_resources(),
            service_name: default_service_name(),
            service_version: None,
        }
    }
}

/// Traces signal configuration.
///
/// Off by default: span synthesis is opt-in, and artifact-transfer spans are
/// not implemented (artifact status lives on the `artifact/status` read
/// procedure, which the exporter never GETs — only `state/*/alert/*` is
/// observed).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TracesConfig {
    /// Synthesize + export alert-lifecycle spans via OTLP (default: false).
    #[serde(default)]
    pub enabled: bool,
}

/// Which OTLP signal an endpoint is being resolved for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Metrics,
    Logs,
    Traces,
}

impl Signal {
    /// The path OTLP/HTTP defines for this signal.
    fn path(self) -> &'static str {
        match self {
            Signal::Metrics => "/v1/metrics",
            Signal::Logs => "/v1/logs",
            Signal::Traces => "/v1/traces",
        }
    }
}

impl OtelConfig {
    /// The endpoint to use for one signal.
    ///
    /// An explicit per-signal override wins. Otherwise, under HTTP the signal
    /// path is appended to the base — which `opentelemetry-otlp` does NOT do
    /// for a programmatic endpoint, only for one read from the environment
    /// (#756). Under gRPC the base is returned unchanged, because gRPC routes
    /// by service name rather than path.
    ///
    /// Trailing slashes on the base are collapsed, and a base that already ends
    /// with the signal path is left alone — so a config carried over from
    /// before this change keeps working.
    pub fn signal_endpoint(&self, signal: Signal) -> String {
        let explicit = match signal {
            Signal::Metrics => self.metrics_endpoint.as_deref(),
            Signal::Logs => self.logs_endpoint.as_deref(),
            Signal::Traces => self.traces_endpoint.as_deref(),
        };
        if let Some(url) = explicit {
            return url.to_string();
        }
        if self.protocol != OtlpProtocol::Http {
            return self.endpoint.clone();
        }
        let base = self.endpoint.trim_end_matches('/');
        let path = signal.path();
        if base.ends_with(path) {
            base.to_string()
        } else {
            format!("{base}{path}")
        }
    }

    /// Get export interval as Duration.
    pub fn export_interval(&self) -> Duration {
        Duration::from_secs(self.export_interval_secs)
    }

    /// Get timeout as Duration.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

/// OTLP protocol selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OtlpProtocol {
    /// gRPC protocol (port 4317).
    #[default]
    Grpc,
    /// HTTP/protobuf protocol (port 4318).
    Http,
}

/// Metric filtering configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilterConfig {
    /// Zenoh subscription key expression for telemetry (R6/#357). Defaults to the
    /// full telemetry class selector; narrow it (e.g.
    /// `v1/*/telemetry/netring/**`) to tame the
    /// firehose at the *subscription* — unwanted protocols never reach this
    /// exporter over the wire, and the state plane / non-telemetry classes
    /// can't match the telemetry class selector by construction. The
    /// `state/*/alert/*` subscriber is separate and unaffected.
    /// `include_protocols` etc. still apply
    /// as a post-receive filter.
    #[serde(default)]
    pub key_expr: Option<String>,

    /// Only include these protocols (empty = all).
    #[serde(default)]
    pub include_protocols: Vec<String>,

    /// Exclude these protocols.
    #[serde(default)]
    pub exclude_protocols: Vec<String>,

    /// Only include these sources (empty = all).
    #[serde(default)]
    pub include_sources: Vec<String>,

    /// Exclude these sources.
    #[serde(default)]
    pub exclude_sources: Vec<String>,
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: "trace", "debug", "info", "warn", "error".
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Log output format: "text" or "json".
    #[serde(default)]
    pub format: LogFormat,
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
        }
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

impl ExporterConfig {
    /// Load configuration from a JSON5 file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: ExporterConfig = json5::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse configuration from a JSON5 string.
    pub fn parse(content: &str) -> Result<Self, ConfigError> {
        let config: ExporterConfig = json5::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.opentelemetry.endpoint.is_empty() {
            return Err(ConfigError::Validation(
                "OTLP endpoint cannot be empty".to_string(),
            ));
        }

        if self.opentelemetry.export_interval_secs == 0 {
            return Err(ConfigError::Validation(
                "export_interval_secs must be > 0".to_string(),
            ));
        }

        if self.opentelemetry.timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "timeout_secs must be > 0".to_string(),
            ));
        }

        if !self.opentelemetry.export_metrics
            && !self.opentelemetry.export_logs
            && !self.opentelemetry.export_alerts
            && !self.opentelemetry.traces.enabled
        {
            return Err(ConfigError::Validation(
                "At least one of export_metrics, export_logs, export_alerts or traces must be enabled"
                    .to_string(),
            ));
        }

        // A selector that spells the deployment base matches NOTHING (#466) —
        // with a perfectly healthy session and an empty dashboard. The README
        // recommended exactly that until #761; the validator existed all along
        // and was simply never called here (#757).
        if let Some(ke) = &self.filters.key_expr
            && let Err(e) = zensight_common::keyexpr::validate_relative_selector(ke)
        {
            return Err(ConfigError::Validation(format!("filters.key_expr: {e}")));
        }

        // An unknown protocol token silently filters out everything it was
        // meant to include: `include_protocols: [..., "syslog"]` — the token is
        // `logs` — dropped 100% of log records while `export_logs: true`.
        for (field, list) in [
            ("include_protocols", &self.filters.include_protocols),
            ("exclude_protocols", &self.filters.exclude_protocols),
        ] {
            for token in list {
                if token
                    .parse::<zensight_common::telemetry::Protocol>()
                    .is_err()
                {
                    return Err(ConfigError::Validation(format!(
                        "filters.{field} contains unknown protocol {token:?}. Valid tokens: \
                         snmp, logs, gnmi, netflow, opcua, modbus, sysinfo, netlink, netring, \
                         systemd, parallax"
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    /// The shipped config must SPELL OUT `opentelemetry.traces.enabled`.
    ///
    /// `scripts/gen-configs.sh` transforms the committed examples with `sed`,
    /// and its header states the rule: a sed can only flip a key that is really
    /// in `configs/*.json5`. The demo profile flips this one to `true`, so if
    /// the key ever stops being written out the sed silently does nothing and
    /// the OTel demo's Tempo pane is quietly empty.
    ///
    /// Asserting the PARSED value would be vacuous — `false` is also the Rust
    /// default, so a missing key parses to exactly the same struct. The raw
    /// JSON5 tree is therefore what gets walked.
    #[test]
    fn shipped_config_spells_out_the_traces_flag() {
        let raw = include_str!("../../configs/otel-exporter.json5");

        // It parses, and traces are off by default.
        let cfg = ExporterConfig::parse(raw).expect("shipped config parses");
        assert!(
            !cfg.opentelemetry.traces.enabled,
            "traces stay opt-in in the shipped config"
        );

        // And the key is physically present for the sed to find.
        let tree: serde_json::Value = json5::from_str(raw).expect("shipped config is valid JSON5");
        let enabled = tree
            .get("opentelemetry")
            .and_then(|o| o.get("traces"))
            .and_then(|t| t.get("enabled"));
        assert_eq!(
            enabled,
            Some(&serde_json::Value::Bool(false)),
            "configs/otel-exporter.json5 must spell out \
             opentelemetry.traces.enabled = false — gen-configs.sh flips it"
        );
    }
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let json = "{}";
        let config = ExporterConfig::parse(json).unwrap();

        assert_eq!(config.opentelemetry.endpoint, "http://localhost:4317");
        assert_eq!(config.opentelemetry.protocol, OtlpProtocol::Grpc);
        assert_eq!(config.opentelemetry.export_interval_secs, 10);
        assert!(config.opentelemetry.export_metrics);
        assert!(config.opentelemetry.export_logs);
    }

    #[test]
    fn test_parse_full_config() {
        let json = r#"{
            zenoh: {
                mode: "client",
                connect: ["tcp/localhost:7447"]
            },
            opentelemetry: {
                endpoint: "http://otel-collector:4317",
                protocol: "grpc",
                export_interval_secs: 30,
                timeout_secs: 60,
                export_metrics: true,
                export_logs: true,
                service_name: "my-zensight",
                service_version: "1.0.0",
                headers: {
                    "Authorization": "Bearer token123"
                },
                resource: {
                    "deployment.environment": "production"
                }
            },
            filters: {
                key_expr: "v1/*/telemetry/netring/**",
                include_protocols: ["snmp", "sysinfo"],
                exclude_sources: ["test-device"]
            },
            logging: {
                level: "debug",
                format: "json"
            }
        }"#;

        let config = ExporterConfig::parse(json).unwrap();

        assert_eq!(config.zenoh.mode, "client");
        assert_eq!(config.opentelemetry.endpoint, "http://otel-collector:4317");
        assert_eq!(config.opentelemetry.protocol, OtlpProtocol::Grpc);
        assert_eq!(config.opentelemetry.export_interval_secs, 30);
        assert_eq!(config.opentelemetry.service_name, "my-zensight");
        assert_eq!(
            config.opentelemetry.service_version,
            Some("1.0.0".to_string())
        );
        assert_eq!(
            config.opentelemetry.headers.get("Authorization"),
            Some(&"Bearer token123".to_string())
        );
        assert_eq!(config.filters.include_protocols, vec!["snmp", "sysinfo"]);
        assert_eq!(
            config.filters.key_expr.as_deref(),
            Some("v1/*/telemetry/netring/**")
        );
        assert_eq!(config.logging.level, "debug");
        assert_eq!(config.logging.format, LogFormat::Json);
    }

    #[test]
    fn test_validate_empty_endpoint() {
        let json = r#"{
            opentelemetry: { endpoint: "" }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("endpoint"));
    }

    #[test]
    fn test_validate_zero_interval() {
        let json = r#"{
            opentelemetry: { export_interval_secs: 0 }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_no_exports() {
        let json = r#"{
            opentelemetry: {
                export_metrics: false,
                export_logs: false,
                export_alerts: false
            }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("At least one of"));
    }

    #[test]
    fn test_validate_alerts_only_is_valid() {
        // Alerts are a first-class signal: enabling only alerts is a valid config.
        let json = r#"{
            opentelemetry: {
                export_metrics: false,
                export_logs: false,
                export_alerts: true
            }
        }"#;

        let result = ExporterConfig::parse(json);
        assert!(result.is_ok(), "alerts-only config should validate");
        assert!(result.unwrap().opentelemetry.export_alerts);
    }

    #[test]
    fn test_traces_default_disabled() {
        let config = ExporterConfig::parse("{}").unwrap();
        assert!(
            !config.opentelemetry.traces.enabled,
            "traces must be opt-in"
        );
    }

    #[test]
    fn test_traces_enabled_via_config() {
        let json = r#"{
            opentelemetry: {
                traces: { enabled: true }
            }
        }"#;
        let config = ExporterConfig::parse(json).unwrap();
        assert!(config.opentelemetry.traces.enabled);
    }

    #[test]
    fn test_validate_traces_only_is_valid() {
        // Traces are a first-class signal: enabling only traces is valid.
        let json = r#"{
            opentelemetry: {
                export_metrics: false,
                export_logs: false,
                export_alerts: false,
                traces: { enabled: true }
            }
        }"#;
        let result = ExporterConfig::parse(json);
        assert!(result.is_ok(), "traces-only config should validate");
    }

    #[test]
    fn test_http_protocol() {
        let json = r#"{
            opentelemetry: {
                endpoint: "http://localhost:4318",
                protocol: "http"
            }
        }"#;

        let config = ExporterConfig::parse(json).unwrap();
        assert_eq!(config.opentelemetry.protocol, OtlpProtocol::Http);
    }

    /// A selector spelling the deployment base matches NOTHING since #466, with
    /// a perfectly healthy session and an empty dashboard. Both READMEs
    /// recommended exactly this shape until #761.
    #[test]
    fn a_base_prefixed_key_expr_is_rejected() {
        let mut cfg = ExporterConfig::default();
        cfg.filters.key_expr = Some("zensight/v1/*/telemetry/**".to_string());
        let err = cfg
            .validate()
            .expect_err("base-prefixed selectors match nothing");
        assert!(format!("{err}").contains("key_expr"), "{err}");

        cfg.filters.key_expr = Some("v1/*/telemetry/**".to_string());
        assert!(cfg.validate().is_ok(), "the base-relative form is correct");
    }

    /// An unknown protocol token silently filters out everything it was meant
    /// to include — `"syslog"` (the token is `logs`) dropped 100% of log
    /// records while `export_logs: true`.
    #[test]
    fn an_unknown_protocol_token_is_rejected() {
        let mut cfg = ExporterConfig::default();
        cfg.filters.include_protocols = vec!["snmp".into(), "syslog".into()];
        let err = cfg.validate().expect_err("syslog is not a protocol token");
        assert!(format!("{err}").contains("syslog"), "{err}");
        assert!(
            format!("{err}").contains("logs"),
            "the error must name the valid token: {err}"
        );

        cfg.filters.include_protocols = vec!["snmp".into(), "logs".into()];
        assert!(cfg.validate().is_ok());
    }

    /// Under HTTP the signal path is appended to the base (#756).
    ///
    /// `opentelemetry-otlp` takes a *programmatic* endpoint verbatim and only
    /// appends a path when falling back to `OTEL_EXPORTER_OTLP_ENDPOINT` from
    /// the environment — so all three signals POSTed to `/` and the collector
    /// 404'd everything. And because one field served three signals, appending
    /// `/v1/metrics` by hand fixed metrics while breaking logs and traces.
    #[test]
    fn http_appends_the_signal_path() {
        let mut cfg = OtelConfig {
            endpoint: "http://collector:4318".into(),
            protocol: OtlpProtocol::Http,
            ..Default::default()
        };
        assert_eq!(
            cfg.signal_endpoint(Signal::Metrics),
            "http://collector:4318/v1/metrics"
        );
        assert_eq!(
            cfg.signal_endpoint(Signal::Logs),
            "http://collector:4318/v1/logs"
        );
        assert_eq!(
            cfg.signal_endpoint(Signal::Traces),
            "http://collector:4318/v1/traces"
        );

        // A trailing slash must not double up.
        cfg.endpoint = "http://collector:4318/".into();
        assert_eq!(
            cfg.signal_endpoint(Signal::Metrics),
            "http://collector:4318/v1/metrics"
        );

        // A config carried over from before this change, where somebody had
        // already appended the path by hand, keeps working.
        cfg.endpoint = "http://collector:4318/v1/metrics".into();
        assert_eq!(
            cfg.signal_endpoint(Signal::Metrics),
            "http://collector:4318/v1/metrics"
        );
    }

    /// gRPC routes by service name, not path, so the base passes through.
    #[test]
    fn grpc_leaves_the_endpoint_alone() {
        let cfg = OtelConfig {
            endpoint: "http://collector:4317".into(),
            protocol: OtlpProtocol::Grpc,
            ..Default::default()
        };
        for signal in [Signal::Metrics, Signal::Logs, Signal::Traces] {
            assert_eq!(cfg.signal_endpoint(signal), "http://collector:4317");
        }
    }

    /// An explicit per-signal override wins over both the base and the
    /// appended path — which is what makes a split-backend deployment
    /// expressible at all.
    #[test]
    fn an_explicit_signal_endpoint_wins() {
        let cfg = OtelConfig {
            endpoint: "http://collector:4318".into(),
            protocol: OtlpProtocol::Http,
            logs_endpoint: Some("https://logs.example.com/ingest".into()),
            ..Default::default()
        };
        assert_eq!(
            cfg.signal_endpoint(Signal::Logs),
            "https://logs.example.com/ingest"
        );
        assert_eq!(
            cfg.signal_endpoint(Signal::Metrics),
            "http://collector:4318/v1/metrics",
            "the other signals still follow the base"
        );
    }
}
