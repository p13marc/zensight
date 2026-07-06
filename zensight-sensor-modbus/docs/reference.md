# zensight-sensor-modbus — reference

Polls Modbus TCP and RTU/serial devices, decoding raw registers into typed,
scaled values from a per-device register map.

## Telemetry & keyspace

All keys are rooted at `key_prefix` (default `zensight/modbus`).

| Key | Payload |
|-----|---------|
| `zensight/modbus/<device>/<register_type>/<register>` | Decoded register value. `<register_type>` is `coil`, `discrete`, `input`, or `holding`; `<register>` is the register's configured `name` (falls back to a `register_names` map entry, else the raw address). |

Example: `zensight/modbus/plc01/holding/temperature`. Each point carries labels
including `register_type`, `address`, `unit_id`, and `data_type`.

`<device>` comes from each device's `name`; the point `source` defaults to the
local hostname unless `modbus.source` is set.

### Control plane (via `zensight-sensor-core`)

- `zensight/modbus/@/health` — sensor health snapshots
- `zensight/modbus/@/devices/*/liveness` — per-device liveness
- `zensight/modbus/@/errors` — error reports
- `zensight/modbus/@/artifact/{request,status,cancel}` — on-demand debug report / snapshot (opt-in via `artifacts`)
- `zensight/_meta/sensors/modbus/<source>` — sensor registration (`SensorInfo`)
- `zensight/_meta/evidence/host/modbus/<source>` — self-reported host evidence

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative contract.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `logging`, `artifacts`,
and `modbus`.

### `modbus` block

| Field | Type | Notes |
|-------|------|-------|
| `key_prefix` | string | Key-expression root (default `zensight/modbus`). |
| `source` | string? | Override the agent-host source id (default: local hostname). |
| `devices[]` | array | Devices to poll (see below). |
| `register_groups` | map | Named, reusable register lists referenced by `device.register_group`. |
| `register_names` | map | `"<type>:<address>"` → friendly name (e.g. `"holding:100": "motor_speed"`). |

### `devices[]`

| Field | Type | Notes |
|-------|------|-------|
| `name` | string | Device id used in key expressions. |
| `connection` | object | Transport (see below). |
| `unit_id` | u8 | Modbus slave/unit id (1–247). |
| `poll_interval_secs` | u64 | Polling cadence. |
| `timeout_ms` | u64 | Per-request timeout. |
| `retries` | u32 | Retry count on failure. |
| `registers[]` | array | Inline register definitions. |
| `register_group` | string? | Reference a predefined `register_groups` entry instead of inline `registers`. |

### `connection`

- **TCP:** `{ type: "tcp", host, port }` (default port 502).
- **RTU:** `{ type: "rtu", port: "/dev/ttyUSB0", baud_rate, data_bits, parity: "none"|"even"|"odd", stop_bits }`.

### `registers[]`

| Field | Type | Notes |
|-------|------|-------|
| `type` | enum | `coil` (FC01), `discrete` (FC02), `holding` (FC03), `input` (FC04). |
| `address` | u16 | Starting register address. |
| `count` | u16 | Number of registers to read. |
| `name` | string? | Metric name used in the key expression. |
| `data_type` | enum | `bool`/`u16`/`i16`/`u32`/`i32`/`f32`/`f64` (word count must match). |
| `scale` | f64 | Multiplier applied to the raw value (default 1.0). |
| `offset` | f64 | Added after scaling (default 0.0). |
| `unit` | string? | Engineering unit label (e.g. `°C`, `bar`). |

## Build / run notes & caveats

- No special build headers required.
- **RTU:** the process needs access to the serial device (`/dev/ttyUSB0`, etc.);
  match `baud_rate`/`parity`/`stop_bits`/`unit_id` to the device or reads fail
  (CRC / illegal-data-address errors).
- 32/64-bit values span multiple 16-bit registers; ensure `count` matches
  `data_type` and the device's word/byte ordering.
