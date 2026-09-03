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
(see [telemetry.md](telemetry.md)). Under the cap, exact-named entries always
survive (#865): an operator who spelled out `sshd.service` gets `sshd.service`,
whatever the cap; wildcard matches fill the remaining room sorted by unit name
(deterministic — never D-Bus enumeration order, which leads with sockets and
once cost every named service its slot). Matches beyond the cap are dropped —
logged by name (exact drops at warn, wildcard drops as a count plus a sample,
full list at debug) — and folded into the `other/*` aggregate bucket. Watched
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

### Three writers, one honest marker (#849)

The set can also be authored **fleet-wide** under
`v1/@desired/state/<this-host>/systemd/expectations` — the same `@desired`
seam hostspec has had since #816. LWW and storage-backed: a sensor that was
offline during a change picks it up when it returns. That is convergence, not
a command.

`state/systemd/applied/expectations` (`AppliedConfig`) says which source is
actually in force — `file`, `desired` or `rpc` — so drift between what was
published and what is running is visible rather than assumed. An operator's
`expectations/set` and a desired publish are two writers to one handle; the
rule between them is LWW **by arrival**, and the marker is what says who won
last. The kill switch (`desired.enabled: false`) stays in file config, so the
mechanism is disarmable from outside itself.

The `expectations/set` body is accepted in **either** shape: the plain
expectation set, which is what the registry declares and what `@desired`
carries, or the GUI's `{"type": "set_expectations", …}` envelope. Only the
envelope used to be accepted, so a fleet tool that built its body from
`describe` was refused by the very sensor that had told it what to send.

The sentinel runs whenever alerting is on, **with or without** a file-config
`expectations` block: the block seeds the set, and a host with no local set is
the primary `@desired` case — the controller supplies it. (For a while the
reconciler was only built when the file block existed, so a stock install —
the shipped config comments it out — never subscribed and never published the
marker at all.) An empty set evaluates to nothing.

Every set, from any writer, passes `sentinel::validate` before it reaches the
handle — the file block at startup (a bad one is a startup error), the RPC body
(refused with `error/invalid-args` and the reason), and a desired document
(kept off the handle; the reason rides the marker's `last_rejected` while the
previous good set keeps running). Refused: `eval_interval_secs: 0`, an empty
or duplicated unit name, a timer with neither window, a window of 0, a restart
rate over a zero window. A hot-swapped `eval_interval_secs` takes effect on the
next sweep; the marker never claims a cadence the sensor is not running.

Expectation types (`zensight-common::systemd`, checked in `src/sentinel.rs`):

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

### Demonstrating it (#866)

Default-off was right; having **no opt-in lever at all** was not. Every
generated config had `actions` commented out since the block existed, so the
allowlist, the arm/confirm/cancel flow, the in-flight lock, the audit ring and
the refuse-don't-hide contract shipped correct and never once watched working —
the same blind spot #845 closed for the exporters.

```bash
sudo scripts/demo-actions.sh install     # an inert unit + a one-unit polkit rule
just actions=1 run                       # start/stop it from the GUI's Units tab
sudo scripts/demo-actions.sh remove      # put the machine back
```

`scripts/demo-actions.sh` installs `zensight-demo.service` — `sleep infinity`
under `DynamicUser`, no network, `ProtectSystem=strict`, empty
`CapabilityBoundingSet` — plus a polkit rule that grants `manage-units` for
**that unit, to that user**, and nothing else (no `manage-unit-files`, no
`reload-daemon`). `gen-configs.sh --actions <glob>` is the underlying flag;
`just actions=1` passes `zensight-demo.service`. The demo therefore never
touches a unit anything depends on, and both halves are removable.

Root is required, and that is the honest cost of demonstrating a privileged
surface: the script asks for it explicitly rather than hiding a `sudo` inside a
build recipe.

### Saying why, when the answer is no (#866)

`ActionCapability` carries an optional `reason` — the host's own words for its
refusal, served alongside `enabled: false`. Two distinct facts it can now
express, which previously looked identical from outside:

| Config | `reason` |
|---|---|
| `actions.enabled` false | names the switch and `configs/systemd.json5` |
| `enabled` true, `allow_units` empty | says every unit is refused, and why |
| `enabled` true, allowlist non-empty | `None` — nothing to explain |

The frontend shows it verbatim in place of its own generic sentence (which
stays as the fallback for a sensor older than the field). A gated control that
cannot say *why* it is gated reads as a broken one.

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
4. **Audit trail.** Every request — accepted or rejected — is recorded through
   the shared `zensight_common::audit` seam (#957), which is no longer this
   sensor's private convention: `action/set` is served through
   `served::serve_write_queryable`, whose only two ways to answer both write the
   record before they reply. A refusal carries `refused_by` as a **field** — the
   config switch that refused (`actions.enabled`, `actions.allow_units`,
   `actions.allow_unit_files`, `actions.allow_daemon_reload`) — rather than only
   inside the sentence, so the trail can be filtered on it. An action that was
   permitted and then failed records `verdict=executed` with the failure in
   `error` and `res=0`; it is not a success.

   With the `linux-audit` feature the record goes to the host's own audit
   subsystem (`ausearch -m USYS_CONFIG`); without it, to the `zensight::audit`
   tracing target with the same fields. See
   [`zensight-common/docs/audit.md`](../../zensight-common/docs/audit.md) for
   the record format and, importantly, for what it cannot say — it records what
   was asked and what happened, never *who* asked.

   Rejections are recorded in the in-memory ring too, so the operator timeline
   on `@rpc/systemd/actions` shows refused attempts, not only successful ones.

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
