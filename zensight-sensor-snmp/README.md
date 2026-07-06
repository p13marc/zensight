# zensight-sensor-snmp

Polls SNMP agents (v1/v2c/v3) and optionally receives SNMP traps, publishing the
results as ZenSight telemetry over Zenoh. OIDs are resolved to human-readable
metric names via a configurable OID map (and, optionally, loaded MIB files).
Runs unprivileged, but the trap listener defaults to UDP 162 (a privileged port).

## Quick start

```bash
# Build (needs OpenSSL / net-snmp headers at build time — see reference)
cargo build -p zensight-sensor-snmp --release

# Run against the example config
cargo run -p zensight-sensor-snmp --release -- --config configs/snmp.json5
```

## Configuration

JSON5. Define the Zenoh connection, per-device poll settings (address, community
or v3 security, OIDs/walks), OID groups, and the OID→name map. See
[`configs/snmp.json5`](../configs/snmp.json5) for a fully-commented example and
[docs/reference.md](docs/reference.md) for the field-by-field breakdown.

## Documentation

- [Telemetry & keyspace](docs/reference.md) — what it publishes, config fields, build caveats
- Canonical keyspace contract: [../docs/KEYSPACE.md](../docs/KEYSPACE.md)
