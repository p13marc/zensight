# ZenSight — build / configure / run the GUI + sensors (netring, netlink, sysinfo, logs, systemd)
#
#   just run            # build, grant caps, configure, then launch everything
#   just demo           # run the GUI in demo mode (simulated data, no sensors)
#   just setup          # build + grant capabilities only
#   just gui            # run just the GUI
#   just <sensor>       # run a single sensor (netring | netlink | sysinfo | logs | systemd)
#
# netring captures packets and needs CAP_NET_RAW (+CAP_IPC_LOCK for AF_XDP);
# netlink's optional collectors (nftables/conntrack + the XFRM monitor) need
# CAP_NET_ADMIN. `just caps` grants both via sudo. sysinfo is unprivileged.
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

# Build the GUI + the sensors.
build:
    cargo build {{relflag}} \
        -p zensight \
        -p zensight-sensor-netring \
        -p zensight-sensor-netlink \
        -p zensight-sensor-sysinfo \
        -p zensight-sensor-logs \
        -p zensight-sensor-systemd

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
    @echo "sysinfo + logs need no capabilities."

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
    # netlink + logs configs are machine-agnostic (hostname auto-detected).
    cp -f configs/netlink.json5 {{rundir}}/netlink.json5
    cp -f configs/logs.json5 {{rundir}}/logs.json5
    # sysinfo: enable a Tier-2 directory snapshot of the repo's docs/ so the
    # feature is demoable from the GUI (Sensors → "Download docs"). Scoped sed
    # over the snapshot block only (leaves the report block's enabled flag alone).
    sed -E '/snapshot: \{/,/dirs: \[/ {
        s/enabled: false/enabled: true/
        s#dirs: \[#dirs: [ { name: "docs", path: "{{justfile_directory()}}/docs" },#
    }' configs/sysinfo.json5 > {{rundir}}/sysinfo.json5
    # systemd: generate a demo config with (nearly) everything on. NOTE the
    # watchlist is deliberately a *curated* set, not `*.service` — the Units /
    # Timers / Sockets / cgroup tabs all populate from the on-demand @/query/*
    # channels regardless of the watchlist, so a broad watch just streams a lot of
    # per-unit telemetry every tick for no UI gain (and, stacked on the other
    # maxed-out sensors, can starve the desktop). `actions` (gated start/stop/
    # restart) stays OFF — it mutates real units and is privileged.
    cat > {{rundir}}/systemd.json5 <<'JSON5'
    {
      zenoh: { mode: "peer", serialization: "json" },
      // On-demand redacted debug bundle (Sensors → report) — safe to enable.
      report: { enabled: true, max_bytes: 67108864, cooldown_secs: 30, ttl_secs: 600, chunk_size: 524288 },
      systemd: {
        key_prefix: "zensight/systemd",
        poll_interval_secs: 15,
        // Curated per-unit stream (timers + sockets + a few high-value services).
        // The full inventory is still browsable via the on-demand query tabs.
        watch_units: ["*.timer", "*.socket", "sshd.service", "NetworkManager.service",
                      "systemd-journald.service", "systemd-logind.service",
                      "dbus-broker.service", "polkit.service", "user@*.service"],
        watch_max: 50,
        ip_io_accounting: true,       // per-unit IP + disk IO byte counters
        events_capacity: 512,         // control-plane event ring (@/query/events)
        alerts: {
          enabled: true,
          for_secs: 15,
          unit_failed: true,
          system_degraded: true,
          restart_storm_threshold: 3,
          restart_storm_window_secs: 300,
          unit_mem_ceiling_bytes: 0,  // 0 = unit-mem rule off (avoids demo noise)
          timer_overdue_grace_secs: 300,
        },
        cgroup: { root: "system.slice", max_depth: 6, max_children: 64, max_pids: 32 },
        // Sentinel: default.target must be active and nothing may be failed — the
        // latter fires a real, actionable alert iff the host has a failed unit.
        expectations: {
          eval_interval_secs: 15,
          for_secs: 15,
          targets_active: [{ target: "default.target" }],
          forbid_failed: true,
        },
        // actions: { enabled: false }  // gated service control — off for the demo.
        collect: { list_units: true, boot: true, mounts: true, journal: true },
      },
      logging: { level: "info" },
    }
    JSON5
    echo "Configured: netring iface='{{iface}}', logs=journald, sysinfo snapshot='docs/', systemd=full  (configs in {{rundir}}/)"

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

# ── Run (everything) ─────────────────────────────────────────────────────────

# Build + caps + configure, then launch the sensors + GUI (close GUI to stop all).
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
    {{bindir}}/zensight-sensor-systemd --config {{rundir}}/systemd.json5 > {{rundir}}/systemd.log 2>&1 &
    # Stop all sensors when the GUI exits (or on Ctrl-C).
    trap 'echo; echo "Stopping sensors…"; kill 0' EXIT
    sleep 1
    echo "Launching GUI (listening on {{hub}}; close it to stop everything)…"
    unset ZENSIGHT_ZENOH_CONNECT
    ZENSIGHT_ZENOH_LISTEN="{{hub}}" {{bindir}}/zensight

# Stop any running sensors started by `just run`.
stop:
    -pkill -f 'zensight-sensor-(netring|netlink|sysinfo|logs|systemd)' || true

# Remove generated run configs and logs.
clean-run:
    rm -rf {{rundir}}
