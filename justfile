# ZenSight — build / configure / run the GUI + sensors (netring, netlink, sysinfo, logs)
#
#   just run            # build, grant caps, configure, then launch everything
#   just demo           # run the GUI in demo mode (simulated data, no sensors)
#   just setup          # build + grant capabilities only
#   just gui            # run just the GUI
#   just <sensor>       # run a single sensor (netring | netlink | sysinfo | logs)
#
# netring captures packets and needs CAP_NET_RAW (+CAP_IPC_LOCK for AF_XDP);
# `just caps` grants them via sudo. netlink reads and sysinfo are unprivileged.
# logs ingests the systemd journal (journald); reading the *system* journal needs
# journal-read access — add your user to the `systemd-journal` group if it can't.

# Build profile: "release" (default) or "dev".
profile := "release"

# Network interface for netring capture (defaults to the default-route iface).
iface := `ip route show default 2>/dev/null | awk '{print $5; exit}' || echo lo`

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

# Build the GUI + the sensors.
build:
    cargo build {{relflag}} \
        -p zensight \
        -p zensight-sensor-netring \
        -p zensight-sensor-netlink \
        -p zensight-sensor-sysinfo \
        -p zensight-sensor-logs

# ── Capabilities ─────────────────────────────────────────────────────────────

# Grant netring packet-capture capabilities via sudo (re-run after each rebuild).
caps: build
    @echo "Granting CAP_NET_RAW,CAP_IPC_LOCK to {{bindir}}/zensight-sensor-netring (sudo)…"
    sudo setcap 'cap_net_raw,cap_ipc_lock=+ep' {{bindir}}/zensight-sensor-netring
    @echo "netlink + sysinfo need no capabilities."

# Build + grant capabilities.
setup: build caps

# ── Configure ────────────────────────────────────────────────────────────────

# Generate run configs in {{rundir}} (netring capture interface = {{iface}}).
configure:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p {{rundir}}
    # netring: point capture at the chosen interface.
    sed -E 's#interfaces: \[[^]]*\]#interfaces: ["{{iface}}"]#' \
        configs/netring.json5 > {{rundir}}/netring.json5
    # netlink + sysinfo + logs configs are machine-agnostic (hostname auto-detected).
    cp -f configs/netlink.json5 {{rundir}}/netlink.json5
    cp -f configs/sysinfo.json5 {{rundir}}/sysinfo.json5
    cp -f configs/logs.json5 {{rundir}}/logs.json5
    echo "Configured: netring iface='{{iface}}', logs=journald  (configs in {{rundir}}/)"

# ── Run (individual) ─────────────────────────────────────────────────────────

# Run the desktop GUI.
# The GUI listens on the loopback hub so separately-run sensors can connect.
gui: build
    ZENSIGHT_ZENOH_LISTEN="{{hub}}" {{bindir}}/zensight

# A built-in simulator feeds realistic telemetry, health, liveness and anomaly
# alerts for every sensor type — no real sensors, capabilities or Zenoh hub.
# Run the GUI in demo mode (great for a quick look at the UI).
demo: build
    {{bindir}}/zensight --demo

# Run the netring sensor (wire flows + anomaly alerts).
netring: caps configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-netring --config {{rundir}}/netring.json5

# Run the netlink sensor (kernel interfaces/sockets + expectation alerts).
netlink: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-netlink --config {{rundir}}/netlink.json5

# Run the sysinfo sensor (CPU/memory/disk/network).
sysinfo: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-sysinfo --config {{rundir}}/sysinfo.json5

# Run the logs sensor (systemd journal via journald + known-event alerts).
logs: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-sensor-logs --config {{rundir}}/logs.json5

# ── Run (everything) ─────────────────────────────────────────────────────────

# Build + caps + configure, then launch the 3 sensors + GUI (close GUI to stop all).
run: setup configure
    #!/usr/bin/env bash
    set -euo pipefail
    # Sensors connect to the GUI's loopback rendezvous (no multicast needed).
    export ZENSIGHT_ZENOH_CONNECT="{{hub}}"
    echo "Starting sensors (logs in {{rundir}}/), connecting to {{hub}}…"
    {{bindir}}/zensight-sensor-sysinfo --config {{rundir}}/sysinfo.json5 > {{rundir}}/sysinfo.log 2>&1 &
    {{bindir}}/zensight-sensor-netlink --config {{rundir}}/netlink.json5 > {{rundir}}/netlink.log 2>&1 &
    {{bindir}}/zensight-sensor-netring --config {{rundir}}/netring.json5 > {{rundir}}/netring.log 2>&1 &
    {{bindir}}/zensight-sensor-logs --config {{rundir}}/logs.json5 > {{rundir}}/logs.log 2>&1 &
    # Stop all sensors when the GUI exits (or on Ctrl-C).
    trap 'echo; echo "Stopping sensors…"; kill 0' EXIT
    sleep 1
    echo "Launching GUI (listening on {{hub}}; close it to stop everything)…"
    unset ZENSIGHT_ZENOH_CONNECT
    ZENSIGHT_ZENOH_LISTEN="{{hub}}" {{bindir}}/zensight

# Stop any running sensors started by `just run`.
stop:
    -pkill -f 'zensight-sensor-(netring|netlink|sysinfo|logs)' || true

# Remove generated run configs and logs.
clean-run:
    rm -rf {{rundir}}
