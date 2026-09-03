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
| `zensight/v1/<origin>/telemetry/snmp/<device>/<metric>` | Polled OID value. `<metric>` is the MIB-/map-resolved name, e.g. `system/uptime`, `if/1/in_octets` — lowercase profile-style names since #559 (built-ins, shipped profiles and the sample config all agree; every chunk satisfies the key grammar). Unmapped OIDs fall back to the raw dotted OID. A grammar-violating custom `oid_names` entry is slugged losslessly at the publish boundary rather than rejected. The `oid` label carries the source OID. |
| `zensight/v1/<origin>/telemetry/snmp/<device>/<metric>.rate` | Derived per-second rate for counter OIDs (`Gauge`, unit `By/s` for octet counters, else `1/s`), published alongside the raw counter from the second poll cycle on. See *Counter semantics*. |
| `zensight/v1/<origin>/state/snmp/<device>/interfaces` | Joined ifTable/ifXTable doc (`InterfaceTable`, #529): per interface — ifName/ifDescr/ifAlias, decoded admin/oper status, speed (ifHighSpeed preferred), MAC, HC-preferred octet/packet/error/discard counters and their derived rates. LWW state, refreshed each poll cycle from whatever IF-MIB columns are walked; cached for late joiners. Disable with `snmp.publish_interfaces: false`. |
| `zensight/v1/<origin>/events/snmp/<sender>/trap/<ulid>` | Durable trap/inform record (#535): an `EventRecord` with the translated trap name (`kind: "trap/link_down"`), severity, and translated varbinds in `fields`. Reliable QoS, one key per record — point a Zenoh storage at `**/events/**` to retain history. `<sender>` is the slugged source IP (`.`/`:` → `-`). **A trap storm cannot become an alert storm** (#825): every trap is a durable event, but the alert half is bounded by construction — identical traps build one `alert_key` and the reporter is idempotent while it fires (pinned by `tests/trap_storm.rs`); only a severity escalation re-publishes. |
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

## Metric names changed in 0.11 (#559) — read before upgrading

The built-in MIB tables published raw MIB object names straight onto the
telemetry key (`sysUpTime.0`, `ifInOctets`). Those violate the key chunk
grammar, which is lowercase alphanumeric plus `._-` per chunk (RFC 03 §2), so
every debug poll cycle panicked in the metric guard and `refine_key` could not
classify SNMP telemetry at all. All 49 built-in names now follow the same
lowercase, profile-style convention the shipped profiles already used.

### Why a stock deployment is affected

It is tempting to assume only exotic configs were, since profiles have been on
by default since #531 and already used lowercase names. But before #559
**built-ins won over profiles**: `MibResolver::add_profile_mappings` inserted
with `.entry().or_insert()`, so for any OID both tables covered, the mixed-case
built-in name is what got published. A stock `load_builtin: true` deployment was
publishing the old names.

### What this does to your dashboards

Both exporters derive the exported series name from the metric path — there is
no SNMP entry in `semconv.rs`, deliberately (the tree is device-defined, so
there is no fixed vocabulary to map). So the rename moves every series:

| | before | after |
|---|---|---|
| Prometheus | `zensight_snmp_sysUpTime_0` | `zensight_snmp_system_uptime` |
| Prometheus | `zensight_snmp_ifInOctets_3` | `zensight_snmp_if_3_in_octets` |
| OTel | `zensight.snmp.sysUpTime.0` | `zensight.snmp.system.uptime` |
| OTel | `zensight.snmp.ifInOctets.3` | `zensight.snmp.if.3.in_octets` |

**Dashboards, recording rules and alerting rules built on the old names stop
matching.** They do not error — they go silent, which is the bad kind. Note the
table columns also move the index into its own key chunk (`ifInOctets.3` →
`if/3/in_octets`), so a per-interface series that was one flat name is now
structured.

### No compatibility aliases, deliberately

The exporters do **not** emit both spellings, and should not. SNMP is the
highest-cardinality producer in the fleet — per-column × per-interface ×
per-device — so dual-publishing doubles that on the wire and in every scrape,
permanently, to spare a one-time dashboard edit. That is a bad trade, and
stating it here is the point: the decision should be findable, not folklore.

The GUI's aliases are a different thing and not a precedent. They are
**read-side** tolerance so a fleet part-way through the upgrade still renders
(`zensight/src/view/specialized/snmp.rs` accepts `system/uptime` *or*
`system/sysUpTime`, `cpu/*/load` or `hrProcessorLoad`, and so on). They cost
nothing on the wire and can be deleted once no pre-0.11 sensor remains.

### The interface table is registered, the rest is still a catch-all (#779)

Since registry **1.8**, `ifTable`/`ifXTable` columns are registered explicitly —
one subject per column, `{device}/if/{index}/<column>` and
`{device}/ifx/{index}/<column>`, plus the `.rate` sibling the poller derives for
every counter. Everything else this sensor publishes still rides the rest-var
catch-all `{device}/{metric...}`.

That distinction is visible in exported metric names. The catch-all has no
literal chunks, so the exporters' family rule (#764) names from the rest
variable's *value* and the table index lands in the metric **name**. A
registered column is named from its literal chunks instead, and every variable
becomes a label:

| | Prometheus series |
|---|---|
| before 1.8 | `zensight_snmp_if_1_in_octets_total{device="router01",oid="…"}` |
| since 1.8 | `zensight_snmp_if_in_octets_total{device="router01",index="1",oid="…"}` |

So `sum by (index) (zensight_snmp_if_in_octets_total)` is now writable; #769
had already attached the index as a label, and this is the other half of it.
**The wire keys did not change** — the poller publishes the same
`…/telemetry/snmp/<device>/if/<index>/<column>` bytes it always did. Only the
exporters' reading of them moved, so a dashboard written against the old names
needs updating and nothing else does.

`cpu/{index}/…`, `ip/{index}/…` and `storage/{index}/…` have the same shape and
the same problem, and are deliberately still on the catch-all.

### `introspect` cannot tell you the old names are gone

Unlike the logs-sensor rename in 0.10.0, which moved registry *subject paths*
and therefore left `deprecated.lock` entries a consumer can query, the SNMP
registry subject is the rest-var `{device}/{metric...}`. The rename happened
*inside* it, so no registry subject was retired and there is nothing in
`deprecated.lock` to find. **This document and the changelog are the only
record.**

### Custom `oid_names`

A name violating the chunk grammar is no longer a panic: since #559 it is
escaped losslessly at the publish boundary and warned about once at startup. So
a stale config keeps working but publishes a *third* spelling matching neither
scheme — `system/sysUpTime` becomes `system/sys_x55_p_x54_ime`, and your
Prometheus series becomes `zensight_snmp_system_sys_x55_p_x54_ime`. Check your
`oid_names` against the table below. (`docker/configs/snmp.json5` was shipping
exactly this. It was corrected in #647 and then deleted in #472 — it turned out
to be referenced by no Dockerfile and no compose file. The config the SNMP
image actually uses is `configs/snmp.json5`, which was already correct.)

### The full table

| before | after |
|---|---|
| `sysDescr.0` | `system/descr` |
| `sysObjectID.0` | `system/object_id` |
| `sysUpTime.0` | `system/uptime` |
| `sysContact.0` | `system/contact` |
| `sysName.0` | `system/name` |
| `sysLocation.0` | `system/location` |
| `sysServices.0` | `system/services` |
| `snmpInPkts.0` | `snmp/in_pkts` |
| `snmpOutPkts.0` | `snmp/out_pkts` |
| `ifNumber.0` | `if_number` |
| `ifIndex` | `if/{index}/index` |
| `ifDescr` | `if/{index}/descr` |
| `ifType` | `if/{index}/type` |
| `ifMtu` | `if/{index}/mtu` |
| `ifSpeed` | `if/{index}/speed` |
| `ifPhysAddress` | `if/{index}/phys_address` |
| `ifAdminStatus` | `if/{index}/admin_status` |
| `ifOperStatus` | `if/{index}/oper_status` |
| `ifLastChange` | `if/{index}/last_change` |
| `ifInOctets` | `if/{index}/in_octets` |
| `ifInUcastPkts` | `if/{index}/in_ucast_pkts` |
| `ifInDiscards` | `if/{index}/in_discards` |
| `ifInErrors` | `if/{index}/in_errors` |
| `ifOutOctets` | `if/{index}/out_octets` |
| `ifOutUcastPkts` | `if/{index}/out_ucast_pkts` |
| `ifOutDiscards` | `if/{index}/out_discards` |
| `ifOutErrors` | `if/{index}/out_errors` |
| `ifName` | `ifx/{index}/name` |
| `ifHCInOctets` | `ifx/{index}/hc_in_octets` |
| `ifHCOutOctets` | `ifx/{index}/hc_out_octets` |
| `ifHighSpeed` | `ifx/{index}/high_speed` |
| `ifAlias` | `ifx/{index}/alias` |
| `hrSystemUptime.0` | `host/uptime` |
| `hrSystemDate.0` | `host/date` |
| `hrSystemNumUsers.0` | `host/users` |
| `hrSystemProcesses.0` | `host/processes` |
| `hrStorageIndex` | `storage/{index}/index` |
| `hrStorageType` | `storage/{index}/type` |
| `hrStorageDescr` | `storage/{index}/descr` |
| `hrStorageAllocationUnits` | `storage/{index}/allocation_units` |
| `hrStorageSize` | `storage/{index}/size` |
| `hrStorageUsed` | `storage/{index}/used` |
| `hrProcessorLoad` | `cpu/{index}/load` |
| `ipForwarding.0` | `ip/forwarding` |
| `ipDefaultTTL.0` | `ip/default_ttl` |
| `ipInReceives.0` | `ip/in_receives` |
| `ipAdEntAddr` | `ip/{index}/addr` |
| `ipAdEntIfIndex` | `ip/{index}/if_index` |
| `ipAdEntNetMask` | `ip/{index}/netmask` |

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
| `ups_on_battery` | `upsOutputSource != normal(3)` — **including `bypass(4)`**, which means the load is running unprotected | critical on `battery(5)`/`none(2)`, warning otherwise |
| `ups_battery_low` | `upsBatteryStatus` is `low(3)` or `depleted(4)`. **Not `unknown(1)`** — that is the UPS saying it does not know, and paging on a missing measurement is not the same as paging on a fault | critical |
| `ups_runtime_low` | `upsEstimatedMinutesRemaining` below `minutes` — **no default; unset never fires** | critical |
| `ups_load_high` | `upsOutputPercentLoad` above `percent` — **no default** | warning |
| `pdu_outlet_off` | an outlet listed in `expect_on` reads off. An outlet the device did not report is **not** an outage, and a transition (Eaton `pendingOn`, Raritan `cycling`) is not off | critical |
| `pdu_overload` | the PDU's **own** load verdict says near/over (APC `rPDU2DeviceStatusLoadState`), or inlet load is above `percent` — **no default** | warning |
| `nas_array_degraded` | a RAID group or ZFS pool is degraded/crashed/faulted. A Synology array that is **repairing, expanding, migrating or syncing** is a planned operation and fires nothing | critical |
| `nas_disk_failed` | a physical disk reports a failure. An **empty bay** (QNAP `noDisk`) and an appliance that declines to answer (`unknown`) are neither | critical |
| `nas_volume_full` | a RAID group / ZFS pool above `percent` used — **no default**. Distinct from `storage_usage`: hrStorage lists mounted *filesystems*, and a pool at 95 % under a half-empty filesystem is exactly what it cannot see | warning |

The last nine read what the `ups`, `pdu-*` and `nas-*` profiles walk (see *Device
profiles* below). A device with neither profile produces no observation for
them, so they reconcile empty every sweep and fire nothing — which is why they
can default to enabled without an ordinary switch paying for the UPS tree.
Unlike the interface rules, **their columns are not auto-added to the walk
set**: pinning or matching a profile is what turns them on.

**Why three of them ship without a number.** A five-minute line-interactive UPS
under a switch and a sixty-minute one under a rack have different answers, and
"80 % loaded" is a property of how a site sized its power, not of power. A
default here would page the whole fleet the first time it ran. Both migrate to
the shared thresholds vocabulary when #931 lands.

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
| `snmp.allow_insecure_versions` | bool | **`false` by default (#825).** A community string is a cleartext credential on the wire, so a configured v1/v2c device — or any `trap_listener.communities` entry — refuses to start until this flag says, explicitly, that you accept the cost. SNMPv3 authPriv is the shipped default and what `configs/snmp.json5` leads with. |
| `trap_listener.enabled` | bool | Enable the SNMP trap receiver. |
| `trap_listener.bind` | string | Trap listen address (default `0.0.0.0:162`). |
| `trap_listener.communities` | string[] | Accepted v1/v2c communities; empty = accept any. |
| `trap_listener.users` | object[] | SNMPv3 notification users (same schema as device `security`; `engine_id` ignored — the receiver is authoritative). |
| `trap_listener.alerts` | object[] | Trap → alert rules: `{ rule, fire: <OID>, resolve?: <OID>, severity }`. |
| `trap_listener.builtin_rules` | bool | Include the built-in linkDown/linkUp mapping (default true). |
| `trap_listener.engine_state_path` | path? | Where the v3 authoritative engine identity + boots counter persist (#650). Unset = `STATE_DIRECTORY` / XDG state default. See *SNMPv3 receiving identity* below. |
| `devices[]` | array | Devices to poll (see below). |
| `oid_groups` | map | Named, reusable `{ oids, walks }` sets referenced by `device.oid_group`. |
| `oid_names` | map | OID→metric-name map; `{index}` is substituted with the table index. |
| `resilience.backoff_cap` | u32 | Max poll-interval multiplier under failure (default 10). |
| `resilience.breaker_after` | u32 | Fully-failed cycles before probe-only polling (default 3). |
| `resilience.jitter_percent` | u8 | Per-cycle scheduling jitter (default 10). |
| `evidence.enabled` | bool | Publish observed-device identity claims (#537, default true). |
| `evidence.refresh_cycles` | u32 | Claim refresh cadence in poll cycles (default 10). |
| `mib.load_builtin` | bool | Load bundled MIB definitions. |
| `mib.files` | string[] | **Removed** (#580, deprecated in #532): legacy JSON pseudo-MIBs. Setting it fails startup with a pointer to `mib.dirs`. |
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
| `max_pdus_per_sec` | f64? | PDU ceiling for **this** device (#825). Absent = none. See "The per-device PDU budget" below. |
| `max_concurrent` | usize? | Outstanding operations against **this** device (#825). Absent = unbounded. |
| `oids` | string[] | Individual OIDs polled with GET. |
| `walks` | string[] | OID subtrees polled with WALK (GETBULK on v2c/v3, GETNEXT on v1; tooBig responses are recovered by bisection). |
| `oid_group` | string? | Reference a predefined `oid_groups` entry instead of inline `oids`/`walks`. |
| `profile` | string? | Pin a device profile by name instead of `sysObjectID` prefix matching (defaults still apply). An unknown name fails startup. |
| `credentials` | string? | Name a `snmp.credentials` set instead of spelling the community/v3 secrets inline (#538). |
| `alerts` | object? | Per-device alert rules. **Replaces** the whole `snmp.alerts` block for this device — it is not a field merge, so a partial block silently reverts every other rule to its default. |

### `security` (SNMPv3)

`username`, `auth_protocol` (`MD5`/`SHA`/`SHA224`/`SHA256`/`SHA384`/`SHA512`),
`auth_password`, `priv_protocol` (`DES`/`3DES`/`AES`/`AES192`/`AES256`/
`AES192-REEDER`/`AES256-REEDER` — `AES192`/`AES256` extend a short localized
key the Blumenthal way, as net-snmp does; Cisco gear wants the `-REEDER` form,
also spelled `-CISCO`),
`priv_password`, optional `engine_id`.

A configured `engine_id` (hex, `0x`/`:` tolerated) pre-seeds the engine cache
and skips the discovery round-trip; it requires a literal `ip:port` device
address (hostnames fall back to auto-discovery, the default). If a device
comes back with a **different** engine identity (agent replaced/reset), the
poller notices the all-auth-failure cycle and asks the client to rediscover
the engine in place (`rediscover_engine`, async-snmp 0.17 / #577) — no
sensor restart needed.

## Subnet discovery (#541)

**Opt-in, propose-only.** No `snmp.discovery` block ⇒ no scanning, ever.
With one, the sensor sweeps the configured IPv4 CIDRs every
`interval_secs` (default 3600) with bounded concurrency
(`max_concurrency`, default 8) and fast-fail probes (1 s, no retries),
trying the named credential sets in order (plain `public` if none). A
responder is identified by sysObjectID/sysName/sysDescr, matched against
the device profiles (#531), and published — **never auto-added** — in a
`DiscoveryReport` state doc on `state/snmp/discovery` with a
copy-pasteable `devices[]` snippet per device.

```json5
discovery: {
  subnets: ["192.168.1.0/24"],
  credentials: ["readonly-v2c"],  // named sets (#538), tried in order
  // interval_secs: 3600, port: 161, max_concurrency: 8,
}
```

Safety and dedup decisions (the issue's research questions):

- The combined sweep size is hard-capped at 4096 addresses — a typo'd `/8`
  fails startup instead of scanning.
- **Never re-proposed**: configured device addresses, plus every IP the
  pollers' identity evidence (#537) has observed — so a configured device
  answering on a second interface is recognized once its ipAddrTable has
  been walked, without any extra config.
- `auto_add` is deliberately **not** implemented: proposal is the only
  mode. Adopting a device stays an operator action (paste the snippet).
- An SNMP sweep can trip IDS in some environments — keep it scoped to
  networks you operate.

### One-shot discovery: `--discover <cidr>` (#825 item 4)

*"The gap between 'supported' and 'usable' for SNMP is always the config."*
The block above answers **"what appeared on my network since I last looked"**;
this answers **"what is out there right now, so I can write a config"** — and
it is the one that makes the sensor pleasant to adopt.

```bash
zensight-sensor-snmp --config configs/snmp.json5 --discover 10.0.0.0/24 \
    > devices.json5
```

It sweeps, identifies what answers, **prints a proposed config to stdout**, and
exits. It never applies anything, and **it never touches the bus**: the runner,
the session and the publishers are not constructed at all on that path. That is
structural rather than a promise — an operator sweeping a subnet from a laptop
should not thereby join a fleet.

Diagnostics go to stderr and the proposal to stdout, so the redirect above
yields a file that is *only* the proposal. Each device is annotated with what it
said about itself (sysName, sysObjectID, a one-line bounded sysDescr, matched
profiles), and the header says three things an operator needs before pasting:
the name becomes the device slug in every key; a device that answered a
community answered a **cleartext** credential and needs
`allow_insecure_versions` (item 1); and anything older or smaller than the
machine polling it wants a `max_pdus_per_sec` (item 2).

Nothing answering prints a sentence rather than an empty file — *silence from
an SNMP agent is indistinguishable from silence from a filtered port* — and
exits 0, because that is a finding, not a failure of the sweep.

| Flag | Default | |
|---|---|---|
| `--discover <CIDR>` | — | the sweep, and the mode switch |
| `--discover-credentials <NAME>` | every set in the config | named sets (#538), tried in order |
| `--discover-port` | 161 | |
| `--discover-timeout` | 1 | seconds per probe |
| `--discover-concurrency` | 32 | |

Addresses already in `snmp.devices` are skipped, so running this against a
subnet you already monitor returns the **new** devices rather than a copy of
your own config. The 4096-address cap applies here too.

## The per-device PDU budget (#825 item 2)

An SNMP sensor's characteristic failure is **hammering a device weaker than
itself** — an eight-year-old switch CPU, or a UPS management card that reboots
under load. Before #825 each device polled on its own timer with no cap on
outstanding requests and no ceiling on PDU rate: correct, and entirely
dependent on the operator having chosen a gentle interval.

This is the SNMP-shaped instance of the fleet-wide budget work in #812. The
resource being bounded is **someone else's device**, which is why it is
declared per device: one switch's tolerance says nothing about another's.

```json5
devices: [
  { name: "old-switch", address: "10.0.0.2:161", version: "v3", /* … */
    max_pdus_per_sec: 20,   // token bucket; burst = one second's worth
    max_concurrent: 2 },    // outstanding operations against THIS device
]
```

Both are optional and independent; absent (or `0`) means no ceiling, so every
deployment from before #825 behaves exactly as it did.

**Honest accounting.** A GET is one PDU and is charged one token *before* it is
issued. A walk is **not** charged in advance: the client issues GETBULK
requests carrying up to `max_repetitions` rows each, and how many that takes is
not knowable before the table is read. Estimating it would make
`max_pdus_per_sec` a number that means something other than what it says. So a
walk is debited **after it completes**, from the rows it really returned —
`ceil(rows / max_repetitions) + 1` for GETBULK, `rows + 1` for the GETNEXT
fallback on v1. A large table therefore drains the bucket and delays the *next*
operation, which is the behaviour wanted: the device gets a rest proportional
to the work it just did.

**Over budget the poller waits.** It never drops a poll. A sensor that skips
work to stay under budget has traded the device's health for a gap in its own
telemetry, which is the wrong trade — and it is pinned by an e2e test that
measures the wall clock against a live agent and then asserts every row still
arrived.

`max_concurrent` gates outstanding operations, and a walk holds its slot for
its whole duration — that is the part that bounds concurrent load on the
device, as distinct from the rate.

Bulk-walking itself is not new: the client has picked GETBULK for v2c/v3 since
#559, honours `max_repetitions`, and is pinned by `v2c_walk_uses_getbulk`.

## Resilience (#539)

Error handling adapts instead of hammering dead devices:

- **Backoff**: consecutive fully-failed cycles double the poll interval
  (2×, 4×, …) up to `resilience.backoff_cap` × base (default 10×); the
  first success snaps back to the base cadence.
- **Circuit breaker**: after `resilience.breaker_after` fully-failed cycles
  (default 3) the poller sends only a cheap sysUpTime probe per cycle
  instead of the full OID set; one successful probe closes the breaker and
  the next cycle polls fully. Pairs with the `device_unreachable` alert.
- **Jitter**: each device's start phase is randomized across its interval
  and every cycle gets ±`resilience.jitter_percent`% scheduling jitter
  (default 10), so a fleet never fires synchronized bursts.
- **No dropped devices**: a device whose client cannot be built at startup
  is retried by the poll loop with the same backoff — it starts working
  when it comes online, no sensor restart.
- **Health accuracy**: every cycle records per-device success/failure and
  poll duration into the health doc (`devices_responding`/`devices_failed`,
  consecutive-failure counts, last-seen), visible in the GUI sensors view.

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
OID sets. Eleven profiles ship **embedded in the binary**:

| Profile | Match | Polls |
|---------|-------|-------|
| `generic-device` | default | SNMPv2-MIB system group |
| `network-interfaces` | default | IF-MIB ifTable + ifXTable |
| `host-resources` | extend/pin | hrStorage descr/units/size/used + hrProcessorLoad |
| `entity-sensors` | extend/pin | entPhySensorTable type/scale/value/status |
| `ups` | `1.3.6.1.2.1.33`, APC `…318.1.3.2`, Eaton `…534` | UPS-MIB (RFC 1628): battery status/charge/runtime, input + output tables, output source, alarms |
| `pdu-apc` | `1.3.6.1.4.1.318.1.3.4` | PowerNet rPDU2 switched + metered outlet tables, device load state and power |
| `pdu-eaton` | `1.3.6.1.4.1.534.6.6.7` | EATON-EPDU outlet designator/control status/current, inlet current and percent load |
| `pdu-raritan` | `1.3.6.1.4.1.13742` | Raritan PDU outlet label/state/current |
| `nas-synology` | `1.3.6.1.4.1.6574` | + `host-resources`: system/power/fan status, RAID group name/status/free/total, per-disk id/status/temperature/remaining life |
| `nas-qnap` | `1.3.6.1.4.1.24681` | + `host-resources`: per-disk id/status/temperature/SMART summary, volume name/filesystem/size **as text** |
| `nas-truenas` | `1.3.6.1.4.1.50536` | + `host-resources`: ZFS pool name/health/size/used/available and per-pool IO counters |

### Power: one set of names, three vendor trees (#955)

There is no standard PDU MIB — RFC 1628 covers UPSes and stops at the outlet —
so each `pdu-*` overlay maps **its own** tree onto the **same** metric names
(`pdu/outlet/{index}/state`, `…/current_ma`). A rule, a dashboard and a query
never have to know which brand answered. Three things follow that are worth
knowing before extending them:

- **`{index}` for an outlet is the table index verbatim**, however many
  integers it is. An Eaton ePDU indexes its outlet tables by `unit.outlet`, so
  `"1.3"` is as legal an outlet id as `"3"` — which is why `expect_on` takes
  strings.
- **A scale lives in the name, never in the value.** `voltage_dv` is decivolts
  because that is what RFC 1628 puts on the wire, and `current_ma` and
  `current_da` are separate families because summing milliamps with tenths of
  an amp is silently meaningless. Rescaling in the poller would need a per-OID
  scale table, and a scale applied in the wrong place is a wrong number nobody
  can see.
- **No vendor OID is shipped unverified.** Every number in these four profiles
  was read out of the vendor MIB (APC PowerNet-MIB v4.5.8) or out of the OID
  set NUT drives that hardware with (Eaton Marlin, Raritan PX). A guessed OID
  does not fail loudly; it publishes a plausible number under a right-looking
  name, which is worse than publishing nothing. That is also why the `ups`
  profile carries the APC and Eaton match prefixes but adds **no** vendor
  OIDs on top of the standard tree: both implement RFC 1628, and what they add
  beyond it needs their MIB in front of us.

Validation against real hardware is still outstanding — see the caveats at the
end of this page.

### NAS: the appliance, on top of the client view (#960)

From the **client** side a NAS is already covered: `hostspec` asserts the mount
is present with the right options, `sysinfo` publishes per-mount space, inodes
and a time-to-full, `probe` checks the service answers. What none of them can
see is the box — which is why the three `nas-*` profiles
`extends = ["host-resources"]` rather than replacing it: hrStorage keeps giving
the capacity floor even on an appliance whose vendor MIB is switched off, and
the vendor tree adds the array and disk health hrStorage has no concept of.

`nas/array/*` (Synology RAID groups) and `nas/pool/*` (TrueNAS ZFS pools) are
separate families on purpose. The columns are genuinely different — a pool
reports used and size in *its own* allocation units, a RAID group free and
total in bytes — and one family with half its columns absent per vendor would
tell a consumer less, not more. The rules read both.

**QNAP is where `extends` earns its place.** Its volume table reports total
size, free size *and* status as `DisplayString`s — `"2.75 TB"`, `"Ready"` — not
integers. Parsing a vendor's free-form size string is how a monitor starts
reporting confident wrong numbers, so those three are published as text, no
rule reads them, and `storage_usage` on hrStorage (which QNAP serves properly)
is the capacity rule for a QNAP. Its disk table *is* an enum, and
`nas_disk_failed` reads it — including the detail that its `hdStatus`
DESCRIPTION contradicts its own SYNTAX, and the SYNTAX is what the device
sends.

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

The harness maps OIDs through lowercase `oid_names` (grammar-valid chunks).
Built-in MIB names satisfy the grammar too — #559 fixed that, and
`built_in_mib_names_are_chunk_grammar_valid` in `src/mib.rs` is what keeps it
fixed.

The simulated agent serves **`1.3.6.1`**, not just `mib-2`: the vendor PDU
profiles live under `1.3.6.1.4.1`, and a fixture the agent does not serve
answers nothing at all — which every rule reading it would score as "healthy".
A fake that agrees with any assertion is worse than no fake.
`SimMib::with_ups_mib()`, `::on_battery()` and `::with_pdu_outlets(n)` are the
power fixtures.

## SNMPv3 receiving identity (#650)

When `trap_listener.users` is set, this sensor is an **authoritative SNMP
engine** — not because it sends anything, but because informs are authenticated
against *its* `snmpEngineID` and `(boots, time)` window, and it signs the
automatic acknowledgement with them. RFC 3414 §2.2 therefore requires a stable
engine ID and a `snmpEngineBoots` counter that increments and persists across
restarts; the ±150 s time window is the only replay defence authenticated
messages have.

Through 0.11 the receiver minted a fresh identity every start. That is worse
than it sounds: a sender that has already discovered this engine does not merely
re-handshake, it has its informs **dropped outright** (localized to an engine ID
the receiver no longer has) until it rediscovers. The identity is now persisted.

| Situation | Behaviour |
|---|---|
| A durable location resolves | `(engine_id, boots)` persist; each start increments boots and reuses the ID |
| No location resolves at all | Per-start ephemeral identity, with one warning. This deployment never asked for durability, and refusing would turn an upgrade into an outage |
| The location resolves but cannot be written | **v3 receiving is refused**; v1/v2c listening continues. You asked for durability and did not get it, so you hear about it now rather than from a sender later |
| The stored file is missing or corrupt | A fresh identity is installed — the old value is unusable, so there is nothing to preserve |
| `boots` has latched at 2147483647 | A **new** engine ID is minted. Restarting into a latched engine rejects all authenticated inbound (RFC 3414 §2.2), i.e. a silently dead receiver |

Resolution order for the location: `trap_listener.engine_state_path`, then the
systemd `STATE_DIRECTORY`, then `$XDG_STATE_HOME/zensight`, then
`~/.local/state/zensight`. The shipped unit sets
`StateDirectory=zensight-snmp`; under `ProtectSystem=strict` that is the only
writable path, so **a unit without it lands in the "refused" row**.

Writes are atomic (temp file, fsync, rename), which fails in the safe
direction: a crash after the rename leaves boots at or above the value actually
used, never below — below is what would re-open a replay window.

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
- **The UPS and PDU profiles have never met the hardware they describe (#955).**
  They are built and tested against the in-process simulated agent, and every
  OID in them was read out of a vendor MIB or out of NUT's driver for that
  hardware rather than remembered — but a fake is not a UPS. The requirement
  that motivated them (SYS-SUP-002) also notes the UPSes are **not yet on the
  network**: implementation could proceed, validation cannot, and it needs a
  management card or a NUT/serial gateway first. A gateway would be a
  different sensor (NUT is not SNMP) and is not designed here. Treat first
  contact with a real UPS or PDU the way #947 treats Proxmox and podman: as
  work still to do, not as work the fake has done.
- **The NAS profiles have not met an appliance either (#960)**, and the same
  rule applied: every OID was read out of the vendor MIB (SYNOLOGY-SYSTEM-,
  -RAID- and -DISK-MIB, QNAP's NAS-MIB, FREENAS-MIB) rather than remembered.
  `nas-truenas` ships **only** the zpool table: the dataset and zvol tables
  exist at `…50536.1.2` / `.1.3`, but the table-versus-entry level was not
  confirmed against the MIB itself, and an OID one arc wrong publishes a
  plausible number under a right-looking name.
- The `ups` profile deliberately carries **no vendor OIDs** on top of RFC 1628,
  and `pdu-raritan` maps the legacy `13742.1` tree rather than PDU2-MIB, for
  the same reason: an OID guessed from memory publishes a plausible number
  under a right-looking name, and nothing downstream can tell. Extending
  either needs the vendor MIB, or a device to check against.
