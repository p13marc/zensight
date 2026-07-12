# 10 — Prior Art

**Status: Draft** · informative chapter

The convention was designed against six existing systems (§1–6, plus
Zenoh's own guidance, §7); the first review round added three more —
D-Bus, Homie, and OPC UA (§8–10). For each: the concrete grammar, what
this RFC took, and what it rejected — with the why. All claims were
verified against the cited sources (fetched July 2026).

---

## 1. Zenoh in automotive — Eclipse uProtocol, VSS, IAC

**uProtocol Zenoh transport** ([up-spec `up-l1/zenoh.adoc`](https://github.com/eclipse-uprotocol/up-spec/blob/main/up-l1/zenoh.adoc),
[uURI spec](https://github.com/eclipse-uprotocol/up-spec/blob/main/basics/uri.adoc)):
fixed-arity 11-chunk keys carrying both source *and sink* —
`up/[src.authority]/[src.ue_type]/[src.ue_instance]/[src.ue_version]/[src.resource]/[sink…×5]`
— hex-encoded numeric ids, literal `{}` placeholder chunks for unused sink
slots in publish keys, message metadata in a protobuf attachment.

- **Took**: fixed arity for the *addressable* positions (our 1–5), so
  positional parsing needs no registry; target-in-key for point-to-point
  control (our origin-targeted `@rpc`); metadata in attachments, not keys.
- **Rejected**: full fixed arity with placeholder chunks — our subjects are
  open-depth (gNMI paths); and mid-key *plain* version chunks — verbatim
  isolation is strictly stronger.

**VSS over Zenoh** ([Eclipse SDV service-to-signal blueprint](https://sdv-blueprints.eclipse.dev/docs/service-to-signal/)):
VSS dot-paths map 1:1 to key chunks (`Vehicle/Body/Horn/IsActive`); the
signal catalog *is* the keyspace, and vehicle identity is deployment
scoping. **Took**: the catalog-as-keyspace idea — our registry-governed
subjects; identity handled at an outer position, not inside the signal
path.

**Indy Autonomous Challenge** ([Zenoh blog](https://zenoh.io/blog/2021-09-28-iac-experiences-from-the-trenches/)):
topic filtering by key expression over constrained radio links.
**Took**: keyspace-as-bandwidth-policy — prefix allowlists as the link
budget mechanism ([09-operations.md §4](09-operations.md)).

## 2. Keelson (RISE Maritime)

([repo](https://github.com/RISE-Maritime/keelson) ·
[protocol spec](https://rise-maritime.github.io/keelson/protocol-specification/) ·
[subject registry](https://rise-maritime.github.io/keelson/subjects-and-types/))

The closest existing analogue — a Zenoh-native convention:
`{base_path}/@v{major}/{entity_id}/pubsub/{subject}/{source_id}`, RPC at
`…/@rpc/{procedure}/{source_id}`, presence via liveliness tokens, every
payload in a protobuf envelope, a ~200-entry subject registry binding
name → type → QoS.

- **Took**: the verbatim `@v{major}` version chunk (their signature move,
  endorsed by Zenoh's own KE RFC); verbatim `@rpc`; identity-first chunk
  order; the subject registry with type+QoS binding; snake_case with units
  in primitive names; presence-in-token-keys.
- **Rejected**: the trailing open-depth `source_id` — parseable only
  because Keelson subjects are single-chunk atoms; our open-depth subjects
  force the producer *before* the subject
  ([03-grammar.md §6.3](03-grammar.md)). Also their `pubsub` literal chunk:
  our class position does that job while carrying more information
  (telemetry/state/events, not just "pubsub"). And their `…/@target/{id}`
  verbatim targeting extension — a bolt-on that concedes the base grammar
  cannot address; our origin position makes targeting structural from the
  start ([05-control-rpc.md §2](05-control-rpc.md)).

## 3. rmw_zenoh / ROS 2 bridges

([rmw_zenoh design](https://github.com/ros2/rmw_zenoh/blob/rolling/docs/design.md) ·
[zenoh-plugin-ros2dds](https://github.com/eclipse-zenoh/zenoh-plugin-ros2dds) ·
[zenoh-plugin-dds](https://github.com/eclipse-zenoh/zenoh-plugin-dds))

Data keys `<domain_id>/<topic>/<type_name>/<type_hash>`; discovery under
the verbatim `@ros2_lv/…` prefix; robot/partition scoping as a leftmost
namespace prefix.

- **Took**: verbatim prefix for non-data traffic (our planes); leftmost
  scoping; liveliness-token keys as the discovery record.
- **Rejected**: type-hash-in-key — schema mismatch becomes *silent*
  non-communication with no operator signal; our registry + out-of-band
  schema version keeps the guarantee diagnosable
  ([03-grammar.md §6.4](03-grammar.md)).

## 4. MQTT Sparkplug B

([spec](https://sparkplug.eclipse.org/specification/version/3.0/documents/sparkplug-specification-3.0.0.pdf) ·
[topic-namespace overview](https://www.hivemq.com/blog/understanding-mqtt-topic-namespace-iiot/) ·
[STATE change in 3.0](https://docs.chariot.io/display/CLD80/Changes+to+the+STATE+message+in+the+Sparkplug+v3.0.0+Specification))

`spBv1.0/<group_id>/<message_type>/<edge_node_id>/<device_id>` — fixed
structure, message *type* as a topic segment (BIRTH/DEATH/DATA/CMD/STATE),
metric aliases to shrink payloads.

- **Took**: message-class-as-segment (our class position); the versioning
  lesson — STATE originally sat *outside* `spBv1.0/` and fixing it broke
  compatibility, hence our "nothing beside the version chunk" rule
  ([02-principles.md P4](02-principles.md)).
- **Rejected/avoided**: the rigid 4-level identity hierarchy (their
  admitted ISA-95 mismatch) — our identity is one opaque chunk plus
  catalog; BIRTH/DEATH lifecycle messages — Zenoh liveliness tokens and
  storage-backed state make retained-birth emulation unnecessary (their
  no-retained-state gap is native Zenoh strength); primary-application
  coupling (their STATE topic) — our catalog is an optional consumer, not
  a single point of failure.

## 5. OpenTelemetry semantic conventions

([naming](https://opentelemetry.io/docs/specs/semconv/general/naming/) ·
[schemas](https://opentelemetry.io/docs/specs/otel/schemas/))

Dot-namespaced names, resource-vs-point attribute split, reverse-domain
prefix ownership, schema URLs out-of-band, deprecate-never-remove.

- **Took**: the resource rule as P2 (identity → key, detail → payload);
  deprecate-never-reuse as P10; prefix ownership as the registry's
  vocabulary-collision rule; out-of-band schema versioning.
- **Diverged**: units — OTel keeps names unit-free (metadata always
  travels); Keelson embeds units (the key is what you see first). We split
  the difference: units in primitive leaf names where not obvious,
  authoritative unit in the registry ([08-registry.md §4](08-registry.md)).

## 6. NATS subject design

([subjects](https://docs.nats.io/nats-concepts/subjects) ·
[hierarchy guide](https://www.synadia.com/blog/designing-nats-subject-hierarchies))

The crispest articulation of the ordering question: namespace-first when
teams/domains own traffic; **identifier-first when the dominant query is
"everything about this entity"** — with per-device IoT as the canonical
identifier-first case and "the first token is the isolation key" for
tenancy. Plus: never encode per-message data in subjects; keep hierarchies
selective, not flat.

- **Took**: P11 (dominant query picks the order — and ours is per-host),
  the per-message-data prohibition (P2), isolation-token-first (our
  configurable base).

## 7. Zenoh's own guidance

([key expressions RFC](https://github.com/eclipse-zenoh/roadmap/blob/main/rfcs/ALL/Key%20Expressions.md) ·
[abstractions](https://zenoh.io/docs/manual/abstractions/) ·
[storage manager](https://zenoh.io/docs/manual/plugin-storage-manager/) ·
[access control](https://zenoh.io/docs/manual/access-control/) ·
[liveliness](https://docs.rs/zenoh/latest/zenoh/liveliness/index.html))

Not prior art so much as the physics the convention is built on: verbatim
chunks as the only hermetic separator (explicitly recommended for API
versioning); `$*` strains the infrastructure — design around it; wildcard
selectors should return one datatype; ACL matching is fastest on literal
keys and config reloads need restarts; storage `strip_prefix` must be a
literal prefix; liveliness tokens are queryable, history-capable presence.
Every one of these appears as a principle in
[02-principles.md](02-principles.md) with its consequence in the grammar.
And one feature is the convention's own front position made native: the
session **namespace** (stable config) transparently prefixes and filters
every keyexpr of a session — the `<base>` chunk implemented by the
middleware itself ([03-grammar.md §1.1](03-grammar.md)).

Two more pieces of the team's own guidance shaped the delivery tiers
([04-planes.md §3.2–3.3](04-planes.md)): the liveliness RFC warns that
universal liveliness "puts a lot of pressure on the infrastructure" —
tokens are opt-in for that reason (the convention's one-token-per-
*producer* roster, never per key, follows directly); and the
fault-tolerance RFC's design goal that publisher state stay independent of
subscriber count is exactly the property per-key heartbeats walk back —
one reason the advanced tier is priced and opt-in rather than default
([roadmap RFCs](https://github.com/eclipse-zenoh/roadmap/tree/main/rfcs/ALL);
field evidence: rmw_zenoh's token-per-entity workload,
[rmw_zenoh#763](https://github.com/ros2/rmw_zenoh/issues/763)).

The `@`-convention also carries a caveat worth recording: Zenoh's *own*
admin space lives under top-level `@/<zid>/…`. This convention never
publishes at top level (everything is under `<base>/`), so no collision is
possible — but adopters MUST NOT choose a `<base>` beginning with `@`.
Precedent inside Zenoh itself: zenoh-ext's AdvancedPublisher parks its
cache/liveliness sidecar under a verbatim `<key>/@adv/**` suffix — the
exact in-namespace verbatim-plane pattern this convention generalizes
(and those sidecar keys need their own ACL cover,
[09-operations.md §3](09-operations.md)).

## 8. D-Bus

([specification](https://dbus.freedesktop.org/doc/dbus-specification.html) ·
[API design guidelines](https://dbus.freedesktop.org/doc/dbus-api-design.html) ·
[GDBusProxy](https://docs.gtk.org/gio/class.DBusProxy.html))

The desktop/system IPC standard — not a keyspace but the most battle-tested
*interface* convention around: reverse-DNS interface names with a numeric
version suffix (`com.example.MyService1`), a methods/signals/properties
triad, standard meta-interfaces (Properties, ObjectManager,
Introspectable), namespaced error names, and broker-arbitrated
well-known-name ownership (`RequestName` allow-replacement/replace/
do-not-queue flags, an ownership queue, `NameOwnerChanged` observed by
everyone).

- **Took**: the ownership vocabulary — our `@catalog` claim protocol
  ([06-identity.md §5.3](06-identity.md)) is `RequestName` rebuilt from
  liveliness tokens: claim key = name request, lexical election = the
  broker's arbitration, standby-with-claim = `IN_QUEUE`, liveliness
  subscriber = `NameOwnerChanged` — with the honest caveat that a broker
  cannot split-brain and a mesh can, so the by-fiat rule became a protocol
  with stated failure modes. The seed pattern — GDBusProxy subscribes,
  snapshots via `GetAll`, and flushes its cache when the name owner
  vanishes; our subscribe-first / reconcile-by-timestamp rule and
  liveliness-driven staleness ([04-planes.md §3.2](04-planes.md),
  [04-planes.md §5](04-planes.md)) do the same job without a broker's
  ordering. `invalidated_properties` — notify-without-value for large
  values — as the registry's `invalidate` delivery mode
  ([04-planes.md §1.2](04-planes.md)). Runtime introspection —
  `@rpc/<producer>/introspect` serves the compiled-in registry slice, for
  the same reason Introspect XML is trustworthy: the implementation emits
  it ([08-registry.md §6](08-registry.md)). Error discipline — "a reply
  always indicates success, and an error always indicates failure":
  failures ride Zenoh's reply-error channel with registry-governed
  namespaced error names, never an `ok:false` payload
  ([05-control-rpc.md §3](05-control-rpc.md)). Versioning granularity —
  convention major ≈ the D-Bus protocol version (frozen at 1, "hopefully
  never"); per-subject `since`/`gone` + numeric-suffix siblings
  (`sockets2`) ≈ `Manager1 → Manager2`; serve-both-generations during
  deprecation ≈ owning both well-known names
  ([08-registry.md §3](08-registry.md)).
- **Rejected**: pull-first properties — D-Bus state exists only while its
  service lives; our state outlives its producer in storage, with
  tombstones D-Bus never had. Writable properties (`Set`) — all mutation
  is an `@rpc` procedure, keeping one validation/error path (large D-Bus
  services route mutation through methods for the same reason).
  ObjectManager's atomic subtree snapshot — no cross-key atomicity on a
  distributed bus; instead every state key must be independently coherent
  ([04-planes.md §1.2](04-planes.md)). Transient signals — our `events`
  are stored and immutable; a missed D-Bus signal is simply gone.
  CamelCase + reverse-DNS naming — case footguns in key chunks; registry
  prefix ownership carries the ownership idea instead. Service activation
  (the bus starting services on demand) — no Zenoh mechanism; producers
  are processes, presence is liveliness.

## 9. Homie (MQTT IoT convention)

([convention](https://homieiot.github.io/) ·
[v4 spec](https://homieiot.github.io/specification/spec-core-v4_0_0/) ·
[v5 spec](https://homieiot.github.io/specification/))

`homie/<device>/<node>/<property>` with metadata published **in-band** as
`$`-attribute topics at reserved positions (`$homie` = convention version,
`$state`, `$name`, per-property `$datatype`/`$unit`/`$settable`);
everything retained (the broker *is* the device-description database);
`$state` lifecycle with the MQTT Last Will forced to `$state = lost`;
settable properties take commands on a co-located `<property>/set` suffix,
acknowledged by reflecting the accepted value back onto the base topic.
Homie 5 made two corrections that read as an endorsement of this RFC's
choices: the convention major moved from an attribute *value* into a topic
*level* (`homie/5/…`), and the scattered `$`-topics collapsed into a
single `$description` document.

- **Took**: `$state`-with-LWT as broker-mediated presence — the pattern
  our liveliness tokens plus storage-backed `state` implement natively;
  command-acknowledged-by-state-reflection (our config-echo idiom); the
  reserved-sigil discipline (`$` is theirs, `@` is ours — but theirs is
  convention-only, ours is enforced by Zenoh's key algebra).
- **Rejected**: **in-band metadata at reserved topic positions** — every
  `$`-attribute is wire cost on every (re)connect, arrival-order races
  corrupt consumers (openHAB's long-standing
  [retained-message init issue](https://community.openhab.org/t/homie-mqtt-device-does-not-initialize-properly-from-retained-messages/95568)),
  and the description drifts from the data it describes; Homie 5 itself
  retreated to one document, and we go further — the registry is
  out-of-band entirely ([08-registry.md](08-registry.md)). Also
  **retained-everything as the device database** — retained topics have no
  tombstone story (ghost devices linger until someone scrubs the broker),
  whereas our `state` class makes retirement a first-class delete. And
  **suffix-discriminated commands** — `<property>/set` sits *inside* the
  data tree, so a device wildcard pulls commands and controller writes
  race device reflections on adjacent topics; our `@rpc` plane is
  verbatim-hermetic, and queryables return actual replies where `/set` is
  fire-and-forget.
- **The versioning witness**: through v4 the major was the `$homie`
  attribute value, so majors shared one topic space; v5 had to move it
  into the path to make coexistence possible. Second independent
  confirmation — after Sparkplug's STATE — of the version-chunk rule
  ([03-grammar.md §1.2](03-grammar.md)); ours is verbatim, so coexistence
  is key algebra, not subscription discipline.

## 10. OPC UA address space & companion specifications

([Part 3 §8.2 NodeId](https://reference.opcfoundation.org/v104/Core/docs/Part3/8.2/) ·
[Part 3 §5.2.4 BrowseName](https://reference.opcfoundation.org/v104/Core/docs/Part3/5.2.4/) ·
[Part 4 §5.8.4 TranslateBrowsePathsToNodeIds](https://reference.opcfoundation.org/v104/Core/docs/Part4/5.8.4/) ·
[companion-spec guideline OPC 11021](https://files.opcfoundation.org/GuidelinesAndTemplates/OPC%2011021%20-%20UA%20Companion%20Specification%20Guideline%201.02.1.pdf))

The industrial reference for separating machine identity from human
navigation: every node is addressed by a **NodeId** (namespace + opaque
identifier, stable across restarts) which all services use, while the
human-readable **BrowseName** "cannot be used to unambiguously identify a
Node" — humans reach nodes by *resolving* a browse path to a NodeId via a
dedicated service. **Companion specifications** — per-industry information
models under an owned namespace URI, where an incompatible major bump
changes the namespace URI itself — are the closest industrial analog to
our subject registry.

- **Took**: the NodeId/BrowseName split as decades of industrial
  validation of the origin design — the stable opaque id is the *only*
  address (`h-<12hex>`), the human name is non-unique metadata, and
  name → id resolution is a *service* (their Translate, our `@catalog`,
  [06-identity.md](06-identity.md)). Companion-spec versioning — an
  incompatible major changes the namespace *identity itself*, the same
  move as our verbatim `@v<int>`. Namespace-URI prefix ownership (with
  OTel, a second witness for the registry collision rule), and the
  harmonization lesson — competing per-vendor vocabularies for one concept
  are the failure mode a registry process must converge, not namespace
  apart ([08-registry.md §5](08-registry.md)).
- **Rejected/avoided**: **index-aliasing in addresses** — OPC UA
  compresses namespace URIs to server-local u16 indices that may change
  across restarts, and every client that persists one gets bitten; a key
  convention must never introduce a compressed alias with a different
  lifetime than the name it stands for (recorded as a constraint on the
  short-token question, [12-open-questions.md §5](12-open-questions.md)).
  Also **types-in-the-address-space** — elegant for a browsable server,
  but ontology-in-the-key at scale (the entity-kind lesson again,
  [03-grammar.md §6.2](03-grammar.md)); our types live in the registry and
  the payload envelope.
