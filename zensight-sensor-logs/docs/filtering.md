# logs filtering

The logs sensor filters at two layers: **static** message filters (applied to
every parsed line before publishing) and **dynamic** filters (added/removed at
runtime over Zenoh). journald additionally supports **server-side matching**
applied inside the journal itself.

## Static filters — `syslog.filter`

Configured under `syslog.filter` (`src/filter.rs`). A message must pass every
enabled criterion. All fields default empty (no filtering).

| Field | Type | Meaning |
|---|---|---|
| `min_severity` | `0..7` | drop messages less severe than this (0=emergency … 7=debug) — i.e. keep severity ≤ `min_severity` |
| `include_facilities` | `[string]` | if non-empty, keep only these facilities |
| `exclude_facilities` | `[string]` | drop these facilities |
| `include_app_patterns` | `[PatternFilter]` | if non-empty, keep only apps matching |
| `exclude_app_patterns` | `[PatternFilter]` | drop apps matching |
| `include_hostname_patterns` | `[PatternFilter]` | if non-empty, keep only hosts matching |
| `exclude_hostname_patterns` | `[PatternFilter]` | drop hosts matching |
| `include_message_patterns` | `[PatternFilter]` | if non-empty, keep only messages matching |
| `exclude_message_patterns` | `[PatternFilter]` | drop messages matching |

A `PatternFilter` is `{ pattern: "...", pattern_type: "glob" | "regex" }`.

```json5
syslog: {
  filter: {
    min_severity: 4,                          // warning and worse
    include_facilities: ["auth", "daemon", "kern"],
    exclude_facilities: ["local7"],
    exclude_app_patterns: [
      { pattern: "systemd-*", pattern_type: "glob" },
      { pattern: "^cron$",    pattern_type: "regex" },
    ],
    exclude_message_patterns: [
      { pattern: "*HEALTHCHECK*", pattern_type: "glob" },
    ],
  },
  enable_dynamic_filters: true,
}
```

## Dynamic filters — `@rpc/logs/filter/set` + `@rpc/logs/filter`

Set `enable_dynamic_filters: true` to expose the runtime control procedures.
Dynamic filters are keyed by id and combine with the static base filter.

- **Write** (a GET with payload): `zensight/v1/<origin>/@rpc/logs/filter/set`
  — a `FilterCommand` (serde-tagged `"type"`, snake_case):

  ```json
  { "type": "add_filter", "id": "my-filter",
    "filter": { "min_severity": 3,
                "exclude_app_patterns": [ { "pattern": "noisy-app", "pattern_type": "glob" } ] } }

  { "type": "remove_filter", "id": "my-filter" }
  { "type": "clear_filters" }
  { "type": "get_status" }
  ```

  `id` is optional on `add_filter` (auto-generated if omitted).

- **Read**: `zensight/v1/<origin>/@rpc/logs/filter` — returns a
  `FilterStatus { base_filter, dynamic_filters, stats }` (the config base filter,
  the active dynamic filters, and filter statistics).

The GUI drives this from `SyslogFilterState` (`zensight/src/view/specialized/syslog.rs`).

## journald server-side matching

For the journald source, prefer **server-side** filters — they are applied in the
journal, so filtered entries are never decoded or transported. Configured under
`syslog.journald` (see [configuration.md](configuration.md)):

| Field | Applied as |
|---|---|
| `units` | `_SYSTEMD_UNIT` allowlist (OR'd; empty = all) |
| `min_priority` | expands to a `PRIORITY=0..min` OR-group (libsystemd has no `<=`) |
| `transports` | `_TRANSPORT` allowlist (e.g. `kernel`, `journal`, `stdout`, `syslog`) |
| `match` | raw `FIELD=value` matches, AND'd with the above |

The static/dynamic message filters above still apply on top of journald entries
after they are read.

## Known-event detection (journald, #61)

`journald.detect_events` (default on) matches well-known systemd events —
coredump / unit-failed / OOM — by their stable `MESSAGE_ID` and raises alerts on
`state/logs/alert/*`. Coredump entries capture `COREDUMP_*` (exe/signal/pid) onto the
record + alert; audit / SELinux records (`_AUDIT_TYPE_NAME`, `_SELINUX_CONTEXT`)
are tagged `category=security` for the Security view. Tuning: `event_dedup_secs`
(coalesce + auto-resolve window), `event_severity` (per-`MESSAGE_ID` override).

## Loss accounting under storms

Both paths keep honest accounting rather than dropping silently. Under a log storm
the reader sheds (or backpressures) per `overflow` (`drop_newest` / `block`) and,
with a configured `max_eps`, keeps 1-in-`sample_ratio` and counts the rest as
sampled-out. Sustained loss beyond `drop_alert_ratio` surfaces as an
`ErrorReport`. Journal rotation (`journalctl --rotate`) is followed transparently.
