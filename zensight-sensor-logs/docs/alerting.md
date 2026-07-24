# Log alerting

The logs sensor raises alerts on `state/logs/alert/*` through the shared
`AlertReporter` (firing/resolve, debounce, identity envelope, late-join seed).
There are four alert families:

| Family | Source | Docs |
|---|---|---|
| **Sentinel** (#543) | declarative pattern→alert rules | this page |
| Error-budget / SLO burn (#105) | per-unit error ratio | [telemetry.md](telemetry.md) |
| Novelty / rate-spike (#103) | template miner | [telemetry.md](telemetry.md) |

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

### Bounded

Regexes compile once per rule; a bad regex or duplicate id is skipped with a
warning at load/replace. Per-rule hit counters are exposed on `@rpc/logs/rules`.
