# zensight-sensor-netflow — reference

Collects NetFlow v5/v7/v9 and IPFIX flow records from exporters over UDP and
publishes each flow record to Zenoh. Template-based versions (v9/IPFIX) are
decoded against templates cached per exporter.

## Telemetry & keyspace

All keys follow the v1 grammar, `zensight/v1/<origin>/…`, where `<origin>` is
the **receiver host's** stable id (`h-<12hex>`). NetFlow is a *proxy producer*:
the observed exporter is the first subject chunk after the producer.

**Per-exporter rollups, never per-flow keys.** A key per conversation is the
unbounded population RFC 04 §1.2 forbids — one series per `(src, dst)` pair on a
transit link is millions. The registry budgets this producer to
`{exporter}/{metric...}`, and the raw records stay available as pull-only detail.

| Key | Value | Notes |
|-----|-------|-------|
| `…/telemetry/netflow/<exporter>/flows_total` | `Counter` | Flow records received. Never scaled — the exporter really did report one flow, and inventing the ones it sampled away would be a different lie. |
| `…/telemetry/netflow/<exporter>/bytes_total` | `Counter` | Octets, **scaled by the sampling interval** when the exporter declared one. Carries `sampling` and `sampled` labels when it did (#1075). |
| `…/telemetry/netflow/<exporter>/packets_total` | `Counter` | Packets, same scaling and labels. |
| `…/telemetry/netflow/<exporter>/by_proto/<proto>/flows` | `Counter` | Flows per L4 protocol name (`tcp`, `udp`, `icmp`, `proto_<n>`). |

`<exporter>` is the exporting device's source IP, slugged one chunk (`.`/`:` →
`-`) and mapped through `exporter_names` when a friendly name is configured. The
point's `source` is the exporter; `origin` is the receiving host.

The rollup period is `aggregation_interval_secs`, and `publish_stats` gates it.

### Sampling (#1075)

An exporter configured `1-out-of-N` reports one flow in N, and the octets it
reports are a sample of the octets that crossed. The interval is declared **out
of band** — in the v5 header's low 14 bits (above a 2-bit mode), or in a v9 /
IPFIX **options template** — so it is remembered per exporter and applied to
every subsequent record.

Three states, and they are different claims:

| `sampling` label | `sampled` label | Means |
|---|---|---|
| absent | absent | The exporter never declared an interval. The counters are raw, and a reader has reason to doubt them. |
| `1` | `false` | It declared that it is not sampling. |
| `N` | `true` | It declared `1 in N`; the counters are scaled by `N`. |

Before this, nothing read any of the three declarations: a router at 1-in-1000
published `bytes_total` at a **thousandth of throughput** as a plain `Counter`,
and a consumer rating it was three orders of magnitude low with nothing on the
wire to say so.

### Field resolution (#1072)

The rollup reads three semantic names — `bytes`, `packets`, `protocol` — and
every version reaches them. The v9 and IPFIX parsers used to mint field names
from the parser library's `Debug` rendering, which gives `inbytes`/`inpkts` for
v9 and `iana(octetdeltacount)` for IPFIX, so on the only two versions anyone
deploys `bytes_total` and `packets_total` stayed at **zero forever** while
`flows_total` counted correctly — the shape that makes an exporter look healthy.
`src/fields.rs` resolves the semantics from the **typed enum variants**, so a
library rename is a build failure there rather than a silent renaming of every
key this sensor publishes. The raw names are still carried on the record, which
is what `@rpc/netflow/flows` serves.

### Control plane (via `zensight-sensor-core`)

- `zensight/v1/<origin>/state/netflow/health` — sensor health document (absorbs the legacy running flag)
- `zensight/v1/<origin>/state/netflow/errors` — error reports
- `zensight/v1/<origin>/@rpc/netflow/artifact/{request,cancel}` — on-demand debug report / snapshot (opt-in via `artifacts`); progress rides the `state/netflow/artifact/<kind>` status document
- `zensight/v1/<origin>/state/netflow/sensor` — sensor registration (`SensorInfo`)
- `zensight/v1/<origin>/state/netflow/evidence/self` — self-reported host evidence
- `zensight/v1/<origin>/state/netflow/alive` — sensor liveliness token
- `zensight/v1/<origin>/@rpc/netflow/flows` — the bounded ring of recent raw flow
  records (`?exporter=…;max=…`, newest first, default 500 of 2048 held). This is
  where the per-flow detail lives; it is pulled, never streamed. Gated by
  `publish_flows`.
- `zensight/v1/<origin>/@rpc/netflow/introspect` — the registry slice this build serves

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
| `publish_flows` | bool | Serve the `@rpc/netflow/flows` detail ring (default true). It does **not** publish per-flow telemetry keys; nothing does. |
| `publish_stats` | bool | Publish the per-exporter rollups (default true). |
| `aggregation_interval_secs` | u64 | Seconds between rollup publications. |

## Build / run notes & caveats

- No special build headers required.
- Point the exporting devices' flow-export destination at the sensor's
  `listeners` bind address/port. Binding ports below 1024 needs elevated
  privileges; the example ports (2055/4739/9995) are unprivileged.
- **Exporters are capped.** NetFlow is UDP with no handshake and the source
  address is whatever the datagram says, so the per-exporter parser map — each
  with its own template cache — is bounded and evicts least-recently-seen past
  the cap. A real exporter re-sends its templates within its refresh interval,
  which is the protocol's own recovery. The sampling registry is bounded the
  same way.
- v9 and IPFIX are **stateful**: a data record cannot be decoded before its
  template arrives. A sensor restarted mid-stream sees nothing from an exporter
  until its next template refresh.
