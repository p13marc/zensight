# ZenSight — build / configure / run the GUI + sensors + correlator
#   (netring, netlink, sysinfo, logs, systemd, parallax + the identity correlator)
#
#   just run            # build, grant caps, configure, then launch everything
#   just demo           # run the GUI in demo mode (simulated data, no sensors)
#   just setup          # build + grant capabilities only
#   just gui            # run just the GUI    (just gui listen=tcp/0.0.0.0:7447 for remote sensors)
#   just sensors        # run just the 6 sensors, no GUI/correlator (Ctrl-C stops them)
#                       # (just sensors connect=tcp/<gui-host>:7447 to feed a remote GUI)
#   just <name>         # run one piece (netring | netlink | sysinfo | logs | systemd | parallax | correlator)
#   just rerun          # optional Rerun sidecar (evaluation, epic #415) — see the recipe
#
# `just run` is the live demo: `configure` writes *demo-max* configs into .run/
# (via scripts/gen-configs.sh — also used by the sensors container image) with
# the opt-in collectors, anomaly detectors and on-demand artifacts (report /
# snapshot / pcap capture) turned ON, and starts the correlator so the GUI shows
# fused host identities. Build-feature-gated detectors (ja4plus/lateral/sigma/
# snmp/ebpf) and privileged systemd unit control stay off.
#
# Multi-machine deployment (one GUI, sensors on N hosts) is containerized —
# see docs/DEPLOYMENT.md and docker/Dockerfile.sensors.
#
# netring captures packets and needs CAP_NET_RAW (+CAP_IPC_LOCK for AF_XDP);
# netlink's optional collectors (nftables/conntrack + the XFRM monitor) need
# CAP_NET_ADMIN. `just caps` grants both via sudo. sysinfo is unprivileged.
# parallax is unprivileged for the demo: it streams a synthetic test pattern
# (video tiles in the GUI's parallax device view) on any machine; real
# /dev/video* cameras additionally need your user in the `video` group.
# logs ingests the systemd journal (journald); reading the *system* journal needs
# journal-read access — add your user to the `systemd-journal` group if it can't.
# systemd reads the org.freedesktop.systemd1 D-Bus (system bus) read-only and is
# unprivileged; the demo config enables everything *except* gated service control
# (`actions`), which is left off because it stops/restarts real units.

# Build profile: "release" (default) or "dev".
profile := "release"

# Network interface for netring capture (defaults to the default-route iface).
iface := `ip route show default 2>/dev/null | awk '{print $5; exit}' | grep -m1 . || ip -o link show up 2>/dev/null | awk -F': ' '$2 != "lo" {print $2; exit}' | grep -m1 . || echo lo`

# Derived: where cargo puts the binaries, and the --release flag.
bindir := if profile == "release" { "target/release" } else { "target/debug" }
relflag := if profile == "release" { "--release" } else { "" }

# Run configs are generated here (gitignored), so committed examples stay clean.
rundir := ".run"

# Local Zenoh rendezvous: the GUI listens here and sensors connect to it, so the
# pieces always find each other on loopback without relying on multicast peer
# discovery (which is unreliable on hosts with a VPN or extra interfaces, e.g.
# tailscale/docker). Honored via the ZENSIGHT_ZENOH_* env vars.
hub := "tcp/127.0.0.1:7447"

_default:
    @just --list

# ── Build ────────────────────────────────────────────────────────────────────

# Build the GUI + the sensors + the identity correlator.
build:
    cargo build {{relflag}} \
        -p zensight \
        -p zensight-sensor-netring \
        -p zensight-sensor-netlink \
        -p zensight-sensor-sysinfo \
        -p zensight-sensor-logs \
        -p zensight-sensor-systemd \
        -p zensight-sensor-parallax \
        -p zensight-correlator

# ── Capabilities ─────────────────────────────────────────────────────────────

# Grant capture/admin capabilities via sudo (re-run after each rebuild):
#   netring → CAP_NET_RAW,CAP_IPC_LOCK  (AF_PACKET/AF_XDP capture)
#   netlink → CAP_NET_ADMIN             (optional nftables/conntrack + XFRM monitor)
# netlink's baseline reads work without this; the cap only unlocks the extras.
# (eBPF additionally needs a `--features ebpf` build + CAP_BPF/CAP_PERFMON.)
caps: build
    @echo "Granting CAP_NET_RAW,CAP_IPC_LOCK to {{bindir}}/zensight-sensor-netring (sudo)…"
    sudo setcap 'cap_net_raw,cap_ipc_lock=+ep' {{bindir}}/zensight-sensor-netring
    @echo "Granting CAP_NET_ADMIN to {{bindir}}/zensight-sensor-netlink (sudo)…"
    sudo setcap 'cap_net_admin=+ep' {{bindir}}/zensight-sensor-netlink
    @echo "sysinfo + logs + parallax need no capabilities."

# Build + grant capabilities.
setup: build caps

# ── Configure ────────────────────────────────────────────────────────────────

# Generate run configs in {{rundir}} (netring capture interface = {{iface}}).
# The demo-max profile itself lives in scripts/gen-configs.sh, shared with the
# sensors container image — edit it there.
configure:
    scripts/gen-configs.sh --iface "{{iface}}" --outdir "{{rundir}}" \
        --configs-dir "{{justfile_directory()}}/configs" \
        --snapshot-dir "{{justfile_directory()}}/docs"

# ── Run (individual) ─────────────────────────────────────────────────────────

# Run the desktop GUI.
# The GUI listens on the hub so separately-run sensors can connect. For
# sensors on OTHER machines, listen on all interfaces:
#   just gui listen=tcp/0.0.0.0:7447
gui listen=hub: build
    ZENSIGHT_ZENOH_LISTEN="{{listen}}" {{bindir}}/zensight

# A built-in simulator feeds realistic telemetry, health, liveness and anomaly
# alerts for every sensor type — no real sensors, capabilities or Zenoh hub.
# Run the GUI in demo mode (great for a quick look at the UI).
demo: build
    {{bindir}}/zensight --demo

# Run the netring sensor (wire flows + anomaly alerts).
netring: caps configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-netring --config {{rundir}}/netring.json5

# Run the netlink sensor (kernel interfaces/sockets + expectation alerts).
netlink: caps configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-netlink --config {{rundir}}/netlink.json5

# Run the sysinfo sensor (CPU/memory/disk/network).
sysinfo: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-sysinfo --config {{rundir}}/sysinfo.json5

# Run the logs sensor (systemd journal via journald + known-event alerts).
logs: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-logs --config {{rundir}}/logs.json5

# Run the systemd sensor (unit/boot telemetry + threshold alerts + sentinel).
systemd: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-systemd --config {{rundir}}/systemd.json5

# Run the parallax sensor (live video: synthetic test pattern + local cameras).
# Open the parallax device in the GUI and "Load streams" → preview tiles.
parallax: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-parallax --config {{rundir}}/parallax.json5

# Run the identity correlator (fuses sensor evidence into one HostEntity per host).
correlator: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-correlator --config {{rundir}}/correlator.json5

# Optional Rerun sidecar (evaluation prototype, epic #415) — NOT part of `just run`.
# Feeds the live bus into Rerun; built on demand (pulls the arrow/tonic stack).
#   just rerun                # live → viewer at rerun+http://127.0.0.1:9876/proxy
#                             #   (start the viewer first: `rerun`)
#   just rerun mode=record    # headless → {{rundir}}/zensight.rrd (replay later)
rerun mode="live":
    cargo build {{relflag}} -p zensight-rerun
    mkdir -p {{rundir}}
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-rerun --config configs/rerun.json5 \
        --mode {{mode}} --rrd-path {{rundir}}/zensight.rrd

# ── Run (everything) ─────────────────────────────────────────────────────────

# Run the 6 sensors in the foreground, no GUI/correlator (Ctrl-C stops them).
# Point them at a remote GUI with: just sensors connect=tcp/<gui-host>:7447
sensors connect=hub: setup configure
    BINDIR="{{bindir}}" CONFDIR="{{rundir}}" LOGDIR="{{rundir}}" \
    CONNECT="{{connect}}" scripts/run-sensors.sh

# Build + caps + configure, then launch the sensors + GUI (close GUI to stop all).
run: setup configure
    #!/usr/bin/env bash
    set -euo pipefail
    # Sensors + correlator via the shared spawner (same process group, so the
    # trap below reaps them when the GUI exits or on Ctrl-C). They connect to
    # the GUI's loopback rendezvous (no multicast needed); logs in {{rundir}}/.
    BINDIR="{{bindir}}" CONFDIR="{{rundir}}" LOGDIR="{{rundir}}" \
    CONNECT="{{hub}}" WITH_CORRELATOR=1 scripts/run-sensors.sh &
    # Stop all sensors when the GUI exits (or on Ctrl-C).
    trap 'echo; echo "Stopping sensors…"; kill 0' EXIT
    sleep 1
    echo "Launching GUI (listening on {{hub}}; close it to stop everything)…"
    echo "GUI log → {{rundir}}/gui.log"
    # Capture the GUI's logs to a file we can inspect afterward, while still
    # echoing to the terminal. Override verbosity with RUST_LOG if needed.
    export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
    export RUST_LOG="${RUST_LOG:-info}"
    ZENSIGHT_ZENOH_LISTEN="{{hub}}" {{bindir}}/zensight 2>&1 | tee {{rundir}}/gui.log

# ── Container image ──────────────────────────────────────────────────────────

# Build the all-in-one sensors image (see docs/DEPLOYMENT.md for running it).
image:
    podman build -t zensight-sensors -f docker/Dockerfile.sensors .

# Stop any running sensors + correlator started by `just run`.
stop:
    -pkill -f 'zensight-sensor-(netring|netlink|sysinfo|logs|systemd|parallax)' || true
    -pkill -f 'zensight-correlator' || true
    -pkill -f 'zensight-rerun' || true

# Remove generated run configs and logs.
clean-run:
    rm -rf {{rundir}}
