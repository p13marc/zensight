# zensight-sensor-snmp — reference

Polls SNMP agents (v1/v2c/v3) with GET and WALK — GETBULK on v2c/v3, GETNEXT
on v1, one persistent UDP socket per device — and optionally listens for SNMP
traps. OIDs are resolved to metric names through the configured `oid_names` map
(with `{index}` placeholders for table columns) and optional built-in/loaded MIBs.

## Telemetry & keyspace

All keys follow the v1 grammar, `zensight/v1/<origin>/…`, where `<origin>` is
the **poller host's** stable id (`h-<12hex>`). SNMP is a *proxy producer*: the
observed device is the first subject chunk after the producer.

| Key | Payload |
|-----|---------|
| `zensight/v1/<origin>/telemetry/snmp/<device>/<metric>` | Polled OID value. `<metric>` is the MIB-/map-resolved name, e.g. `system/sysUpTime`, `if/1/ifInOctets`. Unmapped OIDs fall back to the raw dotted OID. The `oid` label carries the source OID. |
| `zensight/v1/<origin>/telemetry/snmp/<device>/<metric>.rate` | Derived per-second rate for counter OIDs (`Gauge`, unit `By/s` for octet counters, else `1/s`), published alongside the raw counter from the second poll cycle on. See *Counter semantics*. |
| `zensight/v1/<origin>/state/snmp/<device>/interfaces` | Joined ifTable/ifXTable doc (`InterfaceTable`, #529): per interface — ifName/ifDescr/ifAlias, decoded admin/oper status, speed (ifHighSpeed preferred), MAC, HC-preferred octet/packet/error/discard counters and their derived rates. LWW state, refreshed each poll cycle from whatever IF-MIB columns are walked; cached for late joiners. Disable with `snmp.publish_interfaces: false`. |
| `zensight/v1/<origin>/events/snmp/<sender>/trap/<ulid>` | Durable trap/inform record (#535): an `EventRecord` with the translated trap name (`kind: "trap/link_down"`), severity, and translated varbinds in `fields`. Reliable QoS, one key per record — point a Zenoh storage at `**/events/**` to retain history. `<sender>` is the slugged source IP (`.`/`:` → `-`). |
| `zensight/v1/<origin>/telemetry/snmp/<sender>/trap/<trap_id>` | Lightweight cumulative counter per (sender, trap type) for dashboards. `<trap_id>` is the snake_case-translated notification name (or dotted OID). |

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
- `zensight/v1/<origin>/state/snmp/evidence/device/<device>` — observed-device identity claim (#537): `HostEvidence` with `observer: "snmp"` — hostname ← sysName, platform ← sysDescr, vendor ← the sysObjectID enterprise arc, MACs ← ifPhysAddress, IPs ← ipAdEntAddr + the polled address. Refreshed on the first successful cycle then every `evidence.refresh_cycles` (default 10); the correlator fuses these with netring/netlink observations of the same MAC/IP into one `HostEntity`. `host_id` stays unset (observed devices have no hashed machine-id; MAC/IP rules do the merging). Disable with `snmp.evidence.enabled: false`. |
- `zensight/v1/<origin>/state/snmp/alert/<key>` — threshold alerts (see below)
- `zensight/v1/<origin>/state/snmp/alive` — sensor liveliness token
- `zensight/v1/<origin>/@rpc/snmp/introspect` — the registry slice this build serves

See [../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the authoritative contract.

## Counter semantics (#527)

- **Typing**: Counter32/Counter64 publish as `Counter`; Gauge32/Unsigned32 as
  `Gauge`; **TimeTicks converts to seconds** (`Gauge`, unit `"s"`) — sysUpTime
  renders as a duration with no consumer special-casing.
- **Rates**: every counter OID gets a derived sibling metric `<metric>.rate`
  (`Gauge`, per second) once a previous sample exists. Octet counters carry
  unit `By/s`, all other counters `1/s`. The raw lifetime counter keeps
  publishing unchanged (history, exporters).
- **Wrap handling**: deltas use modular arithmetic in the counter's width, so
  a single Counter32 wrap (~5.7 min at a saturated 100 Mb/s link) still
  yields a correct continuous rate.
- **Reset handling**: the poller reads sysUpTime.0 every cycle; if it goes
  backwards, the device rebooted — all rate baselines drop and one interval
  publishes no rates (never negative/garbage spikes). An implausibly large
  single-counter delta (> 1e10/s) re-baselines just that counter. Rate
  eligibility comes from the wire tag, backed by the MIB table's SYNTAX for
  agents that mis-tag counters.
- **Units**: `TelemetryPoint` carries an optional UCUM-style `unit` field
  (serde-default, absent when unknown). The OTel exporter forwards it as the
  instrument unit; the Prometheus exporter exports rates as gauges named
  `..._rate` (dots/slashes sanitized to `_`) without unit annotation.

## Threshold alerts (#528)

The sensor drives sensor-core's `AlertReporter`: firing/resolved alerts ride
`zensight/v1/<origin>/state/snmp/alert/<key>` (reliable QoS, tombstone on
resolve; late joiners seed via the standard alert selector GET). One shared
reporter serves all devices; reconciliation is scoped by the `device` label,
so one device's recovery never resolves another's alerts.

| Rule | Fires when | Severity |
|------|-----------|----------|
| `device_unreachable` | N consecutive poll cycles failed entirely at the transport level (default N=3) | critical |
| `interface_down` | `ifOperStatus != up` while `ifAdminStatus == up` | warning |
| `interface_errors` | error/discard rate above `per_sec` (default 1/s), per direction+kind | warning |
| `interface_utilization` | octet rate ×8 vs `ifHighSpeed`/`ifSpeed` above `percent` (default 90) | warning |
| `device_rebooted` | sysUpTime went backwards; holds `hold_secs` (default 300) then auto-resolves | info |
| `storage_usage` | `hrStorageUsed/hrStorageSize` above `percent` (default 90) — only when hrStorage is walked | warning |
| `processor_load` | `hrProcessorLoad` above `percent` (default 90) — only when walked | warning |

Config: a `snmp.alerts` block — `enabled` (default true), `for_secs`
(continuous-violation debounce, default 0), and one sub-block per rule, each
individually disableable. `devices[].alerts` replaces the whole block for
that device. When the interface rules are on, the sensor auto-adds the
IF-MIB columns they read (status, speed, octet/error/discard counters incl.
HC) to the walk set unless an existing walk already covers them; the
HOST-RESOURCES rules evaluate only tables you explicitly walk. An
unanswering device keeps its interface/storage alert state (no false
resolves) until it responds again.

## Configuration

JSON5, loaded with `--config`. Top-level keys: `zenoh`, `serialization`
(`json`|`cbor`), `logging`, `artifacts`, and `snmp`.

### `snmp` block

| Field | Type | Notes |
|-------|------|-------|
| `source` | string? | Override the agent-host source id in payloads (default: local hostname; v1 keys are origin-scoped, so it no longer appears in key expressions). |
| `trap_listener.enabled` | bool | Enable the SNMP trap receiver. |
| `trap_listener.bind` | string | Trap listen address (default `0.0.0.0:162`). |
| `trap_listener.communities` | string[] | Accepted v1/v2c communities; empty = accept any. |
| `trap_listener.users` | object[] | SNMPv3 notification users (same schema as device `security`; `engine_id` ignored — the receiver is authoritative). |
| `trap_listener.alerts` | object[] | Trap → alert rules: `{ rule, fire: <OID>, resolve?: <OID>, severity }`. |
| `trap_listener.builtin_rules` | bool | Include the built-in linkDown/linkUp mapping (default true). |
| `devices[]` | array | Devices to poll (see below). |
| `oid_groups` | map | Named, reusable `{ oids, walks }` sets referenced by `device.oid_group`. |
| `oid_names` | map | OID→metric-name map; `{index}` is substituted with the table index. |
| `evidence.enabled` | bool | Publish observed-device identity claims (#537, default true). |
| `evidence.refresh_cycles` | u32 | Claim refresh cadence in poll cycles (default 10). |
| `mib.load_builtin` | bool | Load bundled MIB definitions. |
| `mib.files` | string[] | **Deprecated** (#532): legacy JSON pseudo-MIBs, honored one more release. Use `mib.dirs`. |
| `mib.dirs` | string[] | Directories of **standard SMI** `.mib`/`.txt` files (vendor MIBs drop in unmodified); parsed with a real SMI parser, malformed modules fail startup. |

### `devices[]`

| Field | Type | Notes |
|-------|------|-------|
| `name` | string | Device id used in key expressions. |
| `address` | string | SNMP agent `host:port` (e.g. `192.168.1.1:161`). |
| `community` | string | Community string (v1/v2c). |
| `version` | enum | `v1`, `v2c`, or `v3`. |
| `security` | object? | v3 auth/priv (see below). |
| `poll_interval_secs` | u64 | Polling cadence. |
| `timeout_secs` | u64 | Per-request timeout, per attempt (default 5). |
| `retries` | u32 | Retransmissions after a timed-out request (default 2; also budgets SNMPv3 report/resync flows). |
| `max_repetitions` | u32 | GETBULK max-repetitions for walks on v2c/v3 (default 20). |
| `oids` | string[] | Individual OIDs polled with GET. |
| `walks` | string[] | OID subtrees polled with WALK (GETBULK on v2c/v3, GETNEXT on v1; tooBig responses are recovered by bisection). |
| `oid_group` | string? | Reference a predefined `oid_groups` entry instead of inline `oids`/`walks`. |

### `security` (SNMPv3)

`username`, `auth_protocol` (`MD5`/`SHA`/`SHA224`/`SHA256`/`SHA384`/`SHA512`),
`auth_password`, `priv_protocol` (`DES`/`AES`/`AES192`/`AES256`),
`priv_password`, optional `engine_id`.

A configured `engine_id` (hex, `0x`/`:` tolerated) pre-seeds the engine cache
and skips the discovery round-trip; it requires a literal `ip:port` device
address (hostnames fall back to auto-discovery, the default). If a device
comes back with a **different** engine identity (agent replaced/reset), the
poller notices the all-auth-failure cycle and rebuilds its client to force
rediscovery — no sensor restart needed.

## Credentials (#538)

Credentials never need to sit in plaintext config. Every credential value —
`community`, `auth_password`, `priv_password`, trap-listener communities and
users — accepts **secret indirection**:

- `"${SNMP_AUTH_PW}"` → the environment variable (systemd `Environment=`,
  container env);
- `"file:/run/credentials/snmp/community"` → the file's contents, trailing
  whitespace trimmed (systemd `LoadCredential=`, Kubernetes secrets);
- anything else is the literal value (inline stays the escape hatch).

A missing variable or unreadable file **fails startup** — a sensor silently
polling with an empty community would be worse.

**Named credential sets** put a shared credential in one place:

```json5
credentials: {
  "readonly-v2c": { community: "file:/run/credentials/snmp/community" },
  "netops-v3": {
    security: { username: "netops", auth_protocol: "SHA256",
                auth_password: "${SNMP_AUTH_PW}",
                priv_protocol: "AES", priv_password: "${SNMP_PRIV_PW}" },
  },
},
devices: [
  { name: "sw1", address: "10.0.0.1:161", credentials: "readonly-v2c" },
  { name: "r1", address: "10.0.0.2:161", version: "v3", credentials: "netops-v3" },
]
```

Rotating a set (new file/env value + sensor restart) updates every
referencing device; a device's `credentials` reference replaces its inline
community/security. Unknown set names fail startup.

**Scrubbing guarantees** (audited by `test_secrets_never_leak`): the Debug
impls of `DeviceConfig`/`SnmpV3Security`/`CredentialSet` redact credential
fields, so stray `{:?}` log lines can't leak; the on-demand debug-report
bundle redacts every credential key (including the plural
`trap_listener.communities`) before packaging; `introspect`/`describe`
serve the registry/schemas only — never config. Recommendation for a mixed
fleet: prefer v3 authPriv (SHA-256/AES-128 or better) wherever the gear
supports it and keep v2c communities in files, not inline.

## Device profiles (#531)

Onboarding needs only `name` + `address` + credentials: profiles supply the
OID sets. Four base profiles ship **embedded in the binary**:

| Profile | Match | Polls |
|---------|-------|-------|
| `generic-device` | default | SNMPv2-MIB system group |
| `network-interfaces` | default | IF-MIB ifTable + ifXTable |
| `host-resources` | extend/pin | hrStorage descr/units/size/used + hrProcessorLoad |
| `entity-sensors` | extend/pin | entPhySensorTable type/scale/value/status |

Selection per device runs once, on the first cycle that reads
`sysObjectID.0` (deferred while the device is unreachable): every `default`
profile applies, plus the non-default profile with the longest matching
`sys_object_id` prefix — including its `extends` chain. `devices[].profile`
pins a profile by name instead of prefix matching (defaults still apply);
an unknown pin, malformed profile file, or dangling `extends` fails startup.
Configured `oids`/`walks`/`oid_group` merge on top; walks covered by a
broader walk are deduplicated. The applied set is logged and published as
the `system/profile` text metric.

### Authoring a profile

TOML in a directory listed under `snmp.profiles.dirs` (same-name overrides a
shipped profile). Top-level keys **before** the `[match]` table:

```toml
name = "acme-switch"
extends = ["network-interfaces"]
oids  = ["1.3.6.1.4.1.4242.1.1.0"]
walks = ["1.3.6.1.4.1.4242.1.2"]

[match]
sys_object_id = ["1.3.6.1.4.1.4242.1."]  # or: default = true

[oid_names]  # lowercase, chunk-grammar-valid; {index} for table columns
"1.3.6.1.4.1.4242.1.1.0" = "acme/fan_rpm"
"1.3.6.1.4.1.4242.1.2"   = "acme/{index}/port_errors"

[oid_syntax] # rate eligibility for the counter tracker
"1.3.6.1.4.1.4242.1.2" = "Counter32"
```

Naming/SYNTAX tables from all loaded profiles feed the shared resolver
(fleet-wide); built-in MIB names and config `oid_names` win on collisions.
Disable everything with `snmp.profiles.enabled: false`.

## SMI MIBs (#532)

`mib.dirs` loads standard SMI modules (mib-rs; SNMPv2-SMI/-TC base modules
are built in). The SMI layer is the **naming fallback** behind the explicit
tables (built-ins, config `oid_names`, profiles): where nothing explicit
matches, a polled OID resolves to `snake_case(object)` for scalars and
`snake_case(object)/<index>` for table instances (chunk-grammar-valid),
instead of the dotted OID. MIB metadata also feeds:

- **enum decode** — INTEGER named-values ride an `enum` label
  (`ifOperStatus` publishes `2` with `enum: "down"`); the numeric value
  stays numeric for thresholds and plots;
- **units** — the UNITS clause fills `TelemetryPoint.unit` unless the value
  conversion set one (TimeTicks seconds);
- **typing** — the SMI base type (Counter32/64) backs rate eligibility like
  the hand-maintained SYNTAX hints;
- **trap translation** — notification OIDs resolve to snake-case names in
  trap keys (`link_down`, vendor notifications) via the same loaded set.

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

## Build / run notes & caveats

- The SNMP stack ([`async-snmp`](https://docs.rs/async-snmp), pinned pre-1.0)
  is pure Rust — no OpenSSL / net-snmp headers needed to build.
- **Trap listener:** binding UDP 162 requires elevated privileges. Options:
  `setcap cap_net_bind_service=+ep` on the binary (or
  `AmbientCapabilities=CAP_NET_BIND_SERVICE` in the systemd unit), or bind an
  unprivileged port (`bind: "0.0.0.0:1162"`) and redirect with
  `nft add rule ip nat prerouting udp dport 162 redirect to 1162` (or the
  iptables equivalent). Polling itself is unprivileged.
- **Traps end-to-end (#535):** the receiver (async-snmp) accepts v1 traps,
  v2c traps/informs, and v3 traps/informs (USM); **informs are acknowledged
  automatically**, so senders stop retransmitting. Each notification becomes
  a durable events-class record + a telemetry counter, and matching
  `fire`/`resolve` rules drive alerts through the shared reporter (labels:
  `device`, `if_index` when an ifIndex varbind is present). Trap alert
  mapping requires `snmp.alerts.enabled` (the shared reporter).
- MIB resolution is best-effort: unresolved OIDs are published under their raw
  dotted-OID metric name.
