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

### Request/response shape

- Write: a GET on `zensight/v1/<origin>/@rpc/systemd/action/set` carrying JSON
  `{ "verb": "start|stop|restart|reload", "unit": "<name>" }` (`ActionCommand`).
  An accepted request replies the resulting `ActionStatus`; a refused request
  replies `reply_err` with the namespaced `error/gated` name (bad payloads get
  `error/invalid-args`).
- Read: `zensight/v1/<origin>/@rpc/systemd/action` replies the most recent
  `ActionStatus` — `{ unit, verb, accepted, result, error, ts_unix }`.
  `accepted` reflects whether the request passed validation and was issued;
  `result` is the `JobRemoved` outcome (`done`/`failed`/`timeout`/`canceled`/…)
  when the job completed, else `None`.

### The four gates

Every request must clear all of the following before anything happens to a unit:

1. **Master switch (`actions.enabled`).** `run()` returns immediately when false,
   logging `service control disabled`, and never declares the `action/set` or
   `action` queryables. This is the primary gate — with it off there is no
   procedure to call.
2. **Allowlist (`actions.allow_units`).** `validate()` (a pure, unit-tested
   function) requires the target unit to match at least one glob in
   `allow_units`. An **empty allowlist rejects every request** (and the sensor
   warns at startup that it will do so). An empty unit name is also rejected.
   This is described as defence-in-depth *on top of* polkit.
3. **systemd/polkit authorization.** Authorization for the underlying
   `StartUnit`/`StopUnit`/`RestartUnit`/`ReloadUnit` call is delegated to
   systemd/polkit — **not enforced in this code**. The sensor must run as root,
   or unprivileged with a scoped polkit rule granting
   `org.freedesktop.systemd1.manage-units` for the allowlisted units. If polkit
   denies the call, the D-Bus method returns an error that surfaces as
   `accepted: true` with an `error` (the request was allowlisted and issued, but
   the enqueue failed).
4. **Audit log.** Every request — accepted or rejected — is written to the
   `zensight::audit` tracing target: rejections log `decision = "rejected"` with
   the reason; issued jobs log `decision = "accepted"` with the verb, unit, job
   path, and result.

### Execution semantics

- The corresponding `Manager` method is called with `mode = "replace"`. It uses
  `StartUnit`, **not** `StartTransientUnit` — no transient units are created.
- The `JobRemoved` signal stream is subscribed **before** the method is issued
  (so the completion signal can't be missed), then the specific job path is
  tracked to completion with a bounded wait of `actions.job_timeout_secs`
  (default 30). On timeout, `result` is `None` (unknown, not assumed success).

### Points worth flagging

- **Authorization is not in this crate.** The allowlist is only defence-in-depth;
  the real privilege boundary is systemd/polkit + how the process is run. A
  misconfigured deployment (root, broad `allow_units`) grants broad service
  control. The design intent is: keep `enabled: false`, and when enabling, use a
  narrow `allow_units` and a scoped polkit rule rather than running as root.
- The allowlist uses the same glob compilation as the watchlist
  (`config::compile_watch`), so a pattern like `app-*.service` matches a family
  of units — author it as narrowly as the deployment allows.
