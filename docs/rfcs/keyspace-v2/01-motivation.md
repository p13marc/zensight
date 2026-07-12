# 01 — Motivation

**Status: Draft** · informative chapter

This RFC exists because ZenSight's shipped keyspace — documented in
[`docs/KEYSPACE.md`](../../KEYSPACE.md), which remains authoritative for
what is deployed — has reached the limits of its shape. It works, it is
well-guarded by tests, and its plane separation (`@`-verbatim) is sound.
But eight structural problems recur, and each traces back to two original
choices: **protocol-first ordering** and **identity in payloads instead of
keys**.

## 1. The incumbent shape

```
zensight/<protocol>/<source>/<metric...>          telemetry
zensight/<protocol>/<source>/@/…                  host-scoped control (health, status, liveness)
zensight/<protocol>/@/…                           protocol-scoped control (alerts, commands, query, artifact)
zensight/_meta/…                                  cross-sensor metadata (sensors, evidence, entity)
zensight/<protocol>/<source>/@media/…             media plane
zensight/@pdns/<ip-slug>                          historical passive-DNS tier
```

Four planes held apart by the `@`-verbatim rule — the one invariant this
RFC keeps at its core and extends.

## 2. The eight pain points

**P1 — Retrofit debt: two control shapes at once.** Host scoping was added
after the fact (release 0.8), so every control key exists in a legacy
protocol-scoped shape and a host-scoped shape. The GUI runs *four*
liveliness subscribers and duplicated decode branches; correctness rests on
tests pinning that `zensight/*/@/**` and `zensight/*/*/@/**` never
intersect. Lesson encoded in the new grammar: scoping must be a fixed
position from day one ([03-grammar.md §1.3](03-grammar.md)).

**P2 — Identity lives in payloads.** The stable host identity
(`host_id = sha256(machine-id + salt)`) travels inside health snapshots and
evidence documents; keys carry mutable, human-chosen `<source>` names. No
consumer can group keys by machine without decoding payloads and joining
through the correlator. The new grammar puts the same hash *in the key* as
the origin chunk ([06-identity.md](06-identity.md)).

**P3 — Commands fan out to everyone.** `zensight/netlink/@/commands/…`
reaches every netlink host; targeting one host is a payload field that only
the artifact channel implements. A `set_capture` or a systemd `action`
reaches the fleet and relies on each sensor to filter. The new grammar
makes the target a key position ([05-control-rpc.md §2](05-control-rpc.md)).

**P4 — Scope asymmetry.** sysinfo's `@/query/*` is host-scoped;
netlink/netring's is protocol-scoped; parallax's stream control is
host-scoped. Every consumer must know which channel uses which scope.
The new grammar has exactly one scope: the origin.

**P5 — One structured parser, for one plane.** `parse_key_expr` handles
telemetry only (positional split, hard-coded protocol arms); every `@/…`
key is parsed by ad-hoc `strip_prefix`/`split_once` chains in the GUI,
correlator, and blob client. The new grammar is fixed-arity in positions
1–5 and registry-generated for the rest ([08-registry.md §1](08-registry.md)).

**P6 — Client-side firehose filtering.** Exporters subscribe `zensight/**`
and *discard* `_meta` and `@`-chunk keys after the bytes crossed the link.
The class chunk makes "only telemetry" a selector, not a filter
([04-planes.md §4](04-planes.md)).

**P7 — Opaque metric paths.** `<metric>` is an undocumented `/`-joined
path; ~15 GUI view files re-split it with per-protocol positional
assumptions. The registry binds every subject pattern to a type and its
variable chunks ([08-registry.md](08-registry.md)).

**P8 — Same-protocol collisions.** Two sensors of one protocol with the
same `<source>` silently overwrite each other. The producer chunk with
instance suffix makes ownership explicit ([03-grammar.md §1.5](03-grammar.md)).

## 3. What prompted the RFC now

Three converging pressures: multi-machine deployments made P1–P4 daily
frictions rather than theory; the correlator/entity layer proved the value
of stable identity and exposed how awkwardly it bolts onto name-keyed data;
and predecessor drafts (`zensight-key-semantic/`, ChatGPT-assisted)
sketched an entity-centric redesign that deserved a rigorous,
prior-art-informed treatment rather than incremental patching. The
incremental review already happened (`docs/design/zenoh-efficiency.md`
concluded "no restructuring needed" *within* the current shape); this RFC
is the clean-slate counterpart.

## 4. Goals

- A keyspace where **routing, ACL, storage selection, and bandwidth policy
  are all expressible as static literal prefixes** — policy without
  parsing.
- **Stable identity in every key**, minted without coordination, mapped —
  never re-keyed — by the catalog.
- **One scoping rule, one control mechanism, one parser.**
- A **registry** that makes the open part of the key governed, typed, and
  code-generated.
- A convention **other Zenoh applications can adopt**: everything
  application-specific is confined to the base chunk, the producer
  vocabulary, and the registry content. ZenSight is the reference
  application ([11-zensight-profile.md](11-zensight-profile.md)).

## 5. Non-goals

- **Renaming metrics.** Subject vocabulary migrates as-is wherever it is
  already good; this RFC moves *where* keys live, not what things are
  called.
- **Multi-tenancy.** Isolation is a deployment-prefix concern
  ([03-grammar.md §1.1](03-grammar.md)); no tenant machinery is specified.
- **Migration planning.** This RFC specifies the destination, not the
  journey. The `@v1` chunk guarantees the two keyspaces can coexist
  indefinitely without interference ([03-grammar.md §1.2](03-grammar.md));
  how and when ZenSight walks over is deliberately out of scope.
- **Payload schemas.** Payload types are referenced by the registry but
  their definitions and evolution rules stay with the owning crates.
