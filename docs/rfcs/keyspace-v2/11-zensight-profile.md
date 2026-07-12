# 11 — Reference Application Profile: ZenSight

**Status: v1.0 (ratified)** · informative chapter

The convention chapters (02–10) are application-neutral. This chapter binds
them to ZenSight: the concrete base, producers, service origins, and a
conceptual mapping from every shipped key family to its home under the
convention. It is a *profile*, not a migration plan — sequencing,
coexistence, and code changes are explicitly out of scope
([01-motivation.md §5](01-motivation.md)).

---

## 1. Profile constants

| Convention slot | ZenSight binding |
|---|---|
| `<base>` | `zensight` — set as the session `namespace` in the shared zenoh config block (overridable per deployment / `ZENSIGHT_ZENOH_*`), so no crate ever concatenates it ([03-grammar.md §1.1](03-grammar.md)) |
| version | `@v1` |
| host origin | `h-<12hex>` from `sha256(machine-id + app salt)` — the same value the correlator uses today as `host_id`/`entity_id` (currently spelled `h_<12hex>`; the profile normalizes the separator to `-`) |
| service origins | `@catalog` (implemented by `zensight-correlator`) |
| producers | `snmp`, `logs`, `netflow`, `modbus`, `sysinfo`, `gnmi`, `netlink`, `netring`, `systemd`, `parallax` (+ `-<instance>` when doubled on one host) |
| payload default | CBOR, first-byte sniff for JSON interop (unchanged) |

Proxy producers (`snmp`, `modbus`, `gnmi`, `netflow`) put the observed
device as the first subject chunk; host-local producers (`sysinfo`,
`netlink`, `netring`, `systemd`, `logs`, `parallax`) start the subject at
the metric directly — the origin already names the host, so the incumbent
`<source>` hostname chunk disappears for them.

**Delivery tier, honestly.** The shipped sensors currently run the
advanced tier per-key across the *whole telemetry fan* (cache 10 +
heartbeat 5 s + publisher detection on every telemetry key) — a default
that predates the convention's cost analysis and lands squarely in
[04-planes.md §3.3](04-planes.md)'s "what the tier is NOT for" (wide fans:
4 entities and 2 network-wide declarations per key, heartbeat load
proportional to key count). Under this profile the target posture is the
baseline ([04 §3.2](04-planes.md)) for telemetry — the deployment already
runs the storages — with the tier retained only where its decision rule
holds: alerts and expectation/config echoes (`detect_s ≪ ttl_s`,
`sporadic_heartbeat`), and evidence documents keep their shipped
cache-only depth 1.

## 2. Worked examples per sensor

```
# sysinfo (host-local)
zensight/@v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage
zensight/@v1/h-3fa9c2d41b7e/telemetry/sysinfo/memory/usage_percent
zensight/@v1/h-3fa9c2d41b7e/@rpc/sysinfo/processes?sort=cpu;top=20

# snmp (proxy: device first subject chunk)
zensight/@v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime
zensight/@v1/h-3fa9c2d41b7e/state/snmp/device/router01/liveness
zensight/@v1/h-3fa9c2d41b7e/state/snmp/device/router01/alive          (liveliness token)

# netlink
zensight/@v1/h-3fa9c2d41b7e/telemetry/netlink/sockets/tcp/established
zensight/@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1
zensight/@v1/h-3fa9c2d41b7e/@rpc/netlink/sockets?ip=10.0.0.7
zensight/@v1/h-3fa9c2d41b7e/@rpc/netlink/expectations/set

# netring
zensight/@v1/h-3fa9c2d41b7e/telemetry/netring/flow/red/p95_ms
zensight/@v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7
zensight/@v1/h-3fa9c2d41b7e/events/netring/capture/01jgxqz4yqk8v6txw3m9f2a7cd
zensight/@v1/h-3fa9c2d41b7e/@rpc/netring/capture/trigger

# netflow (proxy; REDESIGNED, not migrated as-is — see §3)
zensight/@v1/h-3fa9c2d41b7e/telemetry/netflow/exporter01/flows_per_second
zensight/@v1/h-3fa9c2d41b7e/telemetry/netflow/exporter01/top/talkers/1
zensight/@v1/h-3fa9c2d41b7e/@rpc/netflow/flows?src=10.0.0.1;dst=10.0.0.2;max=500

# modbus (proxy)
zensight/@v1/h-3fa9c2d41b7e/telemetry/modbus/plc01/holding/40001
zensight/@v1/h-3fa9c2d41b7e/state/modbus/device/plc01/liveness

# gnmi (proxy; open-depth subject via the {path...} rest-variable, 08 §2;
# shipped bracketed path-elements are slugged per 03 §2:
# interfaces/interface[name=eth0]/state → interfaces/interface/eth0/state —
# the gNMI key list collapses into a chunk, original path in the payload)
zensight/@v1/h-3fa9c2d41b7e/telemetry/gnmi/router01/interfaces/interface/eth0/state/counters/in_octets

# systemd
zensight/@v1/h-3fa9c2d41b7e/telemetry/systemd/unit/sshd.service/active
zensight/@v1/h-3fa9c2d41b7e/@rpc/systemd/action                       (gated write)

# logs (per-line detail is pull-only, P9)
zensight/@v1/h-3fa9c2d41b7e/telemetry/logs/by_severity/error
zensight/@v1/h-3fa9c2d41b7e/@rpc/logs/events?since=1720000000000;max=500

# parallax (media)
zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main
zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg
zensight/@v1/h-3fa9c2d41b7e/state/parallax/stream/cam0                (catalogue+status doc)
zensight/@v1/h-3fa9c2d41b7e/telemetry/parallax/cam0/stats/fps

# framework (every sensor, via sensor-core)
zensight/@v1/h-3fa9c2d41b7e/state/<producer>/health
zensight/@v1/h-3fa9c2d41b7e/state/<producer>/errors
zensight/@v1/h-3fa9c2d41b7e/state/<producer>/sensor                   (registration doc)
zensight/@v1/h-3fa9c2d41b7e/state/<producer>/alive                    (liveliness token)
zensight/@v1/h-3fa9c2d41b7e/state/<producer>/evidence/self

# catalog (zensight-correlator)
zensight/@v1/@catalog/state/entity/h-3fa9c2d41b7e
zensight/@v1/@catalog/state/alias/h-9d02aa17c44f
zensight/@v1/@catalog/state/pdns/93-184-216-34
zensight/@v1/@catalog/@rpc/names?ip=93.184.216.34
```

## 3. Mapping: every shipped family → its convention home

Conceptual correspondence (shipped grammar per
[`docs/KEYSPACE.md`](../../KEYSPACE.md)):

| Shipped family | Convention home | Notes |
|---|---|---|
| `zensight/<proto>/<source>/<metric>` | `…/<origin>/telemetry/<proto>/[<device>/]<metric>` | `<source>` → origin (host-local) or first subject chunk (proxy) |
| `…/<source>/@/health` `@/errors` `@/status` | `…/<origin>/state/<proto>/health` etc. | status doc merges into health/registration |
| `…/<source>/@/alive` (+ devices) | `…/state/<proto>/alive`, `…/device/<d>/alive` | token keys mirror state grammar ([04-planes.md §5](04-planes.md)) |
| `…/<proto>/@/alerts/<key>` | `…/<origin>/state/<proto>/alert/<key>` | key function **changes**: shipped = `<rule>-<16hex>` of FNV-1a(source+rule+labels), case-preserving; convention = 16 lowercase hex of FNV-1a(rule+labels) — source dropped (origin+producer are in the key), rule prefix dropped (uppercase rules violate the charset). Normative definition: [04-planes.md §1.2](04-planes.md) |
| `…/<proto>/@/query/alerts` | GET `…/*/state/*/alert/*` | seed = state itself ([05 §4](05-control-rpc.md)) |
| `…/@/commands/<t>` + `@/status/<t>` + `@/query/<t>` | `…/<origin>/@rpc/<proto>/…` | full table in [05-control-rpc.md §5](05-control-rpc.md) |
| `…/@/artifact/{request,status,cancel}` | `@rpc` + `state/<proto>/artifact/<kind>` | long-running pattern ([05 §3](05-control-rpc.md)) |
| `…/@/artifact/blob/<id>/**`, `…/@/store/**`, `…/@/tree/**` | `…/<origin>/@blob/{artifact,store,tree}/…` | one plane ([07-bulk-planes.md](07-bulk-planes.md)) |
| `…/<source>/@media/<stream>/…` | `…/<origin>/@media/parallax/<stream>/…` | producer chunk added |
| `_meta/sensors/<name>/<source>` | `…/<origin>/state/<proto>/sensor` | |
| `_meta/evidence/host/<sensor>/<source>` | `…/<origin>/state/<proto>/evidence/self` (or `…/evidence/device/<d>`) | observer split by subject, not payload flag |
| `_meta/evidence/names/<sensor>/<ip>` | `…/<origin>/state/<proto>/evidence/names/<ip>` | |
| `_meta/entity/host/<id>` | `@catalog/state/entity/<id>` | |
| `_meta/query/{entities,names}` | GET on entity state / `@catalog/@rpc/names` | |
| `_meta/correlator/@/alive` | `@catalog/state/alive` | |
| `zensight/@pdns/<ip>` | `@catalog/state/pdns/<ip>` | historical tier = storage choice ([06 §5.2](06-identity.md)) |
| `…/<source>/@/devices/<d>/liveness` | `…/state/<proto>/device/<d>/liveness` (doc) + `…/device/<d>/alive` (token) | |
| netflow `zensight/netflow/<exp>/<src>/<dst>` | **no as-is home — redesigned** | per-flow-pair keys are unbounded-cardinality per-message data ([03 §2](03-grammar.md), [04 R3](04-planes.md)): the family becomes bounded rollups on `telemetry` + on-demand `@rpc/netflow/flows` (§2) |
| gnmi bracketed paths | slugged `{path...}` subject | shipped `[name=eth0]` charset is illegal under [03 §2](03-grammar.md); see §2's slug rule |

Every shipped family has a mapped home, with two honest asymmetries.
*Forward*: netflow's per-pair telemetry and gNMI's bracketed paths do
**not** migrate as-is — their shipped shapes violate the grammar's
cardinality/charset rules and are redesigned above (the
[01 §5](01-motivation.md) "vocabulary migrates as-is" non-goal is
qualified accordingly). *Reverse*: a few convention mechanisms have no
shipped counterpart and are marked as new — `alias/<old-id>` records as
keys (shipped aliases are a payload field on `HostEntity`), the `events`
class's budget machinery, and the ownership-claim keys of
[06 §5.3](06-identity.md). (The one deliberate deletion: protocol-scoped
shared channels have no successor; their two uses — fan-in queries and
fleet-wide commands — are both expressed by `*`-origin RPC selectors.
The `@/status` running/offline flag lands in the `health` document's
status field.)

## 4. What ZenSight-specific knowledge remains

For other adopters, the checklist of what they would replace: the base
chunk; the producer vocabulary and their registry files
([08-registry.md](08-registry.md)); the payload types (`TelemetryPoint`,
`Alert`, `HealthSnapshot`, `HostEntity`, `FrameMeta`…); the catalog
implementation behind `@catalog`; and the application salt constant of the
origin derivation (ZenSight's is `"zensight-host-id-v1"`, compiled-in and
non-configurable — [06-identity.md §1](06-identity.md)). Everything else
in chapters 02–10 transfers unchanged.
