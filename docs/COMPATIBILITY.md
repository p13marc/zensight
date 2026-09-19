# Compatibility

What you can build on, what may move under you, how long you get to react, and what "1.0"
would have to mean. ZenSight is pre-1.0 `0.MINOR.PATCH` and **the minor is the breaking
slot** ([RELEASING.md](../RELEASING.md)); this page says which surfaces that applies to.

The rule for every row below: **the wire contract is defended by a machine, the rest is
defended by a CHANGELOG entry.** Where a promise is enforced by a lock file or a test, it is
named. Where it is only a habit, this page says so rather than implying more.

---

## The surfaces

| Surface | What backs it | Promise |
|---|---|---|
| **Keyspace grammar** (`v1`) | keyspace-v2 v1.2, ratified; the version chunk is in every key ([KEYSPACE.md](KEYSPACE.md)) | **Stable within `v1`.** A grammar change is a new version chunk, not a silent reshape. |
| **Registry subjects and procedures** | `registry.lock` — the RFC 08 §3.1 compatibility snapshot. An incompatible edit (changed type/class/kind/shape on an existing path) is **refused by the build** | **Additive only.** A path's meaning never changes under you. |
| **Retired paths** | `deprecated.lock`, append-only. `build.rs` fails if a ledger line loses its `[[deprecated]]` entry, or an entry is missing from the ledger | **A retired path is never re-used.** `introspect` can tell a consumer that a key it remembers is *gone*, not merely absent. |
| **Wire encoding** | CBOR by default; subscribers go through `decode_with_encoding`, which reads the sample's declared `Encoding` and sniffs only as a fallback — RFC 08 §7's precedence in order, which nothing honoured before #1148 (`zensight-common/src/serialization.rs`) | **Tolerant by construction.** A JSON publisher keeps working through a mixed-version rollout. A payload whose encoding cannot be established is refused rather than guessed at. |
| **`alert_key` derivation** | Normative RFC 11 §3.1, computed by `zenkey::alert::alert_key`. Origin is never hashed in; host-scoped labels are excluded before hashing | **Stable.** If it ever moves, the release carries the state sweep in [RELEASING.md](../RELEASING.md#migration-re-keying-the-alert-state-on-upgrade-737). |
| **The `@desired` never-list** | `zensight-common/src/desired.rs` — a lint over every fragment, plus the structural defence that a reconciler only deserializes its own sentinel's config type | **Stable constraint.** No desired document may carry a secret, or anything a sensor needs to *reach the bus*: endpoints, TLS material, the namespace. One bad publish must never lock a fleet out of its own supervision. |
| **Exported Prometheus / OTel series names** | `build_metric_name` + the semconv table. Nothing pins them | **May break with a minor**, with a rename table in the CHANGELOG. It has happened twice (logs in 0.8.0, SNMP's 49 names in 0.11.0). Dashboards and recording rules are yours to update. |
| **Config file shapes** | JSON5, loaded through `SensorConfig::parse_strict` — `serde_ignored` over the whole document, `zenoh` exempt (#1150). No file version, no schema version | **May break with a minor**, but never in silence: an undeclared key is a startup refusal naming its full path, and `--check-config` tells you before the deploy. See the hazard below. |
| **The GUI's and the historian's local store** | `zensight-store`'s `SCHEMA_VERSION`; a mismatch moves the file aside and starts fresh, and never migrates — through `PersistentStore::open_or_move_aside`, which both callers now use (#1061) | **Explicitly not stable.** It is a cache. The fleet history the GUI's copy shadows lives in the historian; the historian's own copy shadows the live bus. A bump costs one restart's history, not durability from then on. |
| **Rust crate APIs** | Every crate is `publish = false`; there is no crates.io publish in the pipeline | **Not a public surface at all.** Depend on the bus, not on the types. |

### The config hazard, and the half of it that is closed

Every config a producer, exporter or service loads goes through
`SensorConfig::parse_strict` (#1150). It collects **every** key no struct
declares, by full dotted path, in one pass:

> **A key nothing declares is a startup refusal**, naming each one, before any
> session is opened. A misspelled `probe.poll_interval_sec` used to parse clean
> and take the Rust default on a production host.

```
unknown config key(s): probe.poll_interval_sec, snmp.devices.0.comunity.
Fix the typo, or set allow_unknown_fields: true to ignore (mixed-version fleets).
```

This is `serde_ignored`, **not** `serde(deny_unknown_fields)`, and the
difference is the point: `deny_unknown_fields` errors at the first struct to see
a stray key, so the message names the field but not where it sits, and it aborts
before a collector could run — which rules out both the list above and the
escape hatch below. The mechanism is `zensight-sensor-logs`' (#547), lifted into
`zensight-sensor-core` so every producer has it rather than one.

Two exemptions, both deliberate:

- **the `zenoh` block is not checked.** It is the one block a newer participant
  must be able to hand to an older one mid-rollout; refusing an unknown
  transport knob would turn a staged upgrade into an outage;
- **`allow_unknown_fields: true`** downgrades the refusal to one `warn!` naming
  the keys, for a mixed-version fleet sharing one file. It is read off the raw
  tree, so a config struct does not have to declare it to honour it.

Two consequences follow, and both are load-bearing:

- **A removed setting must keep a shim for one minor.** Dropping the field
  outright turns an upgrade into a startup failure on every host that still
  carries the key, which is worse than the silence it replaced. The rule is the
  deprecation window below: keep the field, log the deprecation at startup
  naming what to move to, remove it in `0.N+2`.
- **This refuses what is *extra*, not what is *missing*.** A key that was never
  in the shipped config still parses clean and takes the Rust default — which is
  how `sysinfo`'s `temperatures` and `power` stayed dark for months. The
  per-crate "the shipped config physically contains this key" tests walk the raw
  tree for exactly that reason, and they stay.

Run `--check-config` before a deploy. Every sensor, both exporters, the
correlator, the historian and `zensight-desired` accept it: it parses, validates
and exits, opening no session and joining no fleet, so a deploy script can gate
on the exit status.

```bash
zensight-sensor-snmp --config /etc/zensight/snmp.json5 --check-config
# config ok: /etc/zensight/snmp.json5
```

So a config that "still works" after an upgrade is still not evidence that
nothing changed — a *present* key that stopped being honoured is now refused,
but a key you never set is not. Read the CHANGELOG's `### Removed` entries on a
minor bump.

---

## The deprecation window

**One minor.** A surface deprecated in `0.N` keeps working through `0.N+1` and may be
removed in `0.N+2`.

**Retirement is retire-and-sibling, never a silent rename** (RFC 08 §3). A path whose
meaning must change is retired through a `[[deprecated]]` entry — which lands in the
append-only ledger — and a new sibling path is added beside it. The old path stops being
published; it never comes back meaning something else.

**A deprecation is announced at runtime, not only in rustdoc.** This is the rule the
`netlink.expectations.metrics` deprecation set and it generalises: the sensor logs it on
startup and on every hot-swap, naming what to move, *because the operator with a live config
never opens rustdoc*. A deprecation that only exists in API documentation has not been
communicated to the person it affects.

**Every breaking change appears under `### Changed — BREAKING`** in its release's CHANGELOG
section — even when the entry is also written up elsewhere in that release. A CI guard in
the `lint` job enforces both halves of this: the heading has exactly one spelling, and a
release that contains a `!` commit must have the heading. The changelog is a purely human
artifact that CI otherwise never reads, which is precisely why the one machine-checkable
part of it is worth checking.

---

## What "1.0" would have to mean

**There is no 1.0 until the software has been battle-tested by the community.** Not until
fleets outside this project have run it in production, long enough to have found what one
reference fleet cannot.

The other criteria are these, and they are necessary rather than sufficient:

- the four feature milestones landed (history, topology, incidents, thresholds);
- **two consecutive minors with zero breaking entries** — the surfaces above have stopped
  moving on their own;
- the sizing document measured rather than estimated (#944);
- the canonical demo runnable by someone who did not write it (#945).

Every one of those is something this repository can satisfy **by itself**, which is exactly
why none of them is enough. A 1.0 is a promise made *to* other people; it cannot be earned
by talking to yourself. So the list above is not a checklist that ends in a tag — it is the
work that has to be done before the gating criterion can even start being met.

Consequently: **a 1.0 is not scheduled and does not get a milestone.** The readiness work is
milestoned `0.18.0` (#903), deliberately not called 1.0. `0.MINOR` releases continue until
the evidence exists, and then the release notes say what the evidence was.

---

## Upgrading

1. Read the CHANGELOG entry for every minor you are crossing — start with
   `### Changed — BREAKING`, then `### Removed` (for the silent-config hazard above), then
   `### Deprecated` (for what you have one more minor to fix).
2. **Upgrade every publisher before sweeping any state.** A single old-build sensor
   re-publishes old-shaped keys within its next evaluation cycle, so a sweep run mid-rollout
   deletes keys that immediately come back — and then you cannot tell a leftover from a live
   one. `zenctl node list`, or the GUI's fleet view, flags version skew.
3. Expect exported series names to be the thing that breaks your dashboards, not the bus.
4. The GUI's local cache being reset on upgrade is normal and is not data loss; fleet
   history is the historian's.

## Reporting a compatibility break

If something on the "stable" rows above moved without a CHANGELOG entry, that is a bug and
worth an issue — those rows are the ones this project is asking to be held to.

See also: [POSITIONING.md](POSITIONING.md) (what the project is for and who should run it),
[KEYSPACE.md](KEYSPACE.md) (the wire contract itself), [RELEASING.md](../RELEASING.md) (the
release procedure and the alert re-key migration).
