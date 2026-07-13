# Orphan `@rpc` procedures — the decision (issue #469)

The #453 audit found eight registered, served `@rpc` procedures with **no
consumer anywhere in the tree**. Epic #477 wired three of them and recorded a
decision for the rest. This file is that record.

Retiring a registry entry is an **append-only ledger operation** (RFC 08 §3), not
a delete — so "not wired" is a status, not a removal. None of the five below are
retired.

## Wired (epic #477)

| Procedure | Where it landed | Why it was worth it |
|---|---|---|
| `@rpc/netflow/flows` | NetFlow view, Recent Flows table | The bounded ring keyspace-v2 built to *replace* per-flow-pair keys. Its absence was worse than cosmetic: the view faked flows from telemetry labels the sensor does not emit, so every row read `0.0.0.0:0 → 0.0.0.0:0`. |
| `@rpc/sysinfo/latency` | sysinfo view, "Saturation latency (eBPF)" | runqlat + biolatency percentiles. PSI says how much time was lost to contention; this says how long a single wait was — and the tail is the finding. |
| `@rpc/netring/encrypted_dns` | netring DNS tab, "Encrypted DNS destinations" | The DoT/DoQ/DoH destinations behind the streamed counts. An unrecognised resolver is what a DNS tunnel looks like from the wire. |

## Kept, not wired

All five are **read-side companions of a `/set` the GUI already round-trips**, or
feature-gated. The GUI knows what it last wrote; the read-back only matters when
something *else* could have changed the value, which today nothing can. They stay
registered and served — the cost is a queryable, and the day a second writer or a
config-drift check exists, the read side is already there.

| Procedure | Status |
|---|---|
| `@rpc/netring/ipfix` | Feature-gated (`ipfix`), no consumer demand. Off in the default build. |
| `@rpc/netring/capture_disk` (read) | Companion of `capture_disk/set`, which the netring Capture tab already round-trips. |
| `@rpc/netlink/collection` (read) | Companion of `collection/set`. |
| `@rpc/systemd/failed` | Subsumed by the units table, which already carries failure state; a dedicated fetch would show the same rows twice. |
| `@rpc/logs/filter` (read) | Companion of `filter/set`, round-tripped by the Logs view. |

## The rule this leaves behind

A procedure whose **reply type only the producer can name is a reply nobody can
read.** Two of the three wired above were unconsumable for exactly that reason —
`LatencyReport` lived in `zensight-sensor-sysinfo`, and netflow's `FlowRecord`
lived in `zensight-sensor-netflow`. Both moved to `zensight-common` as part of
#469 (the sensors re-export them, so no call site changed).

**When adding a procedure, put its reply type in `zensight-common`.** A
`reply = "…"` in a registry file that names a type no consumer crate can import is
how a procedure ships with no caller and stays that way.
