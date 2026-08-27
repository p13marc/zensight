# zensight-conformance

Runs [`zenkey-fleet`](https://github.com/p13marc/zenkey)'s RFC judges against a
**live** ZenSight deployment and turns the report into an exit code (#744).

`cargo test --workspace` proves ZenSight's code agrees with itself. It cannot
say whether what a *running* sensor puts on the wire agrees with the
keyspace-v2 RFCs and with the registry TOMLs that same binary claims to serve:
the served-vs-declared slice diff, `alive ⇒ callable` (RFC 04 §5), schema drift
at field granularity, declared-vs-observed QoS, freshness against declared
`ttl_s`, cardinality budgets. Those are properties of a *deployment*, and only
a deployment can be asked.

This is the same argument [`scripts/demo-verify.sh`](../scripts/demo-verify.sh)
makes about the exporters — *"nothing in CI had ever executed an exporter"* —
one layer up: nothing in CI had ever asked a running ZenSight fleet whether it
obeys its own contract.

```
scripts/conformance-verify.sh          stands the deployment up (isolated port,
                                       no containers, no privileges)
zensight-conformance                   opens an observer session, runs
                                       zenkey_fleet::run_doctor, gates the report
.forgejo/workflows/ci.yml  job `conformance`   runs both on every push
```

## Contents

| | |
|---|---|
| [`src/main.rs`](src/main.rs) | CLI, session, the run, the rendering |
| [`src/gate.rs`](src/gate.rs) | report → RFC 13 judgement → exit code, and the exclusion list |
| [`docs/checks.md`](docs/checks.md) | every check, what it means here, and what a clean run looks like |
| [`../scripts/conformance-verify.sh`](../scripts/conformance-verify.sh) | the deployment harness |

## Running it

```bash
# the whole thing, self-contained (builds what it needs, isolated port 17447)
scripts/conformance-verify.sh
PROFILE=debug scripts/conformance-verify.sh        # what CI runs

# against a deployment you already have
cargo run -p zensight-conformance -- \
    --connect tcp/127.0.0.1:7447 \
    --registry zensight-common/registry \
    --for 30

zensight-conformance --list-checks                 # every check id, and the exclusions
```

Exit codes are [`zenkey_fleet::judgement_exit_code`](https://docs.rs/zenkey-fleet)'s,
unmodified — the RFC 13 v1.24 projection every tool in this family shares:

| exit | judgement | meaning |
|---|---|---|
| `0` | `NotEstablished` | the checks ran and found nothing gated |
| `1` | `Established` | gated findings — *the finding is the judged claim*, so establishing it is the failure |
| `2` | `Unobservable` / `NotAsked` | the checks could not carry a verdict (empty roster, failed run) |

The polarity reads backwards for exactly one second and then stops: the claim
is *"this deployment has findings"*, so "not established" is the good news.
`zenctl why` has the same shape for the same reason.

## The boundary

**`zenkey-fleet` must not enter `zensight-common`, or any crate a sensor
links.** It drags a full tokio, zenoh-ext, arc-swap, base64, ciborium and
serde_json tree — an engine for a bus *explorer*, not for a participant.

That is the invariant, and it is worth stating in exactly those words: the
original phrasing was *"used here only"*, which #745 falsified in the same wave
by rebuilding the fleet view on the same engine. **Two members link it** —
`zensight` for the fleet view (#745/#746) and this crate for the judges (#744) —
and both are consumer-side, which is what the rule permits. A sensor linking it
is what the rule forbids. This crate is additionally `publish = false` and
depended on by nothing.

Two more things a reader trips on otherwise:

* **Import from the crate root.** `zenkey_fleet::run_doctor`, never
  `zenkey_fleet::judge::doctor::run_doctor` — upstream states that the crate
  root *is* the supported surface and a module path is only *a* spelling. The
  one exception is the rendering vocabulary (`CheckId`, `DoctorSeverity`,
  `DoctorFinding`), which upstream deliberately keeps behind
  `zenkey_fleet::report::*`; that path is the sanctioned one, not a reach.
* **License.** `zenkey-fleet`'s *package* license is **Apache-2.0**, not the
  MIT of the zenkey workspace root it lives in, and not ZenSight's MIT. Nothing
  here is published, so the only obligation is attribution wherever a built
  binary ships.

## Namespaces

The observer session is deliberately **un-namespaced** (RFC 09 §5): an explorer
that strips a prefix on ingress cannot see a key that leaked outside it, which
is the whole point of running one. `zenkey_fleet::open_with_config` therefore
*rejects* a zenoh config file that sets a session `namespace`.

ZenSight's own `zenoh.namespace` is empty by default, so its keys sit at the bus
root and this costs nothing today. A deployment that does set a base names it
with `--base` here — for this crate the base is a `Fleet` field, never a session
namespace.

## What the gate does and does not fail on

The floor is `--fail-on warning` (`error` and `warning` count; `info` never
does). Then two subtractions, both deliberate:

**`info` findings are not defects.** Calibrated against a real dev deployment:
`admin-unreachable` (no router in a peer-only mesh), `storage-coverage` (no
storage) and `describe-missing` (not every producer is running) all fire at
`info` in a perfectly healthy isolated run, as do the `cardinality-over-declared`
exemptions for `{var...}` families. A gate that reddens on those is a gate
nobody keeps.

**`field-new` is excluded — zenkey#384.** This is the one exclusion, and it is
an upstream bug, not a ZenSight one:

> `schema_drift`'s declared-field-path walker descends `properties` and does
> **not** descend `oneOf`/`anyOf`. Every ZenSight telemetry payload carries a
> `TelemetryValue`, an adjacently-tagged enum
> (`#[serde(tag = "type", content = "value")]`), which schemars renders as a
> `oneOf` whose branches each declare `type` and `value` as `required`. The
> walker never reaches those branches, concludes the paths were never declared,
> and emits `field-new` at **warning** severity for `<key> · value.type` and
> `<key> · value.value` on every telemetry key it observes.

Confirmed both ways: 141 such warnings on a four-producer deployment, 21 on the
one-sensor CI deployment — and the served schema really does declare both
(`zenctl interface show TelemetryPoint --schema --full` renders the `oneOf`
branches with `"required": ["type", "value"]`). Without the exclusion
`--fail-on warning` is unusable, and a gate nobody can turn on protects
nothing.

**The exclusion lifts when zenkey#384 lands.** Delete the entry from
`gate::DEFAULT_EXCLUDED`, flip
`gate::tests::field_new_is_excluded_until_zenkey_384_lands`, done. To check
whether it has landed without touching code:

```bash
zensight-conformance --connect … --registry … --deny field-new
```

Nothing else is excluded. `--allow <check-id>` adds an exclusion for one run;
`--list-checks` prints the vocabulary.

### Observation bounds are reported, not gated

RFC 09 §5.1 O6: a clean report over a lossy window is not a clean fleet. The
report prints every bound it hit — dropped samples, refused field paths, evicted
key projections — unconditionally, at zero too. But a drop is a false-*negative*
risk (the window may have *missed* a finding), so turning a busy-runner drop
into a red build would make the gate flap on load rather than on conformance.
`--strict-window` promotes a lossy window to `Unobservable` for a caller who
wants completeness to be a hard claim.

One bound is hit every single run and is expected: the #223 per-path field table
refuses thousands of path observations (3688 with one sensor over 12 s; 11128
with four over 15 s). It taints the field-intelligence checks only — all of
which are excluded or below the floor — so it is never a reason for a verdict to
change, `--strict-window` included.

## Known findings

Things a real run reports today that are **not** excluded, and are not fixed
here because they are not #744:

* **The correlator's entities seed queryable replies untimestamped**, which the
  deep freshness check reports as
  `[warn] unstamped-state · fleet: 1 state sample(s) carry no HLC timestamp`.
  `zensight-correlator/src/query.rs::serve_entities` answers
  `v1/@catalog/state/entity/*` storage-shaped (RFC 05 §4, one reply per entity
  on its concrete key) with a bare `query.reply(key, payload)`. Session HLC
  timestamping applies to `put`, not to a queryable reply, so a consumer seeding
  from the catalog cannot LWW-order the seed against a live sample (RFC 04 §4).
  It needs its own issue, and a decision this crate should not make: reply with
  the session HLC at reply time, or carry the entity's own write timestamp.

  Because of it, `scripts/conformance-verify.sh` runs the correlator only under
  `CORRELATOR=1`, which is how you reproduce the finding. It is **not** excluded
  from the gate, so the day the correlator stamps its seed replies the
  correlator joins the CI deployment with no change to the gate.

## Adding to the deployment

`scripts/conformance-verify.sh` takes `SENSORS="sysinfo logs …"` and
`CORRELATOR=1`. The first sensor listed becomes the rendezvous (the same trick
demo-verify.sh plays with the exporter — no router, no container, no
privileges); everything else dials it, multicast off on both sides, on port
`17447` and never `7447`.

Keep the CI deployment bounded: the runner has two build lanes, and every
producer added is a build plus a share of the listen window.
