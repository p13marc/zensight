# ZenSight Documentation

This directory holds **cross-cutting** references. Anything specific to one crate lives in
that crate's own `README.md` + `docs/` directory (linked below).

> Diagrams use GitHub-native [Mermaid](https://mermaid.js.org/) fences (` ```mermaid `) —
> they render inline on github.com with no tooling. Keep them theme-neutral (no hardcoded
> colors) so they read in both light and dark mode.

## Cross-cutting references

| Document | What it covers |
|----------|----------------|
| [ARCHITECTURE.md](ARCHITECTURE.md) | System overview, crate dependencies, data flow, runtime/lifecycle, health model |
| [KEYSPACE.md](KEYSPACE.md) | **The canonical Zenoh keyspace contract** — telemetry, control-plane (`@/…`), metadata (`_meta/…`), media (`@media/…`, `@pdns/…`), wildcards, and the key-building helpers |
| [zenkey rfcs/](https://github.com/p13marc/zenkey/blob/main/rfcs/00-index.md) | **The normative spec behind that contract** — the ratified Zenoh Semantic Convention (v1.3): grammar, classes/planes, `@rpc`, identity, the subject registry, operations, prior art. Written application-neutrally; ZenSight is the reference application (ch. 11). Enforced by `zenkey` |
| [design/](design/) | Archived design rationale (historical — implemented in 0.7.0): [correlation](design/correlation.md), [large-data-transfer](design/large-data-transfer.md), [zenoh-efficiency](design/zenoh-efficiency.md) |
| [plans/](plans/) | **Plans & evaluations** (live working notes for in-flight epics — unlike `design/`, nothing here is implemented-and-archived): [rerun](plans/rerun/README.md) (epic #415 — Rerun as an optional viz/replay backend) |

## Per-crate documentation

Each crate is documented in its own directory. Start at the crate's `README.md`; deeper
reference pages are under `<crate>/docs/`.

| Crate | Docs |
|-------|------|
| [zensight](../zensight/) (frontend) | views · testing · design-system · local-store |
| [zensight-common](../zensight-common/) | data-model · identity-evidence · keyspace-helpers |
| [zensight-sensor-core](../zensight-sensor-core/) | framework · artifacts |
| [zensight-sensor-snmp](../zensight-sensor-snmp/) | reference |
| [zensight-sensor-logs](../zensight-sensor-logs/) | telemetry · filtering · configuration |
| [zensight-sensor-netflow](../zensight-sensor-netflow/) | reference |
| [zensight-sensor-modbus](../zensight-sensor-modbus/) | reference |
| [zensight-sensor-sysinfo](../zensight-sensor-sysinfo/) | telemetry · collectors · configuration |
| [zensight-sensor-gnmi](../zensight-sensor-gnmi/) | reference |
| [zensight-sensor-netlink](../zensight-sensor-netlink/) | telemetry · sentinel · configuration |
| [zensight-sensor-netring](../zensight-sensor-netring/) | telemetry · detectors · configuration |
| [zensight-sensor-systemd](../zensight-sensor-systemd/) | telemetry · units-and-actions · configuration |
| [zensight-correlator](../zensight-correlator/) | correlation · keyspace · storage |
| [zensight-exporter-prometheus](../zensight-exporter-prometheus/) | reference |
| [zensight-exporter-otel](../zensight-exporter-otel/) | reference |
| [zblob](https://github.com/p13marc/zblob) | graduated external repo (was in-tree `zenoh-blob/`) |

## Quick start

```bash
cargo build --release --workspace
just run          # GUI + local sensors (netring, netlink, sysinfo, logs/journald)
just netring      # one sensor: netring | netlink | sysinfo | logs
```

See the top-level [`README.md`](../README.md) and [`CLAUDE.md`](../CLAUDE.md) for the project
overview and build/test/lint commands.
