# ZenSight Zenoh Keyspace Reference

**The deployed keyspace is the keyspace-v2 convention, v1.2 (ratified).**
(v1.1 made the version chunk a plain `v1` — a wire break, already deployed.
v1.2 is doc-only: it amends the convention with the lessons of the migration
and changes no key.) The normative reference is the RFC set in
[the zenkey repo](https://github.com/p13marc/zenkey/blob/main/rfcs/00-index.md); ZenSight's concrete
profile (constants, per-sensor worked examples, the mapping of every shipped
key family) is [chapter 11](https://github.com/p13marc/zenkey/blob/main/rfcs/11-zensight-profile.md). The
migration was executed under epic
[#453](https://github.com/p13marc/zensight/issues/453); the pre-v1 keyspace
this file used to document is retired — no shipped component publishes or
subscribes to it.

## The deployed profile in one screen

Grammar (RFC [03](https://github.com/p13marc/zenkey/blob/main/rfcs/03-grammar.md)):

```
zensight/v1/<origin>/<class>/<producer>/<subject...>     data planes
zensight/v1/<origin>/@rpc/<producer>/<procedure...>      request/reply
zensight/v1/<origin>/@media/<producer>/<stream>/…        opaque video
zensight/v1/<origin>/@blob/{artifact,tree,store}/…       bulk content
zensight/v1/@catalog/…                                   the identity catalog
zensight/v1/@desired/state/<host>/<producer>/<topic>     fleet desired state (#816)
```

- `<origin>` = `h-<12hex>` (sha256 of machine-id + salt, RFC
  [06](https://github.com/p13marc/zenkey/blob/main/rfcs/06-identity.md)); the catalog service publishes under
  the verbatim `@catalog` origin.
- `<class>` = `telemetry` (periodic samples) · `state` (LWW documents:
  health, errors, alerts, evidence, expectations, stream/artifact docs) ·
  `events` (append-only records). Classes are disjoint by construction; the
  planes (`@rpc`/`@media`/`@blob`) are verbatim chunks no data selector can
  reach (RFC [04](https://github.com/p13marc/zenkey/blob/main/rfcs/04-planes.md),
  [07](https://github.com/p13marc/zenkey/blob/main/rfcs/07-bulk-planes.md)).
- **Alerts** are LWW state at `…/state/<producer>/alert/<alert_key>`, where
  `alert_key` is the normative RFC 11 §3.1 derivation —
  `lowercase_hex(fnv1a_64(rule ++ ("\n" ++ name ++ "=" ++ value)*))`, 16 chars,
  labels ascending by name — computed by `zenkey::alert::alert_key` (#736).
  The origin is never hashed in (it is already a key chunk), and host-scoped
  labels are excluded before sorting: the RFC's own `host`, plus **ZenSight's
  declared host-scoped vocabulary, the `host.` annotation namespace**
  (`zensight_common::alert::is_host_scoped`), which is what keeps a firing
  alert's key stable across an identity refresh (#738).
- Presence = liveliness tokens at `…/state/<producer>/alive` (+
  `…/state/<producer>/device/<device>/alive`,
  `…/@catalog/state/alive`). Alive ⇒ callable: RPC queryables are declared
  before the token.
- **Telemetry history is pulled, not seeded.** A telemetry key carries the
  current sample and nothing before it; asking "what did this do yesterday" is
  a GET on `…/@rpc/historian/range` (#898), never a wider subscription or a
  seed. A series there is `(origin, producer, subject)` — the wire key minus
  the class chunk — which a reader derives from a sample alone, so it holds
  across a catalog merge and a correlator outage. Several historians may answer
  one fleet selector, so the same target-`All` rule below applies and the
  caller merges per series.
- Commands do not exist: writes are GETs on `…/@rpc/<producer>/<topic>/set`,
  reads on `…/@rpc/<producer>/<topic>` (RFC
  [05](https://github.com/p13marc/zenkey/blob/main/rfcs/05-control-rpc.md)). Fleet callers select
  `zensight/v1/*/@rpc/…` with query target `All`.
- Late joiners seed with a plain GET on the same state selectors (state is
  its own seed; storage-shaped queryables answer one reply per concrete key,
  **stamped from the producer's session HLC** — a producer answering that GET
  is a storage for the duration of the reply, and a consumer merges the seed
  with live samples by timestamp, so an unstamped seed cannot be reconciled
  (RFC 04 §3.2, #782). Serve one through
  `zensight_common::served::serve_state_queryable`; an `@rpc` reply is a
  computed answer, not the value at a key, and is deliberately *not* stamped).
- The `events` class is instantiated (#534): append-only records ride
  `v1/<origin>/events/<producer>/<subject...>/<id>` where `<id>` is the
  record's lowercase ULID — one key per record, nothing overwrites. The
  shared envelope is `zensight_common::EventRecord` (id, timestamp, source,
  protocol, kind, severity, summary, optional `alert_key`, fields), published through
  `zensight_sensor_core::EventPublisher` with `QosClass::Event`
  (reliable + block: a dropped event is unrecoverable). Registry entries use
  `class = "events"` with the RFC 08 §5 `rate = rare|low|burst(n/h)`
  annotation; the first real subjects are the SNMP trap records (#535).
  Retention is the deployment's choice: events are durable in *transit*
  (reliable + block QoS) but the bus stores nothing, so without a storage there
  is no history for a late joiner and the GUI's startup backfill GET (#536)
  returns nothing after a restart. Point a Zenoh storage at `v1/*/events/**` —
  [`configs/router-events-storage.json5`](../configs/router-events-storage.json5)
  is the worked example (#583). Because every record owns a unique ULID key, a
  plain `fs` volume keeps the whole log rather than a latest-per-key view, and
  the GUI's own redb cold store is *additive* to it: records are immutable and
  ULID-identified, so the union needs no precedence rule.
- Telemetry payloads (`TelemetryPoint`) carry an optional UCUM-style `unit`
  field (serde-defaulted, absent when unknown). Proxy pollers with counter
  metrics (today: `snmp`, #527) publish a derived per-second sibling under
  `<metric>.rate` (a `Gauge`; `By/s` for octet counters, else `1/s`) next to
  the raw lifetime counter — a dot-suffix on the leaf chunk, not an extra
  subject chunk, so it stays inside the registered `{metric...}` family.

## The relationship graph rides the bus (#899/#915)

Two new state families, and between them the whole graph:

```
zensight/v1/<origin>/state/<producer>/evidence/relation/{relation_id}   a sensor's CLAIM
zensight/v1/@catalog/state/edge/{edge_id}                               the catalog's CONCLUSION
```

Sensors publish `RelationshipEvidence` — a `kind` (`hosts`, `runs`,
`gateway_of`, `probes`, `l2_adjacent`) and two `EndpointClaim`s, which carry
what was *observed* (a vmid, a MAC, a gateway address, a target name) and
never an entity id: resolving a claim to an entity is the catalog's job,
because the catalog is the only participant that has run the union-find. The
catalog publishes `Edge`, whose ends are resolved — an entity id, or an honest
`External` for something the fleet can see but runs no sensor on (an upstream
router, a probe target on the internet).

`relation_id` and `edge_id` are both derived from `(kind, from, to)` and
nothing else — no timestamp, no publisher, no observer set — so a refresh is an
idempotent LWW overwrite on one key rather than a new document per observation,
and two sensors that see the same relationship land on the same key instead of
counting twice against the cardinality budget. Both hash over a `\u{1f}`-joined
representation, which cannot be forged by field values the way a `-` join can.

**`evidence/relation/**` already falls inside `all_evidence_wildcard()`**
(`v1/*/state/*/evidence/**`), so the correlator's input contract is unchanged
and no new subscription was needed. That is also what made it dangerous: the
correlator's host-identity handler excluded exactly one subtree by substring
and decoded everything else as `HostEvidence`, which has no
`deny_unknown_fields` and requires only `sensor` and `source` — both of which a
relation claim carries. It now dispatches on the refined subject and accepts
only `evidence/self` and `evidence/device/{device}`, so a family added under
`evidence/**` is inert to identity by default. Two tests pin it, one of which
demonstrates that a real relation document does decode as `HostEvidence`.

Neither family carries a `common =` key: `zenkey::CommonState` is a closed RFC
enum in an external crate, so both refine app-side through
`zensight_common::state::ZensightState` — the same escape hatch
`catalog/assertion/{id}` uses.

> **RFC status.** These two families are **implemented and shipped ahead of the
> RFC.** The amendment is [zenkey#416](https://github.com/p13marc/zenkey/issues/416),
> which is open; a comment there records what the implementation turned out to
> be, the three places it is more specific than the amendment text, and two
> `edge_id` details worth making normative because both are silent when wrong.
> Until #416 releases, **the RFC and this code disagree**, deliberately and in
> writing — the same treatment zenkey#415 got for the historian. When it lands,
> `CommonState::{EvidenceRelation, CatalogEdge}` replaces the app-side
> refinement and both families gain a `common =` key; nothing on the wire moves.

`L2Adjacent` is the one kind **no sensor publishes**. The catalog derives it
from the observed-device identity claims already on the bus — "the sensor on
this host saw that device" is a statement about a link-layer segment — which is
the inference the GUI used to make privately from netlink's neighbour table.

The consumer side is written up in
[`zensight-correlator/README.md`](../zensight-correlator/README.md#consumer-recipe-topology-aware-alert-inhibition):
what a key-agnostic notifier needs to inhibit "do not page for a guest whose
hypervisor is down" with no application knowledge at all.

Flow adjacency is deliberately *not* a relation kind. It is per-observed-peer
and unbounded, so it stays an `@rpc` overlay rather than entering a
cardinality-budgeted state family — `edge/{edge_id}` declares 50 000, and a
resolver emitting an edge per observed peer would breach it.

## Clock discipline (#959)

Two halves, deliberately separate, because neither answers the other's
question:

```
zensight/v1/<origin>/telemetry/probe/{target}/ntp_offset_ms   a SERVER, from a vantage
zensight/v1/<origin>/state/sysinfo/timesync                   THIS HOST's own discipline
```

The probe's offset is measured against the **probe host's** clock, so a vantage
that is itself adrift reports every server as adrift. Only
`state/sysinfo/timesync` — the local daemon's own report, via `chronyc -c
tracking` or `timedatectl show` — says which of the two is wrong.

`timesync` is **absent** when no time daemon answers, never a zero offset: a
zero is what a perfectly disciplined clock looks like, and publishing it for a
host with nothing disciplining its clock reports the opposite of the truth. It
is opt-in (`collect.timesync`) because reading it shells out.

## `@desired` — fleet configuration as desired state (#816)

A controller publishes per-host runtime POLICY under the `@desired` service
origin, the **target host id as the first subject chunk** (RFC 07 §3's G1
proxy rule; zenkey-build's H4 lint enforces the ordering):

```
zensight/v1/@desired/state/h-3fa9c2d41b7e/hostspec/expectations
```

LWW and storage-backed (`configs/router-evidence-storage.json5` grows a
`zensight-desired` storage — `*` never matches a verbatim `@` origin, so the
selector is its own, D4). The target sensor **reconciles**: a GET seed
against the storage at startup plus a periodic re-GET (`desired.refresh_secs`,
level-triggered — survives any missed sample or reconnect), with the live
subscription as the accelerator. A `Delete` reverts the target to its
file-config baseline. This is convergence; durable pub/sub *commands* are
the permanently forbidden alternative (RFC 12).

What it carries: the hostspec assertion set (`HostspecExpectations`, #816),
the systemd unit-expectation set (`ExpectationsConfig`, #849), and a
`thresholds` set per producer (`ThresholdsConfig`, #931) — all real schemars
types, which is what the RFC 08 §7 schema gate requires of a state-class
payload and what a sensor-crate type can never be (zensight-common cannot
depend on a sensor, so `describe` could only carry a stub).

The two *expectation* topics joined one sensor at a time, as each set's type
migrated into zensight-common; the netlink and logs sentinels still hold
theirs in their own crates and are the remainder there. `thresholds` needed no
migration — `ThresholdsConfig` was written in zensight-common from the start
(#928) — so every producer joined at once. That is the point of it: one rule
vocabulary, authored fleet-wide, evaluated **at the edge by whichever sensor
publishes the metric**, instead of a rule engine in a GUI whose alerts reached
nothing.

systemd's type name is `ExpectationsConfig` rather than a producer-prefixed
`SystemdExpectations` like hostspec's, and deliberately so: that is the name
`@rpc/systemd/expectations/set` has advertised since 1.0, nothing collides
with it (netlink's set takes `ExpectationCommand`, logs' takes
`LogRulesConfig`), and renaming it would be a breaking registry change to a
shipped path for a payload whose bytes do not move.

**The never-list** (the most important constraint): nothing under `@desired`
may carry secrets or anything a sensor needs to reach the bus — endpoints,
TLS material, the namespace. One bad desired publish must never lock the
fleet out of its own supervision. The consumer enforces this structurally:
the reconciler deserializes only the sentinel's own config type and writes
only that sentinel's handle. A per-sensor kill switch
(`desired.enabled: false`) lives in FILE config.

**Two writers, one honest marker.** An operator's `@rpc/<producer>/<topic>/set`
and the `@desired` reconciler both write the same sentinel handle; the rule
is LWW by arrival, and `state/<producer>/applied/<topic>` (`AppliedConfig`)
says which source won last (`file | desired | rpc`), what document is in
force (JSON-encoded — its schema is the topic type's own), and the most
recent *rejected* desired doc: an invalid document is refused loudly, the
previous good config keeps running, and the refusal is on the bus, not only
in a log.

## Where the machine-readable truth lives

- **Registry** (per-producer subjects/procedures, QoS entitlements, lints):
  [`zensight-common/registry/*.toml`](../zensight-common/registry/) —
  compiled by `zenkey-build` from `zensight-common/build.rs` into typed
  builders/parsers;
  registry violations are build errors. Sensors serve their compiled slice at
  `…/@rpc/<producer>/introspect` — and the GUI's **Fleet** view calls it, parsing
  the reply into a `zensight_keyspace::RegistrySlice` and diffing it against the
  slice it compiled in. RFC 08 §6: a disagreement is a *finding*, not an ambiguity.
  Since zenkey 0.4 the registry also declares the **`@blob` tiers** each producer
  serves (`[[blob]]` entries, RFC 08 §2 v1.8): all ten artifact-capable sensors
  declare `artifact`/`tree`/`store` (blake3), so the slice answers "does this
  producer serve blobs?" and the generated `zensight_common::registry::blob`
  module carries the typed key builders.
- **The registry is load-bearing.** Publishing a telemetry subject that is not
  registered panics in debug builds and warns once per name in release
  (`zensight_common::metric_guard`). This is only meaningful because the host
  producers (sysinfo, netlink, netring, systemd, logs, parallax, hostspec,
  container, probe) — and, though they poll a remote API, `pve` (#818) and
  `bmc` (#953) — register their telemetry as
  real subject families rather than a `{metric...}` catch-all; a catch-all makes
  the lint vacuously true (issue #468). `snmp`/`modbus`/`gnmi`/`netflow` keep a
  rest-var by design: their metric tree belongs to the polled device, not to us.
  `pve` and `bmc` do not, because a hypervisor's and a chassis's vocabularies
  are ours: guests, pools, dumps and quorum — and supplies, fans, thermal
  sensors and a power state — are closed sets we name, not an OID tree a
  vendor owns.
- **The registry must not lie** (RFC 08 §6.1, #484). The check above runs one
  direction — *published ⊆ registered*. The reverse — *registered ⊆ served* —
  is a distinct MUST, and the first does not imply it: a registry may be a
  strict superset of what the code does and every published key still builds.
  That superset is precisely what `introspect` hands the fleet as truth, and
  the #453 audit found **seven** such surfaces. So every queryable is declared
  through `zensight_common::served::serve_queryable` (CI bans raw
  `declare_queryable`), and each producer checks its registry slice against
  what it actually declared at the moment it starts serving `introspect` —
  debug panic, release warn, the same posture as the metric guard.
- **Two ledgers sit beside the registry TOMLs**, both checked by `zenkey-build`
  at build time:
  - [`deprecated.lock`](../zensight-common/registry/deprecated.lock) —
    **append-only** retirement (RFC 08 §3/§5). A line is `<producer>\t<path>`,
    or since zenkey 0.7 / RFC 08 v1.26 `<kind>\t<producer>\t<path>` with
    `kind = subject | procedure`; the two-field form reads as `kind = subject`,
    which is what all 18 shipped lines are. **Kind is part of identity**:
    retiring a subject never releases a procedure of the same name, or the
    reverse — which matters here, because `parallax` has a `streams` procedure
    beside stream-shaped subjects and `@catalog` has `names`/`describe`/
    `introspect` beside `entity`/`alias`. A procedure retirement must spell its
    kind.
  - [`conditional.lock`](../zensight-common/registry/conditional.lock) — the
    RFC 08 §6.1 conditional-subject ledger (zenkey 0.7), the exemption from
    *registered ⊆ served* for a subject this build can legitimately never emit.
    Deliberately **not** append-only: a line leaves when its gate does. It is
    two lines for the whole workspace, and that is correct — a gated
    *procedure* is declared unconditionally and answers `error/gated` or
    `error/unsupported`, so it needs no exemption; only a gauge with no honest
    reading does. The file's header says so, so the absence is not "fixed" by
    the next reader.
- **Type table + self-description** (RFC 08 §5/§7):
  [`zensight-common/registry/types.toml`](../zensight-common/registry/types.toml)
  is the RFC 08 §5 type table — a registry `type`/`request`/`reply` name with no
  entry fails the build (`zenkey-build` lint). At run time,
  [`zensight_common::schema::SCHEMAS`](../zensight-common/src/schema.rs) serves
  the same table as JSON Schemas on `…/@rpc/<producer>/describe` (every sensor
  via its runner, the catalog via the correlator), `build_verified` against the
  generated `TYPE_NAMES` so a gap aborts rather than serving a partial table. A
  consumer goes wire key → subject → type → schema → value with nothing
  producer-specific compiled in (`zenkey-fleet`'s `SchemaStore`/`decode_sample`).
  Every producer put also stamps the sample `Encoding`
  (`application/cbor`/`application/json` from `Format::encoding()`), so
  consumers resolve payloads from metadata before falling back to sniffing.
- **Fleet-wide writes are explicit** (RFC 05 amendment G2): a write procedure
  is origin-scoped unless its registry entry says `fanout = "allowed"`. The
  operator-console fleet pushes (logs filter + sentinel rules, systemd
  expectations, netlink expectations, hostspec expectations, netring
  capture/detectors/filter/threat-intel, parallax stream) carry that marker
  deliberately; everything else refuses a wildcard origin at the type level.
  `systemd action/set` also carries it, but for `zenctl` only — the marker
  sanctions a typed, scriptable, logged fleet push. The GUI is strictly
  origin-scoped there: a per-row restart button that could widen to every host
  is a blast radius nobody asked for, so its key builders take a concrete origin
  and a wildcard action key cannot be spelled. **`snmp action/set` (#956) is the
  same shape and the same restriction**, and it is the sharper case: a fleet
  push that cycles every PDU outlet on the allowlist would take a datacentre
  down. Its four independent gates are in
  [`zensight-sensor-snmp/docs/reference.md`](../zensight-sensor-snmp/docs/reference.md);
  what none of them provides is *attribution*, and that is written down there
  rather than left to be assumed.
- **Bus explorer**: [`zenctl`](https://github.com/p13marc/zenkey/tree/main/zenctl) is the `busctl`/`d-feet`
  equivalent RFC 08 §6 exists to enable — `topic list/info/echo`, `node list`,
  `service list/call`, and `doctor` (fan `introspect` fleet-wide, diff each reply
  against this build's slice, print the findings).
- **Key builders**: `zenkey::V1Context` (producer-side),
  `zensight_common::keyexpr` (consumer-side selectors + fleet/origin RPC
  keys), `zensight_common::command` (topic/artifact procedure keys). New code
  MUST build keys through these — never ad-hoc `format!`.
- **Guard tests**: `zenkey/tests/guard.rs` (zenkey repo) pins the D1–D6
  disjointness algebra; consumer crates pin their own selector shapes.

## The version chunk is plain (`v1`), not verbatim

Everything `@`-prefixed in the grammar is **verbatim** — invisible to `*` and
`**`. That is what keeps the planes out of data selectors (D2) and `@catalog`
out of a `*` origin (D4). The version chunk is deliberately **not** one of them.

It was `@v1` through the migration. Zenoh's advanced pub/sub parks a
publisher-detection liveliness token at `<key>/@adv/pub/<zid>/<eid>/…` and parses
it with `${remaining:**}/@adv/…` — and since `**` cannot cross an `@`,
`remaining` could not span a key containing `@v1`. **Every** token was
unparseable by the only code that reads them: `detect_late_publishers()` was
silently dead, and every subscriber logged *"malformed liveliness token key
expression"* once per publisher. No upstream fix was possible — the
`@`-exclusion is a Zenoh matching rule.

The `@` bought invisibility to an **un-versioned** selector, i.e. coexistence
with the pre-v1 keyspace — a migration property, and the migration is done.
Cross-major isolation (a `v1` selector never matches a `v2` key) never needed
it: they are different literal chunks.

**Consequence:** `zensight/**` now *does* match v1 keys. `cutover_e2e` and
`v1_probe` therefore check that nothing appears **outside** `zensight/v1/`,
rather than relying on key algebra to hide us. Pinned by
`zenkey/tests/adv_token.rs` (zenkey repo) (the token must parse),
`guard.rs::d1_version_isolation`, and
`zensight-sensor-core/tests/adv_publisher_detection.rs` (no warning, end to end).
RFC: [03 §1.2](https://github.com/p13marc/zenkey/blob/main/rfcs/03-grammar.md), [12 §7](https://github.com/p13marc/zenkey/blob/main/rfcs/12-open-questions.md).

## The base is the session namespace, not a chunk anyone types

Application code **never spells the base**. The base names the *deployment*,
not the software, and is **optional — empty by default** (RFC 03 §1.1 as
amended): with no `zenoh.namespace` set, no session namespace is set either
(Zenoh's own default) and the deployment's full wire keys start at `v1/…`.
Setting a base (`zenoh.namespace`, override `ZENSIGHT_ZENOH_NAMESPACE`) is the
opt-in isolation knob for running several deployments on one Zenoh
infrastructure: the runtime prefixes it onto every keyexpr the session emits,
strips it on delivery, and **filters ingress from outside it** (RFC 09 §0 —
issue #466). All participants of one deployment must agree on it — a
mismatched base (empty vs. named, or two different names) partitions them.

So there are two views of every key, and which one you are in is a property of
the *session*, not the key (wire keys shown for a deployment with base
`zensight`; in the default base-less deployment the two views coincide):

| | sees | builds keys with |
|---|---|---|
| **applications** (sensors, GUI, correlator, exporters) — namespaced | `v1/h-…/telemetry/sysinfo/cpu/usage` | `V1Context`, `zensight_common::keyexpr` — all base-relative |
| **routers / storages / ACL** — no namespace | `zensight/v1/h-…/…` | full keys, written by hand in `configs/router-*.json5` |
| **debug tools** (`zenctl`, `v1_probe`) — un-namespaced *on purpose* (RFC 09 §5) | `zensight/v1/h-…/…` | `grammar::with_base(base, …)`, `keyexpr::parse_full_key(base, …)` |

The middle and bottom rows are why `with_base`/`strip_base` exist and why
`zenctl` takes a `--base`: an explorer that ran inside the namespace could not
see a key from *outside* it, and spotting exactly that is what an explorer is
for.

Two CI guards keep this true: application source may not contain a `"zensight/`
literal at all, and only `zensight_common::session` may call `zenoh::open` —
because the namespace is per-session, and a component that hand-rolls its own
`zenoh::Config` would silently miss the deployment's configured base and go
deaf with no error. (There were five such builders before #466. There is one
now.)

The wire is unchanged by all of this: `zensight-sensor-core/tests/cutover_e2e.rs`
pins it with a namespaced sensor and an **un-namespaced** observer, so the same
key is asserted in both spellings at once.

## Operations

Isolated verification: `cargo run -p zensight-common --example v1_probe` opens
a multicast-scouting-off listener, watches the bus, exercises the @rpc plane,
and fails if the deployment root carries non-v1 traffic (the retired legacy
bus). It observes the base-less wire by default; set
`ZENSIGHT_ZENOH_NAMESPACE` to probe a based deployment. Point
sensors at it with `ZENSIGHT_ZENOH_CONNECT=tcp/127.0.0.1:17471
ZENSIGHT_ZENOH_SCOUTING=false` (the `zenoh.scouting` config knob / env
override disables multicast discovery so a session can never join a mesh
beyond its explicit endpoints; gossip has its own `zenoh.gossip` /
`ZENSIGHT_ZENOH_GOSSIP` switch — it only propagates within the connected
graph. Unset, both default mode-aware: off for a client with explicit
`connect` endpoints, on otherwise — #626).

Session config, storage recipes (latest/catalog/pdns), ACL, and
constrained-link profiles: RFC [09](https://github.com/p13marc/zenkey/blob/main/rfcs/09-operations.md).

**Telemetry history is an application, not a storage recipe.** This paragraph
used to name a `timeseries` recipe alongside the others; no such config file has
ever existed in this repository, and RFC 04 §4's InfluxDB storage does not fit
the requirement it was standing in for — `zenoh-backend-influxdb` v2 cannot
answer `*`/`**` selectors, a `_time=` GET has no aggregation or downsampling so a
day of per-second samples travels raw, and an out-of-tree router plugin cannot
run in the CI jobs that execute workspace binaries. What ships instead is
[`zensight-historian`](../zensight-historian/README.md) (#898): a subscriber of
`v1/*/telemetry/**` that writes tiered storage and serves
`@rpc/historian/{range,series,timeline,stats}` — bounded, typed,
cursor-paginated, with counter→rate computed server-side from series that know
their kind. Prometheus remote-write remains the path for deployments that want a
real TSDB.
Shipped router configs: [`configs/router-evidence-storage.json5`](../configs/router-evidence-storage.json5)
(state seed store), [`configs/router-blob-storage.json5`](../configs/router-blob-storage.json5)
(@blob tiers), [`configs/router-events-storage.json5`](../configs/router-events-storage.json5)
(events log), [`configs/router-pdns-influxdb-storage.json5`](../configs/router-pdns-influxdb-storage.json5)
(pdns history).
