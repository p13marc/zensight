# Identity, evidence & entities

ZenSight keeps telemetry keyed by a human-readable `source`
(`zensight/<protocol>/<source>/…`) but still needs to know *which physical host*
each source belongs to. It solves this without re-keying telemetry: sensors
publish identity **evidence**, and a single-writer **correlator** fuses that
evidence into one **entity** per host. This page describes the wire types in
`zensight-common`; the exact keys are in [`../docs/KEYSPACE.md`](../../docs/KEYSPACE.md).

## The pipeline

```
sensors ──HostEvidence / NameObservation──▶  _meta/evidence/**
                                                    │
                                            correlator (single writer)
                                                    │
                                     HostEntity ──▶ _meta/entity/host/<id>
```

Evidence is a **claim, not a verdict**. Two provenance kinds, distinguished by the
`observer` field:

- **self-report** (`observer: None`) — a sensor reporting about the host it runs
  on. Strong.
- **third-party claim** (`observer: Some(sensor)`) — a sensor reporting a device it
  merely *observed* on the wire (netring assets, netlink neighbors, snmp
  sysName). Merge rules weigh these lower.

Evidence is TTL-scoped: consumers ignore any record whose `last_updated` is older
than the evidence TTL, so publishers must periodically refresh live claims (the
sensor framework re-emits self-reports every 60 s).

## HostEvidence

One host-identity claim, published on
`zensight/_meta/evidence/host/<sensor>/<source>` (`evidence.rs`). Every optional
field is `skip_serializing_if`-elided, so a sparse claim stays small on the wire
and an old/minimal publisher still decodes with defaults.

```rust
pub struct HostEvidence {
    pub sensor: String,               // publishing sensor, e.g. "sysinfo"
    pub source: String,               // the source this claim is about
    pub observer: Option<String>,     // None = self-report; Some = third-party
    pub host_id: Option<String>,      // hashed machine-id (never raw), sha256(machine_id+salt)
    pub boot_id: Option<String>,
    pub hostname: Option<String>,
    pub fqdn: Option<String>,
    pub ips: Vec<String>,             // identifying
    pub macs: Vec<String>,            // merge evidence, not identity (VMs clone MACs)
    pub vendor: Option<String>,       // descriptive / display-only
    pub platform: Option<String>,     // descriptive / display-only
    pub container_id: Option<String>, // #311 — host-scoped qualifier, never a merge key
    pub cloud: Option<CloudFacts>,    // #311 — authoritative when present
    pub last_updated: i64,
}
```

Merge strength of the identifying fields (strongest first): `host_id` >
`(cloud.provider, cloud.instance_id)` > `mac + ip` > `fqdn` > `hostname`. Notes:

- **`host_id`** is `sha256(machine_id + salt)` — the raw machine-id (confidential
  per the systemd docs) never leaves the host.
- **`container_id`** is host-scoped (only unique per host runtime), so it is a
  qualifier ("this sensor's view is from inside container X"), *never* a
  cross-host merge key.
- **`CloudFacts`** (`provider`, `instance_id`, optional `region` / `account`) is
  authoritative: cloned images duplicate machine-ids, but a cloud control plane
  never hands out an instance id twice.

## NameObservation

One passive-DNS name observation, published on
`zensight/_meta/evidence/names/<sensor>/<ip-slug>` (#307). A third-party claim
binding an observed IP to a name seen on the wire (DNS answer, PTR, TLS SNI, …),
so the correlator can attach names to entities that emit no telemetry of their
own. One observation per IP key (last-writer-wins); like `HostEvidence`, stale
records past the TTL are ignored.

```rust
pub struct NameObservation {
    pub observer: String,    // observing sensor, e.g. "netring"
    pub ip: String,          // observed IP this name binds to
    pub name: String,        // canonical, lowercased, no trailing dot
    pub provenance: String,  // dns_a, dns_cname, dns_ptr, sni, mdns, ...
    pub last_seen: i64,
}
```

## HostEntity — the correlator's output

The correlator merges every TTL-live `HostEvidence` claim into `HostEntity` docs
(`entity.rs`), published on `zensight/_meta/entity/host/<entity_id>`. An entity is
a **materialized view** — a pure, deterministic function of the current evidence
set — so a restarted correlator rebuilds byte-identical docs from the caches with
no local state.

```rust
pub struct HostEntity {
    pub entity_id: String,          // "h_<12hex>": prefix of hashed machine-id, else of sha256(best key)
    pub aliases: Vec<String>,       // prior entity_ids this one subsumed (upgrade/merge)
    pub host_id: Option<String>,    // identifying — joins allowed
    pub boot_id: Option<String>,
    pub ips: Vec<String>,           // union across members
    pub macs: Vec<String>,
    pub container_ids: Vec<String>, // descriptive union (#311)
    pub hostname: Option<String>,   // descriptive — display only
    pub fqdn: Option<String>,
    pub names: Vec<NameVal>,        // attached from the passive-DNS name map
    pub vendor: Option<String>,
    pub platform: Option<String>,
    pub members: Vec<MemberClaim>,  // the evidence claims merged in
    pub status: Option<String>,     // rolled-up device status, worst-of-members
    pub last_updated: i64,
}
```

- **`MemberClaim`** is one reversible membership: the `(sensor, source)` that was
  merged, the `rule` that bound it (`host_id` | `mac_ip` | `fqdn` | `hostname`),
  the `confidence` (after any observer down-weight), and `last_seen`. The
  `(sensor, source)` pair is the **join key back to per-protocol telemetry** —
  telemetry keys are never re-keyed on entity ids; the entity provides the join
  via `members[]`.
- **`NameVal`** is one provenance-tagged name (`name`, `provenance`, `last_seen`).
  Distinct from the wire-level `NameObservation`: the correlator accumulates
  *multiple* names per IP into `NameVal`s, since #307 publishes only one
  observation per IP key.
- Entity ids never silently swap: on a weak→`host_id` upgrade or a merge, the old
  id moves into `aliases`, is tombstoned, and re-pointed.
- `HostEntity::canonicalize()` sorts/dedups the multi-valued fields so two
  entities built from the same evidence in different input orders serialize
  identically; the correlator calls it before publishing.

### PdnsRecord (durable passive-DNS tier)

Separately, the correlator publishes its *full accumulated* per-IP `NameVal` set
as a `PdnsRecord` on the `@`-verbatim `zensight/@pdns/<ip-slug>` tier (#310), for
a router-hosted storage backend to capture the complete IP↔name history. Because
`@pdns` is a verbatim chunk (like `@/` and `@media`), these records are invisible
to both the telemetry firehose (`zensight/**`) and the per-sensor control-plane
wildcard (`zensight/*/@/**`).

## Query seeds (late joiners)

The correlator also serves queryables so a late-joining consumer can seed state
on demand: `zensight/_meta/query/entities` (full current entity set) and
`zensight/_meta/query/names?ip=<addr>` (resolve names for an arbitrary IP without
flooding the bus). See [`../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) and the
`keyexpr.rs` builders indexed in [keyspace-helpers.md](keyspace-helpers.md).
