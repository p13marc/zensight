# zensight-sensor-gnmi — reference

Subscribes to gNMI streaming telemetry from one or more network devices and
publishes each update to Zenoh.

## Telemetry & keyspace

All keys follow the v1 grammar, `zensight/v1/<origin>/…`, where `<origin>` is
the **collector host's** stable id (`h-<12hex>`). gNMI is a *proxy producer*:
the observed device is the first subject chunk after the producer.

| Key | Payload |
|-----|---------|
| `zensight/v1/<origin>/telemetry/gnmi/<device>/<path>` | One gNMI update leaf. `<path>` mirrors the gNMI path, including path keys, e.g. `interfaces/interface[name=eth0]/state/counters/in-octets`. |

`<device>` comes from each target's `name`, which is also stamped as the point's
`source` payload field.

### Control plane (via `zensight-sensor-core`)

- `zensight/v1/<origin>/state/gnmi/health` — sensor health document (absorbs the legacy running flag)
- `zensight/v1/<origin>/state/gnmi/device/<device>/liveness` — per-device liveness document (a `…/device/<device>/alive` liveliness token is separate machinery)
- `zensight/v1/<origin>/state/gnmi/errors` — error reports
- `zensight/v1/<origin>/@rpc/gnmi/artifact/{request,cancel}` — on-demand debug report / snapshot (opt-in via `artifacts`); progress rides the `state/gnmi/artifact/<kind>` status document
- `zensight/v1/<origin>/state/gnmi/sensor` — sensor registration (`SensorInfo`)
- `zensight/v1/<origin>/state/gnmi/evidence/self` — self-reported host evidence
- `zensight/v1/<origin>/state/gnmi/alive` — sensor liveliness token
- `zensight/v1/<origin>/@rpc/gnmi/introspect` — the registry slice this build serves

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative contract.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `logging`, `artifacts`,
and `gnmi`.

### `gnmi` block

| Field | Type | Notes |
|-------|------|-------|
| `source` | string? | Override the sensor instance id used in registration/evidence payloads (default: local hostname; v1 keys are origin-scoped, so it no longer appears in key expressions). |
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
