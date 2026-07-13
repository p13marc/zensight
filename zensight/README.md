# zensight

Desktop frontend for the ZenSight observability platform. Built with
[Iced 0.14](https://iced.rs/), it is a **host- and incident-centric** viewer:
it subscribes to the v1 keyspace over Zenoh (`zensight/@v1/*/telemetry/**` for
samples, one `zensight/@v1/*/state/**` subscriber for the whole state plane),
auto-discovers every sensor, groups each host's per-protocol facets under one
card, and rolls firing alerts up into unified incidents. Telemetry persists to
a bounded local store so history survives restart.

## Quick start

```bash
# Build + run the whole local stack (sensors + correlator + GUI, loopback rendezvous)
just run

# Just the GUI (connect to an existing hub / discover peers)
cargo run -p zensight --release      # or: just gui

# Demo mode — synthetic telemetry for every sensor, no real sensors or Zenoh hub
cargo run -p zensight --release -- --demo    # or: just demo
```

Demo mode is the fastest way to see the UI: it generates realistic telemetry,
health, liveness, and periodic anomaly alerts for all sensor types.

Settings persist to `~/.config/zensight/settings.json5` (Zenoh mode, connect /
listen endpoints, stale threshold, theme).

## Views

The nav rail routes between these views (`CurrentView` in `src/app.rs`):

| View | What it shows |
|------|---------------|
| **Dashboard** | Fleet overview — host cards with composite health and sensor-health summary bar. |
| **Device** | Per-device metric table + time-series charts (booleans as 0/1 step series, log-rate trends). |
| **Alerts** | Threshold rules plus sensor/external alerts, severity/source filter pills, saved presets. |
| **Security** | NDR anomaly lens with a MITRE ATT&CK by-tactic rollup and runtime detection tuning. |
| **Expectations** | Author sentinel expectations pushed to the netlink sensor at runtime. |
| **Topology** | Force-directed graph of sysinfo/netlink hosts with an alert-severity overlay. |
| **Logs** | Structured log drill-down, MESSAGE_ID catalog, follow/pause, boot lens (seeded from the cold store). |
| **Inventory** | Passive asset inventory + fingerprint explorer (JA3/JA4/JA4H/SNI/HASSH). |
| **Bandwidth** | Bandwidth-by-process/service live monitor. |
| **Incidents** | Unified Incident object — grouped alerts + timeline + evidence pivots. |
| **Sensors** | Sensor registry / health detail. |
| **Settings** | Zenoh connection mode, endpoints, stale threshold, theme. |

The persistent shell (`view/shell.rs`) wraps every view with a left nav rail and
top bar. Three overlays render on top of the current view (not routable):
command palette (Ctrl+P), fuzzy global search (Ctrl+K), keyboard-help (`?`).

## Documentation

- [`docs/views.md`](docs/views.md) — view/state pattern, the shell, overlays, per-view tour.
- [`docs/testing.md`](docs/testing.md) — UI testing with Iced's `Simulator`, the F12 tester recorder, mock data.
- [`docs/design-system.md`](docs/design-system.md) — the D2 design system (colors, tokens) and the CI color guard.
- [`docs/local-store.md`](docs/local-store.md) — the redb-backed tiered telemetry + log store.
- [`../docs/KEYSPACE.md`](../docs/KEYSPACE.md) — the authoritative key-expression contract the frontend subscribes to.
- [`../docs/ARCHITECTURE.md`](../docs/ARCHITECTURE.md) — platform-wide architecture.

## Feature flags

| Feature | Description |
|---------|-------------|
| `tester` | Enable the F12 UI recorder (pulls in `iced/tester`). Build with `cargo build -p zensight --features tester`. |

## Testing

```bash
cargo test -p zensight              # ~330 tests: unit + Simulator UI tests
cargo test -p zensight --test ui_tests   # UI tests only
cargo test -p zensight --lib             # unit tests only
```

See [`docs/testing.md`](docs/testing.md) for the full guide.

## License

MIT OR Apache-2.0
