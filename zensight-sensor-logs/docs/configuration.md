# logs configuration

JSON5, loaded with `--config`. Two shipped examples:
[`../../configs/syslog.json5`](../../configs/syslog.json5) (network listeners,
journald commented) and [`../../configs/logs.json5`](../../configs/logs.json5)
(journald-only, used by `just run`). This page documents each block; defaults are
as parsed by `src/config.rs`. At least one listener **or** enabled journald is
required (validated at load).

## Top level

```json5
{
  zenoh: { mode: "peer" },        // "client" | "peer" | "router"
  serialization: "json",          // "json" | "cbor" (default json)
  syslog: { ... },                // sensor settings (historical key name)
  artifacts: { report: { ... } }, // on-demand artifact procedures (disabled by default)
  logging: { level: "info" },
}
```

## `syslog`

| Key | Default | Meaning |
|---|---|---|
| `source` | hostname | override the agent-host source id in payloads (v1 keys are origin-scoped, so it no longer appears in key expressions) |
| `listeners` | `[]` (example: udp+tcp) | network listeners — see below |
| `hostname_aliases` | `{}` | map sender IP → friendly name when a message has no hostname |
| `include_raw_message` | `false` | keep the raw line in labels (`log.record.original`) |
| `filter` | empty | static message filters — see [filtering.md](filtering.md) |
| `enable_dynamic_filters` | `false` | expose `@rpc/logs/filter/set` + `@rpc/logs/filter` |
| `journald` | `null` (off) | systemd-journald source — see below |
| `derived` | `true` | emit derived rollups |
| `derived_interval_secs` | `10` | rollup emission cadence |
| `top_units` | `10` | distinct per-unit series before the rest fold into `other` |
| `error_budget` | off | per-unit SLO burn-rate alerting — see below |
| `templating` | on | Drain3 template mining — see below |
| `novelty` | off | novelty / rate-spike anomaly alerts — see below |
| `ingest` | safe | network-path rate-limit + loss accounting — see below |
| `multiline` | on | stack-trace joining on stream listeners — see below |
| `events_ring_capacity` | `10000` | in-memory ring served on `@rpc/logs/events` (min 100, ≈3 MB) |

### `syslog.listeners[]`

```json5
listeners: [
  { protocol: "udp",  bind: "0.0.0.0:1514", max_message_size: 65535 },
  { protocol: "tcp",  bind: "0.0.0.0:1514", max_connections: 1000,
    connection_timeout_secs: 300, framing: "auto" },
  { protocol: "unix", bind: "/var/run/zensight-syslog.sock",
    socket_mode: 438, remove_existing_socket: true },  // 438 = 0o666
]
```

| Field | Default | Applies to | Meaning |
|---|---|---|---|
| `protocol` | — | all | `udp` \| `tcp` \| `unix` |
| `bind` | — | all | `host:port` (UDP/TCP) or socket path (unix) |
| `max_message_size` | `65535` | UDP | max datagram bytes |
| `max_connections` | `1000` | TCP/unix | concurrent connections |
| `connection_timeout_secs` | `300` | TCP/unix | idle connection timeout |
| `socket_mode` | `0o666` (438) | unix | socket file permissions |
| `remove_existing_socket` | `true` | unix | unlink before binding |
| `framing` | `auto` | TCP/unix | RFC 6587 framing: `auto` (leading digit ⇒ octet-counting, else LF), `lf`, `octet` |

### `syslog.journald`

Reads the local journal via libsystemd (no `journalctl` subprocess). Minimal form
`{ enabled: true }` tails the system journal with sane defaults.

```json5
journald: {
  enabled: true,
  scope: "system",          // system | user | local_only | runtime_only
  namespace: null,          // a journald log namespace, or null
  start_from: "cursor",     // cursor | tail | head | boot | since
  since: null,              // e.g. "15m" when start_from = "since"
  cursor_file: null,        // null = $STATE_DIRECTORY / XDG state default
  on_missing_cursor: "tail",// tail | since (saved cursor rotated out)

  // server-side matching (applied in the journal, #59):
  units: [],                // _SYSTEMD_UNIT allowlist ([] = all)
  min_priority: null,       // 0..7 (3 = err); expands to a PRIORITY OR-group
  transports: [],           // _TRANSPORT allowlist
  match: {},                // raw FIELD=value matches, AND'd

  extra_fields: [],         // extra raw fields to copy into labels
  include_dev_fields: false,// CODE_FILE/LINE/FUNC, ERRNO

  // known-event alerts (#61):
  detect_events: true,      // coredump / unit-failed / OOM via MESSAGE_ID
  event_dedup_secs: 30,     // coalesce + auto-resolve window
  event_severity: {},       // per-MESSAGE_ID (32-char hex) severity override

  // storm robustness (#62):
  overflow: "drop_newest",  // drop_newest | block
  max_eps: null,            // optional global rate limit (entries/sec)
  sample_ratio: 100,        // keep 1-in-N over budget; count the rest sampled-out
  drop_alert_ratio: 0.01,   // raise ErrorReport once windowed loss exceeds this
}
```

`scope`, `start_from`, `on_missing_cursor`, `overflow` are string enums. See
[filtering.md](filtering.md) for server-side matching and known-event detection.

### `syslog.error_budget` (#105, alerting off by default)

Per-unit SLO / error-budget burn-rate alerting on the derived per-unit rollups.
Emits `error_ratio` / `burn_rate` gauges even when `enabled: false`.

```json5
error_budget: {
  enabled: false,      // master switch for *alerting* (gauges emit regardless)
  target_ratio: 0.05,  // tolerated per-window error fraction (SLO target)
  burn_rate: 2.0,      // fire when window ratio > target_ratio * burn_rate
  burn_windows: 3,     // consecutive over-budget windows before firing
  min_messages: 20,    // min window volume before the ratio is trusted
}
```

Raises the `log-error-budget` alert on sustained multi-window burn; auto-resolves
the first window the unit is back within budget.

### `syslog.templating` (#102, on by default)

```json5
templating: {
  enabled: true,
  depth: 4,            // parse-tree depth
  sim_threshold: 0.4,  // fraction of matching non-wildcard tokens to join a cluster
  max_children: 100,   // literal children per node before folding to wildcard
  max_clusters: 1000,  // hard cap on retained templates (bounds memory)
  top_templates: 50,   // distinct emitted series before the rest fold into `other`
}
```

### `syslog.novelty` (#103, off by default — raises alerts)

Builds on the template miner; requires `templating.enabled`.

```json5
novelty: {
  enabled: false,
  warm_up_secs: 300,          // templates first seen in this window are baseline
  novelty_dedup_secs: 300,    // how long a fired novelty stays firing
  rate_spike_multiplier: 5.0, // known template fires above N× its EWMA baseline (<=1 disables)
  min_spike_count: 10.0,      // absolute floor on window count before a spike fires
  ewma_alpha: 0.3,            // baseline smoothing factor
  max_templates: 2000,        // seen-set cap (bounds memory)
}
```

Raises `log-novelty` (never-seen template) and `log-rate-spike` (known template
rate jump) anomaly alerts.

### `syslog.ingest` (#106, network paths)

```json5
ingest: {
  max_eps: null,            // optional global rate limit (msgs/sec); null = unlimited
  sample_ratio: 100,        // keep 1-in-N over budget
  overflow: "drop_newest",  // drop_newest | block (full telemetry channel)
  drop_alert_ratio: 0.01,   // ErrorReport once windowed dropped fraction exceeds this
}
```

Safe defaults (rate limit off, generous channel) so normal traffic is never
dropped. Emits `logs/ingest/*_total` counters.

### `syslog.multiline` (#107, on by default)

Stream (TCP/Unix) listeners only — folds continuation lines (indented stack
frames, `Caused by:`, `...`, `Traceback …`) into the preceding record so a
traceback stays one event. journald is unaffected (one record per entry).

```json5
multiline: {
  enabled: true,
  flush_timeout_ms: 200,  // emit a buffered record this long after the last frame
  max_lines: 500,         // cap on lines folded into one record
  max_bytes: 65536,       // cap on bytes in one joined record
}
```

## `artifacts`

The on-demand artifact procedures (framework-wide:
`@rpc/logs/artifact/{request,cancel}`, progress on the
`state/logs/artifact/<kind>` status document). Every kind is disabled by
default; enable `report` to allow downloading a redacted `tar.zst` debug bundle
from the GUI (`max_bytes`, `cooldown_secs`, `ttl_secs`, `chunk_size`).

## Build feature

`journald` ingestion is behind the `journald` cargo feature (**on by default**,
links `libsystemd`). On a host without libsystemd, build
`--no-default-features` to drop the journald reader (network syslog only).
