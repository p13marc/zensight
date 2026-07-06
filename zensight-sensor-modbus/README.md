# zensight-sensor-modbus

Polls Modbus devices over TCP or RTU/serial and publishes register values as
ZenSight telemetry over Zenoh. Reads coils, discrete inputs, holding registers,
and input registers, decoding raw words into typed/scaled values from a
per-device register map. Runs unprivileged (serial access needs read/write on
the `/dev/tty*` device).

## Quick start

```bash
cargo build -p zensight-sensor-modbus --release

# Run against the example config (Modbus TCP + RTU devices)
cargo run -p zensight-sensor-modbus --release -- --config configs/modbus.json5
```

## Configuration

JSON5. Define the Zenoh connection plus per-device connection (`tcp`/`rtu`),
`unit_id`, poll interval, and a register map with type/address/count, decoded
`data_type`, and optional `scale`/`offset`/`unit`. Register groups are reusable
across devices. See [`configs/modbus.json5`](../configs/modbus.json5) for a
fully-commented example and [docs/reference.md](docs/reference.md) for the
field-by-field breakdown.

## Documentation

- [Telemetry & keyspace](docs/reference.md) — what it publishes, config fields, notes
- Canonical keyspace contract: [../docs/KEYSPACE.md](../docs/KEYSPACE.md)
