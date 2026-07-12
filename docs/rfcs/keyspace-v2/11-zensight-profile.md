# 11 — Reference Application Profile: ZenSight

**Status: Draft** · informative chapter

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
| `<base>` | `zensight` (configurable per deployment, as today) |
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
zensight/@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7
zensight/@v1/h-3fa9c2d41b7e/@rpc/netlink/sockets?ip=10.0.0.7
zensight/@v1/h-3fa9c2d41b7e/@rpc/netlink/expectations/set

# netring
zensight/@v1/h-3fa9c2d41b7e/telemetry/netring/flow/red/p95_ms
zensight/@v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7
zensight/@v1/h-3fa9c2d41b7e/events/netring/capture/01JGXQZ4YQK8V6TXW3M9F2A7CD
zensight/@v1/h-3fa9c2d41b7e/@rpc/netring/capture/trigger

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
| `…/<proto>/@/alerts/<key>` | `…/<origin>/state/<proto>/alert/<key>` | same `alert_key` hash; now origin-scoped |
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

Nothing in the shipped keyspace lacks a home, and nothing in the convention
exists without a shipped counterpart exercising it — the profile is
closed both ways. (The one deliberate deletion: protocol-scoped shared
channels have no successor; their two uses — fan-in queries and
fleet-wide commands — are both expressed by `*`-origin RPC selectors.)

## 4. What ZenSight-specific knowledge remains

For other adopters, the checklist of what they would replace: the base
chunk; the producer vocabulary and their registry files
([08-registry.md](08-registry.md)); the payload types (`TelemetryPoint`,
`Alert`, `HealthSnapshot`, `HostEntity`, `FrameMeta`…); the catalog
implementation behind `@catalog`; and the salt of the origin derivation.
Everything else in chapters 02–10 transfers unchanged.
