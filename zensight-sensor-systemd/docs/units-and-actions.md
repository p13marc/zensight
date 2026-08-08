# systemd — watchlist, sentinel, and gated actions

Three related surfaces: the read-only **watchlist** that scopes per-unit
telemetry, the read-only **sentinel** that asserts declarative expectations, and
the write-capable **gated action** surface that (opt-in only) can start/stop
units. The first two are always safe; the third is security-sensitive and
described in detail below.

## Watchlist (#273)

Hundreds of units exist per host, so per-unit series are scoped to a watchlist
to bound key cardinality. `systemd.watch_units` is a list of globs (`*`, `?`,
`[…]` semantics via the `glob` crate); invalid patterns are logged and skipped.
Matched units (up to `watch_max`, default 50) stream `unit/<name>/*` telemetry
(see [telemetry.md](telemetry.md)); matches beyond the cap are dropped (logged,
not silently truncated) and folded into the `other/*` aggregate bucket. Watched
`.timer` and `.socket` units get their own extra keys.

## Sentinel (#277)

The sentinel is an embedded evaluator of declarative service-health expectations
(`systemd.expectations`; omit the block to disable). It re-evaluates every
`eval_interval_secs` (default 10) and on every relevant D-Bus event, and
publishes firing/resolved alerts on
`zensight/v1/<origin>/state/systemd/alert/*`. A firing alert is held for
`for_secs` (default 15) before publish. It mirrors the netlink sentinel and is
**hot-swappable at runtime** via a GET on `@rpc/systemd/expectations/set`
(current config readable with a GET on `@rpc/systemd/expectations`).

Expectation types (`src/sentinel.rs`):

| Field | Rule | Satisfied when |
|-------|------|----------------|
| `services_active: [{ unit }]` | `expect-service-active` | the service's `ActiveState` is `active` |
| `targets_active: [{ target }]` | `expect-target-active` | the target's `ActiveState` is `active` |
| `timers: [{ timer, within_secs }]` | `expect-timer` | the timer last fired within `within_secs` |
| `restart_rates: [{ unit, max, window_secs }]` | `expect-restart-rate` | the unit's restart count over `window_secs` is `< max` |
| `forbid_failed: true` | `forbid-failed` | no unit is in state `failed` |

## Gated service control (#283) — security-sensitive

**Default OFF.** The sensor is strictly read-only unless `systemd.actions.enabled`
is explicitly set. When disabled, **no `@rpc/systemd/action` procedure is
declared at all** — there is no write surface to reach. This section describes
the gating as implemented in `src/action.rs`; treat it as the authoritative
security contract.

### The verbs, and why they are gated separately

| Verb | `Manager` method | Enqueues a job | Extra switch | polkit action |
|------|------------------|----------------|--------------|---------------|
| `start` `stop` `restart` `reload` | `StartUnit`/`StopUnit`/`RestartUnit`/`ReloadUnit` | yes | — | `org.freedesktop.systemd1.manage-units` |
| `enable` `disable` | `EnableUnitFiles`/`DisableUnitFiles` | **no** | `actions.allow_unit_files` | `org.freedesktop.systemd1.manage-unit-files` |
| `daemon-reload` | `Reload` | **no** | `actions.allow_daemon_reload` | `org.freedesktop.systemd1.reload-daemon` |

Three consequences worth being explicit about:

- **Different polkit actions.** A rule granting `manage-units` does *not*
  authorize `enable`; a deployment that enables the verb without the matching
  polkit rule sees the call fail at the D-Bus layer, reported as
  `accepted: true` with an `error`.
- **`enable`/`disable` persist.** They write symlinks under `/etc` (`runtime:
  false`), so they survive a reboot and change what the host does at next boot.
  Granting start/stop must not silently grant that, hence the separate switch.
  They are called with `force: false`, so a conflicting symlink is an error
  rather than a silent replacement.
- **`daemon-reload` is manager-wide.** It names no unit, so `allow_units` cannot
  scope it and its own switch is the only gate. It is the one verb where a
  permissive allowlist grants nothing.

`enable`/`disable` do **not** take effect until systemd re-reads unit files.
ZenSight never chains a `daemon-reload` implicitly — that is a separately gated
verb — so the reply carries `needs_daemon_reload: true` and the operator decides.

### Request/response shape

- Write: a GET on `zensight/v1/<origin>/@rpc/systemd/action/set` carrying JSON
  `{ "verb": "start|stop|restart|reload|enable|disable|daemon-reload", "unit":
  "<name>" }` (`ServiceAction`; `unit` is omitted/empty for `daemon-reload`).
  An accepted request replies the resulting `ActionStatus`; a refused request
  replies `reply_err` with the namespaced `error/gated` name (bad payloads get
  `error/invalid-args`).

  **The write blocks until the outcome is known** — for job verbs, until
  `JobRemoved` arrives or `actions.job_timeout_secs` elapses. A caller's own
  query timeout must therefore exceed `job_timeout_secs`, or every slow restart
  looks like a failure. `@rpc/systemd/action/capability` publishes the number so
  callers can size their deadline from it rather than guessing.
- Read: `zensight/v1/<origin>/@rpc/systemd/action` replies the most recent
  `ActionStatus` — `{ unit, verb, accepted, result, error, changes,
  needs_daemon_reload, ts_unix }` — or `null` when nothing has run. (`null`
  rather than an empty record: an all-empty `ActionStatus` reads exactly like a
  rejection.) `accepted` reflects whether the request cleared every gate and was
  issued; `result` is the `JobRemoved` outcome
  (`done`/`failed`/`timeout`/`canceled`/…) for job verbs, `applied` for the
  jobless ones, and `None` when the bounded wait elapsed — unknown, never
  assumed successful.
- Read: `zensight/v1/<origin>/@rpc/systemd/actions` replies a bounded ring of
  recent outcomes (`actions.history_capacity`, default 64), newest first — the
  operator-facing audit timeline.
- Read: `zensight/v1/<origin>/@rpc/systemd/action/capability` replies
  `ActionCapability` — `{ enabled, allow_units, job_timeout_secs, verbs,
  unit_files, daemon_reload }`. **Served unconditionally**, see below.
- Read: `zensight/v1/<origin>/@rpc/systemd/unit/file?name=<u>` replies a
  `UnitFile` — the unit's fragment and drop-ins. Declared only when
  `actions.expose_unit_files` is set (a *read* surface, independent of
  `enabled`). Secret-looking `Key=Value` assignments are redacted with the same
  denylist the debug bundle uses, the reply is capped at 128 KiB, and both facts
  are flagged in the payload (`redacted`/`truncated`) so a reader never mistakes
  it for the file as it exists on disk. Paths are resolved from D-Bus
  `FragmentPath`/`DropInPaths`, never from the request. A sensor in a container
  with the host's system bus mounted will name fragment paths it cannot open —
  it sees its own filesystem, not the host's — and replies with the path but no
  `fragment`; that is a visibility limit, not an error.

### The capability probe is served even when actions are off

This is the one deliberate softening of "when disabled, nothing is declared",
and the reason is that silence is not a usable answer. A caller that gets no
reply cannot tell apart: actions disabled, host offline, pre-1.4 sensor, and a
sensor busy serving someone else's 30-second job. A UI facing that ambiguity has
to either offer controls that will be refused or hide controls that would work.

So `action/capability` is declared before the `actions.enabled` check and replies
`{"enabled": false, "verbs": [], "allow_units": []}` on a read-only host. It is a
**read** naming no units and carrying no unit paths, so it creates no write
surface; `action/set`, `action` and `actions` remain undeclared when disabled,
exactly as before.

It is served from its **own task with its own queryable**, never as an arm of the
action loop — sharing that loop would leave the probe unanswerable for the
duration of a job, which is precisely the silence it exists to remove.

### The four gates

Every request must clear all of the following before anything happens to a unit.
Gates 1–2 are the pure, unit-tested `gate()` function, evaluated before any D-Bus
call is made.

1. **Master switch (`actions.enabled`).** `run()` returns immediately when false
   (after spawning the capability probe), logging `service control disabled`, and
   never declares the `action/set`, `action`, or `actions` queryables. This is the
   primary gate — with it off there is no procedure to call.
2. **Per-verb switch and allowlist.** `enable`/`disable` additionally require
   `actions.allow_unit_files`; `daemon-reload` requires
   `actions.allow_daemon_reload`. Every unit-scoped verb must then match at least
   one glob in `allow_units` via `zensight_common::action::allows` — the *same*
   function the frontend calls to grey out a button, so the preview cannot
   disagree with the gate. An **empty allowlist rejects every unit-scoped
   request** (and the sensor warns at startup that it will do so). An empty unit
   name is also rejected.
3. **systemd/polkit authorization.** Authorization for the underlying call is
   delegated to systemd/polkit — **not enforced in this code** — using the action
   from the verb table above. The sensor must run as root, or unprivileged with a
   scoped polkit rule. If polkit denies the call, the D-Bus method returns an
   error that surfaces as `accepted: true` with an `error` (the request was
   allowlisted and issued, but the call failed).
4. **Audit log.** Every request — accepted or rejected — is written to the
   `zensight::audit` tracing target: rejections log `decision = "rejected"` with
   the reason; issued actions log `decision = "accepted"` with the verb, unit,
   job path (job verbs), and result. Rejections are recorded in the ring too, so
   the timeline shows refused attempts, not only successful ones.

### Execution semantics

- Job verbs call the corresponding `Manager` method with `mode = "replace"`. They
  use `StartUnit`, **not** `StartTransientUnit` — no transient units are created.
  The `JobRemoved` signal stream is subscribed **before** the method is issued
  (so the completion signal can't be missed), then the specific job path is
  tracked to completion with a bounded wait of `actions.job_timeout_secs`
  (default 30). On timeout, `result` is `None` (unknown, not assumed success).
- Jobless verbs have no job to track: the call returning *is* the outcome, so
  `result` is `applied`. `enable`/`disable` report the symlinks they wrote or
  removed in `changes`.
- **Each accepted action runs on its own task.** The request loop does not await
  the D-Bus call, so a 30-second restart no longer blocks the status and history
  reads, nor serializes two operators acting on two different units of one host.

### Points worth flagging

- **Authorization is not in this crate.** The allowlist is only defence-in-depth;
  the real privilege boundary is systemd/polkit + how the process is run. A
  misconfigured deployment (root, broad `allow_units`) grants broad service
  control. The design intent is: keep `enabled: false`, and when enabling, use a
  narrow `allow_units` and a scoped polkit rule rather than running as root.
- The allowlist uses the same glob semantics as the watchlist, so a pattern like
  `app-*.service` matches a family of units — author it as narrowly as the
  deployment allows. A pattern that fails to compile is skipped rather than
  treated as a literal, so a typo narrows the gate; it never widens it.
- **`enable`/`disable` are the sharpest edge here.** `manage-unit-files` granted
  broadly is materially more dangerous than `manage-units`: a persistent change
  to what starts at boot outlives both the operator's session and the sensor.
  Leave `allow_unit_files` off unless the deployment genuinely needs it.

### Example scoped polkit rule

For an unprivileged sensor permitted to restart one family of units, and nothing
else. Note the `manage-unit-files` branch is commented out — add it only if
`actions.allow_unit_files` is on.

```javascript
// /etc/polkit-1/rules.d/49-zensight-systemd.rules
polkit.addRule(function(action, subject) {
    if (subject.user !== "zensight") { return polkit.Result.NOT_HANDLED; }
    var unit = action.lookup("unit");
    if (action.id === "org.freedesktop.systemd1.manage-units" &&
        unit && unit.match(/^app-[^/]*\.service$/)) {
        return polkit.Result.YES;
    }
    // if (action.id === "org.freedesktop.systemd1.manage-unit-files" && …)
    return polkit.Result.NOT_HANDLED;
});
```

Keep the sensor's `allow_units` and the rule's pattern in agreement: the rule is
the privilege boundary, the allowlist is the thing an operator can read.
