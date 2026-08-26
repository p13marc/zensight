# The checks, and what they mean against a ZenSight deployment

Every check here is `zenkey-fleet`'s, not ours — `zensight-conformance` runs
[`zenkey_fleet::run_doctor`], the exact entry point `zenctl doctor` and the
zengui doctor panel call, so a finding here is a finding there. This page is
the ZenSight-side reading: what each one is actually testing about *our*
sensors, and what a healthy run looks like.

Check ids are stable upstream API — new checks append, nothing is renamed — so
the `--allow` / `--deny` tokens below are safe to write into a script.

## The set

`run_doctor` proceeds in four phases. `--shallow` skips phase 3; `--for 0`
skips phase 4 (and the report then carries **no** observation section at all,
rather than an empty one — "not asked" is not "nothing found", RFC 09 §5.1 O4).

### 1. Served vs. declared (always)

| check | severity | what it means here |
|---|---|---|
| `slice-parse` | error | a producer served a registry slice that does not parse. Ours are compiled in by `zenkey-build` from `zensight-common/registry/*.toml`, so this fires only on a corrupt or hand-edited serve path. |
| `slice-sync` | error | **the load-bearing one.** The slice a running binary serves on `@rpc/<producer>/introspect` differs from the TOML in this repo. That is a sensor built against a registry that has since moved — the exact drift the registry exists to prevent. |
| `introspect-coverage` | error | a producer holds an `alive` liveliness token but did not answer `introspect`. RFC 04 §5 says producers declare their `@rpc` queryables *before* their token, so `alive ⇒ callable` and this is a bug, never a boot race. Counted only over producers the local registry names (O4). |

The `@catalog` service origin takes a structurally different introspect key
from a producer slice — a verbatim `@` chunk is unmatchable by a fleet
selector's `*` — so running the correlator exercises a second code path here.

### 2. Mesh (always)

| check | severity | what it means here |
|---|---|---|
| `admin-unreachable` | **info** | no router answered `@/*/router`. Expected in every isolated run and in any peer-only deployment; it means the storage and version checks were skipped, not that anything is wrong. |
| `router-version-skew` | warning | routers on the mesh disagree about their zenoh version. |
| `describe-totality` | warning | a producer's RFC 08 §7 `describe` does not cover every type it declares. |
| `describe-missing` | **info** | a producer serves no `describe` at all — a SHOULD, not a MUST. Fires for every registry slice with no live producer, so it counts *up* as the CI deployment stays small. |

### 3. Deep — freshness and coverage (`--deep`, on by default)

Per state family with a declared `ttl_s`, a bounded state snapshot
(`--sample`, default 64).

| check | severity | what it means here |
|---|---|---|
| `stale-state` | error | a state key's newest sample is older than its declared `ttl_s`. A sensor that stopped publishing a family it still declares. |
| `unstamped-state` | warning | a state sample carries no HLC timestamp, so LWW cannot order it and freshness is unjudgeable. See the correlator entry under "Known findings" in the README — this is currently a **real** ZenSight finding, and deliberately not excluded. |
| `storage-coverage` | **info** | declared state families with no storage behind them. Every one of ours, in every run without a storage plugin: volatile seeding rides the advanced pub/sub cache and the seed queryables instead. |

### 4. The listen window (`--for N`, default 12 s in CI)

Passive observation of the data planes after the GET fan-in: `v1/*/{telemetry,state,events}/**`
plus the `@catalog` equivalents. Everything here is judged on traffic that
actually rode, so its worth is bounded by the window — which is why the report
prints the window, the scopes, and every bound it hit.

| check | severity | what it means here |
|---|---|---|
| `payload-undecodable` | error | a payload did not decode against the type its subject declares. CBOR/JSON mismatch, or a serializer change that outran the registry. |
| `payload-invalid` | error | it decoded and then failed JSON Schema validation against the served schema (#741's conformance verdicts, on the wire). |
| `qos-observed-mismatch` | warning | the QoS axes on the wire differ from the ones the registry declares for that subject — a publisher declared outside `QosClass`, or a class whose mapping drifted. |
| `unregistered-traffic` | warning | a key rode that no registry subject builds. RFC 08 §5's lint, applied to real traffic instead of to source. |
| `rate-over-declared` | warning | a family published faster than its declared rate. |
| `timestamp-stamped-elsewhere` | warning | a sample's HLC came from a different zid than the publisher's. |
| `cardinality-over-declared` | warning / **info** | a family expanded past its declared `cardinality`. Fires at **info**, with an explicit "exempt: rest-variable" note, for `{var...}` families (`snmp/{device}/{metric...}` and friends) — unbounded by construction, so the declared number is not a bound this check can pass or fail. |
| `field-vanished` | warning | a dotted path present early in the window stopped appearing. |
| `field-stuck` | warning | a numeric path never changed across the window. |
| `field-new` | warning | a path appears in samples that the served schema never declared. **Excluded by default — upstream zenkey#384.** See the README. |

## What a clean run looks like

One sysinfo sensor, 12 s window, in-tree registry — the CI deployment:

```
producers: 1 live, 1 answered introspect, 1 serve describe (10 do not)
slices in sync: 1
    h-0ead7da13eea/sysinfo (registry 1.3)
routers: 0   deep checks: ran
listen window: 12.0s, 618 sample(s) over 307 key(s); scopes: v1/*/telemetry/**, …
    dropped: 0 sample(s), 3688 field path(s) refused, 0 key projection(s) evicted, 0 synthetic sample(s)

-- below the severity floor (7) --
  [info] admin-unreachable · mesh: no routers answered @/*/router …
  [info] describe-missing · fleet: 10 producer(s) serve no describe …
  [info] storage-coverage · fleet: 69 state famil(y|ies) have no storage coverage …
  [info] cardinality-over-declared · snmp/{device}/{metric...}: exempt: rest-variable …

-- EXCLUDED FROM THE GATE (21) — field-new --
   field-new: 21 finding(s) suppressed

PASS — no gated findings. 1 producer(s) judged, 28 finding(s) reported, none of them gated
```

Twenty-eight findings and a pass is not a contradiction: twenty-one are the
upstream `oneOf` blind spot, seven are `info`. The two numbers that matter are
`slices in sync: 1` (the diff *ran*, and agreed) and `dropped: 0`.

## When a check fires

1. `--json` gives you the whole `DoctorReport` plus the gate's verdict —
   `.report.findings[]` carries `check`, `subject`, `evidence` and the RFC
   citation per finding.
2. `zenctl` is the same engine with better tools for the follow-up:
   `zenctl doctor --registry zensight-common/registry --deep --for 30`,
   `zenctl why <key>` for a silent one, `zenctl field <selector> --for 30` for
   the field checks, `zenctl interface show <Type> --schema --full` for a
   schema disagreement.
3. If the finding is upstream's rather than ours, say so in the exclusion —
   with the issue number and the condition that lifts it. `gate::DEFAULT_EXCLUDED`
   is a liability list, not a convenience list.

[`zenkey_fleet::run_doctor`]: https://docs.rs/zenkey-fleet
