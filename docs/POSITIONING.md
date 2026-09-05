# What ZenSight is for

ZenSight is fleet monitoring for people who own the fleet: **one Zenoh bus that carries
telemetry, control, media, bulk transfer and desired state**, a set of read-only protocol
sensors that feed it, and a desktop frontend that shows one entity per host rather than one
row per exporter.

It was built for a specific deployment — a Proxmox host and six 1–2 GB VMs, everything a
Podman Quadlet — and it is published because that shape is common and the standard stack
fits it badly. This page says who else it is for, what it does that the standard stack does
not, and, at least as important, what it deliberately refuses to do.

It is **not** a feature list; that is the [README](../README.md).

---

## The problem it was built for

A small fleet on links you do not control is the case where scrape-based monitoring is at
its worst. Every target needs an exporter, every exporter needs a port, every port needs to
be reachable *from the collector*, and the interval is a global compromise between freshness
and traffic. Add a site behind a cellular or satellite backhaul and the model stops
degrading gracefully: a missed scrape is indistinguishable from a dead host, and a bulk
transfer — a packet capture, a debug bundle — has no path at all.

ZenSight starts from the opposite constraint. It
[must run well over low-bandwidth and unreliable links](design/zenoh-efficiency.md) —
"field sensors on cellular/satellite/mesh backhaul, a GUI or exporter reaching a remote
site" — and everything else follows from that.

## Why Zenoh-native, and not a library choice

Zenoh is not an implementation detail swapped in for a message queue. The whole contract is
the keyspace, and the keyspace is what makes five different kinds of traffic share one
session without interfering:

```
zensight/v1/<origin>/<class>/<producer>/<subject...>     data planes
zensight/v1/<origin>/@rpc/<producer>/<procedure...>      request/reply
zensight/v1/<origin>/@media/<producer>/<stream>/…        opaque video
zensight/v1/<origin>/@blob/{artifact,tree,store}/…       bulk content
zensight/v1/@catalog/…                                   the identity catalog
zensight/v1/@desired/state/<host>/<producer>/<topic>     fleet desired state
```

The properties that matter operationally:

- **Sensors publish; nothing scrapes.** Telemetry rides an `AdvancedPublisher` paired with
  an `AdvancedSubscriber`, so a consumer that joins late gets history and recovery instead
  of a gap ([ARCHITECTURE — publish/subscribe pairing](ARCHITECTURE.md)). A sensor behind
  NAT or on an intermittent link dials out; nothing has to reach *in* to it.
- **The classes are disjoint by construction**, and the planes are verbatim chunks no data
  selector can reach ([KEYSPACE](KEYSPACE.md)). A wildcard subscription to telemetry cannot
  accidentally pull video, and a video stream cannot be swept up by a metrics query.
- **Commands are not publications.** A write is a GET on
  `@rpc/<producer>/<topic>/set`, which means it has a reply, an outcome and a caller
  waiting for it — not a message dropped into a topic and hoped about.
- **Video is demand-driven.** No viewer, no pixels
  ([parallax sensor](../zensight-sensor-parallax/README.md)): the encoder does not run
  because a camera exists, it runs because someone opened the stream.
- **Bulk data has a path.** Reports, snapshots and packet captures are requested over
  `@rpc` and delivered, resumably and content-addressed, on `@blob`
  ([artifacts](../zensight-sensor-core/docs/artifacts.md)) — over the same session, without
  a second transport to operate.
- **Configuration is fleet state, not files.** `@desired` exists because the alternative was
  "eighteen hand-edited JSON5 files across six machines"
  ([zensight-desired](../zensight-desired/README.md)). One policy file compiles to the
  per-host documents every sensor reconciles against.

## What the standard stack cannot show you

Prometheus and its ecosystem are very good at *series*. The three things below are not
series, and that is the whole differentiation:

**One entity per host, from evidence.** Sensors do not agree on what a host is called: SNMP
has an IP and a `sysName`, netlink has interfaces and MACs, systemd has a machine-id,
containers have their host's cgroup. The [correlator](../zensight-correlator/README.md)
subscribes only the evidence state — never the telemetry firehose — and fuses claims with a
deterministic union-find over *ranked* identity rules into one `HostEntity`. It holds no
database: on restart it rebuilds identical state from the sensors' own cached self-reports.
A label-join in a query language cannot do this, because the ranking and the transitivity
are the point.

**Structural edges, on the bus.** Sensors publish *observed* relationships — pve a `Hosts`
per guest, container a `Runs` per container, probe a `Probes` per target, netlink a
`GatewayOf` for the default route — and the catalog resolves both ends against the
union-find result and publishes them as edges. Claims never name an entity id, because
resolving a claim to an entity is the catalog's job: it is the only participant that has run
the fusion. `L2Adjacent` is not published by anyone; it is derived.

**Incidents that know what they are downstream of.** With the graph in place, an incident
document carries `symptom_of`: the entity this failure is a consequence of. This is not a
plan — it ships, it is populated by the correlator, and both exporters carry it out to the
rest of your stack, including a ready-made Alertmanager inhibition rule
([Prometheus exporter reference](../zensight-exporter-prometheus/docs/reference.md)). When
a hypervisor dies, the six guests that went with it are labelled as its symptoms rather than
paging as six independent incidents.

## Read-only, on purpose

Most ZenSight sensors cannot do anything to your machines. This is stated per crate and it
is meant literally:

- `hostspec` — "closed vocabulary, read-only, executes nothing";
- `container` — "no action surface: the socket client has two methods, both GETs";
- `pve` — "**it has no action surface at all.** Not disabled, not gated — absent";
- `bmc` — no action surface, and every verdict is the BMC's own enum, never a threshold the
  sensor invented;
- `probe` — a client only, and nothing else.

Each of them logs it at startup, so the claim is checkable on a running host and not only in
a README.

**There are exactly two write surfaces in the tree, and both are off by default.**

1. **systemd service control** — four independent gates: a master switch (with it off there
   is no procedure to call), a per-verb switch, a unit allowlist where empty rejects
   everything, and authorization delegated to systemd/polkit
   ([units-and-actions](../zensight-sensor-systemd/docs/units-and-actions.md)).
2. **An SNMP PDU outlet cycle** — the strictest gate in the tree, because *a monitor that
   can cut power is a different threat model*. Four gates again, and the device must be
   pinned to a profile whose **control** OIDs were verified against the vendor MIB: a wrong
   read publishes a wrong number, a wrong write does something to a machine
   ([SNMP reference](../zensight-sensor-snmp/docs/reference.md)).

The honest limit on both: there is no caller identity on the bus yet. Every attempt,
executed or refused, is recorded on the host's own audit subsystem, which makes a write
**auditable** and not **attributable**. Until Zenoh mTLS identity plus ACLs land, the
accurate description is *anyone who can reach the bus, and whose target is on the allowlist,
can cycle that outlet*. If that is unacceptable in your environment, leave both switches off
— which is what they are.

## What stays in the tools you already run

ZenSight is not trying to replace your dashboards or your long-range storage, and it ships
the exporters that say so:

| You want | Use | How it gets there |
|---|---|---|
| Long-range analytics, PromQL, recording rules | Prometheus / Mimir / Thanos / VictoriaMetrics | `/metrics` scrape **or** remote-write 1.0 push ([prometheus exporter](../zensight-exporter-prometheus/README.md)) |
| Dashboards | Grafana | on top of the above |
| Notification routing, escalation, on-call, inhibition | Alertmanager | the `zensight_alert` gauge, with `symptom_of` as an inhibition matcher |
| Traces, logs and metrics in one backend | OTel Collector, Tempo/Loki, or a vendor | OTLP gRPC/HTTP ([otel exporter](../zensight-exporter-otel/README.md)) |
| Scheduled reporting | Grafana reporting | over the remote-write feed |

The in-tree [historian](../zensight-historian/README.md) exists so that a *headless* fleet
has bounded, queryable history without a second database process on a 1 GB VM. It is not a
TSDB and does not want to be one; if you already run one, remote-write to it.

## What it is not

- **Not Grafana.** There is a desktop frontend with views built for this data model; there
  is no dashboard builder, no panel library and no plan for one.
- **Not a query language.** Range queries are structured, typed and bounded. A
  ZenSight-flavoured PromQL would be PromQL with different syntax minus twelve years of
  hardening; long-range analytics go to the exporter.
- **Not the Prometheus exporter ecosystem.** Fourteen protocol sensors, not ten thousand
  community exporters. If your device speaks something none of them speaks, ZenSight has
  nothing for it and Prometheus probably does.
- **Not Zabbix templates or user administration.** There is no template inheritance engine —
  policy is an extension of `@desired`, which is a fleet-state mechanism, not a
  configuration-management system.
- **Not a full NMS.** No vendor device library, no configuration backup, no IPAM.
- **Not an orchestration or remote-command system.** See the two gated surfaces above; that
  is the entire list, and growing it is a deliberate decision each time.
- **Not link-aware.** Detection is generic and IP-level; there is
  [no protocol knowledge of RF, satellite or acoustic links](latency.md). A device on such a
  link is an SNMP or probe target like any other.

## Who should run it

The shape it is built and proven for:

- **5–50 hosts, one operator or a small team.** Enough machines that per-host configuration
  hurts, few enough that one person holds the whole picture.
- **Mixed and legacy protocols** — SNMP devices, syslog senders, NetFlow, Modbus, a
  hypervisor, containers, plain Linux hosts — that you would otherwise wire up as a dozen
  separate exporters.
- **Links you do not fully control**, or sites you reach intermittently.
- **A preference for read-only tooling**, where monitoring is not also a way into the fleet.
- **Comfort running pre-1.0 software** from source or container images, reading a CHANGELOG
  before upgrading, and filing an issue when something is wrong.

## Who should not — yet

- **Anyone needing multi-tenancy or RBAC.** There is none. It is a single-operator GUI plus
  whatever ACLs you configure in the Zenoh router; a caller identity does not exist yet.
- **Anyone whose primary interface is PromQL over a year of data.** Use the exporter and
  point it at your TSDB — that is what it is for — or use Prometheus and skip ZenSight.
- **Anyone who needs a supported product.** One reference fleet, one maintainer, pre-1.0.
- **Anyone who needs a specific vendor NMS integration.** Check the sensor list first; if it
  is not there, it is not there.

## Maturity, and what "1.0" will mean

ZenSight is pre-1.0 and the minor version is the breaking slot. What is stable today, what
may move, and the deprecation window are written down in
[COMPATIBILITY.md](COMPATIBILITY.md).

There is no 1.0 until the software has been battle-tested by fleets outside this project,
running it in production, long enough to have found what one reference fleet cannot. Every
other criterion — the milestones landed, two clean minors, the sizing measured, the demo
runnable by a stranger — is something this repository can satisfy on its own, which is
exactly why none of them is sufficient. As [RELEASING.md](../RELEASING.md) puts it: *a 1.0
is a promise made to other people; it cannot be earned by talking to yourself.*

That is also the invitation. If you run a fleet like the one above, the most useful thing
you can do for this project is run it and say what broke.
