# zensight-desired

The fleet policy compiler: one `fleet-policy.json5` in, the per-host
`@desired` documents every ZenSight sensor reconciles out.

## Why it exists

`@desired` shipped in 0.12.0 as *"fleet configuration as desired state instead
of eighteen hand-edited JSON5 files across six machines"*. The consumer shipped
with it, the router storage shipped with it, the never-list shipped with it —
and **nothing in this repository published a single desired document**. The
author of the fleet's desired state was a private script somewhere else.

This is the author.

## What it is not

- **Not a configuration-management system.** It runs no command, copies no
  file, installs no package and reaches no host. It publishes documents that
  hosts reconcile *themselves* — the difference RFC 12 draws between
  convergence and durable pub/sub imperatives, and the reason a host that was
  offline during a change picks it up when it returns instead of missing it.
- **Not a second identity service.** It has no opinion about what a host is.
  It asks `@catalog`, which is the only component that ran the union-find.
- **Not a template engine.** Classes compose by overlay, not interpolation.
  There is no expression language, and adding one is how a policy file stops
  being reviewable.
- **Not able to carry a secret.** Every document is checked against the
  `@desired` never-list before publication, and the payload types have no
  field for credentials: SNMP communities and probe headers stay in each
  host's own config file and are referenced by name.

## The property that makes it safe to run unattended

**A pass with unchanged inputs publishes nothing.**

Compilation is pure, output is canonical JSON (keys sorted, so two policies
that mean the same thing produce the same bytes), and publication is gated on
a content diff — seeded on startup from the `@desired` storage itself, so a
**restart** is a no-op too. Without that, every refresh would rewrite every
document on every host, the `applied/<topic>` markers would show a fleet
permanently reconverging, and an operator would have no way to tell a real
change from a bounce.

`an_unchanged_pass_publishes_nothing` asserts it against a real bus: three
passes, one sample.

## Subcommands

| | |
|---|---|
| `plan` | Validate and show what would change. Publishes nothing. **`plan --offline` opens no session at all** — a policy nobody can check before pushing is a policy checked by the fleet. Exits 1 on an invalid policy, so CI can gate a policy change the way it gates code. |
| `apply` | Compile once, publish the difference, exit. **Exits 1 when `@catalog` reports no hosts** (#1039) — `fetch` returns an empty list both when the catalog says "no hosts" and when nobody answered, and this command is read by deploy scripts. Publishing nothing under exit 0 is the one outcome nobody can act on. It asks for up to 10 s first (#1045), so the refusal means the fleet is empty and not that the session was young. |
| `run` | Stay up. Recompiles on catalog change, with a periodic floor — in practice the cadence is the correlator's re-emit (~60 s), and the floor is what remains if the subscription is unavailable. Affordable because an unchanged pass publishes nothing. |
| `override/set` (RPC) | Record a per-host adoption durably (#939). Writes `fleet-policy.overrides.json5`, **never** the policy — see [`docs/policy.md`](docs/policy.md). Gated by `allow_overrides`, off by default. |
| `render <host>` | Print the documents one host would receive, as the sensor will see them. An empty result says **which** kind of empty it is: no catalog at all, a host the catalog does not know, or a policy that selects it for nothing. |

Every command that opens a session waits (up to 5 s) for it to have a
neighbour before asking the catalog anything. `zenoh::open` returns before the
link to a `connect` endpoint is up, so a GET issued straight away reaches
nobody and answers with zero replies — which this crate would read as a fleet
of zero (#1039). The wait is never fatal: a controller started before its hub
still comes up and converges on the next refresh.

**A link is not a route to a queryable** (#1045), which is the other half of the
same problem. Zenoh declares queryables to a new session *after* the link comes
up, so `await_peer` can return true while `@catalog` is still invisible to that
session — and a single GET into that window is indistinguishable from an empty
fleet. So `apply` and `render` keep asking for up to 10 s before believing an
empty answer, and `apply`'s refusal says how long it waited. The ordinary path
costs nothing: a settled session answers on the first attempt.

Two commands deliberately do **not** settle. `plan`'s contract is *what can you
see right now* — callers loop it to watch a fleet appear, and a ten-second wait
per call would change what it means. `run` re-fetches every `refresh_secs`, so
a first pass inside the window is corrected by the next tick, and a key is
tombstoned only after `delete_grace_periods` consecutive passes without it —
one empty pass cannot wipe a fleet's desired state.

```bash
zensight-desired --config /etc/zensight/desired.json5 plan --offline
zensight-desired --config /etc/zensight/desired.json5 plan
zensight-desired --config /etc/zensight/desired.json5 render h-3fa9c2d41b7e
```

## The policy

See [`docs/policy.md`](docs/policy.md) for the file format, the overlay rules
and why each one is what it is. In short: **classes** select hosts by facts the
catalog already knows, each class contributes document fragments, and a host's
effective document is the ordered overlay of every matching class followed by
its own override.

## Deployment

| | |
|---|---|
| systemd | `packaging/systemd/zensight-desired.service` — `DynamicUser`, no capabilities, no state directory |
| Release | in the tarball and as `zensight-desired:<tag>` |
| CI | `cargo test --workspace` covers the compiler, the merge rules and the two bus properties |

**One per deployment, and the bus enforces it** (#1104). `@desired` is a
single-writer service origin: two compilers overwrite each other's documents on
every pass — each seeds its `published` diff map from the storage, so each reads
the other's write as a change and rewrites it — and every sensor flaps between
them with nothing on the bus to say so.

That used to be a sentence in this README and nothing else. `run` now takes the
same RFC 06 §5.3 claim protocol the catalog uses
(`zensight_common::service_guard`): it claims `@desired/state/claim/<zid>`,
defers to any live incumbent, and if it loses **stands by** — waiting for the
owner to go away and taking over within a poll interval, rather than exiting and
leaving the fleet with no compiler. `apply`, which is also a writer, refuses
outright while a `run` instance is alive and names it.

## Related

- [`docs/KEYSPACE.md`](../docs/KEYSPACE.md) §`@desired` — the wire contract
- `zensight_common::desired` — the topic table, the validators, the never-list
- `zensight_sensor_core::desired` — the consumer half, on every sensor
