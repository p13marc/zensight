# Multi-process producer evaluation (#423)

Desk research + reasoning about how a *fleet* of ZenSight machines would feed Rerun. No
multi-machine demo was run in this phase (headless single host); the scouting-off requirement
below is a precondition for ever running one.

## 1. Rerun's multi-producer semantics (facts)

From [apps-and-recordings](https://rerun.io/docs/concepts/apps-and-recordings) (2026-07-11,
also pinned in [01-capabilities.md](01-capabilities.md) §4):

- Streams/files sharing **`recording_id` + `application_id`** are "treated as a single logical
  recording" by the viewer — across processes *and machines*.
- **Default is a random `recording_id` per process** — uncoordinated producers land in
  separate recordings.
- The viewer groups recordings and scopes blueprints by `application_id`.
- Per-producer `.rrd` files can be combined offline with `rerun rrd merge`.

## 2. Topology A (default): one adapter, N sensors — the bus is the fan-in

```text
sensors (N hosts) ──zenoh──▶ zensight-rerun (1) ──grpc/.rrd──▶ viewer / file
```

This is what the adapter implements, and it is the recommended shape:

- **Fan-in already exists.** ZenSight's whole architecture funnels the fleet onto the Zenoh
  bus; the adapter is just another consumer (exporter pattern). No sensor grows a Rerun
  dependency (kill-switch friendly — delete the adapter, nothing else changes).
- **One clock authority for timeline stamping.** Domain timestamps come from the sensors, but
  a single process turns them into `set_timestamp_nanos_since_epoch` calls — no risk of two
  producers disagreeing about timeline *names* or units.
- **One `recording_id` decision point.** The adapter's `rerun.recording_id` config is the only
  knob; no fleet-wide coordination protocol needed.
- QoS/backpressure is handled once (the bounded-queue policy in
  [03-sink-design.md](03-sink-design.md)).

### Failure modes (adapter dies)

- **Live mode**: gap in the viewer for the outage window; the bus does not buffer telemetry
  for late consumers (plain subscribers — the adapter deliberately avoids
  AdvancedSubscriber history, matching the exporters). On restart the adapter re-seeds
  entities via the query and resumes; series continue on the same paths. Alerts *state* can be
  stale-at-zero or missed entirely if the transition happened during the outage — the
  firing-window lane resyncs at the next transition only. Recorded as a limitation: the
  adapter is a **visualization**, not a system of record (the frontend + redb store is).
- **Record mode**: the `.rrd` is truncated at the crash point but loads up to the last
  complete message (append-framed format, 01-capabilities §3).

### Clock skew

The adapter trusts sensor timestamps (02-mapping.md §3). A skewed sensor shifts *its own*
series/events on the shared timeline — visible as implausible lead/lag against `log_time`.
No adapter-side correction; fleet NTP is the operating assumption (same as the frontend).

## 3. Topology B (rejected for now): direct SDK per sensor

Each sensor process opens its own `RecordingStream` with a shared `recording_id` and
`connect_grpc` to a central viewer, or `save()` locally with `rerun rrd merge` afterwards.

- Pro: no single adapter; per-producer `.rrd` survives partitions; Rerun's own merging story.
- Con (decisive):
  - every sensor takes the full arrow/tonic dependency (+5 GiB target-dir class of weight,
    01-capabilities appendix) and Rerun's 6-week breaking cadence;
  - violates the evaluation kill-switch (Rerun types confined to one crate);
  - `recording_id` must be coordinated fleet-wide (config management burden);
  - sensors on constrained links would push a *second* uplink protocol (gRPC) beside Zenoh,
    with none of the bus's QoS classes.

Verdict: only worth revisiting if the #430 decision is "adopt" *and* a use case appears that
needs per-host recording independent of bus connectivity (e.g. black-box flight recorder).
`rerun rrd merge` makes the offline half of that plausible.

## 4. Demo-vs-real publisher fidelity note

The demo publishers (`zensight-rerun-demo`) use `zensight_common::PublisherRegistry` —
**plain declared publishers** with the correct QoS classes. Real sensors publish telemetry the
same way, but their *control-plane* channels (registration, evidence, alerts) go through
`zensight-sensor-core`'s **AdvancedPublisher** registry (cache + late-joiner history). Two
consequences for demos:

- a late-started adapter will NOT receive a demo alert published before it subscribed
  (no cache) — start the adapter first;
- entity docs in demos come from a synthetic `HostEntity` put, not a live correlator, so the
  `zensight/_meta/query/entities` seed queryable is absent (the adapter's 3 s seed timeout
  handles that gracefully).

## 5. Scouting-off requirement (any multi-machine demo)

A live `zensight-sensors` deployment joins **any** default-config Zenoh session via multicast
scouting/gossip — a demo or recording session on a lab host will silently ingest the ambient
fleet. Every demo/test path in this crate therefore supports (and the tests force) the
isolated pattern: `scouting/multicast/enabled=false`, `scouting/gossip/enabled=false`,
explicit loopback endpoints (`session.rs`; `--isolate`). A future multi-machine demo must use
explicit `connect` endpoints between the participating hosts with scouting off on all of them.
