# 02 — Design Principles

**Status: Draft** · normative chapter

Eleven principles, each with its provenance. Every rule in the normative
chapters traces back to one of these; when a future change is debated, the
debate should happen here first. Written application-neutrally: these hold
for any Zenoh application adopting the convention.

---

**P1 — Keys are routing addresses; the catalog carries meaning.**
A key exists to be subscribed, queried, stored, and permissioned. Ontology
— what a thing *is*, what it relates to, what it is called today — belongs
in catalog documents that can be corrected without re-keying. If a fact can
change with better evidence, it must not be a chunk.
*(Inherited from the predecessor drafts — their strongest idea — and the
reason their own `entities/<kind>/` chunk had to go.)*

**P2 — Identity in the key, detail in the payload.**
Chunks carry what selection needs: who (origin, producer), what kind
(class), what about (subject). Everything else — values, timestamps,
labels, per-message data — is payload. The dividing line is OTel's
resource-vs-point rule; the hard prohibition is per-message data in keys
(NATS: request-ids in subjects "explode cardinality and pollute caches").

**P3 — Policy boundaries are static literal prefixes.**
Zenoh ACLs match by keyexpr inclusion and cannot be reloaded at runtime;
storage `strip_prefix` must be a literal prefix; constrained links filter
by prefix. Therefore every boundary policy cares about — deployment,
version, origin, class — is a fixed-position literal from the left, and
variance (open subjects) is pushed right. Corollary the planes impose:
because `**` never crosses a verbatim chunk, "one principal" is a *fixed
set* of prefix rules (one per plane), not one rule
([03-grammar.md §4 D6](03-grammar.md)).
*(Zenoh ACL + storage-manager guidance.)*

**P4 — Verbatim chunks make planes; everything lives under the version.**
`@`-verbatim chunks are the only mechanism that makes separation *hermetic*
— wildcards structurally cannot cross them — so plane boundaries
(`@rpc`, `@media`, `@blob`) and the version boundary (`@v1`) are verbatim.
And nothing is ever placed beside the version chunk: Sparkplug's
STATE-outside-`spBv1.0/` needed a breaking release to fix.
*(Zenoh key-expression formalism; Keelson `@v`/`@rpc`; rmw_zenoh
`@ros2_lv`; Sparkplug's cautionary tale.)*

**P5 — One payload type per wildcard result set.**
Any legal selector must yield a homogeneous, decodable stream. This is
Zenoh's own bandwidth guidance ("a strict hierarchy where any wildcard
returns a single data type") and the registry's core invariant.

**P6 — Never `$*`; design chunks so it is never wanted.**
Multi-valued data goes in separate chunks (`if/eth0/rx_bytes`), not
compound ones (`if-eth0-rx-bytes`). `$*` strains matching infrastructure
and signals a key that should have been split. *(Zenoh guidance.)*

**P7 — Declared publishers only.**
Every publisher on `telemetry`, `state`, and `@media` is declared
(interned key, primed routing, attached QoS); ad-hoc one-shot puts are
banned. Queryables serve everything pull-shaped. (Two scoped exemptions,
both write-once keys where interning buys nothing: `@blob` content-store
seeding and `events` publication — [04-planes.md §3](04-planes.md).)
*(Reference application's existing CI-enforced rule, promoted to
convention.)*

**P8 — Stable opaque ids in keys; mutable names in metadata.**
An id in a key is forever, so it must be mintable alone (no coordinator),
stable across restarts, and promise nothing that evidence could later
contradict. Names, kinds, and roles are catalog facts. When identity
conclusions change, the catalog publishes aliases — data is never re-keyed.

**P9 — The bus is low-cardinality; everything else is pull.**
Push is for bounded or explicitly budgeted key sets (metrics, state,
rate-budgeted events, population-budgeted state —
[04-planes.md §1.2](04-planes.md)).
Per-line, per-flow, per-packet detail lives in bounded rings at the
producer and is served on demand via `@rpc`; bulk bytes are `@blob`
queryables. A consumer that doesn't ask doesn't pay — the property that
makes constrained links workable.

**P10 — Deprecate, never reuse.**
A subject path, once registered, keeps its meaning forever; renames are
addition + deprecation. Consumers may lag years behind; a reused path is a
silent type confusion. *(OTel's deprecate-never-remove.)*

**P11 — The dominant query picks the chunk order.**
Identifier-first vs namespace-first is not aesthetics: put first what the
most valuable selectors and policies group by. For an observability fleet
that is the origin (per-host views, per-host ACL, per-host link budgets) —
with the class second so infrastructure selects on it, and the producer
third so protocol views stay one-`*` cheap. *(NATS subject-design
guidance, applied.)*
