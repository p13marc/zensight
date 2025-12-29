# ZenSight OpenTelemetry Exporter

OpenTelemetry OTLP exporter for ZenSight telemetry. Subscribes to telemetry over Zenoh and exports metrics and logs via OTLP to any OpenTelemetry-compatible backend.

## Features

- **OTLP export**: gRPC (port 4317) or HTTP (port 4318) protocols
- **Metrics**: Counter, Gauge, Boolean values exported as OTEL metrics
- **Logs**: Syslog text messages exported as OTEL log records
- **Severity mapping**: Syslog severity properly mapped to OTEL severity levels
- **Resource attributes**: Configurable service name, version, and custom attributes
- **Filtering**: Filter by protocol or source
- **Periodic export**: Configurable export interval with batching

## Supported Backends

Any OpenTelemetry-compatible backend:

- OpenTelemetry Collector
- Jaeger
- Grafana Tempo / Loki
- Datadog
- New Relic
- Honeycomb
- AWS X-Ray / CloudWatch
- Azure Monitor
- Google Cloud Trace

## Installation

```bash
cargo build -p zensight-exporter-otel --release
```

## Usage

```bash
# With config file
zensight-exporter-otel --config configs/otel-exporter.json5

# Override endpoint
zensight-exporter-otel --config config.json5 --endpoint http://collector:4317

# Debug logging
zensight-exporter-otel --config config.json5 --log-level debug
```

## Configuration

Create a JSON5 configuration file:

```json5
{
  // Zenoh connection
  zenoh: {
    mode: "peer",
    connect: ["tcp/localhost:7447"],
  },

  // OpenTelemetry settings
  opentelemetry: {
    endpoint: "http://localhost:4317",  // OTLP endpoint
    protocol: "grpc",                    // "grpc" or "http"
    export_interval_secs: 10,            // Batch export interval
    timeout_secs: 30,                    // Export timeout
    
    export_metrics: true,   // Export Counter/Gauge/Boolean as metrics
    export_logs: true,      // Export Syslog as log records
    
    service_name: "zensight",
    service_version: "1.0.0",
    
    // Authentication headers (optional)
    headers: {
      "Authorization": "Bearer token",
    },
    
    // Custom resource attributes
    resource: {
      "deployment.environment": "production",
      "service.namespace": "monitoring",
    },
  },

  // Filtering (optional)
  filters: {
    include_protocols: ["snmp", "sysinfo", "syslog"],
    exclude_sources: ["test-device"],
  },
}
```

## Signal Mapping

### Metrics

| ZenSight Value | OTEL Metric Type |
|----------------|------------------|
| `Counter(u64)` | Sum (monotonic) |
| `Gauge(f64)` | Gauge |
| `Boolean(bool)` | Gauge (0/1) |
| `Text(String)` | Not exported as metric |
| `Binary(Vec<u8>)` | Not exported |

Metric names follow the pattern: `zensight.{protocol}.{metric_path}`

### Logs

Only `Syslog` protocol with `Text` values are exported as logs.

| Syslog Severity | OTEL Severity |
|-----------------|---------------|
| Emergency, Alert | Fatal |
| Critical, Error | Error |
| Warning | Warn |
| Notice, Informational | Info |
| Debug | Debug |

Log attributes include:
- `hostname` - Source device
- `syslog.severity` - Original syslog severity
- `syslog.facility` - Syslog facility (if present)
- `syslog.appname` - Application name (if present)

## OpenTelemetry Collector Configuration

Example `otel-collector-config.yaml`:

```yaml
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
      http:
        endpoint: 0.0.0.0:4318

processors:
  batch:
    timeout: 10s

exporters:
  prometheus:
    endpoint: 0.0.0.0:8889
  loki:
    endpoint: http://loki:3100/loki/api/v1/push

service:
  pipelines:
    metrics:
      receivers: [otlp]
      processors: [batch]
      exporters: [prometheus]
    logs:
      receivers: [otlp]
      processors: [batch]
      exporters: [loki]
```

## Docker Compose Example

```yaml
services:
  zensight-otel-exporter:
    build: .
    command: ["zensight-exporter-otel", "--config", "/config/otel-exporter.json5"]
    volumes:
      - ./configs:/config
    depends_on:
      - otel-collector

  otel-collector:
    image: otel/opentelemetry-collector:latest
    ports:
      - "4317:4317"
      - "4318:4318"
    volumes:
      - ./otel-collector-config.yaml:/etc/otelcol/config.yaml
```

## License

MIT
