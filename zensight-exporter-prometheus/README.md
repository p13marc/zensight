# ZenSight Prometheus Exporter

Prometheus metrics exporter for ZenSight telemetry. Subscribes to telemetry over Zenoh and exposes metrics via an HTTP `/metrics` endpoint for Prometheus scraping.

## Features

- **Prometheus-native**: Standard `/metrics` endpoint compatible with Prometheus scrapers
- **Type mapping**: Counter, Gauge, Boolean values exported as Prometheus metrics
- **Info metrics**: Text values exported as Prometheus info metrics
- **Filtering**: Filter by protocol, source, or metric patterns (glob)
- **Staleness handling**: Automatic expiry of stale metrics
- **Memory protection**: Configurable max series limit
- **Health endpoints**: `/health` and `/ready` for Kubernetes probes

## Installation

```bash
cargo build -p zensight-exporter-prometheus --release
```

## Usage

```bash
# With config file
zensight-exporter-prometheus --config configs/prometheus-exporter.json5

# Override listen address
zensight-exporter-prometheus --config config.json5 --listen 0.0.0.0:9091

# Debug logging
zensight-exporter-prometheus --config config.json5 --log-level debug
```

## Configuration

Create a JSON5 configuration file:

```json5
{
  // Zenoh connection
  zenoh: {
    mode: "peer",                    // "client", "peer", or "router"
    connect: ["tcp/localhost:7447"], // For client mode
  },

  // Prometheus settings
  prometheus: {
    listen: "0.0.0.0:9090",   // HTTP listen address
    path: "/metrics",          // Metrics endpoint path
    prefix: "zensight",        // Metric name prefix
    default_labels: {          // Labels added to all metrics
      environment: "production",
    },
  },

  // Aggregation settings
  aggregation: {
    stale_timeout_secs: 300,   // Remove metrics after 5 min without updates
    max_series: 100000,        // Memory protection limit
    cleanup_interval_secs: 60, // How often to run cleanup
  },

  // Filtering (optional)
  filters: {
    include_protocols: ["snmp", "sysinfo"],  // Only these protocols
    exclude_metrics: ["**/debug/**"],         // Glob patterns to exclude
  },
}
```

## Metric Naming

Metrics are named as `{prefix}_{protocol}_{metric_path}`:

| ZenSight Metric | Prometheus Metric |
|-----------------|-------------------|
| `snmp/router01/sysUpTime` | `zensight_snmp_sysUpTime{source="router01"}` |
| `sysinfo/server01/cpu/usage` | `zensight_sysinfo_cpu_usage{source="server01"}` |

## Type Mapping

| ZenSight Value | Prometheus Type |
|----------------|-----------------|
| `Counter(u64)` | Counter |
| `Gauge(f64)` | Gauge |
| `Boolean(bool)` | Gauge (0/1) |
| `Text(String)` | Info metric |
| `Binary(Vec<u8>)` | Not exported |

## Endpoints

| Endpoint | Description |
|----------|-------------|
| `/metrics` | Prometheus metrics (configurable path) |
| `/health` | Always returns 200 OK |
| `/ready` | Returns 200 after receiving telemetry |

## Prometheus Configuration

Add to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'zensight'
    static_configs:
      - targets: ['localhost:9090']
    scrape_interval: 15s
```

## Grafana Dashboard

The exporter also exposes internal metrics:

- `zensight_exporter_series_total` - Current number of time series
- `zensight_exporter_points_received_total` - Total telemetry points received
- `zensight_exporter_points_accepted_total` - Points that passed filters
- `zensight_exporter_points_filtered_total` - Points rejected by filters

## License

MIT
