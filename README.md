# ZenSight

A unified observability platform that sensors legacy monitoring protocols into
[Zenoh](https://zenoh.io/)'s pub/sub infrastructure.

## Overview

ZenSight is a suite of protocol **sensors** that collect telemetry from many sources and
publish it to Zenoh under one data model, a desktop **frontend** that visualizes it in real
time, a **correlator** that fuses per-sensor identity evidence into one entity per host, and
**exporters** that forward everything to Prometheus / OpenTelemetry. Everything is
auto-discovered over Zenoh — add a sensor and it shows up.

## Components

Every crate documents itself in its own `README.md` + `docs/` directory; this table links to
each. The canonical cross-cutting references live in [`docs/`](docs/).

| Crate | Role |
|-------|------|
| [`zensight`](zensight/) | Iced 0.14 desktop frontend (host/incident-centric viewer) |
| [`zensight-common`](zensight-common/) | Shared model — telemetry, alert/command, identity/evidence/entity, artifact, QoS, keyspace helpers |
| [`zensight-sensor-core`](zensight-sensor-core/) | Shared sensor framework — runner, publishers, health, alerting, identity, artifacts |
| [`zensight-sensor-snmp`](zensight-sensor-snmp/) | SNMP v1/v2c/v3 polling + trap receiver |
| [`zensight-sensor-logs`](zensight-sensor-logs/) | Network syslog (RFC 3164/5424, UDP/TCP/Unix) + systemd journald |
| [`zensight-sensor-netflow`](zensight-sensor-netflow/) | NetFlow / IPFIX receiver (v5/v7/v9/IPFIX) |
| [`zensight-sensor-modbus`](zensight-sensor-modbus/) | Modbus TCP/RTU register polling |
| [`zensight-sensor-sysinfo`](zensight-sensor-sysinfo/) | Host USE metrics + saturation + threshold alerts |
| [`zensight-sensor-gnmi`](zensight-sensor-gnmi/) | gNMI streaming telemetry (gRPC) |
| [`zensight-sensor-netlink`](zensight-sensor-netlink/) | Linux kernel networking (RTNETLINK/sock_diag) + sentinel |
| [`zensight-sensor-netring`](zensight-sensor-netring/) | Wire-level flow/L7/NDR (AF_PACKET/AF_XDP or pcap) + detectors |
| [`zensight-sensor-systemd`](zensight-sensor-systemd/) | systemd unit/service/boot telemetry (D-Bus) + sentinel + gated actions |
| [`zensight-correlator`](zensight-correlator/) | Fuses identity evidence → one `HostEntity` per host |
| [`zensight-exporter-prometheus`](zensight-exporter-prometheus/) | Prometheus `/metrics` + remote-write |
| [`zensight-exporter-otel`](zensight-exporter-otel/) | OpenTelemetry OTLP metrics/logs/traces |
| [`zenoh-blob`](zenoh-blob/) | Resumable content-addressed large-data transfer over Zenoh |

## Key expressions

All sensors publish under a unified `zensight/` prefix. The full tree — telemetry,
control-plane (`@/…`), metadata (`_meta/…`), media (`@media/…`), wildcards, and the
key-builder helpers — is the authoritative contract in [`docs/KEYSPACE.md`](docs/KEYSPACE.md).

```
zensight/<protocol>/<source>/<metric>
  zensight/snmp/router01/system/sysUpTime
  zensight/logs/server01/daemon/warning
  zensight/sysinfo/server01/cpu/usage
  zensight/netring/host01/flow/red/p95_ms
```

## Quick start

```bash
# Build everything (release)
cargo build --release --workspace

# Build + configure + launch the GUI with the local sensors (netring, netlink,
# sysinfo, logs/journald, systemd, parallax live video). Close the GUI to stop
# everything.
just run

# Or split the two halves:
just gui listen=tcp/0.0.0.0:7447     # just the GUI (non-loopback for remote sensors)
just sensors                         # just the 6 sensors (Ctrl-C stops them)
just sensors connect=tcp/<gui-host>:7447   # …feeding a GUI on another machine

# One sensor at a time
just netring   # | netlink | sysinfo | logs | systemd | parallax
```

`just run` / `just gui` build the GUI with H.264 live video for parallax streams
(openh264 compiled from source → a C++ compiler is required); a plain
`cargo build --workspace` stays codec-free (JPEG previews only).

To monitor **multiple machines**, run the GUI (+ correlator) on one host and the
all-in-one sensors container (`ghcr.io/p13marc/zensight-sensors`) on each of the
others — the only configuration is `ZENSIGHT_ZENOH_CONNECT=tcp/<gui-host>:7447`.
See [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md).

`just run` pins an explicit loopback rendezvous (the GUI listens on `tcp/127.0.0.1:7447`;
sensors connect to it) so the pieces always find each other **without** relying on multicast
peer discovery — which is unreliable on hosts with a VPN or extra interfaces (tailscale,
docker, …). Since the endpoints are pinned, `just run` also sets `ZENSIGHT_ZENOH_SCOUTING=false`
to turn multicast off; that silences the loopback `CONNECTION_TO_SELF` / "transport to itself"
warnings Zenoh otherwise logs (gossip stays on, so the correlator still finds the sensors). To
target specific endpoints, set `ZENSIGHT_ZENOH_CONNECT`, `ZENSIGHT_ZENOH_LISTEN`, or
`ZENSIGHT_ZENOH_MODE` (comma-separated), which override the config.

> **Seeing no data in the GUI?** It's almost always discovery — the GUI and sensors didn't
> form a Zenoh session. `just run` fixes this; if you launch pieces by hand, give them matching
> `connect`/`listen` endpoints (or the env vars above) instead of bare `peer` mode.

### Install from packages

Each sensor and exporter ships a `.deb` / `.rpm` (built by the release workflow) that installs
the binary, a hardened **systemd unit**, and an example config to `/etc/zensight/<name>.json5`
(a conf-file, so edits survive upgrades). Units are not enabled automatically:

```bash
sudo dpkg -i zensight-sensor-sysinfo_*.deb          # or: rpm -i …
sudoedit /etc/zensight/sysinfo.json5                 # point it at your Zenoh router
sudo systemctl enable --now zensight-sensor-sysinfo
```

See [`packaging/systemd/README.md`](packaging/systemd/README.md) for per-unit privileges (most
run unprivileged under a transient `DynamicUser`).

## Configuration

All sensors and exporters use JSON5 configs; see [`configs/`](configs/) for a working example
per crate. Each shares a `zenoh` block (`mode`/`connect`/`listen`) and a `logging` block, all
overridable via the `ZENSIGHT_ZENOH_*` env vars. The per-field reference for each crate lives
in that crate's `docs/`.

## Data model

Every sensor emits a common `TelemetryPoint` (full model in
[`zensight-common/docs/data-model.md`](zensight-common/docs/data-model.md)):

```rust
pub struct TelemetryPoint {
    pub timestamp: i64,          // Unix epoch milliseconds
    pub source: String,          // device/host identifier
    pub protocol: Protocol,      // snmp, logs, netflow, modbus, sysinfo, gnmi, netlink, netring, systemd
    pub metric: String,          // metric name/path
    pub value: TelemetryValue,   // Counter | Gauge | Text | Boolean | Binary
    pub labels: HashMap<String, String>,
}
```

## Documentation

- **[docs/](docs/)** — cross-cutting references:
  [ARCHITECTURE](docs/ARCHITECTURE.md) (system overview, data flow, lifecycle) ·
  [KEYSPACE](docs/KEYSPACE.md) (the canonical Zenoh key contract) ·
  [design/](docs/design/) (archived design rationale).
- **Per-crate docs** — each crate's `README.md` + `docs/` is the authoritative reference for
  that crate (see the Components table above).

## Development

```bash
cargo test --workspace          # all tests
cargo test -p <crate>           # one crate (snmp/gnmi need openssl-devel/protoc; systemd needs systemd-devel)
cargo fmt --all                 # CI enforces rustfmt + clippy -D warnings + a design-system color guard
cargo clippy --workspace
```

## License

MIT — see [LICENSE](LICENSE).
