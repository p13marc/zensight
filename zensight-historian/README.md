# zensight-historian

The fleet's telemetry history, as a service.

## Why it exists

Telemetry was the only wire class with no history path for a second process.
Logs have `@rpc/logs/events` over a durable redb store; events have a router
`fs` storage plus a startup GET; state has seed storages. Telemetry had two
things, and neither is a history: the AdvancedPublisher's ten-sample-per-key
cache, and a redb file inside the Iced binary that only the GUI which wrote it
could read. On the reference fleet that GUI is open for minutes a week, so what
it held was mostly gaps.

This is an ordinary Zenoh application that closes that. It subscribes
`v1/*/telemetry/**` with history and recovery, writes the same tiers the GUI's
cache does — the shared [`zensight-store`](../zensight-store/README.md) crate —
and serves typed, bounded, cursor-paginated reads over them. The pattern is the
logs sensor's, applied to the class that never had it.

## What it is not

**Not a router plugin, and not a second database process.** RFC 04 §4's
InfluxDB `timeseries` storage was the obvious alternative and does not fit:
`zenoh-backend-influxdb` v2 cannot answer `*`/`**` selectors, a `_time=` GET has
no aggregation or downsampling so a day of per-second samples travels raw, and
an out-of-tree plugin cannot run in CI — where `demo-smoke` and `conformance`
execute workspace binaries. Prometheus remote-write already ships for
deployments that want a real TSDB.

**Not a service origin.** It is a host-origin *producer*
(`v1/h-…/@rpc/historian/range`). Service origins exist for single-writer fleet
state — `@catalog`, `@desired` — and this writes none; it only answers RPC. Two
historians, one per site, are then ordinary RFC 05 §2.1 fan-in with no claim
protocol to get wrong. **Callers must target `All`**; `BestMatching` short-
circuits to whichever answered first and silently drops the rest of the fleet's
history, which is why `zensight_common::keyexpr::historian_range_selector`
exists rather than a `format!` at each call site.

**Not a publisher of telemetry.** RFC 04 §1.1: a history service that re-emitted
what it ingested would be a loop with a database in it. Its own numbers ride the
health document and `@rpc/historian/stats`, and
`tests/registry_conformance.rs` fails if a telemetry subject ever appears in its
slice.

**Not a write surface.** Retention is configuration, not a wire call. There is
no compaction trigger and no delete, and the same test fails on a `write`
procedure.

**Not a query language.** Structured, typed, bounded range queries only.

## Series identity

`(origin, producer, subject)` — the wire key minus the class chunk. It is taken
from the **key**, not the payload, and it has to be: for a proxy producer the
subject is `{device}/{metric...}` while `TelemetryPoint::metric` is only the
second half, so a service that rebuilt the name from the payload would file
every polled device's counters under one another's.

Being derivable from a sample alone is what makes it survive a catalog merge and
a correlator outage, and it is the same name the GUI's local cache uses — which
is what will let a chart read the fleet's history when a historian is alive and
fall back to the local cache when none is (#909). Entity resolution is a
query-time join, not a storage key.

## Procedures

| Procedure | State | Reply |
|---|---|---|
| `introspect`, `describe` | served (framework) | `RegistrySlice`, `SchemaSet` |
| `stats` | **served** | `HistorianStats` |
| `range` | `error/unsupported` until #907 | `RangeReply` |
| `series` | `error/unsupported` until #907 | `Vec<SeriesInfo>` |
| `timeline` | `error/unsupported` until #908 | `TimelineReply` |

A declared-but-unbuilt procedure answers `error/unsupported` rather than going
undeclared. RFC 08 §6.1 requires a build to serve what it advertises, and the
reason is the caller's: an undeclared key **times out**, and a timeout is
indistinguishable from a slow fleet, a dropped reply, or a wrong key.
`error/unsupported` is the third answer — *declared, not built* — and it arrives
immediately.

## Running it

```bash
just historian                     # alongside the local stack
cargo run -p zensight-historian -- --config configs/historian.json5
```

Its store lands in `$STATE_DIRECTORY` under systemd, else
`$XDG_STATE_HOME/zensight`, else `~/.local/state/zensight/history.redb`. If it
cannot open a file it runs **memory-only** and says so loudly at startup: a
historian that answers live questions from the hot ring is more useful than one
that refuses to start, but an operator who wanted durable history and got a ring
must be able to find that out without reading the source.

Ask it what it holds:

```bash
zenctl get -c tcp/127.0.0.1:7447 'v1/*/@rpc/historian/stats'
```

## Resource budget

It takes all three governor steps (#811/#812), because it is the component that
holds a database on a 1–2 GB VM — and the reference fleet is where the sensor
bundle OOM-killed one on 2026-08-17.

- `resources.budget_rss_mb` declares the budget. Absent means **undeclared**,
  never "fine": with no budget there is no ladder and no `sensor-budget` alert.
- The **hot ring** is the evictable table. Under pressure the governor halves
  its capacity, which is the only thing a per-series ring can actually give back
  — "ten minutes became five" is a sentence an operator can act on, where
  "freed 3.7 MiB" is not.
- **Ingest** is the degradable work, and it sheds **booleans first**: a 0/1 step
  series is the cheapest history to lose and the easiest to re-derive, because
  the alert that made it interesting is on the bus anyway.

Everything shed or dropped is counted, and every counter is reported even at
zero — "nothing was dropped" and "nobody asked" are different states, and a
metric that only appears once it is nonzero cannot tell them apart. The reasons
are counted separately because they are different faults: a key outside the
telemetry class means a selector reaching too far, an undecodable payload means
a producer on a format nobody expected, and a text value is normal.

## Documents

- [`docs/configuration.md`](docs/configuration.md) — every knob and what it costs.
- [`docs/storage.md`](docs/storage.md) — tiers, retention, and the measured
  numbers (#911).

## Related

- [`zensight-store`](../zensight-store/README.md) — the tiers themselves.
- [`docs/KEYSPACE.md`](../docs/KEYSPACE.md) — the deployed keyspace contract.
- Epic #898 — history is a fleet service, not a file in one GUI.
