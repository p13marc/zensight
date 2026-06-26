# ZenSight Zenoh Keyspace Reference

This is the canonical reference for every Zenoh key expression ZenSight uses.
All sensors, exporters, and the frontend follow these conventions; new code MUST
build keys through the shared helpers listed in [§7](#7-key-building-helpers)
rather than ad-hoc `format!()`.

The single root is `zensight/`. Everything below it is either **telemetry**
(`zensight/<protocol>/<source>/…`), **control-plane** for one sensor
(`zensight/<protocol>/@/…`), or cross-sensor **metadata** (`zensight/_meta/…`).

> **`@` is special in Zenoh.** A key chunk starting with `@` is matched
> *verbatim*: the wildcards `*` and `**` do **not** cross into it. So
> `zensight/snmp/**` matches telemetry but **not** `zensight/snmp/@/alerts/…`.
> Control-plane consumers must name the `@/…` keyspace explicitly (see
> [§5](#5-wildcards--subscriptions)).

---

## 1. Protocols

`<protocol>` is one of the sensor protocols. Each sensor owns the subtree
`zensight/<protocol>/`. The default `key_prefix` is `zensight/<protocol>`.

| Protocol | Sensor crate | Source identifier |
|----------|--------------|-------------------|
| `snmp`    | zensight-sensor-snmp    | device name |
| `syslog`  | zensight-sensor-logs  | hostname (network or journald) |
| `netflow` | zensight-sensor-netflow | exporter name |
| `modbus`  | zensight-sensor-modbus  | device name |
| `sysinfo` | zensight-sensor-sysinfo | hostname |
| `gnmi`    | zensight-sensor-gnmi    | device name |
| `netlink` | zensight-sensor-netlink | hostname |
| `netring` | zensight-sensor-netring | sensor id |

---

## 2. Telemetry — `zensight/<protocol>/<source>/<metric>`

The universal pattern. `<metric>` may contain `/` (it is a path), so a key can
have more than four chunks.

```
zensight/snmp/router01/system/sysUpTime
zensight/syslog/web01/auth/crit                 # <facility>/<severity>
zensight/sysinfo/server01/cpu/usage
zensight/netflow/exporter01/10.0.0.1/10.0.0.2
zensight/modbus/plc01/holding/temperature
zensight/gnmi/router01/interfaces/interface[name=eth0]/state/counters
zensight/netlink/host01/sockets/tcp/established
zensight/netring/sensor01/flow/by_l4/tcp/bytes_total
```

Payload: a serialized [`TelemetryPoint`] (JSON or CBOR per the sensor's
`serialization` config). Built via [`KeyExprBuilder::build(source, metric)`].

> **Published with a zenoh-ext `AdvancedPublisher`** (per-key cache + miss/
> publisher detection), so it pairs with the GUI's `AdvancedSubscriber` on
> `zensight/**` (history + recovery). The control-plane below uses plain
> `put`/`delete` and a plain subscriber. See
> [Architecture → Zenoh Transport & Pub/Sub Model](ARCHITECTURE.md#zenoh-transport--pubsub-model).

---

## 3. Control-plane — `zensight/<protocol>/@/…`

Per-sensor operational channels. All are derived from the sensor's `key_prefix`.

| Key | Direction | Payload | Emitted by |
|-----|-----------|---------|------------|
| `@/health` | put | `HealthSnapshot` | every sensor (`SensorRunner`) |
| `@/errors` | put | `ErrorReport` | every sensor (`HealthReporter`) |
| `@/status` | queryable | status JSON | every sensor (`StatusPublisher`) |
| `@/alive` | liveliness token | — | every sensor (`LivelinessManager`) |
| `@/devices/<device>/liveness` | put | `DeviceLiveness` | sensors with per-device tracking |
| `@/devices/<device>/alive` | liveliness token | — | sensors with per-device tracking |
| `@/alerts/<alert_key>` | put / delete | `Alert` (firing → resolved → tombstone) | snmp, syslog, netlink, netring |
| `@/query/alerts` | queryable | `Vec<Alert>` (current firing set) | sensors with alerts (late-joiner seed) |
| `@/commands/<topic>` | subscribe | topic command | sensors with runtime control |
| `@/status/<topic>` | queryable | topic status | sensors with runtime control |
| `@/query/<topic>` | queryable | topic detail (`Vec<Record>`) | netlink, netring |

`<alert_key>` is a stable hash of `source + rule + sorted-labels`
([`Alert::alert_key`]) so the same logical alert always maps to the same key
(firing and resolving are state transitions on one key, not new keys).

### 3.1 Control topics in use

| Sensor | `@/commands/<topic>` · `@/status/<topic>` | Purpose |
|--------|---|---|
| syslog | `filter` | add/remove/clear dynamic message filters |
| netlink | `expectations` | hot-swap sentinel expectations |
| netlink | `collection` | toggle collectors at runtime |
| netring | `detectors` | runtime detection tuning: allowlist + per-detector mute/threshold |

### 3.2 On-demand detail queries — `@/query/<topic>`

High-cardinality detail is **served on request, never streamed** onto the
telemetry bus (principle: keep the bus low-cardinality). Parameters are passed
as Zenoh selector params (e.g. `?top=20`, `?state=&port=`).

| Sensor | `@/query/<topic>` | Reply |
|--------|---|---|
| netlink | `routes`, `neighbors`, `sockets?state=&port=`, `addresses`, `events`, `tc`, `xfrm`, `nft` | `Vec<…Record>` |
| netring | `flows`, `tls`, `talkers?top=N`, `elephant_flows`, `dns?top=N`, `http?top=N`, `quic`, `ssh`, `assets` | `Vec<…Record>` |

---

## 4. Metadata — `zensight/_meta/…`

Cross-sensor, protocol-independent registries.

| Key | Payload | Emitted by |
|-----|---------|------------|
| `zensight/_meta/sensors/<name>` | `SensorInfo` (registration/discovery) | every sensor |
| `zensight/_meta/correlation/<ip>` | `CorrelationEntry` (which sensors see a device) | sensors with correlation |

---

## 5. Wildcards & subscriptions

| Wildcard | Used by | Catches |
|----------|---------|---------|
| `zensight/**` | frontend (history sub), exporters | all telemetry *and* `_meta` (but **not** `@/…`) |
| `zensight/*/@/**` | frontend | all control-plane (health/errors/alerts/liveness) |
| `zensight/*/@/alive` | frontend | sensor liveliness tokens |
| `zensight/*/@/devices/*/alive` | frontend | device liveliness tokens |
| `zensight/*/@/query/alerts` | frontend (GET at startup) | firing-set seed for late joiners |
| `zensight/<protocol>/@/alerts/**` | any alert consumer | one sensor's alerts (note explicit `@`) |
| `zensight/_meta/sensors/*` | frontend | sensor registrations |
| `zensight/_meta/correlation/*` | frontend | device correlations |

Exporters (`prometheus`, `otel`) subscribe to `zensight/**` and **skip**
control/metadata by filtering keys containing `/@/` or starting with
`zensight/_meta/` — only true telemetry is exported.

---

## 6. Full tree at a glance

```
zensight/
├── <protocol>/
│   ├── <source>/<metric…>              # telemetry  (TelemetryPoint)
│   └── @/
│       ├── health                      # HealthSnapshot
│       ├── errors                      # ErrorReport
│       ├── status                      # queryable
│       ├── alive                       # liveliness token
│       ├── devices/<device>/liveness   # DeviceLiveness
│       ├── devices/<device>/alive      # liveliness token
│       ├── alerts/<alert_key>          # Alert (firing/resolved)
│       ├── query/alerts                # firing-set seed (queryable)
│       ├── query/<topic>               # on-demand detail (queryable)
│       ├── commands/<topic>            # runtime control (sub)
│       └── status/<topic>              # control status (queryable)
└── _meta/
    ├── sensors/<name>                  # SensorInfo
    └── correlation/<ip>                # CorrelationEntry
```

---

## 7. Key-building helpers

Do not hand-write keys. Build them through these so the conventions stay
enforced and a single change propagates everywhere.

| Helper | Location | Produces |
|--------|----------|----------|
| `KeyExprBuilder::build(source, metric)` | `zensight-common/src/keyexpr.rs` | `zensight/<proto>/<source>/<metric>` |
| `KeyExprBuilder::status_key()` | `zensight-common/src/keyexpr.rs` | `…/@/status` |
| `KeyExprBuilder::alert_key_expr(key)` | `zensight-common/src/keyexpr.rs` | `…/@/alerts/<key>` |
| `command::command_key(prefix, topic)` | `zensight-common/src/command.rs` | `…/@/commands/<topic>` |
| `command::status_key(prefix, topic)` | `zensight-common/src/command.rs` | `…/@/status/<topic>` |
| `command::query_key(prefix, topic)` | `zensight-common/src/command.rs` | `…/@/query/<topic>` |
| `all_*_wildcard()` | `zensight-common/src/keyexpr.rs` | the wildcards in §5 |

The control-plane keys for `health`, `errors`, `alive`, `devices/*`, and
`alerts/*` are produced inside `zensight-sensor-core` (`health.rs`,
`liveliness.rs`, `alert.rs`) so every sensor inherits them identically by using
the framework — sensors never build these by hand.

[`TelemetryPoint`]: ../zensight-common/src/telemetry.rs
[`KeyExprBuilder::build(source, metric)`]: ../zensight-common/src/keyexpr.rs
[`Alert::alert_key`]: ../zensight-common/src/alert.rs
