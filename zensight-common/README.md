# zensight-common

The shared model library for the ZenSight observability platform. Every sensor,
exporter, the correlator, and the frontend depend on it for a single source of
truth: the wire types, the QoS profiles, and the key-expression contract they all
speak.

## What's in here

- **Telemetry model** — `TelemetryPoint`, `Protocol`, `TelemetryValue`
  (`Counter` / `Gauge` / `Text` / `Boolean` / `Binary`) with labels.
- **Alert & command model** — `Alert{Kind,Severity,State}` + `alert_key`, and the
  `@/commands` / `@/status` / `@/query` control channels.
- **Identity, evidence & entities** — `HostEvidence`, `NameObservation`,
  `HostEntity` / `MemberClaim` / `NameVal`: the sensor-published evidence that the
  correlator fuses into one entity per physical host.
- **Artifact channel** — `ArtifactRequest` / `ArtifactKind` / `ArtifactStatus` /
  `Delivery` wire types for on-demand large-data transfer (report / snapshot /
  capture) over `zenoh-blob`.
- **QoS** — `QosClass`, the per-traffic-class Zenoh profile (telemetry drops,
  control blocks) tuned for low-bandwidth / unreliable links.
- **Keyspace helpers** — builders in `keyexpr.rs` for every telemetry, `@/`
  control, `_meta` and `@`-verbatim (`@media` / `@pdns`) key.
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

- [Data model](docs/data-model.md) — telemetry, alerts, commands, serialization, QoS.
- [Identity, evidence & entities](docs/identity-evidence.md) — the evidence → entity pipeline.
- [Keyspace helpers](docs/keyspace-helpers.md) — index of the `keyexpr.rs` builders.
- [`../docs/KEYSPACE.md`](../docs/KEYSPACE.md) — the authoritative, full key-expression contract.

## License

MIT OR Apache-2.0
