# zensight-sensor-gnmi

Subscribes to gNMI (gRPC Network Management Interface) streaming telemetry from
network devices and publishes it as ZenSight telemetry over Zenoh. Supports
SAMPLE / ON_CHANGE / TARGET_DEFINED subscriptions, JSON / JSON_IETF / PROTO
encodings, and TLS/mTLS. Runs unprivileged.

## Quick start

```bash
# Build (needs `protoc` on PATH at build time — see reference)
cargo build -p zensight-sensor-gnmi --release

# Run against the example config
cargo run -p zensight-sensor-gnmi --release -- --config configs/gnmi.json5
```

## Configuration

JSON5. Define the Zenoh connection plus per-target `address`, optional
`credentials` and `tls`, wire `encoding`, and a list of `subscriptions` (gNMI
path + mode + interval). See [`configs/gnmi.json5`](../configs/gnmi.json5) for a
fully-commented example and [docs/reference.md](docs/reference.md) for the
field-by-field breakdown.

## Documentation

- [Telemetry & keyspace](docs/reference.md) — what it publishes, config fields, build caveats
- Canonical keyspace contract: [../docs/KEYSPACE.md](../docs/KEYSPACE.md)
