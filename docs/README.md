# ZenSight Documentation

Reference documentation for the ZenSight observability platform. (Design plans
and proposals are intentionally **not** kept here — this directory holds only
current, factual documentation.)

| Document | What it covers |
|----------|----------------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | System overview, crate dependencies, data flow, runtime/lifecycle, health model, exporters, directory layout |
| [SENSORS.md](SENSORS.md) | Per-sensor reference: sources, configuration, and the Zenoh keys each sensor publishes/serves |
| [KEYSPACE.md](KEYSPACE.md) | Canonical Zenoh keyspace reference — telemetry, control-plane (`@/…`), metadata (`_meta/…`), wildcards, and the key-building helpers |
| [STORAGE.md](STORAGE.md) | Durable router-hosted storages: evidence/entity fs durability + the historical passive-DNS (`@pdns`) tier, and the mutable-key/timestamping caveat (#310) |
| [BLOB-ROUTER-STORAGE.md](BLOB-ROUTER-STORAGE.md) | Router-hosted Tier-2 content store for `zenoh-blob` large-data transfer (#201) |
| [UI_TESTING.md](UI_TESTING.md) | Testing the Iced 0.14 frontend with the `iced_test` simulator |

## Quick start

```bash
# Build everything (release)
cargo build --release --workspace

# Build + configure + run the GUI and all local sensors (netring, netlink,
# sysinfo, logs/journald) — close the GUI to stop everything.
just run

# Run a single sensor
just netring   # | netlink | sysinfo | logs
```

See the top-level `README.md` and `CLAUDE.md` for build/test/lint commands and
the project overview.
