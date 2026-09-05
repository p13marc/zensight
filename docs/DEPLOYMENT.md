# Deploying ZenSight on multiple machines

One machine runs the **GUI** (and the identity **correlator**); every machine
you want to monitor runs one **sensors container** — the five host sensors
`just run` spawns locally (sysinfo, netlink, netring, logs, systemd, hostspec) with the
same demo-max defaults, bundled into a single image. The only thing you
configure is the Zenoh endpoint the sensors connect to.

Once it runs, [**§6**](#6-day-two-one-policy-file-instead-of-eighteen) is the
part that keeps it maintainable: one `fleet-policy.json5`, compiled against the
catalog, instead of editing thresholds and expectations on every host.

> The parallax live-video sensor is **not** in this bundle image, on purpose —
> but it is packaged (#512). It has its own component image,
> `git.marcpardo.eu/marcpardo/zensight-sensor-parallax`, and its binary ships in
> the release tarball with the rest.
>
> It stays out of the bundle because it is the only component that is not pure
> Rust (openh264 is compiled from C++ source, so it is the only one linking
> `libstdc++`) and the only one that needs `/dev/video*`. Folding it in would
> put both on every host that wants the six host sensors and no camera. Run it
> where there is a camera, from its own image or as a system service
> (`packaging/systemd/zensight-sensor-parallax.service`, #411 — note
> `SupplementaryGroups=video` and the `DeviceAllow`).

```
┌─ GUI machine ──────────────────┐        ┌─ monitored machine (×N) ────────┐
│ just gui listen=tcp/0.0.0.0:7447│◀──────│ podman: zensight-sensors        │
│ just correlator                 │  tcp  │   sysinfo · netlink · netring   │
│                                 │ 7447  │   logs · systemd · hostspec     │
└─────────────────────────────────┘        └─────────────────────────────────┘
```

Every key is scoped by **origin** (`zensight/v1/<origin>/<class>/<producer>/…`,
where `<origin>` is `h-<12hex>` derived from the machine's `/etc/machine-id`), so any
number of machines coexist on the bus and the GUI shows one sensor card, one host
entity, and one topology node per machine. The origin is the *host's*, not the
container's — which is why each container mounts `/etc/machine-id` read-only. See
[`KEYSPACE.md`](KEYSPACE.md).

> **Mount `/etc/machine-id` or the deployment breaks silently.** Without it each
> container start mints a fresh random origin, and the catalog fills with ghost hosts
> that never resolve. `docker/docker-compose.yml` does this for you.

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
sudo podman pull git.marcpardo.eu/marcpardo/zensight-sensors:latest

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
  git.marcpardo.eu/marcpardo/zensight-sensors:latest
```

`ZENSIGHT_ZENOH_CONNECT` is the **only required knob** — the container exits
immediately with a usage message without it. (For a single-host demo with
`--net=host`, `tcp/127.0.0.1:7447` reaches a local `just gui`.)
`ZENSIGHT_ZENOH_NAMESPACE` also passes through and is supported: it sets the
deployment base (empty by default — no session namespace), for isolating
several deployments on one Zenoh infrastructure; it must match the rest of the
fleet. `ZENSIGHT_ZENOH_MODE`/`ZENSIGHT_ZENOH_LISTEN` technically pass through
to the sensors but are unsupported for this image.

`ZENSIGHT_PROFILE` selects the generated config profile (see
`scripts/gen-configs.sh`): the default **`production`** keeps telemetry,
health and actionable ops alerts on but leaves the whole anomaly/security
detector suite (netring beaconing/RITA, DNS tunnelling, NOD, DGA, exfil,
encrypted-DNS bypass, connection floods) and the log error-budget alerts at
their shipped default, off — those are false-positive noise on most real
fleets. Set `ZENSIGHT_PROFILE=demo-max` to light up the entire feature
surface (what `just run` demos locally).

### TLS to the router

When the fleet publishes to a zenohd router behind TLS (or mutual TLS), give
the container the certificate material and a `tls/` endpoint:

| Variable | Meaning |
|----------|---------|
| `ZENSIGHT_ZENOH_TLS_CA` | CA certificate (PEM) that signed the router's cert |
| `ZENSIGHT_ZENOH_TLS_CERT` | this host's client certificate (mTLS only) |
| `ZENSIGHT_ZENOH_TLS_KEY` | private key for the client certificate (mTLS only) |
| `ZENSIGHT_ZENOH_TLS_MTLS` | `true` to present the client certificate |

```bash
sudo podman run -d --name zensight-sensors \
  ... \
  -v /srv/containers/zensight-sensors/tls:/etc/zensight/tls:ro \
  -e ZENSIGHT_ZENOH_CONNECT=tls/<router-host>:7447 \
  -e ZENSIGHT_ZENOH_TLS_CA=/etc/zensight/tls/ca.crt \
  -e ZENSIGHT_ZENOH_TLS_CERT=/etc/zensight/tls/host.crt \
  -e ZENSIGHT_ZENOH_TLS_KEY=/etc/zensight/tls/host.key \
  -e ZENSIGHT_ZENOH_TLS_MTLS=true \
  ...
```

The connect endpoint's scheme must be `tls/` — TLS material with a plain
`tcp/` endpoint is loaded but never used (the session logs a warning). The
entrypoint fails fast (exit 64) if a `ZENSIGHT_ZENOH_TLS_*` path is set but
the file isn't visible inside the container — that's always a forgotten
`-v` mount. The same variables work for the GUI (a flatpak install uses
`flatpak override --user --env=…` with certs under `~/.config/zensight/tls`)
and for the native sensors/correlator (whose configs also accept an
equivalent `zenoh.tls` block — see `configs/sysinfo.json5`).

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
| *(nothing)* | the hostspec sensor needs no privilege, mount or capability at all — its assertions read `/proc`, `lstat` and bounded file contents in whatever view of the filesystem the container has. NOTE that inside a container that view is the **container's**: assert host paths only where they are mounted in. |
| `-v /var/log/journal` + `/run/log/journal` (ro) | the logs sensor reads the host journal (persistent and volatile locations). |
| `--restart=on-failure` | belt-and-braces for the whole container; since #813 the entrypoint supervises each sensor individually (restart with backoff), so one sensor's crash no longer takes the set down. |

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

> **The bundle is the demo (#813).** A fleet runs ONE CONTAINER PER SENSOR —
> `packaging/quadlet/` ships a `.container` unit per sensor, each with its
> own `MemoryMax` and restart policy, against the per-sensor images every
> release already builds. The all-in-one unit below remains the one-command
> demo path; its entrypoint now supervises each sensor with restart-and-
> backoff (one crash never blanks the rest) and honours
> `ZENSIGHT_SENSORS=sysinfo,systemd,logs` for subsetting.

Drop this in `/etc/containers/systemd/zensight-sensors.container`, then
`systemctl daemon-reload && systemctl start zensight-sensors`:

```ini
[Unit]
Description=ZenSight sensors (sysinfo, netlink, netring, logs, systemd, hostspec)
After=network-online.target
Wants=network-online.target

[Container]
Image=git.marcpardo.eu/marcpardo/zensight-sensors:latest
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

Per-sensor images (`git.marcpardo.eu/marcpardo/zensight-sensor-<name>`) still exist for
single-sensor deployments — see `docker/docker-compose.yml`. This bundle is
the "monitor this whole machine" path.

## 5. Exporting to Prometheus / OpenTelemetry

Two exporters forward the bus to external systems. Both are plain unprivileged
binaries that join the deployment like any other participant.

| | |
|---|---|
| Images | `git.marcpardo.eu/marcpardo/zensight-exporter-prometheus:latest`, `…-otel:latest` (built from `docker/Dockerfile.runtime`; they ship **no** config — mount one into `/etc/zensight` and pass `--config`) |
| systemd | `packaging/systemd/zensight-exporter-{prometheus,otel}.service` — `DynamicUser`, `ProtectSystem=strict`, no capabilities |
| Configs | `configs/{prometheus,otel}-exporter.json5` |
| Prometheus scrape port | **9464** (not 9090 — that is Prometheus's own port) |
| OTLP endpoint | `http://<collector>:4317` (gRPC) by default |

**The one knob that matters is discovery.** The shipped configs are
`mode: "peer"` with `connect` commented out, which means multicast — and a
deployment that pins its endpoints has multicast off. Set the endpoint
explicitly, exactly as the sensors container does:

```bash
ZENSIGHT_ZENOH_CONNECT=tcp/<gui-host>:7447 ZENSIGHT_ZENOH_SCOUTING=false \
  zensight-exporter-prometheus --config /etc/zensight/prometheus-exporter.json5
```

Get this wrong and the exporter starts, serves a healthy `/health`, exports
nothing, and logs nothing about it. `curl :9464/ready` returns **503** until the
first telemetry point arrives — that endpoint exists to tell those two states
apart.

`zenoh.namespace` must match the rest of the deployment (empty by default), or
the exporter subscribes to a keyspace nobody publishes to.

**To try it locally first**, see [`demo/README.md`](../demo/README.md):
`just demo-prometheus` brings up the exporter, the sensors, Prometheus and a
provisioned Grafana in one command.

## 6. Day two: one policy file instead of eighteen

Sections 1–5 got the fleet running. Everything above is **file config on each
host** — and the moment there are six machines, keeping thresholds and
expectations in step by editing files on all of them is the problem `@desired`
exists to solve.

`zensight-desired` is the fleet policy compiler (#938). It reads one
`fleet-policy.json5`, asks `@catalog` what hosts exist and what they are, and
publishes the per-host documents the sensors already know how to reconcile. It
runs no command, copies no file and reaches no host — the hosts converge on the
documents themselves, which is why a machine that was offline during a change
picks it up when it comes back.

**Run exactly one per deployment**, next to the correlator. `@desired` is a
single-writer service origin; two compilers with different policies would
overwrite each other every pass and every sensor would flap between them.

```bash
sudo install -m 755 zensight-desired /usr/local/bin/
sudo install -D -m 644 configs/desired.json5 /etc/zensight/desired.json5
sudo install -D -m 644 demo/fleet-policy.json5 /etc/zensight/fleet-policy.json5
sudoedit /etc/zensight/fleet-policy.json5     # this is the file to review

# Check it before it reaches anyone. Opens no session; exits 1 if invalid.
zensight-desired --config /etc/zensight/desired.json5 plan --offline

# Now with the bus: what would each host receive?
zensight-desired --config /etc/zensight/desired.json5 plan
zensight-desired --config /etc/zensight/desired.json5 render h-3fa9c2d41b7e

sudo install -m 644 packaging/systemd/zensight-desired.service /etc/systemd/system/
sudo systemctl enable --now zensight-desired
```

`plan --offline` needs no bus and exits non-zero on an invalid policy, so a
policy change can be gated in CI the way code is. Do that before the first
`apply`: a policy nobody can check before pushing is a policy checked by the
fleet.

### What the policy carries — and what stays on the host

The split is not a style preference. It is the **never-list**, and it is the
single most important constraint on this origin.

| | |
|---|---|
| **Policy** (`fleet-policy.json5`) | thresholds per producer, hostspec assertions, systemd and netlink expectations, log sentinel rules — anything an operator authors *about* what a host should look like |
| **The host's own file config** | the Zenoh block (`connect`, `listen`, `namespace`), TLS material, SNMP communities and v3 credentials, probe request headers, the PVE API token, and `desired.enabled` |

**Nothing under `@desired` may carry a secret, or anything a sensor needs to
reach the bus.** One bad desired publish must never lock the fleet out of its
own supervision — the fix would have to travel over the bus it just broke. So
endpoints, TLS and the namespace stay local, always, and credentials are
referenced **by name** into each host's file config rather than carried.

That is enforced three times over, which is deliberate rather than redundant:
the payload types have no field for a credential; the compiler runs a
never-list lint over every fragment before publishing (it tests the *value*, so
`NetlinkExpectations`' `listen: 22` — a port — passes while
`listen: "tcp/0.0.0.0:7447"` does not); and the sensor-side reconciler
deserializes only its own sentinel's config type and writes only that
sentinel's handle.

The kill switch is `desired.enabled: false` in each sensor's **file** config —
outside the mechanism it disarms.

### What lands where

A host reconciles a desired document over its file baseline and publishes
`state/<producer>/applied/<topic>` saying which of three writers won last —
`file`, `desired`, or `rpc` — what document is in force, and the most recent
one it **refused**. That marker is where to look when a policy change does not
appear to have taken:

```bash
zenctl get 'zensight/v1/*/state/*/applied/*'
```

That GET answers because the marker is **seeded** (#1034), not merely
published: a consumer that was not listening when the writer won can still ask.

A `DELETE` of a desired key reverts that host to its own config file, never to
an empty set. And the compiler deletes a document only when the policy stops
yielding it for a host the catalog **still shows**, after a grace of several
passes — one slow catalog read must not revert the whole fleet at once.

### Trying the whole loop on one machine

`just run desired=1` starts the stack with the controller in it (#941).
`just configure` copies `demo/fleet-policy.json5` into `.run/` and points the
generated `desired.json5` at that copy, so the file the daemon names is the
file you edit. It is **opt-in and stays opt-in**: this daemon writes the
desired state every sensor in the run reconciles, and starting it by default
would reconfigure the demo fleet from a policy nobody had read.

Watch it land:

```bash
just desired-plan                                    # publishes nothing
just run desired=1
zenctl get 'zensight/v1/*/state/sysinfo/applied/thresholds'   # source: desired
```

CI runs that same loop on every pull request — `scripts/demo-verify.sh` phase 4
starts a correlator, applies the shipped policy and waits for a real sensor's
marker to read `source: desired`. Compiling a policy and *publishing* one that
a host actually accepts are different claims, and only the second one is worth
making.

### Selectors read the catalog

Classes select on facts `@catalog` resolved: `host_id`, a hostname glob, a
sensor that runs there, an IP CIDR, `vendor`, `platform`. Two consequences
worth knowing before writing the file:

- **The correlator must be running.** With no catalog there are no entities, so
  the compiler has no fleet, publishes nothing, and after the grace deletes
  what it published. Run it alongside.
- **`platform` selectors glob.** The value is `<ID>-<VERSION_ID>` —
  `debian-13`, `proxmox-13` — so a class writes `debian-*`. An exact
  `debian-13` silently stops matching after a point-release upgrade: nothing is
  broken, no error is raised, the host simply stops receiving configuration.

The file format, the overlay rules and the reasoning behind each are in
[`zensight-desired/docs/policy.md`](../zensight-desired/docs/policy.md).

## 7. Verifying

On the GUI machine, after starting a container on another host you should see:

- **Sensors** view: five new cards labelled `<sensor> @ <that-hostname>`.
- **Inventory / Topology**: one new host entity/node (needs the correlator).
- `podman logs zensight-sensors` on the monitored machine: the detected
  capture interface, no preflight WARNs (if all mounts were given), and
  per-sensor startup lines.
