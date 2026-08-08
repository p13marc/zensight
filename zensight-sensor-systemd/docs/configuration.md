# systemd — configuration

JSON5, loaded with `--config`. Top-level blocks: `zenoh`, `systemd`, `logging`,
optional `serialization` and `artifacts`. See `configs/systemd.json5` for a
fully commented example. Validation rejects `poll_interval_secs == 0`.

## Top level

| Key | Default | Meaning |
|-----|---------|---------|
| `zenoh` | — | Zenoh connection (`mode`, `connect`, `listen`). |
| `serialization` | `json` | telemetry encoding (`json` or `cbor`). |
| `systemd` | — | sensor settings (below). |
| `logging.level` | `info` | log level. |
| `artifacts` | all disabled | on-demand artifact limits (`@rpc/systemd/artifact/{request,cancel}`); every kind off by default. |

## `systemd`

| Key | Default | Meaning |
|-----|---------|---------|
| `poll_interval_secs` | `15` | Manager read interval (must be > 0). |
| `source` | hostname | sensor instance id in payloads; falls back to `unknown` (v1 keys are origin-scoped, so it no longer appears in key expressions). |
| `watch_units` | `[]` | glob list of units to stream `unit/<name>/*` for (empty = aggregates only). |
| `watch_max` | `50` | hard cap on watched units; excess folded into `other/*`. |
| `ip_io_accounting` | `false` | include IP/IO byte + `ip_*_bps` series for watched units that enabled accounting. |
| `events_capacity` | `256` | bounded control-plane event ring (`@rpc/systemd/events`). |
| `alerts` | see below | threshold alerts (#276). |
| `expectations` | absent (disabled) | embedded sentinel (#277). |
| `cgroup` | see below | `@rpc/systemd/cgroups` walk caps (#280). |
| `actions` | disabled | gated service control (#283). |
| `collect` | see below | collector toggles. |

## `systemd.collect`

| Flag | Default | Family |
|------|---------|--------|
| `list_units` | true | `units/*` state aggregates (off = manager scalars + boot only) |
| `boot` | true | `boot/*_usec` phase durations |
| `mounts` | **false** | `mounts/{total,mounted,failed}` (#279) |
| `journal` | **false** | `journal/{disk_usage_bytes,disk_available_bytes}` (#279) |

## `systemd.alerts` (#276)

Published on `zensight/v1/<origin>/state/systemd/alert/*`.

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | true | master switch |
| `for_secs` | 15 | debounce before a firing alert publishes |
| `unit_failed` | true | watched unit `ActiveState=failed` (dedups with the logs sensor's `MESSAGE_ID` rule — set false to defer) |
| `system_degraded` | true | `SystemState=degraded` / `NFailedUnits>0` |
| `restart_storm_threshold` | 3 | restarts within the window that trip the storm alert |
| `restart_storm_window_secs` | 300 | restart-storm observation window |
| `unit_mem_ceiling_bytes` | 0 | per-unit memory ceiling; `0` disables the rule |
| `timer_overdue_grace_secs` | 300 | grace past a timer's next elapse before overdue fires |

## `systemd.expectations` (sentinel, #277)

Absent block = sentinel disabled. Fields (see
[units-and-actions.md](units-and-actions.md) for the rule semantics):

| Key | Default | Meaning |
|-----|---------|---------|
| `eval_interval_secs` | 10 | re-evaluation cadence |
| `for_secs` | 15 | debounce before a firing sentinel alert publishes |
| `services_active` | `[]` | `[{ unit }]` — expect service active |
| `targets_active` | `[]` | `[{ target }]` — expect target active |
| `timers` | `[]` | `[{ timer, within_secs }]` — expect timer fired within window |
| `restart_rates` | `[]` | `[{ unit, max, window_secs }]` — restarts < max per window |
| `forbid_failed` | false | alert if any unit is `failed` |

Hot-swappable at runtime via a GET on `@rpc/systemd/expectations/set`; the
current config is readable with a GET on `@rpc/systemd/expectations`.

## `systemd.cgroup` (#280)

Caps for the on-demand `@rpc/systemd/cgroups[?path=<rel>]` walk (never
streamed; unprivileged `/sys/fs/cgroup` v2 walk).

| Key | Default | Meaning |
|-----|---------|---------|
| `root` | `system.slice` | default subtree when the query carries no `?path=` |
| `max_depth` | 6 | recursion depth cap |
| `max_children` | 64 | child directories walked per node |
| `max_pids` | 32 | member PIDs recorded per node |

## `systemd.actions` (#283) — **default OFF**

Gated, write-capable service control. Disabled by default; the sensor is
strictly read-only unless this is explicitly enabled. **Read
[units-and-actions.md](units-and-actions.md) before enabling** — the gating and
authorization model are security-sensitive.

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | false | master switch; when false, no writable `@rpc/systemd/action` procedure is declared |
| `allow_units` | `[]` | unit-name globs a unit-scoped verb may target; **empty = reject all** |
| `job_timeout_secs` | 30 | bounded wait for the `JobRemoved` completion result |
| `allow_unit_files` | **false** | additionally permit `enable`/`disable` |
| `allow_daemon_reload` | **false** | additionally permit `daemon-reload` |
| `history_capacity` | 64 | bounded action ring served on `@rpc/systemd/actions` |
| `expose_unit_files` | **false** | serve unit-file contents on `@rpc/systemd/unit/file` |

The three switches are separate because the verbs they gate need three
*different* polkit actions and have three different blast radii:

| Verbs | Gated by | polkit action | Persists across reboot |
|-------|----------|---------------|------------------------|
| `start` `stop` `restart` `reload` | `enabled` + `allow_units` | `org.freedesktop.systemd1.manage-units` | no |
| `enable` `disable` | + `allow_unit_files` | `org.freedesktop.systemd1.manage-unit-files` | **yes** |
| `daemon-reload` | + `allow_daemon_reload` | `org.freedesktop.systemd1.reload-daemon` | n/a (manager-wide) |

`daemon-reload` takes no unit, so `allow_units` cannot scope it — its own switch
is the only gate, which is why it is separate. Granting start/stop must not
silently grant persistent boot-order changes, which is why `enable`/`disable`
are separate too.

Authorization for the underlying call is delegated to systemd/polkit — run as
root, or add a scoped polkit rule granting the action from the table above for
the allowlisted units. The allowlist is defence-in-depth on top of polkit, not a
substitute for it.

Regardless of these switches, `@rpc/systemd/action/capability` is **always**
served, replying `{"enabled": false, …}` on a read-only host. It is a read-only
probe naming no units, and it exists so a caller can tell "this host refuses"
from "nobody answered".

`expose_unit_files` is a *read* surface and independent of `enabled` — it can be
turned on for a read-only sensor. It is off by default because unit files
routinely carry credentials in `Environment=` lines. When on, the sensor redacts
secret-looking assignments (the same denylist the debug bundle uses) before the
reply leaves the host, caps the reply at 128 KiB, and flags both facts in the
payload. That is a denylist, not a proof: review your unit files before enabling
it on a host whose files you did not write. Paths come from D-Bus
(`FragmentPath`/`DropInPaths`), never from the request, so the procedure cannot
be pointed at an arbitrary file.
