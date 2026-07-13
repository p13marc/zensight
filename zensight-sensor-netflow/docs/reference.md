# zensight-sensor-netflow — reference

Collects NetFlow v5/v7/v9 and IPFIX flow records from exporters over UDP and
publishes each flow record to Zenoh. Template-based versions (v9/IPFIX) are
decoded against templates cached per exporter.

## Telemetry & keyspace

All keys follow the v1 grammar, `zensight/@v1/<origin>/…`, where `<origin>` is
the **receiver host's** stable id (`h-<12hex>`). NetFlow is a *proxy producer*:
the observed exporter is the first subject chunk after the producer.

| Key | Payload |
|-----|---------|
| `zensight/@v1/<origin>/telemetry/netflow/<exporter>/<src_ip>/<dst_ip>` | One per-conversation flow record. IP addresses are slugified in the key (`.`/`:` → `_`), e.g. `192_168_1_1`; the point's `metric` field is `<src>/<dst>/<proto>` with the resolved protocol name (e.g. `tcp`). The point value is the flow's byte count. |

`<exporter>` is the source IP of the exporting device, mapped through
`exporter_names` when a friendly name is configured. Flow fields are carried as
labels: `version`, `src_ip`, `dst_ip`, `src_port`, `dst_port`, `protocol`,
`packets`, `bytes`, and timing fields.

The point `source` payload field defaults to the local hostname unless
`netflow.source` is set.

### Control plane (via `zensight-sensor-core`)

- `zensight/@v1/<origin>/state/netflow/health` — sensor health document (absorbs the legacy running flag)
- `zensight/@v1/<origin>/state/netflow/errors` — error reports
- `zensight/@v1/<origin>/@rpc/netflow/artifact/{request,cancel}` — on-demand debug report / snapshot (opt-in via `artifacts`); progress rides the `state/netflow/artifact/<kind>` status document
- `zensight/@v1/<origin>/state/netflow/sensor` — sensor registration (`SensorInfo`)
- `zensight/@v1/<origin>/state/netflow/evidence/self` — self-reported host evidence
- `zensight/@v1/<origin>/state/netflow/alive` — sensor liveliness token
- `zensight/@v1/<origin>/@rpc/netflow/introspect` — the registry slice this build serves

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative contract.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `serialization`
(`json`|`cbor`), `logging`, `artifacts`, and `netflow`.

### `netflow` block

| Field | Type | Notes |
|-------|------|-------|
| `source` | string? | Override the agent-host source id in payloads (default: local hostname; v1 keys are origin-scoped, so it no longer appears in key expressions). |
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
  emitted as telemetry). The registry's redesigned shape for this producer —
  bounded per-exporter rollups (`<exporter>/flows_per_second`, top talkers, …)
  plus an on-demand `@rpc/netflow/flows` detail procedure (`?src=;dst=;max=`) —
  is not implemented yet either; the sensor still publishes the per-flow-pair
  keys above.
