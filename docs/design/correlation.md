> **Archived design doc.** Historical rationale, implemented in 0.7.0. For current
> operator/reference documentation see [`zensight-correlator/`](../../zensight-correlator/)
> and its `docs/`, plus the canonical [`docs/KEYSPACE.md`](../KEYSPACE.md).

# Cross-Sensor Identity Correlation — Analysis, Architecture & Proposal

*Status: implemented — 2026-07-04 (Part II design realized as `zensight-correlator`
+ the `_meta/evidence/**` and `_meta/entity/**` keyspaces; retained as the design
rationale). See `docs/KEYSPACE.md` §4 for the authoritative as-built keyspace.*

This document analyzes what ZenSight could correlate across sensors (PID ↔ process
metadata, socket ↔ process, IP ↔ hostname, flow ↔ process, unit ↔ PID, host identity
across protocols), what identity data we already capture, what the industry does,
**where the correlation logic should live**, and a phased plan. Backward
compatibility is explicitly allowed to break.

---

## 0. Executive summary

**The questions, answered up front:**

1. **Do we need new sensors? No.** All required identity data is observable by the
   sensors we already ship — the gaps are *inside* them (DNS answers in netring,
   socket→PID in netlink, MainPID in systemd, cmdline in sysinfo). What we do need
   is new *shared infrastructure*: a **host-identity module in `zensight-sensor-core`**
   so every sensor stamps the same host identity, instead of five sensors
   independently calling `hostname::get()` (today the config field isn't even
   consistently named: `hostname` in sysinfo/netlink vs `source` in systemd).
   §9.5 lists sensor candidates considered and rejected.

2. **Do we need a correlation service? Yes — a new `zensight-correlator` crate.**
   Fleet-wide entity resolution (which devices are the same host; which names an IP
   has) needs a fleet-wide view that no sensor has, and doing it in the frontend
   means N frontends = N independent, possibly divergent resolvers while exporters
   and headless deployments get nothing. Every production system studied converges
   on the same shape (§8): a single-writer service that consumes raw streams and
   publishes **materialized entity state** the consumers just read.

3. **Do Zenoh storages change the architecture? They refine it, not redefine it
   (§11).** A storage is a materialized cache with query — it stores and serves
   keys but runs no merge logic, so it cannot replace the correlator (compute).
   What storages add, as **optional deployment tiers** on the zenohd router we
   already ship for the blob store (`configs/router-blob-storage.json5`):
   evidence/entity **durability** across sensor and correlator outages
   (offline-asset inventory), delegated late-joiner seeding, and — with the
   InfluxDB backend — the **historical pDNS tier** ("what did this IP resolve to
   last Tuesday") that the live design deliberately left out.

4. **How to architect it: three tiers, each with a distinct job (§8–§9):**
   - **Sensors** enrich with what only that host can see, at capture time, before
     the join key evaporates (socket↔PID, PID↔unit via cgroup, PID↔container), and
     publish standardized identity **evidence** — they never resolve fleet entities.
   - **`zensight-correlator`** is the single writer of a derived
     `zensight/_meta/entity/**` keyspace: it merges evidence into Host entities and
     an IP↔name map, with periodic state re-emission for late joiners. If it's
     down, the system degrades to exactly today's uncorrelated behavior.
   - **The frontend** does display-time joins against materialized entities only.

**Key findings from the code inventory (Part I):**

- We already have a correlation mechanism and it is **dead code**:
  `zensight-sensor-core/src/correlation.rs` publishes to
  `zensight/_meta/correlation/<ip>`, the frontend fully consumes it
  (`subscription.rs:398`, `topology/mod.rs:170`) — but no sensor ever publishes.
- Most identity data already exists, siloed per sensor: netring's passive asset
  inventory (MAC↔IP↔hostname↔vendor), netlink's neighbor table (IP↔MAC), journald's
  `_CMDLINE`/`_EXE`/`_MACHINE_ID`, systemd's cgroup tree (unit↔PID+comm).
- Highest-value data gaps: socket→PID attribution (netlink), passive-DNS answers
  (netring parses questions only), process metadata depth (sysinfo has no
  cmdline/exe/cgroup/start_time), unit MainPID/InvocationID (systemd).
- **Env vars: do not capture by default** — secrets risk (§6).
- Adopt **OTel semantic-convention names** for identity labels; process identity is
  always `(process.pid, process.start_time)`, never a bare PID (§5).

---

# Part I — What can be correlated (data inventory & opportunities)

## 1. Current state — the dead registry

- `zensight-sensor-core/src/correlation.rs` — `CorrelationEntry { ip, hostnames,
  sensors, sources, last_updated }` published to `zensight/_meta/correlation/<ip>`.
  Strictly IP ↔ hostname ↔ (sensor, source); no MAC/PID/inode/cgroup dimensions.
- Grep across all 20 crates: only the definition and its unit tests reference it.
  Never populated → the frontend plumbing (`Message::CorrelationReceived`,
  `topology.apply_correlations`, `topology_ip_to_node` for flow edges) is inert.
- `docs/SENSOR-REDESIGN-ANALYSIS.md:727` already flags `CorrelationEntry` as "the
  only cross-sensor join" and `:803` proposes a `Host` join key that doesn't exist yet.

## 2. Identity data per sensor

| Sensor | Identity data captured | Identity data missing |
|---|---|---|
| **sysinfo** | `ProcessRecord { pid, name, uid, cpu, rss, … }` via `@/query/processes` (`zensight-common/src/query_detail.rs:261`); cgroup-path *label* on opt-in cgroup metrics | cmdline, exe, environ, cgroup-per-PID, ppid, start_time, username, machine-id |
| **netlink** | `SocketRecord { local, remote, state, uid, inode, tcp_info… }`; interfaces: ifindex ↔ MAC (`info` metric) + ifindex ↔ IP (`AddressRecord`); **`NeighborRecord`** = ARP/NDP table → remote IP ↔ MAC; opt-in eBPF `ConnRecord { pid, comm, 5-tuple, bytes }` | **socket inode → PID/comm** (no /proc scan); socket cookie (stable socket id) |
| **netring** | `FlowRecord` with 5-tuple + **Community ID v1**; TLS/QUIC **SNI**, JA3/JA4, HASSH, HTTP Host, JA4H; **`AssetRecord { mac, ipv4[], ipv6[], hostname, vendor, platform, seen_via }`** from ARP/LLDP/CDP/DHCP/mDNS — the richest identity object we have | **DNS answers** (only questions parsed → no passive IP↔name store); flow ↔ hostname join; flow ↔ process |
| **systemd** | `UnitSample` (states, restarts, mem/cpu); **cgroup tree** `CgroupNode { path, unit, pids: [{pid, comm}] }` via `@/query/cgroups` | **`MainPID`**, **`ControlGroup`**, **`InvocationID`** on the unit object (not read from D-Bus) |
| **logs** | Best per-line identity source: journald `_SYSTEMD_UNIT→unit`, **`_COMM`/`_EXE`/`_CMDLINE`**, `_UID`/`_GID`, `_PID`, `_MACHINE_ID`, `_BOOT_ID` (`zensight-sensor-logs/src/journald.rs:475`); labels `app`, `pid`, `unit`, `sd.journald.*` | `_SYSTEMD_INVOCATION_ID` not in the standard field list; PID not linked to anything else |
| **frontend** | Join key = `DeviceId { protocol, source }` exact string match (`message.rs:818`); `view/inventory.rs` joins only netring's own asset/fingerprint queries | A `Host` entity; any cross-protocol merge |

The **only** IP→hostname mechanism in the workspace is the logs sensor's static
config `aliases: HashMap<ip, hostname>` (`receiver.rs:520`). No reverse DNS, no
passive DNS, no `hickory`/`getnameinfo` anywhere.

## 3. Correlation opportunity matrix

| # | Correlation edge | Value | Sources of truth | Effort | Priority |
|---|---|---|---|---|---|
| C1 | PID ↔ cmdline/exe/cgroup/ppid/start_time | Process explorer becomes actually useful; joins to units & sockets | sysinfo `/proc/<pid>/*` | Low | **P0** |
| C2 | Socket ↔ owning process (PID, comm) | "Which process holds this connection" — the #1 netlink gap | netlink inode + `/proc/*/fd` scan; eBPF upgrade | Medium | **P0** |
| C3 | IP ↔ hostname (passive DNS) | Names on flows/talkers/beacons; FQDN pivot under CDN IP churn | netring DNS answer parsing | Medium | **P0** |
| C4 | Host entity (same machine across protocols) | Dashboard/topology merge; everything else hangs off this | machine-id (sysinfo/systemd/logs), MAC+IP evidence (netlink/netring) | Medium | **P0** |
| C5 | Unit ↔ MainPID / InvocationID / cgroup path | Unit ↔ process ↔ log-line joins | systemd D-Bus properties | Low | **P1** |
| C6 | Flow ↔ process | NDR gold standard ("this beacon is `curl` run by uid 1000") | netlink eBPF `ConnRecord` (exists!) ⋈ netring flow via 5-tuple/Community ID | Medium | **P1** |
| C7 | MAC ↔ IP ↔ hostname registry feed | Asset inventory + neighbor table into the shared entity store | netring `AssetRecord`, netlink `NeighborRecord` | Low | **P1** |
| C8 | Log line ↔ unit run ↔ process | Click a log error → see the unit, its PIDs, restarts | logs `_SYSTEMD_INVOCATION_ID` ⋈ systemd `InvocationID` | Low | **P2** |
| C9 | Container ID ↔ process | Container-aware process explorer | `/proc/<pid>/cgroup` + mountinfo fallback | Medium | **P2** |
| C10 | Env vars per process | Debugging convenience | `/proc/<pid>/environ` | Low | **P3 — off by default** (secrets, §6) |

## 4. Per-edge design details

### C1 — Enrich sysinfo `ProcessRecord`

Add to `ProcessRecord` (`zensight-common/src/query_detail.rs:261`), read from `/proc/<pid>/`:

- `cmdline: String` (NUL-joined argv, **scrubbed** — §6), `exe: Option<String>`
  (readlink, may fail without ptrace perms — keep `Option`), `ppid: i32`,
  `cgroup: Option<String>` (v2 unified path → the join key to systemd units, C5),
  `start_time: u64` (field 22 of `/proc/<pid>/stat`, clock ticks since boot),
  `user: Option<String>` (resolved from uid via `/etc/passwd` cache).
- **`(pid, start_time)` is the process identity everywhere** — bare PIDs get reused;
  OTel semconv makes this pair the spec-level unique process id.
- Stays on the on-demand query channel (per-PID data never rides the telemetry bus —
  existing principle P2). No new key expressions.

### C2 — Socket → process attribution (netlink)

Two tiers, matching what `ss -p` / Datadog / Elastic converged on:

1. **Poll-cycle `/proc` scan (default).** Once per sockets poll, walk
   `/proc/<pid>/fd/*` readlinking for `socket:[inode]`, building one
   `inode → (pid, comm)` map per cycle (never per socket — cost is
   O(processes × fds), amortize it). Add `pid: Option<i32>` / `process:
   Option<String>` to `SocketRecord`. Accepted limitations: other users' sockets
   invisible without privileges (uid already narrows it); short-lived sockets race
   the snapshot — *documented*, not a bug. Config flag `collect.socket_processes`
   + a soft process-count ceiling to bound scan cost.
2. **eBPF event-time attribution (opt-in, already half-built).** The existing
   `--features ebpf` `ConnRecord` (pid + comm + 5-tuple at event time) is the
   race-free path — polling and packet capture both see sockets *after* the process
   context can be gone; only event-time capture (tracepoint
   `sock/inet_sock_set_state`) is complete. Extend it to also annotate live
   `SocketRecord`s on map hits.

Also capture the **socket cookie** (`InetSocket.cookie`, already exposed by nlink
0.23) as the stable socket identity — inodes get reused, cookies effectively
don't. `InetSocket.cgroup_id` (also already exposed) gives the unit/container
join without a /proc scan.

### C3 — Passive DNS: IP ↔ hostname (netring)

Today `monitor.rs:1440` parses only DNS *queries*. Parse **answer records**
(A/AAAA/CNAME/PTR) too, per the reference design (Zeek/Corelight "Namecache"):

- **Live cache**: `IP → Vec<(fqdn, provenance, first_seen, last_seen, ttl)>`,
  TTL-bounded expiry (+ grace), scoped per client IP where possible (CDN answers
  differ per client; the name that most closely *preceded* a flow from the *same
  client* is the right one), global fallback.
- **Names are plural, time-scoped, provenance-tagged** (Corelight `name.vals[]` +
  `name.src`); never a single "the" hostname per IP. Provenances: `dns_a`,
  `dns_aaaa`, `dns_ptr`, `sni`, `http_host`, `dhcp`, `mdns`, `lldp`, `static_alias`.
- **Consumers**: (a) name enrichment on `FlowRecord`/`TalkerRecord` query responses;
  (b) **FQDN-pivoted beaconing** — RITA aggregates beacons by hostname because C2
  infra rotates IPs behind CDNs; our RITA-style detector gains the same pivot;
  (c) name-evidence deltas to the correlator (§9.3).
- **PTR: passive only.** Active lookups leak monitoring presence, and PTR names are
  the infrastructure's, not the service's. Learn from observed answers, rank last.

### C4 — Host entity resolution

Moved to Part II (§9.3) — this is the architectural core.

### C5 — systemd: MainPID, InvocationID, ControlGroup

Three cheap D-Bus properties the sensor doesn't read yet (`unit.rs`):

- `Service.MainPID` (+ its start_time via `/proc`) → direct unit ↔ process link.
- `Unit.InvocationID` → durable identity for *one run* of a unit — solves "same
  unit, restarted" the way start_time solves PID reuse, **and** equals journald's
  `_SYSTEMD_INVOCATION_ID` → precise log ↔ unit-run join (C8).
- `Service.ControlGroup` → the cgroup path.

**The unit ↔ process join key across sensors is the cgroup path string**: systemd's
cgroup tree already yields `unit + pids[]`; once sysinfo publishes per-process
`cgroup` (C1), `process.cgroup == unit.control_group` joins them with no D-Bus
round-trips, covering *all* member processes, not just MainPID.

### C6 — Flow ↔ process

With C2 in place: netring `FlowRecord` (5-tuple / Community ID) ⋈ netlink
socket-or-`ConnRecord` (5-tuple + pid + comm) **on the same host**, joined in the
frontend flow drill-down. This is ntopng-libebpfflow / Datadog-NPM's headline
feature — we get it by joining two sensors we already ship. Community ID also makes
the join work against Zeek/Suricata data if ever ingested.

### C8 — Log ↔ unit-run ↔ process

Add `_SYSTEMD_INVOCATION_ID` to the logs sensor's `STANDARD_FIELDS`
(`journald.rs:475`, one line) → label `invocation_id`. With C5, the Logs view links
any journald line to the exact unit run, and the systemd view pulls "logs for this
run" precisely, not by time window.

### C9 — Container ID (later)

`/proc/<pid>/cgroup` yields container IDs under cgroup v1, but under v2 the ID is
often *not in the path*; OTel's detectors fall back to `/proc/<pid>/mountinfo` and
still differ across docker/containerd/cri-o. Do both, treat extraction failure as
expected, label as `container.id`. Defer until C1/C5 land.

## 5. Naming contract — adopt OTel semconv

Use OpenTelemetry semantic-convention attribute names verbatim as label keys
wherever correlation identities appear (they already flow to the OTLP exporter):

| Concept | Label key |
|---|---|
| Process | `process.pid`, `process.start_time` (the identity pair), `process.command_line`, `process.executable.name`, `process.executable.path`, `process.owner`, `process.parent_pid` |
| Host | `host.name`, `host.id` (hashed machine-id) |
| Container | `container.id`, `container.name` |
| Unit run | `systemd.unit`, `systemd.invocation_id` (no semconv; keep ours, prefixed) |

Adopt OTel's *entity* discipline: each entity type declares its **identifying**
attributes (joins run only on these) vs **descriptive** ones (display metadata, may
drift, never joined on). Every identity carries a generation qualifier:
pid+start_time, socket cookie not inode, unit+invocation-id, machine-id not
hostname, fqdn+observation-window. All the industry bugs surveyed (Elastic
beats#17165 wrong-process flows, ECS host duplication) trace to omitting the
qualifier.

## 6. Security & privacy considerations

- **Environment variables: do not capture by default.** `/proc/<pid>/environ` is a
  secrets goldmine and kernel-protected for a reason (same-user or `CAP_SYS_PTRACE`
  only). If ever added: opt-in, allowlist of variable *names*, values redacted
  unless explicitly allowlisted, query-channel only — never the telemetry bus.
- **Scrub cmdline in the sensor, before publish.** Datadog's default posture: argv
  values whose key matches `password, passwd, mysql_pwd, access_token, auth_token,
  api_key, apikey, secret, credentials, stripetoken` are replaced; extensible with
  wildcard words; match both `key=value` and `--key value` shapes. Secrets must not
  transit Zenoh even on query channels.
- **Hash machine-id before shipping.** systemd's docs treat `/etc/machine-id` as
  confidential; publish `sha256(machine_id + app_salt)` — still a perfect join key.
- **Passive-only network naming.** No active DNS/PTR lookups from sensors (leaks
  monitoring presence); everything is learned from observed traffic or config.

---

# Part II — Architecture: where correlation lives

## 7. The question

Three candidate placements: (a) in each sensor, (b) a dedicated correlator service
on the Zenoh bus, (c) in the frontend. The answer from surveying OTel Collector,
Elastic (Agent + Security Entity Store), Datadog, Zeek/Corelight, Security Onion,
Malcolm, and Kafka-Streams-style stream processing is unambiguous — **all three,
each for a different class of correlation**. No production system studied resolves
entities in the UI, and none does fleet-wide resolution in the sensor.

## 8. Principles (with the prior art behind each)

**P1 — Enrich at the edge what only the edge can see, before the key evaporates.**
Host-local joins depend on ephemeral state: a socket's inode→PID mapping dies with
the socket; NAT pre/post translation is only visible on the host. Datadog moved
conntrack/process correlation *into* the agent (system-probe, eBPF) for exactly
this reason; OTel's resourcedetection processor must run on the host it describes
("running the processor on a separate machine results in incorrect data").
→ Socket↔PID, PID↔unit(cgroup), PID↔container, NAT: **in-sensor** (C1/C2/C5/C9).

**P2 — Fleet-wide entity resolution is a single-writer materialized view.**
Elastic's Entity Store is a central transform folding ECS events into entity
indices; OTel's entity model (OTEP 0256) has downstream *enrichers* compose global
identity because the emitter can't know its full context; Kafka Streams codifies
"one processor is the sole writer of its derived state, consumers read the
materialized view". OTel's gateway doc names the failure mode of skipping this:
concurrent writers of the same derived data → data loss/degraded quality.
→ Host merging + IP↔name map: **one `zensight-correlator` service** publishing a
derived keyspace. Frontends re-deriving correlation independently would be N
divergent resolvers — a multiple-writer smell — and exporters/headless setups
would get nothing.

**P3 — The contract is schema, not topology.** Elastic's entity store works because
any integration emitting standard ECS identity fields contributes automatically;
Datadog's correlation is "just a join" because `env/service/version` tags are
enforced at emission. → ZenSight's equivalent: the shared **identity envelope**
(§9.1) + OTel-named labels (§5). Sensors don't call each other; they emit the
contract.

**P4 — Consumers tolerate eventual consistency; late joiners get seeded state.**
Corelight's Namecache accepts seconds of propagation (conn.log writes at flow
expiry); Elastic's enrich processor accepts snapshot staleness. OTel entities are
a stream of `EntityState` events **periodically re-emitted even when unchanged**
(doubling as liveness) + best-effort deletes. → The correlator re-emits entity
state periodically and serves a queryable seed — the exact pattern our alert
firing-set seed (`@/query/alerts`) already uses.

**P5 — Derived state must fail soft.** The correlator is a new component that can
die. Sensors keep publishing raw telemetry and evidence regardless; if the
correlator is absent, the frontend renders per-protocol devices exactly as today.
Correlation is an overlay, never a dependency.

**P6 — Keep sensor binaries separate; unify the identity contract, not the
process.** Nobody unifies agents for correlation *logic* — Elastic Agent is a
supervisor that still runs Beats as separate subprocesses (privilege + failure
isolation); Datadog keeps system-probe (root/eBPF) apart from the core agent.
ZenSight's split is already right: netring needs `CAP_NET_RAW`, systemd needs
D-Bus, sysinfo needs nothing. What's missing is only the shared identity envelope.
(A Fleet-style supervisor for enrollment/config is a separate, later concern.)

## 9. Component design

### 9.1 `zensight-sensor-core::identity` — the shared identity envelope (new module)

One module, used by every sensor at startup:

```rust
pub struct HostIdentity {
    pub host_id: Option<String>,   // sha256(/etc/machine-id + salt), hex — None if unreadable
    pub boot_id: Option<String>,   // /proc/sys/kernel/random/boot_id
    pub hostname: String,          // gethostname()
    pub fqdn: Option<String>,
    pub ips: Vec<String>,          // non-loopback, from getifaddrs/netlink
    pub macs: Vec<String>,         // non-loopback interface MACs
}
```

- Read once at startup (+ refresh on a slow timer for DHCP address churn).
- Stamped into `SensorInfo` (`_meta/sensors/<name>`, already published by all
  sensors) and published as **host evidence** (§10) — every sensor gets fleet
  identity for free, replacing five divergent `hostname::get()` call sites.
- Config: standardize on one field name **`source`** (override) across all sensors
  (breaking, trivial). `host.id` becomes a label on health snapshots and alerts so
  even those correlate without the entity store.
- Sensors observing **remote** devices (snmp, gnmi, modbus, netflow, remote syslog)
  have two identities: the *agent host* (the envelope above) and each *observed
  device*. They publish observed-device evidence too — e.g. snmp `sysName` +
  management IP, netflow exporter IP — marked with `observer: <sensor>` so the
  correlator knows these are third-party claims, not self-reports.

### 9.2 In-sensor (edge) enrichment — what stays local

Per P1, these never leave the host unjoined (all detailed in Part I):

| Join | Sensor | Mechanism |
|---|---|---|
| socket → PID, comm | netlink | per-cycle `/proc/*/fd` inode map; eBPF upgrade |
| process → cmdline/exe/cgroup/start_time | sysinfo | `/proc/<pid>/*` (+ scrubber) |
| unit → MainPID/InvocationID/cgroup | systemd | D-Bus properties |
| PID/unit on log lines | logs | journald trusted fields (already there) |
| flow → local names (SNI, DNS cache) | netring | passive parsing |

Edge enrichment emits **labels/fields** on existing telemetry and query records —
it does not create new topics and it never blocks the hot path (identity lookups
are cache reads; cache misses degrade to unenriched records).

### 9.3 `zensight-correlator` — the fleet entity service (new crate)

A small headless service, deployed like an exporter (one per fleet), that is the
**single writer** of the derived entity keyspace.

**Inputs (subscriber side):**
- `zensight/_meta/evidence/**` — host evidence (§10) from every sensor, and
  name-observation deltas from netring/logs.
- `zensight/*/@/devices/*/liveness` — to carry device status onto entities.
- (It does **not** consume the telemetry firehose — evidence keys only. Bandwidth
  stays negligible.)

**Core logic — deterministic evidence merge:**
- Union-find over evidence records with ranked rules:
  1. same `host_id` (hashed machine-id) ⇒ same host — *certain*
  2. same MAC + overlapping IP (within evidence TTL) ⇒ same host — *strong*
  3. same FQDN ⇒ *medium*; bare hostname ⇒ *weak* (twenty `MacBook-Pro.local`s);
     IP alone ⇒ *hint only* (DHCP/NAT/multi-homing)
- MAC is merge *evidence*, not identity (VMs clone MACs; hosts have many).
- Deterministic and stateless-recomputable: the entity set is a pure function of
  current evidence, so restart = resubscribe + queryable-sweep of evidence, no
  local DB needed at fleet sizes of hundreds of hosts. (Determinism also means an
  accidentally duplicated correlator publishes identical docs — wrong, but not
  corrupting; a Zenoh liveliness token on a well-known key lets a second instance
  detect the first and warn/exit.)
- **Merges are reversible resolution groups** (Elastic's model): the entity lists
  its member `(sensor, source)` claims + which rule bound each member and its
  confidence. `DeviceId` keys never get rewritten; un-merging is dropping a claim.
- **Entity id stability**: `h_<12 hex of host_id>` when machine-id is known, else
  `h_<12 hex of sha256(best evidence key)>`; when a weakly-identified entity later
  gains a machine-id, the old id is kept in `aliases[]` and a tombstone re-points
  it — ids never silently swap.

**Outputs (single-writer materialized view, §10 for keys):**
- `HostEntity` docs on `_meta/entity/host/<entity_id>` via `AdvancedPublisher`
  (cache = late-joiner seed) + periodic re-emission (~60s, doubles as correlator
  liveness, per OTel entity events) + tombstones on merge/retire.
- **IP↔name map is served, not broadcast**: fleet-host names ride inline on their
  `HostEntity`; arbitrary/external IPs (every CDN IP netring ever saw) would flood
  the bus, so they're resolved on demand via a queryable
  `_meta/query/names?ip=…` returning `name.vals[]` + provenance + time window.
  This mirrors Corelight Namecache's "propagate additions, rate-limited" lesson.

**Failure modes (P5):** correlator down → entities go stale, frontends fall back
to per-protocol devices (today's behavior); evidence keeps flowing; restart
recovers state from evidence caches. No sensor or exporter depends on it.

**Why not in the frontend:** N frontends would each re-derive (divergent views,
duplicated work), exporters couldn't tag metrics with `host.id`/entity, and
headless/API consumers would get nothing. The frontend keeps only *display-time*
joins (e.g. flow drill-down ⋈ socket table) against materialized state.

**Why not a Zenoh storage plugin:** storages persist keys; they can't run merge
logic. The correlator is compute + a cached publisher — an ordinary Zenoh app,
consistent with how every other ZenSight component works.

### 9.4 Frontend integration (full design)

The frontend never resolves entities (P2) — it consumes the correlator's
materialized state and performs *display-time* joins. Four layers:

**9.4.1 Data plumbing — `EntityStore`** (new `zensight/src/entity.rs`)

- `subscription.rs` gains a subscriber on `zensight/_meta/entity/**` plus a
  connect-time seed GET on `_meta/query/entities` (the alerts firing-set-seed
  pattern). New messages: `EntitySeed(Vec<HostEntity>)`, `EntityReceived(HostEntity)`,
  `EntityRemoved(entity_id)` (tombstone). This **replaces** the dead
  `correlations: HashMap<ip, CorrelationEntry>` (`app.rs:146`) and
  `Message::CorrelationReceived` plumbing — delete, don't deprecate.
- `EntityStore` keeps the docs plus derived indexes, rebuilt on every upsert:
  - `hosts: HashMap<EntityId, HostEntity>`
  - `by_device: HashMap<DeviceId, EntityId>` (from `members[]`) — the one lookup
    every view uses
  - `by_ip: HashMap<IpAddr, EntityId>` (identifying IPs) — feeds topology flow
    edges and flow drill-downs
  - `aliases: HashMap<EntityId, EntityId>` (old id → current, from `aliases[]`)
- **Staleness**: an entity older than ~3× the correlator re-emission period is
  marked stale (reuse `view/freshness.rs` indicators); a fully absent correlator
  ⇒ empty store ⇒ every view falls back to per-protocol devices. Degradation is
  a first-class rendering path, not an error state.

**9.4.2 Views — host-first rendering, device drill-down**

- **Dashboard**: devices sharing an entity group under one *host card* — merged
  status (worst-of-members), protocol facet chips (sysinfo · netlink · systemd …),
  alert-count rollup across members. Entity-less devices render as today.
  "Group by host" is a persisted toggle so the old flat view stays reachable.
- **Topology**: node key becomes `EntityId`-or-`DeviceId`; `apply_correlations`
  is replaced by `EntityStore` lookups, and `topology_ip_to_node` reads `by_ip` —
  the flow-edge bridging that has always been inert finally works. Wire-only
  assets (entities built purely from netring/netlink observation, no ZenSight
  sensor member) appear as passive nodes, visually distinct.
- **Host view** (`view/host.rs` exists as the per-host aggregate): becomes the
  entity landing page — identity header (hostname, `host.id` short-hash, IPs,
  MACs, vendor/platform, provenance-tagged names), facet tabs reusing the
  existing specialized views per member `DeviceId`.
- **Merge transparency**: a "merged from N sources" affordance lists each member
  with its binding rule + confidence (from `MemberClaim`), so a wrong merge is
  visible and diagnosable — never silently glue devices.
- **Alerts/Incidents**: alert `source` → `by_device` → entity, so incident
  grouping (`groups.rs`) rolls up by *host*, and the Security view's by-source
  rollup gains host names next to raw sources.

**9.4.3 Identity pivots — cross-view navigation on join keys**

Every identity join lands as a clickable chip; all joins are query-time reads of
data other issues already publish:

| From | Chip | To | Join key |
|---|---|---|---|
| process explorer row | unit name | systemd unit detail | `process.cgroup == unit.control_group` |
| systemd unit detail | MainPID / member PIDs | process explorer (filtered) | cgroup path / `(pid, start_time)` |
| journald log line | unit run | systemd unit detail (that run) | `invocation_id` |
| systemd unit detail | "logs for this run" | Logs view (pre-filtered) | `invocation_id` |
| netlink socket row | process | process explorer | `(pid, start_time)` |
| netring flow | process | socket/process detail | 5-tuple ⋈ sockets/`ConnRecord` (C6) |
| any IP anywhere | hostname tooltip + pivot | host view / names popover | `by_ip`, else `_meta/query/names?ip=` |

Unresolvable pivots render as plain text (never a dead button); cross-host
pivots route through the entity to pick the right member device.

**9.4.4 Inventory & search**

- `view/inventory.rs` currently joins only netring's own asset/fingerprint
  queries. Add an *entity* column: assets whose MAC/IP maps into `EntityStore`
  link to their host view; wire-only assets stay standalone rows. One inventory,
  observed and managed hosts side by side.
- Global search (Ctrl+K) and the command palette index entity hostnames/names →
  "jump to host"; searching an IP offers the naming pivot.

**9.4.5 Demo mode & testing**

- Demo mode publishes mock `HostEntity` docs consistent with its mock devices
  (the demo/mock-contract rule) — GUI development needs no live correlator.
- Simulator tests: host card renders members + merged status; pivot chips emit
  the right navigation messages; **empty-entity-store test pins the degraded
  path** (feature parity with today) so correlation can never become a hard
  dependency of basic rendering.

### 9.5 New sensors considered — and why none are needed

| Candidate | Verdict |
|---|---|
| DHCP-lease / ARP watcher | **No** — netring already passively extracts DHCP/ARP/mDNS/LLDP into `AssetRecord`; netlink has the neighbor table. |
| Dedicated "identity agent" per host | **No** — that's the §9.1 core module; a sixth process per host adds enrollment burden for zero new data. |
| DNS-server log sensor | **Not now** — passive answer parsing (C3) covers it; a resolver-log ingester is a later option for encrypted-DNS (DoH/DoT) environments where the wire is opaque. |
| Cloud metadata (instance-id, tags) | **Later, inside sysinfo** — a small optional collector hitting the metadata endpoint; feeds evidence as a stronger-than-machine-id cloud identity. Not a sensor. |
| NetBox / CMDB connector | **Later, as a correlator input** — an *authority* evidence source (Malcolm's NetBox pattern): curated truth that observed evidence is matched against. Fits the evidence model unchanged. |
| eBPF process/exec tracer (Tetragon-style) | **No new sensor** — extends the existing netlink eBPF feature (already has tcplife-style `ConnRecord`). |

### 9.6 Upstream crate work (we own netring, flowscope, nlink)

Verified against the actual upstream code (not assumptions); issues filed:

| Crate | Finding | Action |
|---|---|---|
| **flowscope** | DNS **answers are already parsed** (`dns/parser.rs` maps `pkt.answers` incl. rdata) and a per-client `DnsResolutionCache` exists (Plan 85) — but it's single-name per `(client, target)`, A/AAAA-only, fixed TTL, no provenance, no global reverse index | [flowscope#130](https://github.com/p13marc/flowscope/issues/130): pDNS-grade `NameMap` (plural provenance-tagged claims, answer-TTL expiry, CNAME/PTR, `drain_new` delta feed for rate-limited propagation) |
| **flowscope** | `BeaconDetector<K>` is **already generic over its key** | **No change needed** — FQDN-pivoted beaconing = key a second detector by resolved name in the sensor |
| **netring** | `Monitor` has ARP/NDP/LLDP/CDP/fingerprint handlers but **no DNS handler**; datagram taps exist only at the stream/pcap layer — live AF_PACKET/XDP capture through `Monitor` never sees DNS | [netring#120](https://github.com/p13marc/netring/issues/120): monitor DNS handler/subscription emitting query+answer events with client attribution, same event type on the pcap path |
| **nlink** | `sockdiag::InetSocket` **already exposes** `cookie: u64` *and* `cgroup_id: Option<u64>` (plus `uid`, `inode`, `mark`) — the earlier "no cookie" claim was from checking the wrong repo (`nlink-lab`, a lab tool, not the library). What's actually missing is the **inode→PID/comm attribution utility** (lives in nlink's `ss` bin, not the library) | [nlink#161](https://github.com/p13marc/nlink/issues/161) **closed** (cookie already present). New: [nlink#162](https://github.com/p13marc/nlink/issues/162) socket→process attribution utility (inode→pid/comm + cgroup_id→unit helper) |

Dependency edges: zensight#308 (passive DNS) consumes netring#120 + flowscope#130;
zensight#304 (socket→process) uses the already-available `cookie`/`cgroup_id`
directly and benefits from nlink#162 for the shared inode→process scan, but its
/proc-scan tier proceeds independently.

Beyond the correlation-specific asks above, a broader forward-looking roadmap was
filed in all three crates (tracker epics: [flowscope#140](https://github.com/p13marc/flowscope/issues/140),
[netring#129](https://github.com/p13marc/netring/issues/129),
[nlink#170](https://github.com/p13marc/nlink/issues/170)) — unified detector
registry, threat-backbone consolidation, JA4+ license re-tiering, modern-kernel
telemetry, etc. Not correlation-blocking; tracked separately.

## 10. Keyspace & data-model redesign (breaking)

Compatibility is allowed to break; this section is the new contract.

**Removed:**
- `zensight/_meta/correlation/<ip>` + `CorrelationEntry` + frontend plumbing
  (dead code — delete, don't deprecate).
- Per-sensor `hostname`/`source` config divergence → one `source` field.

**New evidence keyspace (writers: sensors; cached AdvancedPublisher, slow re-emit):**

```
zensight/_meta/evidence/host/<sensor>/<source>      # HostEvidence (self-report or observed)
zensight/_meta/evidence/names/<sensor>/<ip-slug>    # NameObservation (one per IP, rate-limited)
```

```rust
pub struct HostEvidence {
    pub sensor: String, pub source: String,
    pub observer: Option<String>,        // None = self-report; Some = third-party claim
    pub host_id: Option<String>,         // hashed machine-id
    pub boot_id: Option<String>,
    pub hostname: Option<String>, pub fqdn: Option<String>,
    pub ips: Vec<String>, pub macs: Vec<String>,
    pub last_updated: i64,               // evidence older than TTL is ignored by rules
}

pub struct NameObservation {             // batched: Vec<NameObservation> per publication
    pub ip: String, pub name: String,
    pub provenance: String,              // dns_a | dns_aaaa | dns_ptr | sni | http_host | dhcp | mdns | lldp | static_alias
    pub client: Option<String>,          // scoping client IP when known
    pub first_seen: i64, pub last_seen: i64, pub ttl_s: Option<u32>,
}
```

**New entity keyspace (single writer: zensight-correlator):**

```
zensight/_meta/entity/host/<entity_id>              # HostEntity (cached, re-emitted ~60s, tombstoned)
zensight/_meta/query/entities                       # queryable: full entity seed for late joiners
zensight/_meta/query/names?ip=<ip>                  # queryable: name.vals[] for arbitrary IPs
```

```rust
pub struct HostEntity {
    pub entity_id: String,               // "h_<12hex>", stable; see §9.3
    pub aliases: Vec<String>,            // prior entity_ids after upgrades/merges
    // identifying (joins allowed):
    pub host_id: Option<String>, pub boot_id: Option<String>,
    pub ips: Vec<String>, pub macs: Vec<String>,
    // descriptive (display only):
    pub hostname: Option<String>, pub fqdn: Option<String>,
    pub names: Vec<NameClaim>,           // provenance-tagged, from the name map
    pub vendor: Option<String>, pub platform: Option<String>,   // via netring assets
    // membership:
    pub members: Vec<MemberClaim>,       // { sensor, source, rule, confidence, last_seen }
    pub last_updated: i64,
}
```

**Deliberately unchanged:** telemetry keys stay
`zensight/<protocol>/<source>/<metric>` with human-readable `source`. Re-keying
telemetry on opaque entity ids was considered and rejected: it destroys key
readability/debuggability, can't work for observed remote devices (their identity
is only *resolved* fleet-side), and the entity layer already provides the join.
Identity travels as **labels + evidence**, not as key structure. This also means
sensors don't take the correlator as a dependency for publishing — P5 holds.

## 11. Zenoh storages: what they change — and what they don't

The zenoh-plugin-storage-manager (volumes: memory, filesystem, RocksDB, InfluxDB,
S3) subscribes to a configured key expression, persists what it sees, and replies
to GETs on those keys — including **replica alignment** between storages
(`replication/` in the plugin) and, with the InfluxDB backend, **full time-series
history with time-ranged queries** rather than latest-per-key. ZenSight already
deploys this pattern: `configs/router-blob-storage.json5` runs zenohd +
storage-manager + fs backend as the blob content store (#193), so a
storage-capable router is an established, not hypothetical, deployment shape.

### What a storage cannot do

A storage is a **passive materialized cache**: no merge logic, no joins, no rule
evaluation. It cannot resolve entities — the correlator (compute) stays. We also
considered implementing the correlator *as* a storage backend plugin
(`zenoh-backend-traits::Volume`) and reject it: backends are KV plugins loaded
into zenohd, which couples domain logic to the router's lifecycle and to exact
Rust-version/ABI pinning (the known SIGSEGV footgun for zenohd plugins), and the
Volume API has no place for cross-key derivation anyway.

### What storages upgrade (three concrete roles)

1. **Evidence durability → offline-asset inventory.** In the base design, a
   sensor's `HostEvidence` lives in its AdvancedPublisher cache and vanishes when
   the sensor host goes down — an offline host disappears from the entity store
   on the next recompute. A storage on `zensight/_meta/evidence/**`
   (fs/RocksDB volume) retains last-known evidence across sensor outages *and*
   correlator restarts, turning the entity store into a true asset inventory:
   "host X, last seen Tuesday, these MACs/IPs" instead of "no data". This also
   makes correlator recovery independent of which sensors happen to be up.
2. **Entity availability while the correlator is down.** A storage on
   `zensight/_meta/entity/**` keeps serving the last materialized entities
   (stale but present) when the correlator is absent — strengthening the
   fail-soft story (P5) from "degrade to uncorrelated" to "degrade to stale".
   It also subsumes the custom `_meta/query/entities` seed queryable: late
   joiners GET the entity keyexpr and the storage replies. The AdvancedPublisher
   cache remains the zero-infrastructure baseline; the storage is additive.
3. **The historical tier — pDNS time-travel (InfluxDB volume).** The live design
   (§9.3) deliberately serves only *current* names and excluded broadcasting
   high-cardinality IP↔name records. With a storage, netring publishes
   `NameObservation`s to a **verbatim-chunk keyspace** — e.g.
   `zensight/@pdns/<ip>` — which `zensight/**` subscribers never see (`@`-chunks
   are excluded from wildcards; the same mechanism that already isolates the
   `@/` control plane), while the storage explicitly subscribes to
   `zensight/@pdns/**` and ingests everything. Consumers then GET on demand,
   **including time-ranged queries**: "what did 203.0.113.7 resolve to last
   Tuesday" — the zeek-pdns historical tier (first-seen/last-seen per (ip, name,
   provenance)) essentially for free. The same volume can keep entity-membership
   history and alert history for incident timelines.

### Deployment tiers (all optional beyond Tier 0)

| Tier | Infrastructure | You get |
|---|---|---|
| **0** | none (pure peer mesh, today's default) | full correlation; caches only; evidence/entities die with their publishers; current-names only |
| **1** | zenohd router + storage-manager + fs/RocksDB volume (the blob-store router, extended) | durable evidence + entities; offline-asset inventory; correlator-down = stale-not-absent; seed queryables subsumed |
| **2** | + InfluxDB volume | historical pDNS, entity/alert history, time-ranged forensics |
| **HA** | second router, replicated storages | replica alignment across sites for evidence/entity data |

Design rule carried through: **storages are accelerators, never dependencies** —
every consumer must work at Tier 0, and the correlator treats a storage as just
another replier to its evidence sweep. The frontend's local redb store keeps its
separate job (per-frontend hot ring, custom downsampling, log template sampling);
a fleet storage complements it (shared history for late-joining frontends) rather
than replacing it.

## 12. Phasing (revised for the architecture)

**Phase 1 — identity contract (independently shippable, no correlator yet):**
1. `zensight-sensor-core::identity` envelope + `source` config unification +
   evidence publishing from all sensors (§9.1, §10) — delete the dead registry.
2. C1 sysinfo `ProcessRecord` enrichment + argv scrubber.
3. C5 systemd MainPID/InvocationID/ControlGroup + C8 logs `_SYSTEMD_INVOCATION_ID`
   label (tiny; unit↔process↔log works host-locally via cgroup-path join).
4. C2 tier-1 socket→PID `/proc` scan in netlink.

**Phase 2 — the correlator:**
5. `zensight-correlator` crate: evidence merge rules, `HostEntity` publishing,
   seeds/queryables (§9.3, §10). Demo mode gains mock evidence so the GUI is
   developable without live sensors (per the demo/mock contract).
6. Frontend `Host` entity + merged dashboard/topology (§9.4).
7. C7 netring `AssetRecord` + netlink `NeighborRecord` → evidence feeds.

**Phase 3 — NDR payoff:**
8. C3 passive-DNS answers in netring + `NameObservation` deltas + `_meta/query/names`
   + FQDN-pivoted beaconing.
9. C6 flow↔process display join (uses C2/eBPF + Community ID).

**Phase 2½ — storage tier (optional, parallel to Phase 3):**
- Extend the blob-store router config with evidence + entity storages (Tier 1,
  §11) — config-only, no code. Correlator evidence sweep verified against a
  storage replier.

**Phase 3 addendum:** when C3 lands, add the `zensight/@pdns/<ip>` keyspace +
InfluxDB storage (Tier 2) and a time-range mode on `_meta/query/names`.

**Phase 4 — later:** C9 container IDs; cloud-metadata evidence; NetBox authority
feed; C10 env vars (if ever, opt-in); Fleet-style sensor supervisor (out of scope
here); replicated storages for multi-site HA.

## 13. Key sources

*Mechanisms (Part I):* sock_diag(7); iproute2 `ss.c` (`user_ent_hash_build`); LWN on
SOCK_DESTROY (kill-op, not a notification stream — hence eBPF for lifecycle); eBPF
`sock/inet_sock_set_state` (tcpstates/tcplife); Corelight Namecache blog (plural
`name.vals` + `name.src`); zeek-pdns (pDNS schema, TTL + first/last-seen); Active
Countermeasures on RITA FQDN beacons; OTel resource semconv (`(process.pid,
process.start_time)`); proc(5), proc_pid_environ(5); Datadog `data_scrubber.go`
(default sensitive-word list); org.freedesktop.systemd1(5); systemd.journal-fields(7)
(`_`-fields are journald-asserted, unfakeable).

*Architecture (Part II):* OTel Collector agent/gateway docs (resourcedetection must
run on-host; gateway doc's single-writer warning); k8sattributes processor (the
canonical correlation-processor: central metadata cache joined at the collector);
OTEP 0256 entity data model (EntityState periodic re-emission; enrichers compose
global identity); Elastic Security Entity Store (central transforms over ECS
events) + Entity Resolution (reversible groups); Elastic Agent = supervisor over
separate Beats subprocesses (unify enrollment, not privilege domains); ECS RFC 0006
host identifiers (never merged — the cautionary tale); Datadog unified service
tagging (correlation = join on an emission-time contract) + system-probe eBPF
conntrack move (edge-enrich before keys evaporate); Zeek cluster + Broker stores
(workers→manager aggregation; Namecache rate-limited delta propagation); Security
Onion (sensors forward, all enrichment central); Malcolm (central Logstash
enrichment; NetBox as asset authority); Kafka Streams KTable/GlobalKTable +
KIP-99 (single-writer materialized views; codified enrichment staleness); Vector
prod guidance ("agents should be simple forwarders"); Grafana Alloy FAQ (agent
sprawl makes correlation harder → shared identity, not merged logic).
