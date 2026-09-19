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

### A silent slave is bounded, and says so (#1133)

The realistic shape of a dead PLC is not a refused connection: the TCP
handshake completes, because the kernel answers that, and then nothing comes
back. `timeout_ms` used to apply to `tcp::connect_slave` and to nothing else,
and `run()` awaits the poll — so the first read against such a slave stopped
that device **permanently**, with `alive` still declared, because the sensor's
liveliness token is about the process and not about the device.

Now:

- every read is wrapped in `timeout_ms`, on TCP **and** RTU;
- `retries` is honoured, and bounds the cycle at `timeout_ms × (retries + 1)`
  per register. A **timeout** is not retried on the same connection: the slave
  accepted the socket and did not answer, and asking again down the same
  half-open pipe buys another `timeout_ms` and the same silence. An error that
  came back *as* an error — an exception response, a framing fault — is;
- a cycle in which **no** register answered is a failed cycle, not an empty
  one, and reaches the bus through the framework's per-device liveness. The
  `state/modbus/device/<device>/liveness` document this page has advertised
  since the crate existed had **no publisher** until #1133: `grep liveness
  src/` came back empty;
- the connection is reused across polls and dropped after a failed cycle. One
  fresh TCP connection per poll with no `disconnect()` is how a PLC runs out —
  they cap concurrent connections at four to eight.

A register block larger than the protocol allows is refused **at startup**:
125 registers for holding/input, 2 000 bits for coils/discrete. Above it the
read is not slow, it is illegal — the slave answers with an exception, or a
permissive stack truncates and this sensor publishes a short window under the
configured names.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `serialization`,
`logging`, `artifacts`, and `modbus`.

`serialization` is the shared vocabulary's, CBOR by default. This sensor
hard-coded JSON and offered no knob at all until #1133, so a deployment that
set it got JSON from this one producer with nothing to say so.

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
| `timeout_ms` | u64 | Per-request timeout — the connect **and** every read (#1133). |
| `retries` | u32 | **Retries**, not attempts: `2` is up to three reads. A read that came back as an *error* is retried; one that came back as silence is not — see below. |
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
| `count` | u16 | Number of **values** to decode, not registers. A block spans `count × registers_per_value`, and a 32-bit type is two registers per value — `{address: 100, count: 3, data_type: "f32"}` reads 100–105 and publishes at 100, 102, 104. |
| `name` | string? | Metric name in the key. **Refused with `count > 1`** — a name names one value (#1073). For a multi-value block, drop it and use `register_names`, which is keyed by address. |
| `data_type` | enum | `u16`/`i16`/`u32`/`i32`/`f32` and the little-endian `u32le`/`i32le`/`f32le`. |
| `scale` | f64 | Multiplier applied to the raw value (default 1.0). |
| `offset` | f64 | Added after scaling (default 0.0). |
| `unit` | string? | Engineering unit label (e.g. `°C`, `bar`). |

## Build / run notes & caveats

- No special build headers required.
- **RTU:** the process needs access to the serial device (`/dev/ttyUSB0`, etc.);
  match `baud_rate`/`parity`/`stop_bits`/`unit_id` to the device or reads fail
  (CRC / illegal-data-address errors).
- 32-bit values span two 16-bit registers. `count` is values, so the block's
  span is `count × 2` and consecutive values are **two apart** in the address
  space — the `address` label, and any `register_names` lookup, follow that
  stride (#1073).
- There is no `bool` and no `f64` in `data_type`. Coils and discrete inputs are
  read by `type` and decode straight to a boolean without consulting
  `data_type`; 64-bit values are not implemented. Both were listed here for
  three releases and neither existed.
- **A `name` with `count > 1` is refused at startup.** It used to be returned
  for every decoded value in the block, so ten sensors published to one key, ten
  times a cycle, and nine were lost.
