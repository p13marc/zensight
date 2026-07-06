# zensight-sensor-netflow

Receives network flow records exported by routers/switches — NetFlow v5, v7, v9,
and IPFIX (NetFlow v10) — over UDP, and publishes each flow as ZenSight telemetry
over Zenoh. Template-based versions (v9/IPFIX) are decoded against cached
templates learned from the exporter. Runs unprivileged as long as the listener
ports are unprivileged (the example uses 2055/4739).

## Quick start

```bash
cargo build -p zensight-sensor-netflow --release

# Run against the example config (UDP 2055 + 4739 listeners)
cargo run -p zensight-sensor-netflow --release -- --config configs/netflow.json5
```

## Configuration

JSON5. Define the Zenoh connection plus one or more UDP `listeners`, an optional
`exporter_names` map (IP → friendly name), and publish toggles. See
[`configs/netflow.json5`](../configs/netflow.json5) for a fully-commented example
and [docs/reference.md](docs/reference.md) for the field-by-field breakdown.

## Documentation

- [Telemetry & keyspace](docs/reference.md) — what it publishes, config fields, notes
- Canonical keyspace contract: [../docs/KEYSPACE.md](../docs/KEYSPACE.md)
