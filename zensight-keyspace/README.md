# zensight-keyspace

The executable form of the **keyspace-v2 convention**
([`docs/rfcs/keyspace-v2/`](../docs/rfcs/keyspace-v2/00-index.md), v1).
Producers and consumers emit and parse conforming keys through this crate and
never spell raw key strings.

```text
@v1/<origin>/<class>/<producer>/<subject...>        (base-relative; the base
                                                     is the session namespace)
```

| Module | Enforces | Mechanism |
|---|---|---|
| `grammar` | RFC 03 — chunk charset, reserved tokens, structural assembly/parse | validation + typed builders: an invalid key does not construct |
| `origin` | RFC 06 §1 — `h-<12hex>` minting, fallbacks | one function, pinned to the RFC test vector |
| `slug` | RFC 03 §2 — RFC-5952 IP canon, lossless `_xNN_` escape | pure functions, injectivity-tested |
| `qos` | RFC 04 §3 — the five named profiles | closed enum → zenoh QoS triple |
| `slice` | RFC 08 §6 — `RegistrySlice`, the `introspect` reply type + the diff | parse a served slice, diff it against ours; a disagreement is a *finding* |
| `tests/guard.rs` | RFC 03 §4 — design properties D1–D6, ACL inclusion | key algebra pinned as CI tests |

The subject vocabulary is governed by the registry (RFC 08); generated
per-subject builders/parsers are produced by this crate's build script from
`registry/*.toml` (epic #453, issue #455).

## The two directions

Codegen is normative in **both** directions (RFC 08 §1), and both have callers.

**Build** — an unregistered subject does not construct:

```rust
let key = ctx.telemetry_key(sysinfo::Subject::DiskUsed { mount: "_".into() })?;
```

**Parse** — the direction that exists to delete positional `split('/')` from
consumers (issue #475). `TelemetryPoint::metric` *is* the telemetry subject tail,
verbatim, so a metric name refines straight into a typed subject with its
variables named:

```rust
match sysinfo::Subject::parse_metric(&point.metric) {
    Some(sysinfo::Subject::DiskUsed { mount }) => …,     // not parts[1]
    Some(sysinfo::Subject::SensorsTemp { chip, label }) => …,
    None => { /* unregistered — see the guard below */ }
}
```

For a whole wire key, `zensight_common::keyexpr::refine_wire_key` gives you
`(StructuralKey, producer, AnySubject)` in one step, and `AnySubject::common_state()`
classifies the shared state subjects (health/errors/alert/evidence/…) as an
exhaustive match — so moving a subject in the registry becomes a compile error in
every consumer, which is the whole point.

## The registry is load-bearing

A telemetry subject that is not registered does not publish quietly: the guard on
the publish path (`zensight_common::metric_guard`) panics in debug builds and warns
once per metric name in release. That check is only meaningful because the six host
producers register their telemetry as real subject families rather than one
`{metric...}` catch-all (issue #468) — a catch-all makes RFC 08 §5's lint
vacuously true.

`snmp` / `modbus` / `gnmi` / `netflow` keep a rest-var **by design**: their metric
tree is defined by the polled device or the exporter's template, not by us. That is
a genuinely open tail and the correct use of `{var...}`.

Each of the six host producers funnels metric construction through a checked
constructor, so its existing mapper tests *are* its conformance suite — and each has
a `#[should_panic]` test proving the guard bites, because a conformance suite that
cannot fail is the same mistake as the catch-all it replaced.

**A consumer that cannot parse a subject drops it — it does not fall back to string
parsing.** The #477 plan originally hedged the other way, so a metric the registry
missed would still render. That hedge is deliberately retired: a fallback silently
masks an unregistered subject, which is precisely the defect this crate exists to
prevent ("a subject that is not registered does not exist"), and it would keep
positional parsing alive across ~30 view sites. The guards above make a gap loud at
*publish* time instead, which is strictly better than quiet in one view.

Migration status: introduced by epic
[#453](https://github.com/p13marc/zensight/issues/453); absorbs
`zensight-common/src/{keyexpr,command}.rs` as the waves land.
