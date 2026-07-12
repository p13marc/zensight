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
  code (Rust constants + typed builders in the reference implementation),
  so producers and consumers share one source of truth and a typo is a
  compile error. Positional `split('/')` re-parsing scattered across
  consumers — the incumbent's ~15 view files' worth — is replaced by
  generated accessors for each subject's variable chunks.
- **Reviewable evolution.** Adding a subject is a registry diff — visible,
  reviewable, and versioned — not a key that quietly appears on the bus.

## 2. Entry format

One TOML document per producer (or service), checked into the repository
that owns the producer. Fields:

```toml
# registry/netring.toml
[producer]
name = "netring"
description = "wire-level flow/L7/NDR sensor"

[[subject]]
path        = "flow/red/{quantile}"        # subject pattern; {var} = one documented chunk
class       = "telemetry"                   # telemetry | state | events
type        = "TelemetryPoint"              # payload type (registry-wide type table)
qos         = "telemetry"                   # class default; override with justification
unit        = "ms"                          # primitive subjects: unit suffix convention, see §4
cardinality = "per-quantile (≤5)"           # human-checked budget
since       = "1.0"                         # registry version that introduced it
description = "flow-lifetime RED quantiles from the capture path"

[[subject]]
path        = "alert/{alert_key}"
class       = "state"
type        = "Alert"
qos         = "alert"                       # reliable/block/interactive-high
cardinality = "per firing rule+labels hash"
since       = "1.0"
description = "detector alerts; firing→resolved on one key, delete = tombstone"

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

Rules:

- `{var}` chunks MUST document their domain (device name, unit slug,
  ip-slug, ULID, hash…). A pattern with a `{var}` still binds one payload
  type across all its expansions.
- Every `events` subject MUST carry a rate-class in `cardinality`
  ([04-planes.md §1.3](04-planes.md)); registry review is where the budget
  is enforced.
- Service origins (`@catalog`) register the same way, with `[service]`
  instead of `[producer]`.

## 3. Versioning policy

Two independent version axes, deliberately decoupled:

| Axis | Mechanism | Bumped when |
|---|---|---|
| **Convention major** | the `@v<int>` key chunk | grammar positions or their semantics change incompatibly — hermetic break by key algebra ([03-grammar.md §1.2](03-grammar.md)) |
| **Registry version** | `since`/`gone` fields, MAJOR.MINOR | MINOR: additive (new subjects/procedures). MAJOR: a subject's meaning, type, or class changes |

- **Deprecate, never reuse.** A retired subject keeps its registry entry
  (`gone` + `replaced_by`) forever; its path is never rebound to a
  different meaning or type. Renames are additions plus deprecations
  (OTel's model; [02-principles.md P10](02-principles.md)).
- **A subject's payload type may evolve compatibly** (additive fields) under
  the payload format's own rules (self-describing encodings — CBOR/JSON —
  tolerate additive change). An incompatible payload change is a new
  subject (`…/v2` leaf or a new name), not a silent rebind.
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
  convention repository holds the shared type table and the reserved-token
  list ([03-grammar.md §3](03-grammar.md)).
- Prefix ownership is the collision rule at the vocabulary level: a
  producer may only register subjects under its own producer chunk
  (OTel's namespace-squatting rule, adapted).
- CI SHOULD enforce: every published key is buildable from a registry
  entry; every registry path is lexically legal; no `deprecated` path is
  re-registered; every `events` entry has a rate class.
