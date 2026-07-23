# zensight-sensor-snmp — reference

Polls SNMP agents (v1/v2c/v3) with GET and WALK, and optionally listens for SNMP
traps. OIDs are resolved to metric names through the configured `oid_names` map
(with `{index}` placeholders for table columns) and optional built-in/loaded MIBs.

## Telemetry & keyspace

All keys follow the v1 grammar, `zensight/v1/<origin>/…`, where `<origin>` is
the **poller host's** stable id (`h-<12hex>`). SNMP is a *proxy producer*: the
observed device is the first subject chunk after the producer.

| Key | Payload |
|-----|---------|
| `zensight/v1/<origin>/telemetry/snmp/<device>/<metric>` | Polled OID value. `<metric>` is the MIB-/map-resolved name, e.g. `system/sysUpTime`, `if/1/ifInOctets`. Unmapped OIDs fall back to the raw dotted OID. The `oid` label carries the source OID. |
| `zensight/v1/<origin>/telemetry/snmp/<sender>/trap/<trap_id>` | Received trap (when `trap_listener.enabled`). `<trap_id>` is the enterprise/generic trap OID; `<sender>` is the slugged sender IP (`.`/`:` → `-`). |
| `zensight/v1/<origin>/telemetry/snmp/<sender>/trap/<trap_id>/<varbind>` | Per-varbind value from the trap PDU. |

`<device>` comes from each device's `name`. The point `source` payload field
defaults to the local hostname unless `snmp.source` is set.

### Control plane (via `zensight-sensor-core`)

Standard sensor metadata is published by the shared runner:

- `zensight/v1/<origin>/state/snmp/health` — sensor health document (absorbs the legacy running flag)
- `zensight/v1/<origin>/state/snmp/device/<device>/liveness` — per-device liveness document (a `…/device/<device>/alive` liveliness token is separate machinery)
- `zensight/v1/<origin>/state/snmp/errors` — error reports
- `zensight/v1/<origin>/@rpc/snmp/artifact/{request,cancel}` — on-demand debug report / snapshot (opt-in via `artifacts`); progress rides the `state/snmp/artifact/<kind>` status document
- `zensight/v1/<origin>/state/snmp/sensor` — sensor registration (`SensorInfo`)
- `zensight/v1/<origin>/state/snmp/evidence/self` — self-reported host evidence (`with_identity`)
- `zensight/v1/<origin>/state/snmp/alive` — sensor liveliness token
- `zensight/v1/<origin>/@rpc/snmp/introspect` — the registry slice this build serves

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative contract.

> Note: this sensor does not currently emit `state/snmp/alert/*`; it has no
> threshold/alert engine of its own.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `serialization`
(`json`|`cbor`), `logging`, `artifacts`, and `snmp`.

### `snmp` block

| Field | Type | Notes |
|-------|------|-------|
| `source` | string? | Override the agent-host source id in payloads (default: local hostname; v1 keys are origin-scoped, so it no longer appears in key expressions). |
| `trap_listener.enabled` | bool | Enable the SNMP trap receiver. |
| `trap_listener.bind` | string | Trap listen address (default `0.0.0.0:162`). |
| `devices[]` | array | Devices to poll (see below). |
| `oid_groups` | map | Named, reusable `{ oids, walks }` sets referenced by `device.oid_group`. |
| `oid_names` | map | OID→metric-name map; `{index}` is substituted with the table index. |
| `mib.load_builtin` | bool | Load bundled MIB definitions. |
| `mib.files` | string[] | Extra MIB files to load for OID resolution. |

### `devices[]`

| Field | Type | Notes |
|-------|------|-------|
| `name` | string | Device id used in key expressions. |
| `address` | string | SNMP agent `host:port` (e.g. `192.168.1.1:161`). |
| `community` | string | Community string (v1/v2c). |
| `version` | enum | `v1`, `v2c`, or `v3`. |
| `security` | object? | v3 auth/priv (see below). |
| `poll_interval_secs` | u64 | Polling cadence. |
| `oids` | string[] | Individual OIDs polled with GET. |
| `walks` | string[] | OID subtrees polled with WALK (GETNEXT). |
| `oid_group` | string? | Reference a predefined `oid_groups` entry instead of inline `oids`/`walks`. |

### `security` (SNMPv3)

`username`, `auth_protocol` (`MD5`/`SHA`/`SHA256`), `auth_password`,
`priv_protocol` (`DES`/`AES`/`AES256`), `priv_password`, optional `engine_id`.

## Testing

`tests/e2e.rs` drives real UDP round-trips against an **in-process SNMP agent**
(the [`async-snmp`](https://docs.rs/async-snmp) agent framework, a
dev-dependency) — no snmpsim/net-snmp needed, CI-safe on localhost. The
harness lives in `tests/harness/mod.rs`:

- `SimMib` — mutable OID→value store (`MibHandler`), with builders for a
  synthetic system group and ifTable/ifXTable; values can be changed between
  poll cycles (counter advancement, status flips, sysUpTime resets).
- `SimAgent` — agent on `127.0.0.1:0`; per-test v2c communities and/or
  SNMPv3 USM users (the v3 matrix covers noAuthNoPriv → authPriv,
  SHA-1/SHA-256 × AES-128/AES-256, wrong-credential failure modes).
- `FlakyProxy` — UDP forwarder with *drop next N datagrams* and *blackhole*
  knobs, and a swappable backend (agent restart behind a stable address).
- `rig()` — an initialized `SnmpPoller` on an isolated in-process Zenoh peer
  with a `v1/*/telemetry/snmp/**` subscriber; assertions read decoded
  `TelemetryPoint`s.

The harness maps OIDs through lowercase `oid_names` (grammar-valid chunks);
built-in MIB names currently violate the chunk grammar — see issue #559.
Two tests are `#[ignore]`d until the async-snmp client migration (#526):
noAuthNoPriv (snmp2 panics on an empty password) and engine re-discovery
after an agent restart.

## Build / run notes & caveats

- **Build dependency:** the SNMP stack needs OpenSSL / net-snmp headers at build
  time (`openssl-devel` / `libssl-dev`). Missing headers cause a build failure.
- **Trap listener:** binding UDP 162 requires elevated privileges (or a
  `CAP_NET_BIND_SERVICE` capability / higher bind port). Polling itself is
  unprivileged.
- MIB resolution is best-effort: unresolved OIDs are published under their raw
  dotted-OID metric name.
