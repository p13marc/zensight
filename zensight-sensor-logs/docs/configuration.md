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
  allow_unknown_fields: false,    // forward-compat escape hatch (see below)
}
```

### Strict loading (#547)

Unknown keys are **rejected at load** with an error that names them (including
nested paths, e.g. `syslog.novelty.enabld`). This closes the silent-default trap
where a typo — `novlety:` instead of `novelty:` — left an analytic at its default
while the operator believed it was on. Set `allow_unknown_fields: true` to
downgrade rejection to a startup warning for mixed-version fleets sharing one
config file across sensor versions.

On start the sensor logs a one-line **configuration summary** (active sources +
which analytics are on/off + ingest posture) at `info`, so a source that never
came up or an analytic left off is visible immediately rather than inferred later
from missing telemetry.

## `syslog`

| Key | Default | Meaning |
|---|---|---|
| `source` | hostname | override the agent-host source id in payloads (v1 keys are origin-scoped, so it no longer appears in key expressions) |
| `listeners` | `[]` (example: udp+tcp) | network listeners — see below |
| `hostname_aliases` | `{}` | map sender IP → friendly name when a message has no hostname |
| `host_timezones` | `{}` | map sender IP → IANA timezone (`"America/New_York"`) for RFC 3164 senders; overrides the listener `timezone` per sender (#545) |
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
| `sentinel` | empty | log sentinel pattern→alert rules (#543) — see [alerting.md](alerting.md) |
| `store` | off | durable per-line history (#544) — see below |
| `files` | none | file-tailing sources (#549) — see below |
| `evidence` | on | observer evidence for remote senders (#552) — see below |
| `logbundle` | off | filtered log-export artifact limits (#555) — see below |

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
| `protocol` | — | all | `udp` \| `tcp` \| `unix` \| `tls` |
| `bind` | — | all | `host:port` (UDP/TCP) or socket path (unix) |
| `max_message_size` | `65535` | UDP | max datagram bytes |
| `max_connections` | `1000` | TCP/unix | concurrent connections |
| `connection_timeout_secs` | `300` | TCP/unix | idle connection timeout |
| `socket_mode` | `0o666` (438) | unix | socket file permissions |
| `remove_existing_socket` | `true` | unix | unlink before binding |
| `framing` | `auto` | TCP/unix | RFC 6587 framing: `auto` (leading digit ⇒ octet-counting, else LF), `lf`, `octet` |
| `timezone` | `null` (UTC) | all | IANA zone RFC 3164 senders on this listener use (#545); DST-correct via the tz database. No effect on RFC 5424 |
| `tls` | `null` | tls | TLS cert/key config (#550) — required for `protocol: "tls"`; see below |

### `tls` (RFC 5425 TLS listener, #550)

`protocol: "tls"` is TLS over TCP with octet-counting framing (rustls, `ring`
provider — no OpenSSL). Cleartext connections to a TLS port are rejected.

```json5
{ protocol: "tls", bind: "0.0.0.0:6514",
  tls: {
    cert_file: "/etc/zensight/server.crt",   // PEM chain
    key_file:  "/etc/zensight/server.key",   // PEM key; paths accept ${ENV}/file: (#538)
    client_ca_file: "/etc/zensight/ca.crt",  // optional: require + verify client certs (mTLS)
    min_version: "1.3",                       // "1.3" (default) | "1.2"
  } }
```

Key material is referenced **by path only** (never inline PEM). With
`client_ca_file` set, clients must present a certificate verified against it and
its CN is attached as `sd.tls.peer_cn`. Certs are **hot-reloaded** on mtime
change (rotation needs no restart). `max_connections` / `connection_timeout_secs`
apply as for TCP.

Generate a quick self-signed server cert for testing:
```sh
openssl req -x509 -newkey rsa:2048 -nodes -keyout server.key -out server.crt \
  -days 365 -subj "/CN=logs.example.com"
```
Point an rsyslog sender at it with `omfwd` (`StreamDriver="ossl"`,
`StreamDriverMode="1"`), or test with `openssl s_client -connect host:6514`.

For **reliable delivery** across receiver restarts (sender-side disk-assisted
queueing over TCP/TLS, and why RELP is deferred), see
[reliable-delivery.md](reliable-delivery.md) (#551).

### RFC 3164 timestamps (#545)

RFC 3164 (BSD) stamps carry neither a year nor a timezone. The parser:

- **infers the year** that puts the instant closest to receive time — so a
  `Dec 31 23:59:58` message received just after midnight is dated to the
  *previous* year, and vice-versa;
- interprets the wall clock in the sender's zone — the listener's `timezone`,
  overridden per-sender by `syslog.host_timezones` (both IANA names, default
  UTC), applying DST from the tz database;
- **sanity-clamps**: if the reconstructed instant is still more than ~90 days
  from receive time, it's treated as garbage, replaced with the receive time,
  and labelled `ts_source=receiver`.

A bad IANA name fails at load (it is not silently ignored). RFC 5424 carries its
own explicit offset and is unaffected.

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
  channel_capacity: 1000,   // intake channel slots (#546); raise to absorb bursts
  collapse_repeats: false,  // fold consecutive identical lines (#546); off by default
  collapse_window_ms: 1000, // idle gap that closes a run of identical lines
}
```

Safe defaults (rate limit off, generous channel) so normal traffic is never
dropped. Emits `logs/ingest/*_total` counters plus a windowed
`logs/ingest/dropped_ratio` gauge (#546). Sustained-loss reporting is
level-triggered: it re-reports periodically while loss persists and emits an
explicit recovery report when it clears.

**Channel capacity** (`channel_capacity`) sizes the intake queue between the
listeners and the processing loop; under a burst larger than this, `drop_newest`
sheds before the (optional) rate limiter engages — raise it to trade memory
(one parsed message per slot) for burst absorption.

**Repeat collapse** (`collapse_repeats`) folds consecutive identical
`(source, message)` lines into a single record carrying a `repeat_count` label —
syslog's classic "last message repeated N times" — so a screaming line doesn't
exhaust the ring one copy at a time. A run is emitted once a *different* line
arrives or no matching line has for `collapse_window_ms` (also the max added
latency for a line with no follow-up, which is why it's opt-in). The collapsed
count still feeds the rollup counters, so totals stay honest.

### `syslog.store` (#544, durable history — off by default)

A disk-backed redb store behind the hot ring, so `@rpc/logs/events` can serve
**days** of history across restarts. The ring stays the hot cache; the store
answers `from`/`to`/`after_uid` queries.

```json5
store: {
  enabled: false,           // opt-in
  path: null,               // null = $STATE_DIRECTORY / XDG state / ~/.local/state
  max_age_days: 7,          // prune by age
  max_records: 2000000,     // and by size, whichever first
  batch_size: 500,          // flush a batch at this many queued records
  flush_interval_secs: 2,   // ...or at least this often
  prune_interval_secs: 300, // prune + health cadence
  queue_capacity: 100000,   // writer-channel bound; full → drop + count (never block intake)
}
```

Writes are batched on a dedicated thread **off the hot intake loop** — a slow
disk drops (counted as `store/write_drops_total`) rather than adding ingest
latency. Health gauges: `store/records`, `store/oldest_age_secs`,
`store/write_drops_total`.

**Query** (`@rpc/logs/events`): `from=<ms>`/`to=<ms>` select an inclusive time
window; `after_uid=<uid>` is a pagination cursor (pass the previous page's
last/oldest uid) and `limit=<n>` caps the page. Pages are newest-first. The
legacy `since`/`max` ring selectors keep working.

**Server-side search** (#553): `pattern=<regex>` (message; a
metacharacter-free pattern takes a substring fast path), `severity_min=<slug|n>`
(worse-or-equal), `unit=`, `app=`, `facility=` — applied at the sensor over both
the ring and the store, so a content search spans days of history without
shipping it all to the client. A bad or oversized regex is rejected (the `regex`
engine is linear-time; the compiled size is capped); a long-range scan is bounded
(a partial page you paginate on). Cheap field prefilters run before the regex.

### `syslog.files` (#549, file tailing — none by default)

Tail log files into the same pipeline as the network/journald sources (so
filtering, templating, rollups, and sentinel rules apply). Lines carry
`source_type=file` and an `sd.file.path` label.

```json5
files: {
  rescan_secs: 15,          // re-expand globs (pick up new files) this often
  poll_ms: 500,             // poll tracked files for new bytes this often
  offsets_path: null,       // null = $STATE_DIRECTORY / XDG state
  max_line_bytes: 1048576,  // truncate a single (joined) line beyond this
  sources: [
    {
      paths: ["/var/log/app/*.log"],   // globs
      unit: "app.service",             // attribute like journald `unit`
      app:  "app",                     // program name
      format: "plain",                 // plain | syslog (run the <PRI> parser)
      severity: "info",                // default severity for plain lines
      severity_regex: "^\\[(\\w+)\\]", // optional: extract ERROR/WARN/... per line
      labels: { env: "prod" },         // static labels (as sd.file.*)
      multiline: true,                 // join stack traces (on by default)
    },
  ],
}
```

**Rotation-aware**: each file keeps its open handle, so a logrotate
rename+recreate keeps draining the rotated-away inode to EOF before switching
(no lost lines); a copytruncate (same inode, shrank) resets to offset 0.
**Offsets** are persisted atomically (same scheme as the journald cursor) so a
restart resumes without re-ingesting or skipping. File sources share the
network ingest stats + rate limiter (`ingest.max_eps`/`overflow`).

### `syslog.evidence` (#552, observer evidence — on by default)

A central collector publishes a `HostEvidence` claim per remote syslog sender on
`state/logs/evidence/device/<host>` (`observer = "logs"`), so those devices reach
the correlator's entity catalog and fuse with SNMP / netring observations of the
same gear. Names come from the message header / `hostname_aliases` / mTLS peer CN
— no DNS on the intake path.

```json5
evidence: {
  enabled: true,        // publish observer evidence for remote senders
  refresh_secs: 300,    // re-publish + prune cadence
  expire_secs: 21600,   // drop a sender silent this long (6h)
  max_senders: 4096,    // cardinality cap (bounds the key space)
  reverse_dns: false,   // opt-in PTR FQDN enrichment (cached, off the hot path)
}
```

No-op without a network/TLS listener (only remote peers are "observed"). With
`reverse_dns`, PTR lookups run only in the publish tick and are cached per IP, so
they never block ingestion.

### `logbundle` (#555, filtered log export — off by default)

An on-demand artifact (`@rpc/logs/artifact/request` kind `logbundle`, delivered
over `@blob`) that packages the log lines matching a filter into a single zstd
bundle — for attaching to a ticket or handing to a vendor. The request selectors
mirror the events/search query (`from`/`to`/`pattern`/`severity_min`/`unit`/
`app`/`source`) plus a `format` (`jsonl` | `text`). The bundle's first line is a
JSON manifest (query, counts, range, truncation flag).

```json5
logbundle: {
  enabled: false,          // opt-in
  max_bytes: 67108864,     // 64 MiB cap (stops + flags truncation)
  max_lines: 1000000,      // line cap (stops + flags truncation)
  cooldown_secs: 30,
  ttl_secs: 600,
  chunk_size: 524288,      // blob transfer chunk (256 KiB–1 MiB)
}
```

Lines come from the durable store (#544) when enabled, plus the hot ring.
Producer runs off the intake loop. `zenctl` can request the bundle headlessly;
the GUI Export button (prefilled from the active Logs filter) rides with #554.

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
