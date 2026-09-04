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
| `apply` | Compile once, publish the difference, exit. |
| `run` | Stay up. Recompiles on catalog change, with a periodic floor — in practice the cadence is the correlator's re-emit (~60 s), and the floor is what remains if the subscription is unavailable. Affordable because an unchanged pass publishes nothing. |
| `override/set` (RPC) | Record a per-host adoption durably (#939). Writes `fleet-policy.overrides.json5`, **never** the policy — see [`docs/policy.md`](docs/policy.md). Gated by `allow_overrides`, off by default. |
| `render <host>` | Print the documents one host would receive, as the sensor will see them. An empty result says **which** kind of empty it is: no catalog at all, a host the catalog does not know, or a policy that selects it for nothing. |

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

**Run exactly one per deployment.** `@desired` is a single-writer service
origin; two compilers with different policies would overwrite each other's
documents on every pass and every sensor would flap between them.

## Related

- [`docs/KEYSPACE.md`](../docs/KEYSPACE.md) §`@desired` — the wire contract
- `zensight_common::desired` — the topic table, the validators, the never-list
- `zensight_sensor_core::desired` — the consumer half, on every sensor
