# ZenSight

Fleet monitoring for people who own the fleet: **one [Zenoh](https://zenoh.io/) bus**
carrying telemetry, control, media, bulk transfer and desired state, fed by read-only
protocol sensors that publish instead of waiting to be scraped. The correlator fuses their
identity evidence into **one entity per host**, resolves the relationships between those
hosts into a graph, and an incident says what it is a *symptom of* rather than paging six
times for one dead hypervisor. Long-range analytics, dashboards and paging stay in
Prometheus / Grafana / Alertmanager, through the exporters that ship with it.

Built for a Proxmox host and six 1–2 GB VMs on links you do not control; published because
that shape is common. **What it is for, who should run it, and what it deliberately is
not: [docs/POSITIONING.md](docs/POSITIONING.md).**

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
| [`zensight-store`](zensight-store/) | Tiered time-series store — hot ring + redb minute/hour tiers, logs/events/chunks; shared by the GUI and the historian |
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
| [`zensight-sensor-hostspec`](zensight-sensor-hostspec/) | machine-checked desired-state assertions (mounts/files/listeners/symlinks/content/perms) — read-only sentinel, executes nothing |
| [`zensight-sensor-container`](zensight-sensor-container/) | the whole workload on a Quadlet fleet: per-container memory/OOM/restart, image digest, healthcheck state including the never-ran case; read-only, no action surface |
| [`zensight-sensor-probe`](zensight-sensor-probe/) | the outside-in view: HTTP/TLS/DNS/TCP checks and local certificate expiry, with a timeout as its own outcome and the vantage point on every result |
| [`zensight-sensor-pve`](zensight-sensor-pve/) | the hypervisor as a hypervisor — guest `onboot`/firewall config, thin-pool over-commitment, vzdump outcomes and size trend; read-only, no action surface |
| [`zensight-sensor-bmc`](zensight-sensor-bmc/) | out-of-band hardware health over Redfish — power supplies, fans, thermal, chassis rollup; the only view of a physical fault the host sensors never see. Read-only, no action surface |
| [`zensight-correlator`](zensight-correlator/) | Fuses identity evidence → one `HostEntity` per host |
| [`zensight-historian`](zensight-historian/) | Durable fleet telemetry history — ingests `v1/*/telemetry/**` into the store's tiers under a resource budget, serves bounded range queries |
| [`zensight-desired`](zensight-desired/) | The fleet policy compiler — one `fleet-policy.json5` in, the per-host `@desired` documents every sensor reconciles out |
| [`zensight-exporter-prometheus`](zensight-exporter-prometheus/) | Prometheus `/metrics` + remote-write |
| [`zensight-exporter-otel`](zensight-exporter-otel/) | OpenTelemetry OTLP metrics/logs/traces |
| [`zensight-sensor-parallax`](zensight-sensor-parallax/) | live video (V4L2/RTSP/test pattern) → H.264 + JPEG previews on `@media` |
| [`zensight-conformance`](zensight-conformance/) | CI harness — stands a deployment up and runs `zenkey-fleet`'s RFC judges against the live bus |
| [`zblob`](https://github.com/p13marc/zblob) | Resumable content-addressed large-data transfer over Zenoh (external crate) |

## Key expressions

Everything rides the ratified **keyspace-v2 convention** (v1). `zensight` is the
session namespace, not a key chunk; `<origin>` is `h-<12hex>`, derived from the host's
machine-id.

```
zensight/v1/<origin>/<class>/<producer>/<subject...>     data planes
zensight/v1/<origin>/@rpc/<producer>/<procedure...>      request/reply
zensight/v1/<origin>/@media/<producer>/<stream>/…        opaque video
zensight/v1/<origin>/@blob/{artifact,tree,store}/…       bulk content
zensight/v1/@catalog/…                                   the identity catalog

  zensight/v1/h-9706b31ddad3/telemetry/sysinfo/cpu/usage
  zensight/v1/h-9706b31ddad3/state/netlink/health
  zensight/v1/h-9706b31ddad3/@rpc/netlink/sockets
```

`<class>` is `telemetry` (periodic samples), `state` (LWW documents), or `events`
(append-only). Commands are `@rpc` GETs, not publications. Never `format!` a key —
use the typed builders (`zensight_common::registry`, over the external
[`zenkey`](https://github.com/p13marc/zenkey) grammar crate).

The deployed-profile summary is [`docs/KEYSPACE.md`](docs/KEYSPACE.md); the normative
spec lives in [the zenkey repo](https://github.com/p13marc/zenkey/blob/main/rfcs/00-index.md). The machine-readable
truth is [`zensight-common/registry/*.toml`](zensight-common/registry/) — or ask a
running build: `zenctl topic list`.

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
just sensors                         # just the local sensors (Ctrl-C stops them)
just sensors connect=tcp/<gui-host>:7447   # …feeding a GUI on another machine

# Or export the bus into real dashboards, one command each (see demo/README.md):
just demo-prometheus   # sensors + exporter + Prometheus + Grafana, provisioned
just demo-otel         # sensors + exporter + grafana/otel-lgtm (Prometheus/Tempo/Loki)

# One sensor at a time
just netring   # | netlink | sysinfo | logs | systemd | parallax
```

`just run` / `just gui` build the GUI with H.264 live video for parallax streams
(openh264 compiled from source → a C++ compiler is required); a plain
`cargo build --workspace` stays codec-free (JPEG previews only).

`just sensors` spawns six sensors — sysinfo, netlink, netring, logs, systemd, hostspec — plus
**parallax if its binary has been built** (it is skipped otherwise).

parallax **is packaged** as of #512: the release tarball carries its binary
alongside the others, and it has its own component image
(`git.marcpardo.eu/marcpardo/zensight-sensor-parallax`). It is deliberately
**not** in the all-in-one `zensight-sensors` bundle — it is the only component
that compiles openh264 from C++ source, and the only one that needs
`/dev/video*` access, so folding it in would put a C++ toolchain dependency and
a device grant on every host that just wants the six host sensors.

Its hardened systemd unit is `packaging/systemd/zensight-sensor-parallax.service`
(#411); it needs `SupplementaryGroups=video` and a `DeviceAllow` for
`/dev/video*` — see `packaging/systemd/README.md`.

To monitor **multiple machines**, run the GUI (+ correlator) on one host and the
all-in-one sensors container (`git.marcpardo.eu/marcpardo/zensight-sensors`) on each of the
others — the only configuration is `ZENSIGHT_ZENOH_CONNECT=tcp/<gui-host>:7447`.
See [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md).

`just run` pins an explicit loopback rendezvous (the GUI listens on `tcp/127.0.0.1:7447`;
sensors connect to it) so the pieces always find each other **without** relying on multicast
peer discovery — which is unreliable on hosts with a VPN or extra interfaces (tailscale,
docker, …). Since the endpoints are pinned, `just run` also sets `ZENSIGHT_ZENOH_SCOUTING=false`
to turn multicast off; that silences the loopback `CONNECTION_TO_SELF` / "transport to itself"
warnings Zenoh otherwise logs (gossip stays on, so the correlator still finds the sensors). To
target specific endpoints, set `ZENSIGHT_ZENOH_CONNECT`, `ZENSIGHT_ZENOH_LISTEN`, or
`ZENSIGHT_ZENOH_MODE` (comma-separated), which override the config. A **client-mode** process
with explicit `connect` endpoints turns both multicast and gossip scouting off by default —
it dials its router and never needs discovery (#626); `zenoh.scouting`/`zenoh.gossip` (or
`ZENSIGHT_ZENOH_SCOUTING`/`ZENSIGHT_ZENOH_GOSSIP`) override explicitly.

> **Seeing no data in the GUI?** It's almost always discovery — the GUI and sensors didn't
> form a Zenoh session. `just run` fixes this; if you launch pieces by hand, give them matching
> `connect`/`listen` endpoints (or the env vars above) instead of bare `peer` mode.

### Install from a release

Two packaged paths (deb/rpm packaging was retired with the move to Forgejo releases):

**Containers** — every sensor/exporter/correlator as a per-component image, plus the
all-in-one `zensight-sensors` bundle (see [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md)):

```bash
sudo podman pull git.marcpardo.eu/marcpardo/zensight-sensors:latest
```

**Native binaries** — each release ships `zensight-<ver>-linux-amd64.tar.gz` with all
binaries, the hardened systemd units, and example configs. For hosts where containers
are not wanted (e.g. a hypervisor):

```bash
tar xf zensight-<ver>-linux-amd64.tar.gz && cd zensight-<ver>-linux-amd64
sha256sum -c SHA256SUMS
sudo install -m 755 zensight-sensor-sysinfo /usr/local/bin/
sudo install -m 644 systemd/zensight-sensor-sysinfo.service /etc/systemd/system/  # every unit says /usr/bin: adjust ExecStart, or install to /usr/bin
sudo install -D -m 644 configs/sysinfo.json5 /etc/zensight/sysinfo.json5          # point it at your Zenoh router
sudo systemctl enable --now zensight-sensor-sysinfo
```

See [`packaging/systemd/README.md`](packaging/systemd/README.md) for per-unit privileges (most
run unprivileged under a transient `DynamicUser`).

## Configuration

All sensors and exporters use JSON5 configs; see [`configs/`](configs/) for a working example
per crate. Each shares a `zenoh` block (`mode`/`connect`/`listen`, plus an optional `tls`
block — CA / client cert / key / `enable_mtls` — for `tls/…` endpoints to a TLS or mTLS
router) and a `logging` block, all overridable via the `ZENSIGHT_ZENOH_*` env vars
(`ZENSIGHT_ZENOH_TLS_{CA,CERT,KEY,MTLS}` for the TLS material — see
[`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md) §TLS). The per-field reference for each crate lives
in that crate's `docs/`.

## Data model

Every sensor emits a common `TelemetryPoint` (full model in
[`zensight-common/docs/data-model.md`](zensight-common/docs/data-model.md)):

```rust
pub struct TelemetryPoint {
    pub timestamp: i64,          // Unix epoch milliseconds
    pub source: String,          // device/host identifier
    pub protocol: Protocol,      // snmp, logs, netflow, modbus, sysinfo, gnmi, netlink, netring, systemd,
                                 // hostspec, pve, container, probe, parallax (+ opcua, reserved)
    pub metric: String,          // metric name/path
    pub value: TelemetryValue,   // Counter | Gauge | Text | Boolean | Binary
    pub labels: HashMap<String, String>,
}
```

## Documentation

- **[docs/](docs/)** — cross-cutting references:
  [POSITIONING](docs/POSITIONING.md) (what it is for, who runs it, what it is not) ·
  [COMPATIBILITY](docs/COMPATIBILITY.md) (what is stable before 1.0, what may break) ·
  [ARCHITECTURE](docs/ARCHITECTURE.md) (system overview, data flow, lifecycle) ·
  [KEYSPACE](docs/KEYSPACE.md) (the canonical Zenoh key contract) ·
  [DEPLOYMENT](docs/DEPLOYMENT.md) (running it on a fleet) ·
  [design/](docs/design/) (archived design rationale).
- **Per-crate docs** — each crate's `README.md` + `docs/` is the authoritative reference for
  that crate (see the Components table above).

## Development

```bash
cargo test --workspace          # all tests
cargo test -p <crate>           # one crate (gnmi needs protoc; systemd needs systemd-devel)
cargo fmt --all                 # CI enforces rustfmt + clippy -D warnings + a design-system color guard
cargo clippy --workspace
```

## License

MIT — see [LICENSE](LICENSE).
