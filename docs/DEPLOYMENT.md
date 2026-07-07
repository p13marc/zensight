# Deploying ZenSight on multiple machines

One machine runs the **GUI** (and the identity **correlator**); every machine
you want to monitor runs one **sensors container** — the same 5 host sensors
`just run` spawns locally (sysinfo, netlink, netring, logs, systemd) with the
same demo-max defaults, bundled into a single image. The only thing you
configure is the Zenoh endpoint the sensors connect to.

```
┌─ GUI machine ──────────────────┐        ┌─ monitored machine (×N) ────────┐
│ just gui listen=tcp/0.0.0.0:7447│◀──────│ podman: zensight-sensors        │
│ just correlator                 │  tcp  │   sysinfo · netlink · netring   │
│                                 │ 7447  │   logs · systemd                │
└─────────────────────────────────┘        └─────────────────────────────────┘
```

Telemetry and per-instance state keys are host-scoped
(`zensight/<protocol>/<source>/…` with `<source>` = the machine's hostname),
so any number of machines coexist on the bus and the GUI shows one sensor
card, one host entity, and one topology node per machine. See
[`KEYSPACE.md`](KEYSPACE.md).

## 1. GUI machine

```bash
just gui listen=tcp/0.0.0.0:7447    # listen on all interfaces, not loopback
just correlator                     # once per deployment, fuses host identities
```

The correlator is deliberately **not** in the sensors image: it must run
exactly once (it is the single writer of `_meta/entity/**`).

Open the firewall for TCP 7447 on this machine.

## 2. Monitored machines

Get the image (from the registry, or build it on any machine with the repo):

```bash
# from the release registry
sudo podman pull ghcr.io/p13marc/zensight-sensors:latest

# or build locally
podman build -t zensight-sensors -f docker/Dockerfile.sensors .
```

Run it (rootful podman — see [Why rootful?](#why-rootful) below):

```bash
sudo podman run -d --name zensight-sensors \
  --net=host --uts=host --pid=host \
  --cap-add=NET_RAW --cap-add=NET_ADMIN --cap-add=IPC_LOCK \
  --security-opt label=disable \
  -v /etc/machine-id:/etc/machine-id:ro \
  -v /run/dbus/system_bus_socket:/run/dbus/system_bus_socket:ro \
  -v /var/log/journal:/var/log/journal:ro \
  -v /run/log/journal:/run/log/journal:ro \
  -e ZENSIGHT_ZENOH_CONNECT=tcp/<gui-host>:7447 \
  --restart=on-failure \
  ghcr.io/p13marc/zensight-sensors:latest
```

`ZENSIGHT_ZENOH_CONNECT` is the **only supported knob** and it is required —
the container exits immediately with a usage message without it. (For a
single-host demo with `--net=host`, `tcp/127.0.0.1:7447` reaches a local
`just gui`.) `ZENSIGHT_ZENOH_MODE`/`ZENSIGHT_ZENOH_LISTEN` technically pass
through to the sensors but are unsupported for this image.

### Why each flag

| Flag | Why |
|------|-----|
| `--net=host` | netring captures with AF_PACKET on the **host's** interfaces; netlink observes the host's sockets/routes; and the sensors reach the GUI endpoint directly. The capture interface is auto-detected at container start from the host's default route. |
| `--uts=host` | the sensors' `<source>` key segment defaults to the hostname — without this every key says the container id, not the machine name. |
| `--pid=host` | sysinfo's process explorer sees the host's processes. |
| `--cap-add=NET_RAW` | netring's AF_PACKET capture. |
| `--cap-add=NET_ADMIN` | netlink's optional collectors (nftables/conntrack + the XFRM monitor). |
| `--cap-add=IPC_LOCK` | netring's AF_XDP path (locked memory). |
| `--security-opt label=disable` | SELinux hosts (Fedora & co.): the mounted host files (`/etc/machine-id`, the D-Bus socket, journal dirs) cannot be `:Z`-relabeled — they belong to the host. |
| `-v /etc/machine-id:…:ro` | `host_id` = sha256(machine-id ‖ salt) is the correlator's identity anchor. Without it, every container reports the image's (empty) machine-id and hosts can't be told apart reliably. |
| `-v /run/dbus/system_bus_socket:…:ro` | the systemd sensor reads `org.freedesktop.systemd1` on the host's system bus. |
| `-v /var/log/journal` + `/run/log/journal` (ro) | the logs sensor reads the host journal (persistent and volatile locations). |
| `--restart=on-failure` | the entrypoint is fail-fast: if any sensor dies the container exits non-zero and podman restarts the set. |

Missing mounts are **warnings, not failures** — the affected sensor idles and
the other four keep publishing. `podman logs zensight-sensors` shows the
preflight warnings and each sensor's output with a `[name]` prefix.

### Known limitations

- Disk/filesystem metrics reflect the **container's** mount table, not the
  host's full set of mounts.
- The sysinfo directory-snapshot artifact is disabled (it snapshots the
  repo's `docs/` in local runs, which doesn't exist in the image).
- The journal-read requires rootful podman (journal files are readable by
  root / the `systemd-journal` group).

### Why rootful?

Rootless podman breaks three things this container needs: AF_PACKET capture
in the host network namespace, reading the journal files' root-owned
directories, and the system D-Bus policy for `org.freedesktop.systemd1`.
Rootful + the explicit `--cap-add` list above is the conventional shape for
host-observing agents (same as node-exporter-style deployments).

## 3. Start on boot (quadlet)

Drop this in `/etc/containers/systemd/zensight-sensors.container`, then
`systemctl daemon-reload && systemctl start zensight-sensors`:

```ini
[Unit]
Description=ZenSight sensors (sysinfo, netlink, netring, logs, systemd)
After=network-online.target
Wants=network-online.target

[Container]
Image=ghcr.io/p13marc/zensight-sensors:latest
Network=host
PodmanArgs=--uts=host --pid=host
AddCapability=NET_RAW NET_ADMIN IPC_LOCK
SecurityLabelDisable=true
Volume=/etc/machine-id:/etc/machine-id:ro
Volume=/run/dbus/system_bus_socket:/run/dbus/system_bus_socket:ro
Volume=/var/log/journal:/var/log/journal:ro
Volume=/run/log/journal:/run/log/journal:ro
Environment=ZENSIGHT_ZENOH_CONNECT=tcp/<gui-host>:7447

[Service]
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

## 4. What's inside the image

- The 5 sensor binaries + the committed example configs
  (`/usr/share/zensight/configs/`).
- `gen-configs.sh` — the same demo-max profile `just configure` uses
  (single-sourced in `scripts/gen-configs.sh`): opt-in collectors, anomaly
  detectors, and on-demand artifacts ON; feature-gated detectors and
  privileged systemd unit control OFF. Configs are generated at **container
  start**, so the netring capture interface is detected in the host netns.
- `run-sensors.sh` — the shared spawner (`scripts/run-sensors.sh`) in
  fail-fast mode.

Per-sensor images (`ghcr.io/p13marc/zensight-sensor-<name>`) still exist for
single-sensor deployments — see `docker/docker-compose.yml`. This bundle is
the "monitor this whole machine" path.

## 5. Verifying

On the GUI machine, after starting a container on another host you should see:

- **Sensors** view: five new cards labelled `<sensor> @ <that-hostname>`.
- **Inventory / Topology**: one new host entity/node (needs the correlator).
- `podman logs zensight-sensors` on the monitored machine: the detected
  capture interface, no preflight WARNs (if all mounts were given), and
  per-sensor startup lines.
