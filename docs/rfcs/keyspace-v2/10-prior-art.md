# 10 — Prior Art

**Status: Draft** · informative chapter

The convention was designed against six existing systems. For each: the
concrete grammar, what this RFC took, and what it rejected — with the why.
All claims were verified against the cited sources (fetched July 2026).

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
  (telemetry/state/events, not just "pubsub").

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

The `@`-convention also carries a caveat worth recording: Zenoh's *own*
admin space lives under top-level `@/<zid>/…`. This convention never
publishes at top level (everything is under `<base>/`), so no collision is
possible — but adopters MUST NOT choose a `<base>` beginning with `@`.
