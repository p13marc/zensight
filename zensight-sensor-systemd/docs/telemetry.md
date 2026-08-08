# systemd — telemetry reference

Telemetry is published as `zensight/v1/<origin>/telemetry/systemd/<metric>`,
where `<origin>` is the host's stable id (`h-<12hex>`; systemd is a host-local
producer). The sensor refreshes every `poll_interval_secs` (default 15) by
reading `org.freedesktop.systemd1.Manager` on the system D-Bus, read-only and
unprivileged.

## Streamed metrics

### Manager scalars (always)

`manager/{n_names,n_failed_units,n_jobs,n_installed_jobs}` (Gauge).

### Unit-state aggregates (`collect.list_units`, default on)

From `ListUnits`: `units/{total,active,failed,loaded,inactive}` (Gauge). Turn
`list_units` off to collect only the cheap Manager scalars + boot timings.

### Boot performance (`collect.boot`, default on)

`boot/{firmware,loader,kernel,initrd,userspace,total}_usec` (Gauge, µs),
computed from the Manager monotonic timestamps exactly like `systemd-analyze`.

### Per-unit watchlist (#273)

Units matching a `watch_units` glob (capped by `watch_max`) stream
`unit/<name>/{active,state,restarts_total,active_since_usec,mem_bytes,cpu_usec,
tasks,exit_code}`. The `unit` label carries the raw unit name; overflow beyond
`watch_max` is folded into `other/units_total` (logged, not silently truncated).

- **Timers** (#279): watched `.timer` units add `unit/<t>/{last_trigger_usec,
  next_trigger_usec}`.
- **Sockets** (#279): watched `.socket` units add `unit/<s>/{n_accepted,
  n_connections,n_refused}`.

### Per-service bandwidth (#315, epic #320)

With `ip_io_accounting` and a unit that itself enabled `IPAccounting=`,
successive `IPIngressBytes`/`IPEgressBytes` deltas give
`unit/<name>/{ip_ingress_bps,ip_egress_bps}` (bytes/sec) plus the underlying
`ip/io_*_bytes` counters. These are **wire-L3** (cgroup_skb: L3+ bytes including
retransmits, no L2 framing) and carry `bw.source=systemd` / `bw.semantics=wire-l3`
labels so the GUI never blends them with app-goodput (sock_diag/eBPF) or wire-L2
(capture) numbers. A unit restart resets the counters → that tick is re-baselined
(no negative spike). An *active* unit with IPAccounting off emits
`unit/<name>/ip_accounting=false` (an explicit "off" state, not a silent zero).

### Mounts (`collect.mounts`, opt-in, #279)

`mounts/{total,mounted,failed}` from `ListUnits`.

### Journal health (`collect.journal`, opt-in, #279)

`journal/{disk_usage_bytes,disk_available_bytes}` — an unprivileged file-size
walk + `statvfs`.

### Event counters (#275)

`events/<kind>_total` for `unit_new`, `unit_removed`, `job_new`, `job_removed`.

## D-Bus event stream (#275)

`Manager.Subscribe()` feeds watched `UnitNew`/`UnitRemoved` + `JobNew`/
`JobRemoved` into a bounded timeline ring (`events_capacity`, default 256). Job
completions carry the `ActiveState` from→to transition. Each event also nudges
the sentinel for instant re-evaluation.

## On-demand reads (`@rpc/systemd/<topic>`)

Never streamed — served on request as GETs on
`zensight/v1/<origin>/@rpc/systemd/<topic>` (fleet callers select
`zensight/v1/*/@rpc/systemd/<topic>` with query target `All`).

| Topic | Reply | Notes |
|-------|-------|-------|
| `units` | `Vec<UnitRecord>` | the host's unit **inventory** (see below) |
| `failed` | `Vec<UnitRecord>` | failed units only — loaded units, never the unloaded half |
| `unit?name=<u>` | `UnitDetail` | props + deps + identity fields (below) |
| `timers` | `Vec<TimerRecord>` | with an `overdue` flag (#279) |
| `events` | recent control-plane timeline | the bounded event ring |
| `cgroups[?path=<rel>]` | `CgroupNode` tree | `systemd-cgls`-style slice→service→scope walk |

**`units` is an inventory, not a snapshot (since 1.4).** `ListUnits` reports only
what the manager currently holds in memory, so a service that is disabled and has
not run this boot (`sshd.service` on a desktop) is absent from it — which makes it
unfindable in the frontend's Units table, the one place an operator would go to
start it. The reply therefore merges `ListUnitFiles` in: installed units the
manager has not loaded appear with `load_state = "not-loaded"` (a ZenSight value —
systemd has no `LoadState` for "not in memory"), `active_state = "inactive"`,
`sub_state = "dead"`, an empty `description` (reading it would mean loading every
unit), and their real `unit_file_state`. Templates (`getty@.service`) are listed
because they are worth finding, but the frontend offers no actions on them — only
an instance can be started. `failed` is unaffected: a unit that was never loaded
cannot have failed.

**`unit?name=` identity fields (#303, detail-only):** `main_pid` +
`main_pid_start_time` (the `(pid, start_time)` pair joining to a sysinfo process
/ netlink socket owner), `invocation_id` (hex — the same id journald stamps as
`_SYSTEMD_INVOCATION_ID`, joining a unit to its log lines), and `control_group`
(joins to a process `cgroup`).

**`cgroups` walk:** an unprivileged `/sys/fs/cgroup` v2 walk with per-node
mem/cpu/tasks/io + pid/comm (#280), depth/breadth/pid-capped by `systemd.cgroup`.

## Alerts & sentinel

Both publish on the standard `zensight/v1/<origin>/state/systemd/alert/<alert_key>`
channel (`<alert_key>` = 16-hex FNV-1a of rule + labels; firing = Put, resolved
= Put(Resolved) then a Delete tombstone):

- **Threshold alerts** (#276, `systemd.alerts.*`): `systemd-unit-failed`,
  `systemd-system-degraded`, `systemd-restart-storm`, `systemd-timer-overdue`,
  `systemd-unit-mem`.
- **Sentinel** (#277, `systemd.expectations`): declarative service-health
  expectations, hot-swappable via a GET on `@rpc/systemd/expectations/set`.

See [units-and-actions.md](units-and-actions.md) for both.

## Control-plane keys

```
zensight/v1/<origin>/state/systemd/health              # sensor health document (absorbs the legacy running flag)
zensight/v1/<origin>/state/systemd/alive               # sensor liveliness token
zensight/v1/<origin>/state/systemd/errors              # error reports
zensight/v1/<origin>/state/systemd/alert/<alert_key>   # threshold + sentinel alerts
zensight/v1/<origin>/@rpc/systemd/<topic>              # on-demand reads (above)
zensight/v1/<origin>/@rpc/systemd/expectations/set     # hot-swap sentinel expectations (GET + payload)
zensight/v1/<origin>/@rpc/systemd/expectations         # current expectations config
zensight/v1/<origin>/@rpc/systemd/action/set           # gated service control (only if enabled)
zensight/v1/<origin>/@rpc/systemd/action               # last action outcome
zensight/v1/<origin>/@rpc/systemd/introspect           # the registry slice this build serves
zensight/v1/<origin>/state/systemd/sensor              # SensorInfo registration
zensight/v1/<origin>/state/systemd/evidence/self       # self-identity claim
```

The telemetry class selector `zensight/v1/*/telemetry/systemd/**` matches only
telemetry — `state` and the `@rpc` plane are disjoint by construction (and the
legacy `zensight/**` wildcard matches nothing v1). See
[../../docs/KEYSPACE.md](../../docs/KEYSPACE.md) for the full contract.

## Exporters (#282)

Per-unit series export with a clean name + `unit` label (e.g.
`zensight_systemd_unit_active{unit="sshd.service"}`) via the shared
`zensight-common::semconv` table; aggregates + alerts flow through unchanged.
