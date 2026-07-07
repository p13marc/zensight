# zensight-sensor-gnmi — reference

Subscribes to gNMI streaming telemetry from one or more network devices and
publishes each update to Zenoh.

## Telemetry & keyspace

All keys are rooted at `key_prefix` (default `zensight/gnmi`).

| Key | Payload |
|-----|---------|
| `zensight/gnmi/<device>/<path>` | One gNMI update leaf. `<path>` mirrors the gNMI path, including path keys, e.g. `interfaces/interface[name=eth0]/state/counters/in-octets`. |

`<device>` comes from each target's `name`. The full gNMI path is also carried in
the `path` label. The point `source` defaults to the local hostname unless
`gnmi.source` is set.

### Control plane (via `zensight-sensor-core`)

- `zensight/gnmi/<source>/@/health` — sensor health snapshots (host-scoped)
- `zensight/gnmi/@/devices/*/liveness` — per-device liveness
- `zensight/gnmi/@/errors` — error reports
- `zensight/gnmi/@/artifact/{request,status,cancel}` — on-demand debug report / snapshot (opt-in via `artifacts`)
- `zensight/_meta/sensors/gnmi/<source>` — sensor registration (`SensorInfo`)
- `zensight/_meta/evidence/host/gnmi/<source>` — self-reported host evidence

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative contract.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `logging`, `artifacts`,
and `gnmi`.

### `gnmi` block

| Field | Type | Notes |
|-------|------|-------|
| `key_prefix` | string | Key-expression root (default `zensight/gnmi`). |
| `source` | string? | Override the agent-host source id (default: local hostname). |
| `serialization` | enum | Telemetry encoding: `json` or `cbor`. |
| `targets[]` | array | Devices to subscribe to (see below). |

### `targets[]`

| Field | Type | Notes |
|-------|------|-------|
| `name` | string | Device id used in key expressions. |
| `address` | string | gRPC endpoint `host:port` (e.g. `192.168.1.1:9339`). |
| `credentials` | object? | `{ username, password }` for password auth. |
| `tls` | object | TLS settings (see below); disabled unless `enabled`. |
| `encoding` | enum | gNMI wire encoding: `JSON`, `JSON_IETF`, `PROTO`, `ASCII`. |
| `subscriptions[]` | array | Paths to subscribe to (see below). |

### `tls`

`enabled` (bool), `skip_verify` (bool — dev only), optional `ca_cert`,
`client_cert`, `client_key` (paths; the latter two enable mTLS).

### `subscriptions[]`

| Field | Type | Notes |
|-------|------|-------|
| `path` | string | gNMI/OpenConfig path (wildcards allowed, e.g. `[name=*]`). |
| `mode` | enum | `SAMPLE`, `ON_CHANGE`, or `TARGET_DEFINED`. |
| `sample_interval_ms` | u64 | Sampling interval for `SAMPLE` mode. |
| `suppress_redundant` | bool | Suppress unchanged samples. |
| `heartbeat_interval_ms` | u64 | Force periodic updates even when suppressed. |

## Build / run notes & caveats

- **Build dependency:** the gRPC/protobuf stack requires `protoc` (Protocol
  Buffers compiler) on `PATH` at build time. Without it the build fails.
- Enable gNMI/gRPC on the target device (common ports 9339, 6030, 50051) and
  ensure the user has gNMI/telemetry permissions.
- `skip_verify: true` disables server-certificate validation — development only.
- The `artifacts.report.redact_extra` list (see the example config) can add extra
  keys — e.g. `username` — to the debug-bundle redaction set.
