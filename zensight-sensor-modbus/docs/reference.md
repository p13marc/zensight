# zensight-sensor-modbus — reference

Polls Modbus TCP and RTU/serial devices, decoding raw registers into typed,
scaled values from a per-device register map.

## Telemetry & keyspace

All keys follow the v1 grammar, `zensight/v1/<origin>/…`, where `<origin>` is
the **poller host's** stable id (`h-<12hex>`). Modbus is a *proxy producer*: the
observed device is the first subject chunk after the producer.

| Key | Payload |
|-----|---------|
| `zensight/v1/<origin>/telemetry/modbus/<device>/<register_type>/<register>` | Decoded register value. `<register_type>` is `coil`, `discrete`, `input`, or `holding`; `<register>` is the register's configured `name` (falls back to a `register_names` map entry, else the raw address). |

Example: `zensight/v1/h-3fa9c2d41b7e/telemetry/modbus/plc01/holding/temperature`.
Each point carries labels including `register_type`, `address`, `unit_id`, and
`data_type`.

`<device>` comes from each device's `name`; the point `source` payload field
defaults to the local hostname unless `modbus.source` is set.

### Control plane (via `zensight-sensor-core`)

- `zensight/v1/<origin>/state/modbus/health` — sensor health document (absorbs the legacy running flag)
- `zensight/v1/<origin>/state/modbus/device/<device>/liveness` — per-device liveness document (a `…/device/<device>/alive` liveliness token is separate machinery)
- `zensight/v1/<origin>/state/modbus/errors` — error reports
- `zensight/v1/<origin>/@rpc/modbus/artifact/{request,cancel}` — on-demand debug report / snapshot (opt-in via `artifacts`); progress rides the `state/modbus/artifact/<kind>` status document
- `zensight/v1/<origin>/state/modbus/sensor` — sensor registration (`SensorInfo`)
- `zensight/v1/<origin>/state/modbus/evidence/self` — self-reported host evidence
- `zensight/v1/<origin>/state/modbus/alive` — sensor liveliness token
- `zensight/v1/<origin>/@rpc/modbus/introspect` — the registry slice this build serves

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative contract.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `logging`, `artifacts`,
and `modbus`.

### `modbus` block

| Field | Type | Notes |
|-------|------|-------|
| `source` | string? | Override the agent-host source id in payloads (default: local hostname; v1 keys are origin-scoped, so it no longer appears in key expressions). |
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
