# Registry honesty — the four checks, and what none of them checks

RFC 08 §6.1 is one sentence:

> **Every subject and procedure in a registry MUST be served by the build that
> ships it.**

It is a MUST because `introspect` hands a producer's registry slice to the
fleet *as truth*, and a generic explorer has nothing else to go on. An entry
for a surface the code does not serve is not aspirational — it is a lie
transmitted to every consumer that asks.

Nothing enforces that sentence by itself. Four checks do, between them, and
they cover different halves in different places. This page says which is which,
and — more usefully — what is still not covered.

## The four checks

| Check | Direction | When | Covers |
|---|---|---|---|
| [`metric_guard`](../src/metric_guard.rs) | published ⊆ registered | run time, every put | subjects |
| `tests/registry_conformance.rs` (per sensor) | published ⊆ registered | CI | subjects |
| [`served`](../src/served.rs) | registered ⊆ served | run time, before `alive` | **procedures** |
| [`registry_audit`](../src/registry_audit.rs) | registered ⊆ emittable | CI | **subjects** |

The two directions are not mirror images and the first does not imply the
second. A registry may be a strict superset of what the code does and every
published key still builds — and that superset is exactly what `introspect`
ships. The #453 audit found seven such surfaces advertised by builds that
served none of them.

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

A registry entry has no way to say *"only in builds with feature X"*. The TOML
schema is owned by the external `zenkey` crate; adding a `feature`/`when` field
needs a schema change, a codegen change and a crates.io release. Until then a
build-conditional surface has exactly two honest options, and **silence is not
one of them** (#648):

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
"this gauge does not exist in this build", so such families are listed in the
sensor's `CONDITIONAL_FAMILIES` ledger with the condition that gates them.
`registry_audit::assert_families_covered` checks the ledger in both directions —
an entry the build *does* emit fails, and an entry the registry no longer
declares fails — so it cannot decay into a permanent excuse.

## Coverage today

| Producer | Subject check | Note |
|---|---|---|
| `sysinfo` | ✅ `tests/registry_conformance.rs` | 121 families, ledger empty |
| `snmp`, `modbus`, `gnmi`, `netflow` | **exempt** | rest-var telemetry (`{device}/{metric...}`); the check is vacuous and `assert_families_covered` refuses to run rather than pass them for free |
| `netlink`, `netring`, `systemd`, `logs`, `parallax`, `catalog` | ❌ not yet | finite families, no coverage test — the honest gap. `netlink`'s `sockets/tcp/connlat_us_p50`/`p95` are the known conditional pair awaiting a ledger |

The procedure half covers **every** producer, and is verified by starting each
sensor binary on its stock config: see the sweep in #648.

## See also

- [`keyspace-helpers.md`](keyspace-helpers.md) — how keys are built
- RFC 08 §5/§6.1 in the [zenkey repo](https://github.com/p13marc/zenkey)
