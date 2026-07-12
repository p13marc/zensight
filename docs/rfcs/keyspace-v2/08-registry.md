# 08 — The Subject Registry

**Status: Draft** · normative chapter

The grammar fixes positions 1–5 of every key; the registry governs the rest.
It is the single, machine-readable inventory of every subject, procedure,
media stream shape, and service origin a deployment's components may use —
the convention's equivalent of Keelson's subject registry and OTel's
semantic-convention YAML ([10-prior-art.md](10-prior-art.md)).

The registry is what keeps an open-ended `<subject...>` from decaying into
folklore: a subject that is not registered does not exist.

---

## 1. What the registry buys

- **One payload type per selector.** Every registry entry binds a subject
  pattern to exactly one payload type, so any wildcard result set is
  homogeneous and decodable without sniffing
  ([02-principles.md P5](02-principles.md)).
- **Generated constants, not string literals.** The registry compiles to
  code, so producers and consumers share one source of truth and a typo is
  a compile error. The codegen contract has two directions, both
  normative:
  - *build*: per subject, a constant for the pattern and a typed builder
    taking one argument per `{var}` (a chunk-list for a `{var...}`),
    producing a canonical key;
  - *parse*: per producer, a matcher taking a key's subject tail and
    returning which registered subject it is plus its extracted variables
    — this is what replaces positional `split('/')` re-parsing scattered
    across consumers (the incumbent's ~15 view files' worth). Match
    precedence is most-literal-first: literal chunks beat `{var}` beats
    `{var...}`, position by position, so a deprecated literal
    (`flow/duration_p50_ms`) still matches its own entry, not a
    later-added `flow/{x}`.
- **Reviewable evolution.** Adding a subject is a registry diff — visible,
  reviewable, and versioned — not a key that quietly appears on the bus.

## 2. Entry format

One TOML document per producer (or service), checked into the repository
that owns the producer. Example (fields annotated inline, normative field
table below):

```toml
# registry/netring.toml
[registry]
version = "1.2"                             # this file's MAJOR.MINOR (§3)
app     = "zensight"                        # owning application
convention = 1                              # the @v major it targets

[producer]
name = "netring"                            # base name; instances suffix -<int> in keys
description = "wire-level flow/L7/NDR sensor"

[[subject]]
path        = "flow/red/{quantile}"        # subject pattern
class       = "telemetry"                   # telemetry | state | events
type        = "TelemetryPoint"              # payload type (shared type table, §5)
qos         = "sampled"                     # QoS profile (04-planes §3); omit = class default
unit        = "ms"                          # primitive subjects: unit suffix convention, see §4
cardinality = 5                             # expected key-population bound for the {var}
since       = "1.0"                         # registry version that introduced it
description = "flow-lifetime RED quantiles from the capture path"

[[subject]]
path        = "alert/{alert_key}"
class       = "state"
type        = "Alert"
qos         = "alert"
cardinality = 64                            # max concurrently-firing keys, order of magnitude
ttl_s       = 900                           # live-state staleness TTL (04-planes §1.2)
since       = "1.0"
description = "detector alerts; firing→resolved on one key, delete = tombstone"

[[subject]]
path        = "capture/{ulid}"
class       = "events"
type        = "CaptureRecord"
rate        = "rare"                        # events only: rare | low | burst(n/h)
replay      = "window(7d)"                  # events only: deployment must keep 7 days queryable
since       = "1.0"
description = "a capture was triggered; immutable audit record"

[[procedure]]
path        = "capture/trigger"
kind        = "write"                       # read | write | long-running
request     = "CaptureTrigger"
reply       = "Ack"
idempotent  = false
since       = "1.0"
description = "fire the pre-trigger ring / rotate the spool"

[[media]]
path        = "{stream}/video/{codec}/{profile}"
encoding    = "video/*"
attachment  = "FrameMeta"
since       = "1.0"

[[deprecated]]
path        = "flow/duration_p50_ms"
class       = "telemetry"
since       = "1.0"
gone        = "1.2"                          # still reserved; never reused
replaced_by = "flow/red/p50_ms"
```

Open-depth subjects use the rest-variable, in their **own producer's**
file (§5's ownership rule — a `gnmi/…` path inside `netring.toml` would be
namespace-squatting):

```toml
# registry/gnmi.toml
[producer]
name = "gnmi"

[[subject]]
path        = "{device}/{path...}"          # {path...}: rest-variable, see rules
class       = "telemetry"
type        = "TelemetryPoint"
cardinality = 10000                         # bounded by the device subscription list
since       = "1.0"
description = "gNMI subscription paths, slugged per 03-grammar §2"
```

Normative field table (`[[subject]]`; `[[procedure]]`/`[[media]]` analogous):

| Field | Type | Required | Meaning |
|---|---|---|---|
| `path` | pattern string | yes | subject pattern; see variable rules below |
| `class` | enum `telemetry\|state\|events` | yes | data class ([04-planes.md §1](04-planes.md)) |
| `type` | type-table name | yes | the one payload type of every expansion |
| `qos` | enum, profiles of [04-planes.md §3](04-planes.md) | no (class default) | named QoS profile |
| `unit` | string | primitive numerics only | unit of the leaf value |
| `cardinality` | integer | yes if `path` has any `{var}` | expected key-population bound (order of magnitude); the budget review enforces |
| `ttl_s` | integer | live `state` only | staleness TTL; publishers refresh ≤ ttl/2, consumers age out at ttl |
| `rate` | `rare` \| `low` \| `burst(n/h)` | `events` only | rate class (CI-checked, [04-planes.md §1.3](04-planes.md)) |
| `seed` | `none` \| `latest` \| `tail(n)` | no (class default: `state` → `latest`, `telemetry` → `none`) | late-joiner entitlement ([04-planes.md §3.1](04-planes.md)); *how* it is met (storage vs cache) is deployment config |
| `detect_s` | integer | no (live `state` only; default = `ttl_s`) | max latency to detect a missed transition; values ≪ `ttl_s` require the advanced tier ([04-planes.md §3.3](04-planes.md)) |
| `replay` | `none` \| `window(t)` | `events` only | how far back events must stay queryable (met by the events storage) |
| `delivery` | `full` (default) \| `invalidate` | no | oversized-state pattern ([04-planes.md §1.2](04-planes.md)) |
| `since` / `gone` / `replaced_by` | registry versions / path | `since` yes | lifecycle (§3) |
| `description` | string | yes | one line, human |

Variable rules:

- `{var}` = exactly one chunk; MUST document its domain (device name, unit
  slug, ip-slug, ULID, hash…) in the description or a `domain` sub-key.
- `{var...}` = **rest-variable**: one or more chunks, allowed only in
  trailing position, at most one per pattern. This is how open-depth
  subjects (gNMI paths, directory-like metrics) register without
  enumerating every path: the pattern still binds exactly one payload type
  across all expansions, and its `cardinality` budget covers the whole
  family. Generated accessors expose the rest as a chunk list.
- A pattern with any variable still binds one payload type across all its
  expansions ([02-principles.md P5](02-principles.md)).
- Service origins (`@catalog`) register the same way with `[service]`
  replacing `[producer]` — same fields minus instances (services have no
  instance suffix), subjects keyed directly under the class chunk:

```toml
[service]
name = "catalog"
origin = "@catalog"
description = "identity/ontology service (zensight-correlator)"
```

## 3. Versioning policy

Two independent version axes, deliberately decoupled:

| Axis | Mechanism | Bumped when |
|---|---|---|
| **Convention major** | the `@v<int>` key chunk | grammar positions or their semantics change incompatibly — hermetic break by key algebra ([03-grammar.md §1.2](03-grammar.md)). The posture is D-Bus's "hopefully never": the protocol version froze at 1 |
| **Registry version** | the `[registry] version` header + `since`/`gone` fields, MAJOR.MINOR, one stream per registry file | MINOR: additive (new subjects/procedures, deprecations). MAJOR: a reviewed break that would otherwise be a forbidden rebind — reserved for the exceptional case where deprecate-and-add cannot express the change; in the normal course MAJOR never moves |

Each registry *file* versions independently (its producer's stream);
`since`/`gone` values refer to the file's own stream. A second application
adopting the convention starts its own files at 1.0 — there is no global
registry version to coordinate.

- **Deprecate, never reuse.** A retired subject keeps its registry entry
  (`gone` + `replaced_by`) forever; its path is never rebound to a
  different meaning or type. Renames are additions plus deprecations
  (OTel's model; [02-principles.md P10](02-principles.md)).
  `[[deprecated]]` entries are **append-only**: CI fails if one disappears
  from the file — that is what makes never-reuse mechanically checkable.
- **A subject's payload type may evolve compatibly** (additive fields) under
  the payload format's own rules (self-describing encodings — CBOR/JSON —
  tolerate additive change). An incompatible payload change is a **new
  sibling name with a numeric suffix** (`sockets` → `sockets2`, D-Bus's
  `Manager1 → Manager2` move), never a version *leaf*: a `sockets/v2` leaf
  would sit inside every wildcard that matches `sockets/**`, putting two
  payload types in one result set (violating P5), whereas a suffixed
  sibling is invisible to selectors written against the original. During a
  deprecation window the producer SHOULD serve/publish both generations
  (D-Bus services own both well-known names); `replaced_by` tells consumers
  where to migrate.
- **The wire carries the registry version out-of-band** (payload envelope or
  attachment), not in the key — the key algebra only needs to isolate
  *grammar* breaks; payload-schema evolution is diagnosable from the data
  (contrast rmw_zenoh's silent type-hash isolation,
  [03-grammar.md §6.4](03-grammar.md)).

## 4. Naming rules

- Chunks: lowercase snake_case within the lexical rules of
  [03-grammar.md §2](03-grammar.md); prefer chunk hierarchy over compound
  names (`cpu/usage`, not `cpu_usage`; `if/eth0/rx_bytes`, not
  `if-eth0-rx-bytes`) — hierarchy is what wildcards can select on.
- **Primitive numeric leaves carry their unit as a suffix** where the unit
  is not obvious from the name: `total_usec`, `p95_ms`, `rx_bytes`,
  `usage_percent` (Keelson's convention). Structured payloads carry units
  in metadata instead (OTel's convention); the registry `unit` field is
  authoritative either way. The key suffix exists for the human reading a
  raw bus, not for machines.
- Counters are singular with a `_total` suffix; gauges are bare; ratios
  say their scale (`_percent` vs `_ratio`).
- Subject vocabulary SHOULD reuse established semantic names where a
  mapping exists (OTel host metrics, SNMP MIB names) — the registry entry
  is the right place to record the cross-standard mapping, as the reference
  application does for its exporter semconv table.

## 5. Ownership and process

- Each producer's registry file lives with the producer's code; the
  convention repository holds the **shared type table** — one TOML/JSON
  document mapping each `type` name to its schema location (crate + item
  in the reference implementation, or a schema URL) — and the
  reserved-token list ([03-grammar.md §3](03-grammar.md)). A `type` name
  not present in the type table fails CI; that is what makes
  `type = "TelemetryPoint"` resolvable for a second application.
- Prefix ownership is the collision rule at the vocabulary level: a
  producer may only register subjects under its own producer chunk
  (OTel's namespace-squatting rule, adapted).
- When two producers observe the same real-world concept, the process
  SHOULD converge them on one shared subject vocabulary (recorded in the
  shared type table) rather than letting parallel producer-prefixed
  spellings coexist — OPC UA's harmonized companion specifications are the
  precedent ([10-prior-art.md](10-prior-art.md)).
- CI SHOULD enforce: every published key is buildable from a registry
  entry; every registry path is lexically legal (including: no producer
  base name ending in `-<int>`, [03-grammar.md §1.5](03-grammar.md); no
  reserved token as a subject leaf); no `deprecated` path is re-registered
  and no `[[deprecated]]` entry is ever deleted; every `events` entry has
  a `rate`; every `{var}`-bearing entry has a `cardinality`; every live
  `state` entry has a `ttl_s`.

## 6. Runtime introspection

The static TOML is the *authority*; a running fleet additionally serves
the *observation* of it. Every producer MUST serve
`@rpc/<producer>/introspect` (read, idempotent) returning the registry
slice it was **compiled against** — its subjects, procedures, media
shapes, and registry file version. The reply is generated from the same
source as the producer's key constants, so it cannot drift from behavior
(the reason D-Bus introspection XML is trustworthy: the implementation
emits it — [10-prior-art.md](10-prior-art.md)).

What it buys: `GET <base>/@v1/*/@rpc/*/introspect` is a fleet
capability-and-version inventory in one round trip (which hosts still
serve a deprecated subject; which run last month's registry); generic
explorer tooling — the `busctl`/`d-feet` equivalent — needs no compiled-in
registry. A disagreement between introspection and the checked-in TOML is
a finding, not an ambiguity: the TOML says what *should* run, the
introspection says what *does*.
