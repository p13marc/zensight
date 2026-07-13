# zensight-common

The shared model library for the ZenSight observability platform. Every sensor,
exporter, the correlator, and the frontend depend on it for a single source of
truth: the wire types, the QoS profiles, and the key-expression contract they all
speak.

## What's in here

- **Telemetry model** — `TelemetryPoint`, `Protocol`, `TelemetryValue`
  (`Counter` / `Gauge` / `Text` / `Boolean` / `Binary`) with labels.
- **Alert & RPC model** — `Alert{Kind,Severity,State}` + `alert_key` (LWW alert
  state documents), and the `@rpc` procedure key builders (`command_key` /
  `status_key` / `query_key` — writes are `<topic>/set` GETs, reads are `<topic>`).
- **Identity, evidence & entities** — `HostEvidence`, `NameObservation`,
  `HostEntity` / `MemberClaim` / `NameVal`: the sensor-published evidence that the
  catalog (correlator) fuses into one entity per physical host.
- **Artifact channel** — `ArtifactRequest` / `ArtifactKind` / `ArtifactStatus` /
  `Delivery` wire types for on-demand large-data transfer (report / snapshot /
  capture) over `zenoh-blob` on the `@blob` plane.
- **QoS** — `QosClass`, the per-traffic-class Zenoh profile (telemetry drops,
  control blocks) tuned for low-bandwidth / unreliable links.
- **Keyspace helpers** — consumer-side v1 selectors and keys in `keyexpr.rs`
  (class wildcards, fleet/origin `@rpc` selectors, `@catalog` entity/pdns keys);
  producer-side keys come from `zensight_keyspace::V1Context`.
- **Serialization** — JSON / CBOR `encode` / `decode`, with first-byte-sniffing
  `decode_auto` so JSON and CBOR senders interoperate on the wire.
- **Config & session** — JSON5 `load_config`, `ZenohConfig`, and the `connect`
  session helper.

## Quick start

This is a library crate — it has no binary. Add it as a path dependency:

```toml
[dependencies]
zensight-common = { path = "../zensight-common" }
```

```rust
use zensight_common::{TelemetryPoint, TelemetryValue, Protocol};

let point = TelemetryPoint::new(
    "router01",
    Protocol::Snmp,
    "system/sysUpTime",
    TelemetryValue::Counter(123_456),
)
.with_label("oid", "1.3.6.1.2.1.1.3.0");
```

Run the tests:

```bash
cargo test -p zensight-common
```

## Documentation

- [Data model](docs/data-model.md) — telemetry, alerts, RPC, serialization, QoS.
- [Identity, evidence & entities](docs/identity-evidence.md) — the evidence → entity pipeline.
- [Keyspace helpers](docs/keyspace-helpers.md) — index of the `keyexpr.rs` / `command.rs` builders.
- [`../docs/KEYSPACE.md`](../docs/KEYSPACE.md) — the deployed keyspace profile
  (normative spec: [`../docs/rfcs/keyspace-v2/`](../docs/rfcs/keyspace-v2/00-index.md)).

## License

MIT OR Apache-2.0
