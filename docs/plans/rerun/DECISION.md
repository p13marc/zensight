# The Rerun decision (#430)

> **Outcome 3 — an optional debugging backend.** `zensight-rerun` stays in-tree,
> documented, `publish = false`, out of the release train, and off unless
> someone runs the binary. It is supported for **bounded incident capture and
> replay**, and for nothing else.
>
> Decided 2026-08-26, against `rerun 0.34.1`, on the evidence in
> [`01`](01-capabilities.md)–[`10`](10-viewer-assessment.md) of this directory
> and in `zensight-rerun/`. Phases 4 and 6 were not run; §7 says exactly how
> that bounds this.

---

## 1. Why this is being written now rather than after more evidence

The epic opened with a warning to itself:

> **⚠️ This epic is an evaluation, not a commitment to adopt Rerun.** Its
> deliverable is an evidence-backed decision — including, possibly, "do not
> adopt".

and listed, under Risks, *"sunk-cost pressure after building the prototype — the
charter's 'evaluation ≠ commitment' line exists precisely for this issue."*

There is a second failure mode the charter did not name, and it is the one that
actually happened: **an evaluation that never ends**. Phases 1–3 finished on
2026-07-11. Phase 3.5 started on 2026-07-12 and its document still says *"in
progress"*. Eleven issues have been open since, and the decision they exist to
inform has not been made. Every week of that is a week the workspace carries 532
extra crates and a `=0.34.1` pin, without having decided that it wants to.

Meanwhile the thing the epic most wanted to measure measured itself. The pin is
`=0.34.1`, published 2026-07-07. Upstream is **0.36.3**, published 2026-08-24 —
**two minor releases, each with its own migration guide, in the seven weeks
since we pinned.** #416 predicted a ~6-week breaking cadence and called it *"a
first-class input to the #430 decision"*. It was right, and waiting longer only
makes the number larger.

So: decide on what is known, and be explicit about what is not. That is what
§7 is for, and the charter permits it — *"if any prototype issue was skipped via
the kill-switch checkpoints, the report must say what evidence is missing and
how it bounds the decision."*

---

## 2. The ten questions

| # | Question | Answer | Evidence |
|---|---|---|---|
| 1 | Live data in, with acceptable latency and resources? | **Yes for a bounded session; unmeasured at fleet rates.** Live mode runs against a real viewer; the adapter's own cost was never benchmarked. | [10](10-viewer-assessment.md); #426 not run |
| 2 | Multiple sensors into one coherent visualization? | **Yes**, and the architecture question is settled: one adapter, not an SDK per sensor. | [08](08-multi-process.md) |
| 3 | Can it represent our entities, events, metrics, topology, correlations clearly? | **Metrics yes. Events poorly. Topology as a replay supplement only.** | [04](04-live-metrics.md), [05](05-events.md), [09](09-topology.md) |
| 4 | Record and replay effectively? | **Yes — this is the strongest result.** Valid `.rrd`, decoded end to end in CI, replayed cold, and a crash-truncated file is *repairable*. | [07](07-record-replay.md), [10](10-viewer-assessment.md) |
| 5 | Offline / disconnected? | **SDK side yes, by construction. Viewer side: prebuilt binary, mirrorable, GPU-dependent.** Never proven on a network-disabled host. | [01](01-capabilities.md) §1, [10](10-viewer-assessment.md); #427 not run |
| 6 | Modular and optional? | **Yes, and it stayed that way.** `publish = false`, nothing depends on it, all Rerun types in one module, not in the release train. | `zensight-rerun/Cargo.toml`, `README.md` |
| 7 | Technical / licensing / deployment / maintenance risks? | **Licensing clean. Maintenance is the real cost.** See §4. | [01](01-capabilities.md) §1, §5 |
| 8 | Which use cases fit? | Incident replay, timeline scrubbing across heterogeneous data, handing a colleague a recording. | §3 |
| 9 | Which require the ZenSight UI? | Everything operational — alert acknowledgement, configuration, artifacts, protocol-aware diagnostics, dashboards, anything a person acts on. | §3 |
| 10 | Abandon / keep / support? | **Outcome 3.** | §5 |

---

## 3. What worked, what disappointed

### Worked

**Record and replay, unreservedly.** `tests/record_e2e.rs` publishes real CBOR
through real declared publishers, runs the real subscriber → worker → sink
pipeline, and then *decodes the resulting file end to end* with the same pinned
`re_log_encoding` — not a magic-number check. The planned timebox ("size+magic
suffices if the decoder is disproportionate") was not needed. That test runs in
`cargo test --workspace` today, which is more than most evaluation prototypes
can say.

**And the recording survives abuse.** `kill -9` six seconds into a thirty-second
recording produces a file that strict `verify` rejects for a missing footer —
and that `rerun rrd optimize` turns back into a fully valid recording, chunks and
entity paths intact. Optimize is also the repair tool. That closed the one open
question in [07](07-record-replay.md).

**`rrd optimize` also demolished the storage objection**, which was the
evaluation's loudest reject-signal. Live write is ~1.3 KiB per scalar point — a
streaming artifact, one chunk per log call — and compaction takes
`metrics.rrd` from 480 000 B to 36 877 B, **13×**, landing at ~100 B/point,
which is ZenSight's own CBOR wire cost. The "~110 MB/day for a modest host,
untenable" extrapolation becomes ~8–9 MB/day *archived*. The rule that follows
is one line: **always `optimize` before storing or sharing.**

**One adapter, not an SDK per sensor**, and the rejected alternative is written
down with its reasons: every sensor would take arrow+tonic and a 6-week
breaking cadence, `recording_id` would need fleet-wide coordination, and
constrained links would carry a second uplink protocol beside Zenoh with none of
the bus's QoS classes. Worth revisiting only for a black-box-recorder use case
that does not exist.

**The mapping is honest where it could have cheated.** Counters become rates
with the first sample absorbed and counted (`rate_absorbed`), timestamps are the
sensors' domain timestamps rather than arrival time, and the alert `Delete`
tombstone is deliberately ignored because the recording is append-only and
cannot retract.

### Disappointed

**Structured events are second-class, exactly as the epic feared.** There is no
single "structured log record" archetype: `TextLog` carries text+level+colour,
`AnyValues` carries fields and has no text-log rendering, and emitting both on
one path is *producer-side convention* with nothing stopping the two from
drifting. [05](05-events.md) names the comparison that settles it: **OTel's
`LogRecord` — body, severity and attributes in one record — is strictly better
modelled.** ZenSight already exports to OTel.

Worse, **attributes are per-timestamp state, not per-record**: two events on one
path in the same millisecond overwrite each other's columns, and ZenSight
timestamps are milliseconds. The 50-event burst demo exists to measure that
collision, and the measurement was never taken.

**Topology is a replay supplement, not a topology tool.** The Graph mapping is
cheap and the scrub-through-time property is genuinely novel against the Iced
view — but the Iced topology's operator features (lenses, grouping, alert
tinting, drill-down) have no path into Rerun's Graph view. [09](09-topology.md)
pre-committed to the right response: *"if the viewer assessment shows unstable
layouts at scrub time, drop the lane entirely rather than invest further."*

**The default network bind is wrong.** `rerun --serve-web` binds **both** the
web viewer and the gRPC proxy to `0.0.0.0` unless told otherwise. On a monitored
network, carrying hostnames, IPs, MACs, flow matrices and log lines, over a
transport with no authentication and no encryption. `--bind 127.0.0.1` exists
and works. This is not a subtle finding and it belongs at the top of any
document anyone reads before running this near real data — §6 makes it a rule.

**No retraction, ever.** A mis-published event is permanent in the recording.
For a debugging capture that is acceptable; for anything resembling a record it
is not, and it is one reason this decision does not go further than it does.

---

## 4. The costs, measured

| Cost | Number |
|---|---|
| Dependency count | **532 distinct crates**, 42 of them `re_*`/`rerun`, including `arrow 58.3` and `tonic 0.14` |
| Target-dir | **+5 GiB** debug, workspace-wide |
| Cold build | minutes-scale, dominated by arrow/tonic |
| MSRV | Rerun demands **1.92** and ratchets fast; `zensight-rerun` carries the workspace's only `rust-version` pin, `1.98` |
| API churn | a migration guide for **every minor**; twelve in ~15 months; **two more since we pinned** (0.35, 0.36) |
| `.rrd` compatibility | N→N+1 only — not an archival format without re-migration |
| Transport | unauthenticated, unencrypted, `0.0.0.0` by default |
| Licensing | **clean.** MIT/Apache-2.0, and the `analytics` feature — on by default upstream — is not even *compiled* in an sdk-only build |

The maintenance cost is the decisive one, and it is not hypothetical: it is
`=0.34.1` against 0.36.3, seven weeks later, with two migration guides in
between.

---

## 5. The decision, and why not the neighbours

**Outcome 3: an optional debugging backend — in-tree, documented, off by
default, supported for incident capture and replay.**

**Why not outcome 1 (do not adopt).** The unique value is real and demonstrated,
not brochure copy. Scrubbing backwards through an incident across metrics,
alerts, events and topology on one time axis is something ZenSight cannot do and
would be expensive to build. The crate works, is tested in CI, and costs the
product nothing because nothing depends on it. Deleting working code to reduce
an issue count is not a decision, it is tidying.

**Why not outcome 2 (experimental developer tool, no repo commitments).** This
is the closest call, and the honest difference is one of documentation rather
than code. The crate is already better than "experimental": it has an e2e test
in the workspace suite, a `just` recipe, three demo generators, a storage-cost
script and a real README. Outcome 2 would mean pretending that is unsupported
while continuing to run it in CI. Outcome 3 says what is actually true, and adds
the one thing missing — a written support boundary (§6), so nobody has to guess
what "supported" covers.

**Why not outcome 4 (official visualization backend, packaged and released).**
Four independent reasons, any one sufficient:

1. **It was never benchmarked** (#426). Nobody knows what it does at 1 000 or
   10 000 events/s, or what the viewer's RAM does over hours. A backend you
   package is a backend someone leaves running.
2. **The transport is unauthenticated and unencrypted, and binds the world by
   default.** Shipping that in the deployment story means shipping a footgun.
3. **The release cadence would become ours.** Packaging means an upgrade
   obligation; two minors in seven weeks is what that obligation costs.
4. **Events are modelled worse than in a backend we already ship.** OTel's
   `LogRecord` beats `TextLog` + `AnyValues` for the same data, and
   `zensight-exporter-otel` exists.

**Why not outcome 5 (deeper integration / embedding).** Everything above, plus
the epic's own non-goal: Rerun can only ever be an *additional* surface — it has
no path to alert acknowledgement, configuration, asset management or dashboards.
Two UIs are justified only when the second does something the first cannot; that
is true of replay and false of everything else.

---

## 6. Consequences — what this actually changes

**Keep, unchanged:** `zensight-rerun` in the workspace, `publish = false`,
nothing depending on it, all `rerun::` types confined to `rerun_sink.rs`, out of
`release.yml`, `tests/record_e2e.rs` in the workspace suite.

**Write down the support boundary** in `zensight-rerun/README.md`:

- **Supported**: bounded incident capture, offline replay, handing a `.rrd` to a
  colleague, timeline scrubbing across metrics/alerts/events/topology.
- **Not supported**: continuous recording, always-on live monitoring, `.rrd` as
  an archive across Rerun versions, anything an operator acts on.
- **The adapter is a visualization, not a system of record.** The frontend and
  its redb store are. An adapter outage is a gap; alert state can be missed
  entirely and resyncs only at the next transition.

**Three operating rules, in the README and in the `just` recipe's help:**

1. **`--bind 127.0.0.1`, always.** The default exposes the viewer and the gRPC
   proxy to the network. Remote viewing is an SSH tunnel.
2. **`rerun rrd optimize` before storing or sharing.** 13× on our own data, and
   it doubles as the crash-repair tool.
3. **`--memory-limit`, always**, for any session that outlives a demo.

**Do not chase upstream.** The pin stays `=0.34.1` until a concrete need moves
it — an evaluation-only crate has no reason to pay a migration every six weeks.
Renovate's `rerun`/`re_log_encoding`/`re_log_types` updates (dashboard #615) can
be declined on sight; a note in the crate's `Cargo.toml` says so, next to the
pin, so the next person does not re-litigate it.

**Leave the topology lane alone.** It is cheap and it is a replay supplement.
Do not invest in it, and do not let it grow toward the Iced topology view's
feature set — [09](09-topology.md) already rejected that direction.

**What ZenSight's own UI should take from this**, recorded and not designed:
*timeline scrubbing over the redb store*. The single most valuable thing this
evaluation demonstrated is not Rerun; it is that scrubbing backwards through a
correlated incident is worth having. ZenSight already stores the samples. That
is a native feature waiting to be specified, and it is the strongest possible
argument that this evaluation earned its cost even ending at outcome 3.

---

## 7. What was not evidenced, and how that bounds this

Per the charter, stated rather than papered over.

| Not run | What it would have told us | How it bounds this decision |
|---|---|---|
| **#426 performance** | adapter and viewer CPU/RSS at 10 / 1 000 / 10 000 ev/s; sustained-ingest limits; overload behaviour; the sampling defaults `configs/rerun.json5` should ship | **This is the binding gap.** It is why outcome 4 is refused rather than deferred, and why the support boundary says "bounded capture". Anyone who wants outcome 4 must run #426 first — that is the price, and it is a fair one. |
| **#427 offline packaging** | a build in a network-disabled container; a viewer install on a clean box; a deb/rpm; GPU-less behaviour; the port table | Weakly binding. [10](10-viewer-assessment.md) established the mirrorable route — `cargo binstall rerun-cli@0.34.1` fetched a prebuilt binary in ~49 s, no compile — and sdk-only builds are analytics-free by construction. An air-gapped deployment is *plausible* and *unproven*, which is acceptable for a debugging tool and would not be for a shipped backend. |
| **#428 security** | the threat summary, the field-exposure table, the plant-a-secret test, the forbidden-configuration list | Partly answered by accident: the `0.0.0.0` default bind is the headline finding and §6 rule 1 acts on it. What remains unmeasured is *which fields* reach a recording — notably whether log-line telemetry carries secrets into a `.rrd` that gets shared. **Until that is answered, treat every `.rrd` as bulk telemetry exfiltration by design** and share it as you would share a packet capture. |
| **#429 capability matrix** | fourteen rows, verdict + evidence + owner each | Not binding. §3 and §6 give the split the matrix would have produced — Rerun for replay, ZenSight for everything operational, neither for long-term reporting — with less ceremony. |
| **The viewer-visual checklist** ([10](10-viewer-assessment.md), 13 items) | whether the incident *reads* without a blueprint; TextLog burst legibility; graph stability while scrubbing; memory-purge behaviour under live load | Mildly binding, and mostly on the parts already rated weakest: event legibility and topology. If those had come back excellent, outcome 3 would be unchanged; only outcome 4 could have moved, and #426 blocks that independently. |
| **#449 blueprints, #450 live fleet, #451 video spike, #452 company demo** | curated layouts; tuned sampling defaults; H.264 on the same timeline; a rehearsed demo | Not binding on the decision. All four are *adoption* work, and adoption is not what was chosen. #451 in particular is a large spike whose premise — video and telemetry on one scrubbable axis — is attractive and would only be worth building after #426. |

**The honest summary:** this decision rests on capability evidence, which is
strong, and not on performance evidence, which does not exist. That is exactly
why it stops at "optional debugging backend" instead of going further. A future
argument for outcome 4 has one entry price, and it is #426.

---

## 8. Closing the epic

#415 and its eleven open children close with this document. Each closes with a
pointer to the section that consumed it, and #426/#428 close with the section
that records what their absence costs — if either is ever run, it should be
re-filed with what changed, not resurrected.
