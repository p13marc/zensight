# Log alerting

The logs sensor raises alerts on `state/logs/alert/*` through the shared
`AlertReporter` (firing/resolve, debounce, identity envelope, late-join seed).
There are four alert families:

| Family | Source | Docs |
|---|---|---|
| **Sentinel** (#543) | declarative pattern→alert rules | this page |
| Error-budget / SLO burn (#105) | per-unit error ratio | [telemetry.md](telemetry.md) |

## Sentinel (`syslog.sentinel`, `@rpc/logs/rules`)

The sentinel evaluates every intake line against a ruleset and fires a
structured alert on a match. Rules are declared in config **and** managed at
runtime over `@rpc/logs/rules/set` (fleet-fanout allowed) — no restart, no code
change per condition. The read side `@rpc/logs/rules` returns the active ruleset
plus per-rule lifetime hit counters.

```json5
sentinel: {
  eval_interval_secs: 10,   // reconcile / window-prune cadence
  include_builtins: true,   // ship the journald known-event rules (see below)
  rules: [
    {
      id: "sshd-bruteforce",
      description: "repeated SSH auth failures",
      match: {
        unit: "sshd.service",           // journald _SYSTEMD_UNIT
        pattern: "Failed password",     // regex on the message (unanchored)
        // min_severity: 4,             // syslog severity <= 4 (warning-and-worse)
        // facility: "auth", app: "sshd", template_id: "...", message_id: "..."
      },
      threshold: { count: 5, within_secs: 60 },  // fire only after 5 in 60s
      severity: "warning",              // info | warning | critical
      summary: "SSH bruteforce on {host}: {count}× (e.g. {message})",
      for_secs: 300,                    // auto-resolve this long after the last match
    },
  ],
}
```

### Rule fields

- **`match`** — all present criteria must hold (AND): `pattern` (regex on the
  message), `min_severity` (syslog number `<=` this; 0=emerg…7=debug, lower is
  worse), `facility`, `unit`, `app`, `template_id`, `message_id` (case-insensitive).
  An empty match matches every line.
- **`threshold`** — optional `count >= N within within_secs`, to suppress
  single-line noise. Without it the rule is one-shot per match.
- **`severity`** — `info` / `warning` / `critical`.
- **`summary`** — template with `{message}`, `{count}`, `{unit}`, `{app}`,
  `{host}`, `{severity}`, and regex capture groups `{1}`..`{9}`. The count and
  sample line ride the summary, not labels, so an ongoing alert keeps one
  identity as its count grows.
- **`labels_from`** — journald structured fields to lift into the alert labels
  (e.g. `coredump_exe`), on top of the always-included `unit`/`app`/`message_id`.
- **`for_secs`** — auto-resolve TTL. The alert clears this long after its last
  matching line (the "quiet period").

  **This already *is* the recovery hold**, which is why this sensor gained no
  `recover_after_secs` when netlink, hostspec and systemd did (#932). Those
  three evaluate a condition that is either currently violated or currently
  satisfied, so "how long must it be violated before firing" and "how long must
  it be clear before resolving" are two separate windows. A log rule has
  neither state: a line either matched or it did not. `for_secs` here is the
  quiet period, implemented in the sentinel's own `active` map with an expiry
  sweep, and the reporter is called with a zero debounce precisely because its
  debounce means nothing here. A second hold would be two timers meaning the
  same thing, and the alert would clear after the sum of them.
- **`rate_limit`** — optional `{ max_fires, per_secs }` cap on alert
  *publications* (#824). Distinct from `threshold`, which delays the first
  fire: this bounds how often a flapping rule can page. Suppressed fires are
  counted per rule and surfaced in `@rpc/logs/rules` (`suppressed`), so a
  capped rule is visibly capped.

Alert **identity** is `(rule id, unit, app, message_id)` — two lines that differ
only in their volatile payload (count, sample) update one alert rather than
spawning new ones.

### Built-in known-events

The four journald known-events (coredump, unit-failed, oomd-kill, kernel-oom)
ship as built-in rules folded into this same mechanism. They match on
`message_id` and are included whenever `journald.detect_events` is on and
`include_builtins` is true (both default true). Set `include_builtins: false` to
drop them, or add a rule with the same `id` to override. A custom `message_id`
rule needs no code change.

### Built-in kernel patterns (#824)

A second built-in set, gated by `include_kernel_builtins` (**off by default**
— the quiet-alerts stance: silence unless asked): `ext4-fs-error`,
`xfs-corruption`, `md-raid-failure`, `block-io-error`. These are the handful
of lines that mean a machine's storage is dying, and they are the alerts a
host most needs exactly when its error-budget rules are (rightly) disabled as
noise. Pattern-based rather than `MESSAGE_ID`-based, so they work on any
source — journald, network syslog, a tailed file — and each ships Critical
with a modest `rate_limit` (a dying disk can print its last words thousands
of times). Same override path as every built-in: a user rule with the same
`id` wins.

### Redaction (#824)

A quoted line is scrubbed before it leaves the host: secret-looking
`key=value` assignments (the same denylist the debug bundles use,
`is_secret_key`) are replaced with the redaction marker in `{message}` **and**
in regex capture groups — matching runs on the raw line, quoting never does.
A scrubbed summary is suffixed `(redacted)`, so it is never passed off as
verbatim; the flag stays out of the labels so a rule that sometimes matches a
secret keeps one alert identity.

### Bounded

Regexes compile once per rule; a bad regex or duplicate id is skipped with a
warning at load/replace. Per-rule hit counters are exposed on `@rpc/logs/rules`.
