# zensight-sensor-netflow — reference

Collects NetFlow v5/v7/v9 and IPFIX flow records from exporters over UDP and
publishes each flow record to Zenoh. Template-based versions (v9/IPFIX) are
decoded against templates cached per exporter.

## Telemetry & keyspace

All keys are rooted at `key_prefix` (default `zensight/netflow`).

| Key | Payload |
|-----|---------|
| `zensight/netflow/<exporter>/<src_ip>/<dst_ip>/<proto>` | One per-conversation flow record. IP addresses are slugified in the key (`.`/`:` → `_`), e.g. `192_168_1_1`; `<proto>` is the resolved protocol name (e.g. `tcp`). The point value is the flow's byte count. |

`<exporter>` is the source IP of the exporting device, mapped through
`exporter_names` when a friendly name is configured. Flow fields are carried as
labels: `version`, `src_ip`, `dst_ip`, `src_port`, `dst_port`, `protocol`,
`packets`, `bytes`, and timing fields.

The point `source` defaults to the local hostname unless `netflow.source` is set.

### Control plane (via `zensight-sensor-core`)

- `zensight/netflow/<source>/@/health` — sensor health snapshots (host-scoped)
- `zensight/netflow/@/errors` — error reports
- `zensight/netflow/@/artifact/{request,status,cancel}` — on-demand debug report / snapshot (opt-in via `artifacts`)
- `zensight/_meta/sensors/netflow/<source>` — sensor registration (`SensorInfo`)
- `zensight/_meta/evidence/host/netflow/<source>` — self-reported host evidence

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative contract.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `serialization`
(`json`|`cbor`), `logging`, `artifacts`, and `netflow`.

### `netflow` block

| Field | Type | Notes |
|-------|------|-------|
| `key_prefix` | string | Key-expression root (default `zensight/netflow`). |
| `source` | string? | Override the agent-host source id (default: local hostname). |
| `listeners[]` | array | UDP listeners; each `{ bind, max_packet_size? }`. Common ports: 2055 (NetFlow), 4739 (IPFIX), 9995 (alt). `max_packet_size` defaults to 65535. |
| `exporter_names` | map | Exporter IP → friendly name, used in the `<exporter>` key segment. |
| `publish_flows` | bool | Publish individual flow records (default true). |
| `publish_stats` | bool | Enable aggregate statistics (default true). |
| `aggregation_interval_secs` | u64 | Aggregation window in seconds; `0` = no aggregation. |

## Build / run notes & caveats

- No special build headers required.
- Point the exporting devices' flow-export destination at the sensor's
  `listeners` bind address/port. Binding ports below 1024 needs elevated
  privileges; the example ports (2055/4739/9995) are unprivileged.
- **Current behaviour:** only per-flow telemetry (`publish_flows`) is published to
  Zenoh. `publish_stats` / `aggregation_interval_secs` are accepted config fields
  but aggregate/stats publishing is not yet wired up (flow counts are logged, not
  emitted as telemetry).
