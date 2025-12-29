# Observability Integration Plan: OpenTelemetry & Prometheus

## Executive Summary

This document proposes adding OpenTelemetry and Prometheus export capabilities to ZenSight. The goal is to bridge ZenSight's unified telemetry model into industry-standard observability ecosystems, enabling integration with Grafana, Datadog, CloudWatch, Jaeger, and other popular tools.

## Current State

ZenSight collects telemetry from 6 protocols (SNMP, Syslog, NetFlow, Modbus, Sysinfo, gNMI) and normalizes everything to a unified `TelemetryPoint` structure:

```rust
pub struct TelemetryPoint {
    pub timestamp: i64,                    // Unix epoch milliseconds
    pub source: String,                    // Device identifier
    pub protocol: Protocol,                // Origin protocol
    pub metric: String,                    // Metric name/path
    pub value: TelemetryValue,             // Counter, Gauge, Text, Boolean, Binary
    pub labels: HashMap<String, String>,   // Additional context
}
```

**Current limitation**: Data stays within the Zenoh ecosystem. No export to external observability platforms.

## Why Add These Integrations?

| Benefit | Description |
|---------|-------------|
| **Ecosystem compatibility** | Most organizations already have Prometheus/Grafana or OTEL-based stacks |
| **Historical storage** | Enable long-term metric retention in Prometheus, InfluxDB, or cloud services |
| **Alerting** | Leverage Prometheus Alertmanager or OTEL-compatible alerting systems |
| **Correlation** | Combine ZenSight data with application metrics/traces in unified dashboards |
| **Minimal effort** | ZenSight's data model maps cleanly to standard formats |

## Proposed Architecture

```
                         ZenSight Bridges
                               │
                               ▼
                   ┌───────────────────────┐
                   │     Zenoh Mesh        │
                   │  zensight/** topics   │
                   └───────────────────────┘
                               │
           ┌───────────────────┼───────────────────┐
           │                   │                   │
           ▼                   ▼                   ▼
    ┌─────────────┐   ┌────────────────┐   ┌──────────────────┐
    │  Frontend   │   │  Prometheus    │   │  OpenTelemetry   │
    │  (Iced UI)  │   │  Exporter      │   │  Exporter        │
    └─────────────┘   │                │   │                  │
                      │ /metrics HTTP  │   │ OTLP gRPC/HTTP   │
                      └────────────────┘   └──────────────────┘
                               │                   │
                               ▼                   ▼
                      ┌────────────────┐   ┌──────────────────┐
                      │  Prometheus    │   │  OTEL Collector  │
                      │  Server        │   │  or Backends     │
                      └────────────────┘   └──────────────────┘
                               │                   │
                               ▼                   ▼
                      ┌────────────────┐   ┌──────────────────┐
                      │   Grafana      │   │ Jaeger, Datadog, │
                      │                │   │ CloudWatch, etc. │
                      └────────────────┘   └──────────────────┘
```

## Phase 1: Prometheus Exporter

### Overview

Create a standalone exporter that subscribes to Zenoh and exposes a `/metrics` HTTP endpoint for Prometheus scraping.

### New Crate: `zensight-exporter-prometheus`

```
zensight-exporter-prometheus/
├── Cargo.toml
├── src/
│   ├── main.rs           # Binary entry point
│   ├── lib.rs            # Library exports
│   ├── config.rs         # Configuration (JSON5)
│   ├── collector.rs      # Zenoh subscriber + metric aggregation
│   ├── http.rs           # HTTP server for /metrics
│   └── mapping.rs        # TelemetryPoint -> Prometheus conversion
└── tests/
    └── integration.rs    # End-to-end tests
```

### Type Mapping

| TelemetryValue | Prometheus Type | Notes |
|----------------|-----------------|-------|
| `Counter(u64)` | Counter | Monotonically increasing |
| `Gauge(f64)` | Gauge | Point-in-time value |
| `Text(String)` | Info metric | Expose as label on info metric |
| `Boolean(bool)` | Gauge | 0/1 value |
| `Binary(Vec<u8>)` | Skip | Not applicable for metrics |

### Label Mapping

```rust
// TelemetryPoint labels become Prometheus labels
// Plus automatic labels:
{
    source: "router01",           // Always present
    protocol: "snmp",             // Always present
    metric: "ifInOctets",         // Becomes metric name suffix
    // ... user labels preserved
}

// Prometheus metric name: zensight_snmp_ifInOctets{source="router01", ...}
```

### Configuration Example

```json5
{
  // Zenoh connection
  zenoh: {
    mode: "peer",
    connect: ["tcp/localhost:7447"],
  },
  
  // Prometheus exporter settings
  prometheus: {
    listen: "0.0.0.0:9090",
    path: "/metrics",
    
    // Metric filtering (optional)
    include_protocols: ["snmp", "sysinfo"],  // null = all
    exclude_metrics: ["**/debug/**"],
    
    // Label configuration
    default_labels: {
      environment: "production",
      datacenter: "us-east-1",
    },
  },
  
  // Aggregation settings
  aggregation: {
    // How long to keep metrics without updates before expiring
    stale_timeout_secs: 300,
    
    // Maximum unique time series (memory protection)
    max_series: 100000,
  },
}
```

### Key Design Decisions

1. **Pull model**: Standard Prometheus scrape pattern, not push
2. **Aggregation in exporter**: Keep latest value per metric+labels, with staleness expiry
3. **Memory bounded**: Configurable max series limit
4. **Metric naming**: `zensight_{protocol}_{metric_path}` with sanitization

### Implementation Steps

1. [ ] Create crate structure and Cargo.toml with dependencies:
   - `prometheus-client` (official Prometheus Rust client)
   - `hyper` or `axum` for HTTP server
   - `zenoh` for subscription
   - `zensight-common` for TelemetryPoint

2. [ ] Implement Zenoh subscriber using existing pattern from frontend

3. [ ] Implement TelemetryPoint to Prometheus metric conversion

4. [ ] Implement HTTP server with `/metrics` endpoint

5. [ ] Add configuration loading (reuse `zensight-common::config`)

6. [ ] Add metric staleness/expiry logic

7. [ ] Write integration tests with mock telemetry

8. [ ] Add documentation and example config

### Dependencies

```toml
[dependencies]
zensight-common = { path = "../zensight-common" }
zenoh = { version = "1.0", features = ["tokio"] }
prometheus-client = "0.22"
axum = "0.7"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json5 = "0.1"
tracing = "0.1"
tracing-subscriber = "0.3"
clap = { version = "4", features = ["derive"] }
```

## Phase 2: OpenTelemetry Exporter

### Overview

Create a more comprehensive exporter supporting the OpenTelemetry Protocol (OTLP), enabling export to any OTEL-compatible backend.

### New Crate: `zensight-exporter-otel`

```
zensight-exporter-otel/
├── Cargo.toml
├── src/
│   ├── main.rs           # Binary entry point
│   ├── lib.rs            # Library exports
│   ├── config.rs         # Configuration
│   ├── collector.rs      # Zenoh subscriber
│   ├── metrics.rs        # Metric conversion and batching
│   ├── logs.rs           # Log conversion (for syslog)
│   └── resource.rs       # Resource attribute mapping
└── tests/
    └── integration.rs
```

### Signal Mapping

| ZenSight Data | OTEL Signal | Mapping |
|---------------|-------------|---------|
| Counter/Gauge values | Metrics | Direct mapping with resource attributes |
| Syslog messages | Logs | Structured log records with severity |
| Device correlation | Resource | Device as resource with attributes |
| Error reports | Events/Logs | Error log records |

### OTEL Metrics Mapping

```rust
// TelemetryPoint -> OTEL Metric
// Resource attributes from source/protocol
// Metric data point from value

Resource {
    attributes: {
        "service.name": "zensight",
        "device.id": source,
        "telemetry.protocol": protocol,
    }
}

Metric {
    name: format!("zensight.{}.{}", protocol, metric),
    data: match value {
        Counter(v) => Sum { value: v, is_monotonic: true },
        Gauge(v) => Gauge { value: v },
        // ...
    },
    attributes: labels,
}
```

### OTEL Logs Mapping (for Syslog)

```rust
// Syslog TelemetryPoint with Text value -> OTEL LogRecord
LogRecord {
    time_unix_nano: timestamp * 1_000_000,
    severity_number: syslog_severity_to_otel(labels.get("severity")),
    severity_text: labels.get("severity"),
    body: value.as_text(),
    attributes: {
        "syslog.facility": labels.get("facility"),
        "syslog.appname": labels.get("appname"),
        "syslog.hostname": source,
    },
    resource: { /* device resource */ },
}
```

### Configuration Example

```json5
{
  zenoh: {
    mode: "peer",
    connect: ["tcp/localhost:7447"],
  },
  
  opentelemetry: {
    // OTLP exporter configuration
    endpoint: "http://localhost:4317",  // gRPC endpoint
    protocol: "grpc",                   // "grpc" or "http"
    
    // Headers for authentication
    headers: {
      "Authorization": "Bearer ${OTEL_AUTH_TOKEN}",
    },
    
    // Export settings
    batch_size: 1000,
    export_interval_secs: 10,
    timeout_secs: 30,
    
    // Signal selection
    export_metrics: true,
    export_logs: true,       // Syslog -> OTEL Logs
    export_traces: false,    // Future: correlation as traces
    
    // Resource attributes
    resource: {
      "service.name": "zensight",
      "service.version": "0.1.0",
      "deployment.environment": "production",
    },
  },
  
  // Filtering
  filters: {
    include_protocols: null,  // All protocols
    exclude_protocols: [],
    include_metrics: ["**"],
    exclude_metrics: [],
  },
}
```

### Implementation Steps

1. [ ] Create crate structure with OTEL SDK dependencies:
   - `opentelemetry` (core API)
   - `opentelemetry-otlp` (OTLP exporter)
   - `opentelemetry-sdk` (SDK implementation)

2. [ ] Implement resource attribute mapping from device/protocol info

3. [ ] Implement metric conversion (Counter/Gauge/Boolean)

4. [ ] Implement log conversion for syslog Text values

5. [ ] Implement batching and periodic export

6. [ ] Add configuration with filtering options

7. [ ] Write integration tests (use OTEL collector in Docker)

8. [ ] Add documentation and example configs

### Dependencies

```toml
[dependencies]
zensight-common = { path = "../zensight-common" }
zenoh = { version = "1.0", features = ["tokio"] }
opentelemetry = "0.24"
opentelemetry-sdk = { version = "0.24", features = ["rt-tokio"] }
opentelemetry-otlp = { version = "0.17", features = ["grpc-tonic", "http-proto"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
tracing = "0.1"
clap = { version = "4", features = ["derive"] }
```

## Phase 3: Enhanced Integration (Future)

### 3.1 Trace Correlation

Use device correlation data to create distributed traces:
- NetFlow flows as spans (src -> dst)
- SNMP poll cycles as parent spans
- Error events as span events

### 3.2 Prometheus Remote Write

Add push-based Prometheus Remote Write support for:
- Cortex
- Thanos
- Grafana Mimir
- Victoria Metrics

### 3.3 Direct Backend Integrations

Optional direct exporters for:
- InfluxDB (line protocol)
- TimescaleDB (SQL inserts)
- Datadog (API)
- CloudWatch (SDK)

## Testing Strategy

### Unit Tests

- Type conversion accuracy
- Label sanitization
- Configuration parsing
- Staleness logic

### Integration Tests

```bash
# Start test infrastructure
docker compose -f tests/docker-compose.otel.yml up -d

# Run integration tests
cargo test -p zensight-exporter-prometheus --test integration
cargo test -p zensight-exporter-otel --test integration
```

### Test Infrastructure

```yaml
# tests/docker-compose.otel.yml
services:
  prometheus:
    image: prom/prometheus:latest
    ports: ["9091:9090"]
    
  otel-collector:
    image: otel/opentelemetry-collector:latest
    ports: ["4317:4317", "4318:4318"]
    
  jaeger:
    image: jaegertracing/all-in-one:latest
    ports: ["16686:16686"]
```

## Documentation Updates

1. [ ] Update main README with exporter overview
2. [ ] Add `docs/PROMETHEUS_EXPORTER.md` with usage guide
3. [ ] Add `docs/OPENTELEMETRY_EXPORTER.md` with usage guide
4. [ ] Add example Grafana dashboards in `examples/grafana/`
5. [ ] Add example OTEL Collector config in `examples/otel/`

## Success Criteria

### Phase 1 (Prometheus)

- [ ] Exporter starts and connects to Zenoh
- [ ] `/metrics` endpoint returns valid Prometheus format
- [ ] All TelemetryValue types correctly mapped
- [ ] Prometheus can scrape and store metrics
- [ ] Grafana can visualize ZenSight data
- [ ] Staleness/expiry works correctly
- [ ] Memory usage bounded under load

### Phase 2 (OpenTelemetry)

- [ ] Metrics export to OTEL Collector works
- [ ] Syslog messages appear as OTEL Logs
- [ ] Resource attributes correctly populated
- [ ] Batching and retry logic works
- [ ] Compatible with Jaeger, Datadog, CloudWatch backends

## Risks and Mitigations

| Risk | Mitigation |
|------|------------|
| High cardinality metrics causing memory issues | Configurable max_series limit, staleness expiry |
| OTEL SDK complexity | Start with metrics only, add logs incrementally |
| Version compatibility | Pin OTEL crate versions, test against multiple collector versions |
| Performance overhead | Benchmark early, use async batching |

## Alternatives Considered

### 1. Embed Prometheus in Frontend

**Rejected**: Adds complexity to UI application, limits deployment flexibility.

### 2. Use Zenoh Storage Plugin

**Rejected**: Doesn't provide standard export formats, still requires custom querying.

### 3. OpenTelemetry Only (Skip Prometheus)

**Rejected**: Prometheus is simpler and more widely deployed for metrics-only use cases. OTEL adds value for logs/traces but has higher complexity.

## Conclusion

Adding Prometheus and OpenTelemetry export capabilities to ZenSight is a natural extension that:

1. Leverages the existing unified data model
2. Follows established patterns (Zenoh subscription)
3. Opens integration with the broader observability ecosystem
4. Enables enterprise adoption where standard tooling is required

The phased approach (Prometheus first, then OTEL) allows quick wins while building toward comprehensive observability integration.
