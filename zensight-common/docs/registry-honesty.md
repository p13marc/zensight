# Registry honesty — the six checks, and what none of them checks

RFC 08 §6.1 is one sentence:

> **Every subject and procedure in a registry MUST be served by the build that
> ships it.**

It is a MUST because `introspect` hands a producer's registry slice to the
fleet *as truth*, and a generic explorer has nothing else to go on. An entry
for a surface the code does not serve is not aspirational — it is a lie
transmitted to every consumer that asks.

Nothing enforces that sentence by itself. Six checks do, between them, and
they cover different halves in different places. This page says which is which,
and — more usefully — what is still not covered.

## The six checks

| Check | Direction | When | Covers |
|---|---|---|---|
| [`metric_guard`](../src/metric_guard.rs) | published ⊆ registered | run time, every put | subjects |
| `tests/registry_conformance.rs` (per sensor) | published ⊆ registered | CI | subjects |
| [`served`](../src/served.rs) | registered ⊆ served | run time, before `alive` | **procedures** |
| [`registry_audit`](../src/registry_audit.rs) | registered ⊆ emittable | CI | **subjects** |
| [`served::check_write_coverage`](../src/served.rs) | registered **write** ⊆ audited | run time, before `alive` | **procedures** |
| [`registry::kind_matches`](../src/registry.rs) | published **kind** = declared | run time, every put (debug) | **subjects** |

The fifth is #957's, and it is a different question from the third: the third
asks whether a declared procedure is *answered at all*, the fifth whether a
declared **write** is answered through a seam that records the outcome. A build
can pass the third and fail the fifth — that is precisely the state the tree was
in before #957, with eleven of twelve write surfaces leaving no trail. See
[`audit.md`](audit.md).

The two directions are not mirror images and the first does not imply the
second. A registry may be a strict superset of what the code does and every
published key still builds — and that superset is exactly what `introspect`
ships. The #453 audit found seven such surfaces advertised by builds that
served none of them.

## The sixth check: the value is the kind the registry declares (#1071)

The first five are all about *names*: is this subject registered, is this
procedure served, is this write audited. None of them could say what a number
**is**.

That gap had a bill. `zensight-sensor-container` published `restart_count`,
`cpu_usage_usec_total`, `cpu_throttled_usec_total`, `oom_kills_total` and
`memory_max_events_total` as `TelemetryValue::Gauge`. Both exporters derive the
wire type from the variant and nothing else
(`prometheus/src/mapping.rs::from_value`, `otel/src/metrics.rs::from_value`), so
`container_…_oom_kills_total` was scraped as `# TYPE … gauge` and exported to
OTLP as a Gauge — which no backend can `rate()` or delta-aggregate. Every
sibling sensor happened to get it right. `checked_point` validated
*registration* and had nothing to check *semantics* against, so nothing in the
tree could have noticed.

zenkey 0.8.0 closed the upstream half: `kind = "counter" | "gauge" | "text" |
"bool"` on `SubjectDecl` (RFC 08 §2 v1.32), carried into the generated
`Subject::kind()` and into `registry.lock` as an optional sixth column. Adding
a `kind` is **stale** (regenerate the lock); changing or removing one is
**incompatible** (retire and add a sibling) — so each declaration has to be
right the first time.

`registry::kind_matches(producer, metric, &value)` is the local half, run from
each crate's `checked_point`. Two asymmetries are deliberate:

- **An undeclared subject is unjudged, never wrong.** Most subjects carry no
  `kind` yet; the check returns `Ok(())` for them, which is the same asymmetry
  zenkey-fleet's `kind-mismatch` doctor check uses on a live bus.
- **`bool` accepts a `Gauge`.** A 0/1 step series is legitimately published
  either way today and both render as a gauge; forcing the variant is a
  separate change with its own wire note.

`container.toml` is the first slice declared. The remaining ~300 subjects are a
mechanical pass, and one worth doing carefully: the CI conformance job runs
`sysinfo logs systemd hostspec probe` against a live bus, where zenkey-fleet
0.13.0's `kind-mismatch` judge reports an Error per disagreeing key — so a
declaration and its publish site have to land in the same commit.

## Why the subject half cannot be a runtime check

`served` runs at `introspect` time because that is the moment the claim is
made. A *procedure* suits that moment: it is served by a declaration the
process makes once, unconditionally, on a known key.

A *subject* does not:

- **Publishers are declared lazily.** `PublisherRegistry::ensure` declares one
  on the first put for a key, so at `introspect` time a perfectly healthy
  producer has declared almost nothing.
- **Later, the served set is still incomplete — correctly.** It is the
  intersection of "this build can emit it" with "this host has that hardware,
  traffic and permission this minute". A box with no WireGuard never publishes
  `wireguard/*`; a kernel without eBPF never publishes
  `sockets/tcp/connlat_us_*`. Both are right, and a runtime check cannot tell a
  registry that lies from a host that is simply boring.

So the subject half is checked at **test time**, against the producer's
mappers, which is a question about code rather than about this host.

## Conditional surfaces — the actual gap

A registry entry still has no way to say *"only in builds with feature X"* —
the `feature`/`when` field remains deferred upstream (zenkey #171). What zenkey
0.7 added instead is a **ledger beside** the TOMLs:
[`registry/conditional.lock`](../registry/conditional.lock), one
`<producer>\t<path>\t<condition>` line per conditional subject, checked by
`zenkey-build` at build time (a line naming no live registry subject fails the
build) and read at test time through
`registry_audit::conditional_families(producer)`. The ledger *conditions* an
entry; it does not replace one — the subject is still declared and still served
through `introspect` unmarked.

So a build-conditional surface still has exactly two honest options, and
**silence is not one of them** (#648):

**Procedures — declare unconditionally, answer an error.** This is now the rule
throughout the workspace. Four outcomes stay distinguishable for a caller:

| what the caller sees | what it means |
|---|---|
| no reply at all | no such producer on the bus |
| `error/unsupported` | producer present, capability not in this build → **rebuild** |
| `error/gated` | capability built in, switched off here → **reconfigure** |
| an empty value reply | capability live, nothing to report |

Declaring nothing collapses the middle two into the first. `[]` collapses them
into the fourth. Both are the silence the check exists to prevent.

**Subjects — a reviewed ledger.** A procedure that cannot answer can still
*reply*; a gauge that has no reading cannot *publish*. A sentinel value
(`-1`, `NaN`) would corrupt every downstream consumer, and publishing nothing is
indistinguishable from an idle host. There is no honest wire representation of
"this gauge does not exist in this build", so such families are listed in
`registry/conditional.lock` with the condition that gates them.

The ledger is checked in both directions, and since #739 the two halves live in
two places on purpose: **`zenkey-build` fails the build** when a line names no
live registry subject (so an excuse cannot outlive its entry, even for a
producer with no conformance test), and `registry_audit::assert_families_covered`
fails the test when a ledgered family *is* emitted (so the excuse cannot outlive
the gate). Neither half lets it decay into a permanent excuse.

The whole workspace has **two** lines, and that is correct rather than an
oversight — the lock file's header says why, at length, so the next reader does
not "fix" it. In short: a gated *procedure* needs no exemption, because it is
declared unconditionally and answers `error/gated` or `error/unsupported`; and
netring's detector features widen the value space of `anomaly/{kind}/total`
rather than adding subjects.

## Coverage today

Every producer with a finite telemetry tree is now covered (#654).

| Producer | Families | Ledger |
|---|---|---|
| `sysinfo` | 121 | empty |
| `netlink` | 106 | **2** — `sockets/tcp/connlat_us_{p50,p95}` (the only two in the workspace) |
| `netring` | 70 | empty |
| `systemd` | 37 | empty |
| `logs` | 23 | empty |
| `parallax` | 6 | empty |
| `snmp`, `modbus`, `gnmi`, `netflow` | **exempt** | rest-var telemetry (`{device}/{metric...}`); the check is vacuous, and `assert_families_covered` refuses to run rather than pass them for free |
| `catalog` | 0 telemetry subjects | n/a |

The procedure half covers **every** producer, and is verified by starting each
sensor binary on its stock config: see the sweep in #648.

### Writing one of these tests

Two shapes, and which you get is decided by the producer, not by preference:

- **Drive the real mappers** where metric names come from pure functions
  (netlink, netring, systemd, logs). Strongest, because the test cannot drift
  from the code.
- **List one representative per family** where names are built inline with
  `format!` (parallax, and sysinfo's collector-built families). Pair it with a
  forward assertion that each representative *is* registered — without that, the
  reverse test can pass on a list of typos.

**Fixture completeness is the whole difficulty**, and it fails in two directions
that both look like a registry bug:

- *Empty is not neutral.* `IfaceSample::default()` has an empty interface name
  and produces `iface//rx_bytes`, which trips the grammar guard before coverage
  is even evaluated.
- *Zero is not neutral either.* Many families are gated on a value being `> 0`
  or an `Option` being `Some` — netlink's socket percentiles, systemd's
  accounting fields, netring's `Some(pcts)` arguments. A zeroed fixture reports
  live families as unemitted and sends the next reader hunting a bug that is not
  there.

Also watch for mappers that pick a *name* from an argument: netring's
`shed_points` chooses `sampled_total` or `new_flows_total` by policy, so one
call covers one family. Call it once per branch.

## What none of the six checks checks: does the payload conform?

Every check above is about *names* — is this subject registered, is this
procedure served, does this type appear in the type table. None of them looks at
a single byte of payload. A producer can register `TelemetryPoint`, serve
`describe` with its schema, and publish something else entirely.

`zensight_common::schema::verdict_for(type_name, &value)` (#741, behind the
`validate-json` feature) is the byte-level check: real draft-2020-12 validation
against the served schema. It answers in **three states, never a boolean** —
"I did not check" must never render like "I checked and it passed":

| Answer | Meaning |
|---|---|
| `Valid` | checked against a real schema, conformant |
| `Invalid(errors)` | checked, one sentence per violation with its instance path |
| `NotValidated(FeatureOff)` | this binary was built without `validate-json` |
| `NotValidated(NoSchema)` | the table was consulted and serves nothing for this type |
| `NotValidated(KindUnsupported)` | a `protobuf`/`cdr` entry, whose decode *is* the check |
| `NotValidated(BadSchema)` | the served document does not compile as a schema |

`NoSchema` and `FeatureOff` are deliberately different: one is "asked, and the
type has none", the other is "nobody looked" (RFC 09 §5.1 O4).

Two limits worth knowing before reading a `Valid` as a strong claim:

- The **summary entries** in `SCHEMAS` — types whose Rust definition lives in a
  sensor crate, plus the declared-only names — are `{"type": "object"}`. A
  `Valid` against one means "it is a JSON object" and no more. The schema is
  thin, so the claim is thin; that is honest, and upgrading it is the follow-up
  already noted per entry.
- **Nothing calls it yet.** The GUI is the natural consumer, and it has no
  payload-inspection surface — it decodes bytes into typed structs at
  `subscription.rs`'s `decode_sample` and drops them. Building that surface is
  a feature in its own right; `src/schema.rs` carries the note on what it needs.

## The fifth check: a state family serves a complete schema (#815)

The fleet notifier (`zenwatch`, zenkey#388) is key-agnostic: it renders a
notification by decoding the payload **through the producer's served schema**,
with no compiled-in knowledge of `Alert` or any other ZenSight type. That turns
schema completeness from a nicety into a wire contract — and the upstream
checks cannot hold it: `describe-totality` is name-presence only (a
`{"type":"object"}` stub satisfies it), and `describe-missing` is Info by
design.

So the rule is gated the way the subject half of §6.1 is — **at test time**,
in `schema.rs`'s `every_state_family_serves_a_generated_schema`: for every
registered `class = "state"` subject, the served entry must be
schemars-generated (`$schema` stamped, non-empty `properties`), and **no
property may be an anything-goes schema** (`true`, `{}`, or description-only —
the shape a bare `serde_json::Value` field silently produces; `SensorInfo.
metadata` was exactly that hole until #815 typed it). The fields a renderer
actually reads (severity, rule, labels, summary, state, …) are additionally
pinned by name in `state_document_fields_a_renderer_reads_are_pinned`, so a
`#[serde(rename)]` breaks the build before it breaks a page.

The rule for producers: **a new state subject's `type` must be a
schemars-derived type in this crate** — a summary entry backs a procedure,
never a state family. The deployment-side complement (re-enabling `field-new`
in the conformance gate) waits on zenkey#384.

## See also

- [`keyspace-helpers.md`](keyspace-helpers.md) — how keys are built
- RFC 08 §5/§6.1 in the [zenkey repo](https://github.com/p13marc/zenkey)
