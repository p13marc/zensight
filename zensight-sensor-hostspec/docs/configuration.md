# Configuration

`configs/hostspec.json5` is the operator reference — heavily commented, one
example per assertion kind (the #821 motifs), **empty by default**: an empty
set is a valid state (every procedure served, `spec` answers "held to
nothing", the gauge publishes 0), which is also what keeps the conformance
CI deployment green on a runner with none of your paths.

```json5
{
  zenoh: { mode: "peer", connect: [] },   // shared block + ZENSIGHT_ZENOH_* env
  serialization: "cbor",
  hostspec: {
    // source: "host-01",                 // default: hostname
    expectations: {
      // eval_interval_secs: 60,
      // default_for_secs: 0,
      // mounts: [...], files: [...], listening: [...],
      // symlinks: [...], absent: [...], content: [...], perms: [...],
    },
  },
  logging: { level: "info" },
}
```

Validation runs at startup **and** on every `expectations/set`: the same
`validate()` in both places, so a set that would be refused over the bus
refuses to start, with one message naming every offender (duplicate names,
relative paths, uncompilable regexes, non-octal modes, vacuous entries).

There is no `enabled` flag: hostspec *is* its sentinel. Delete the
expectations (or push the empty set) to hold the host to nothing.

## `@desired` (#816)

The assertion set is the fleet's first `@desired`-authorable topic: a
controller (or `zenctl`) publishes it under
`v1/@desired/state/<host>/hostspec/expectations`, and the sensor reconciles —
seed GET + periodic re-GET against the deployment storage as the primary
path, live subscription as the accelerator, LWW between the three writers
(file baseline, desired, RPC). `state/hostspec/applied/expectations` says
which won last and carries any rejected desired doc. `desired.enabled: false`
in file config is the kill switch. The full contract: `docs/KEYSPACE.md`.
