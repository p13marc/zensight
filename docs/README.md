# ZenSight Documentation

This directory holds **cross-cutting** references. Anything specific to one crate lives in
that crate's own `README.md` + `docs/` directory (linked below).

> Diagrams use GitHub-native [Mermaid](https://mermaid.js.org/) fences (` ```mermaid `) —
> they render inline on github.com with no tooling. Keep them theme-neutral (no hardcoded
> colors) so they read in both light and dark mode.

## Cross-cutting references

| Document | What it covers |
|----------|----------------|
| [POSITIONING.md](POSITIONING.md) | **What ZenSight is for** — the problem it was built for, why Zenoh-native, what the standard stack cannot show, the read-only stance and its two gated exceptions, who should run it and who should not (yet) |
| [COMPATIBILITY.md](COMPATIBILITY.md) | **What is stable before 1.0** — the surface table, what may break with a minor, the deprecation window, and the 1.0 criteria |
| [ARCHITECTURE.md](ARCHITECTURE.md) | System overview, crate dependencies, data flow, runtime/lifecycle, health model |
| [KEYSPACE.md](KEYSPACE.md) | **The canonical Zenoh keyspace contract** — the deployed keyspace-v2 profile: `v1` grammar, the `telemetry`/`state`/`events` classes, the verbatim `@rpc`/`@media`/`@blob` planes, `@catalog` and `@desired`, and the typed key builders |
| [latency.md](latency.md) | **Detection latency per sensor** — which shipped defaults meet SYS-SUP-004's 10 s bound and which do not, with measured figures from tests that time what a *subscriber* receives (#961) |
| [zenkey rfcs/](https://github.com/p13marc/zenkey/blob/main/rfcs/00-index.md) | **The normative spec behind that contract** — the ratified Zenoh Semantic Convention (v1.3): grammar, classes/planes, `@rpc`, identity, the subject registry, operations, prior art. Written application-neutrally; ZenSight is the reference application (ch. 11). Enforced by `zenkey` |
| [DEPLOYMENT.md](DEPLOYMENT.md) | Running it on a fleet — containers and Quadlets, native binaries, TLS/mTLS to the router, exporting to Prometheus/OpenTelemetry, and one policy file instead of eighteen |
| [TOPOLOGY-REDESIGN.md](TOPOLOGY-REDESIGN.md) | The topology view's design — prior art, lenses, non-goals |
| [ops/](ops/) | **Operating a fleet** — [SIZING](ops/SIZING.md): what each component actually uses, how the two on-disk stores grow, and what the 2026-08-17 OOM looks like in today's health documents |
| [design/](design/) | Archived design rationale (historical — implemented in 0.7.0): [correlation](design/correlation.md), [large-data-transfer](design/large-data-transfer.md), [zenoh-efficiency](design/zenoh-efficiency.md) |
| [plans/](plans/) | **Plans & evaluations** (live working notes for in-flight epics — unlike `design/`, nothing here is implemented-and-archived): [rerun](plans/rerun/README.md) (epic #415 — **closed**, [DECISION.md](plans/rerun/DECISION.md): an optional debugging backend, off by default), [adaptive-media](plans/adaptive-media/README.md) (epic #712 — measurement, receiver feedback and tier adaptation on `@media`) |

## Per-crate documentation

Each crate is documented in its own directory. Start at the crate's `README.md`; deeper
reference pages are under `<crate>/docs/`.

| Crate | Docs |
|-------|------|
| [zensight](../zensight/) (frontend) | views · testing · design-system · local-store · media-receiver |
| [zensight-common](../zensight-common/) | data-model · identity-evidence · keyspace-helpers · registry-honesty · audit |
| [zensight-store](../zensight-store/) | the tiered store shared by the GUI and the historian |
| [zensight-sensor-core](../zensight-sensor-core/) | framework · artifacts |
| [zensight-sensor-snmp](../zensight-sensor-snmp/) | reference |
| [zensight-sensor-logs](../zensight-sensor-logs/) | telemetry · filtering · configuration · alerting · reliable-delivery · testing |
| [zensight-sensor-netflow](../zensight-sensor-netflow/) | reference |
| [zensight-sensor-modbus](../zensight-sensor-modbus/) | reference |
| [zensight-sensor-sysinfo](../zensight-sensor-sysinfo/) | telemetry · collectors · configuration |
| [zensight-sensor-gnmi](../zensight-sensor-gnmi/) | reference |
| [zensight-sensor-netlink](../zensight-sensor-netlink/) | telemetry · sentinel · configuration |
| [zensight-sensor-netring](../zensight-sensor-netring/) | telemetry · detectors · configuration |
| [zensight-sensor-systemd](../zensight-sensor-systemd/) | telemetry · units-and-actions · configuration |
| [zensight-sensor-hostspec](../zensight-sensor-hostspec/) | assertions · configuration |
| [zensight-sensor-container](../zensight-sensor-container/) | assertions · configuration |
| [zensight-sensor-bmc](../zensight-sensor-bmc/) | assertions · configuration |
| [zensight-sensor-probe](../zensight-sensor-probe/) | assertions · configuration |
| [zensight-sensor-pve](../zensight-sensor-pve/) | assertions · configuration |
| [zensight-sensor-parallax](../zensight-sensor-parallax/) | streams · configuration · qos-and-latency · qos-express |
| [zensight-correlator](../zensight-correlator/) | correlation · keyspace · storage |
| [zensight-historian](../zensight-historian/) | storage · range-api · configuration |
| [zensight-desired](../zensight-desired/) | policy |
| [zensight-exporter-prometheus](../zensight-exporter-prometheus/) | reference |
| [zensight-exporter-otel](../zensight-exporter-otel/) | reference |
| [zensight-conformance](../zensight-conformance/) | the live RFC-judge harness (`publish = false`) |
| [zensight-btf](../zensight-btf/) | BTF/CO-RE struct offsets for the eBPF collectors |
| [zensight-rerun](../zensight-rerun/) | rerun evaluation harness (`publish = false`, off by default — see [plans/rerun/DECISION.md](plans/rerun/DECISION.md)) |
| [packaging](../packaging/) | the two unit forms, and the table `scripts/packaging-check.sh` keeps true |
| [zblob](https://github.com/p13marc/zblob) | graduated external repo (was in-tree `zenoh-blob/`) |

## Quick start

```bash
cargo build --release --workspace
just run          # GUI + local sensors (sysinfo, netlink, netring, logs/journald,
                  # systemd, hostspec) — nine processes including the router and correlator
just netring      # one sensor: netring | netlink | sysinfo | logs

just demo-prometheus   # sensors + exporter + Prometheus + Grafana  (demo/README.md)
just demo-otel         # sensors + exporter + grafana/otel-lgtm     (demo/README.md)
just demo-verify       # prove sensor -> exporter -> /metrics, no containers
```

See the top-level [`README.md`](../README.md) and [`CLAUDE.md`](../CLAUDE.md) for the project
overview and build/test/lint commands.
