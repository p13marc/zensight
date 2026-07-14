# ZenSight — build / configure / run the GUI + sensors + correlator
#   (netring, netlink, sysinfo, logs, systemd, parallax + the identity correlator)
#
#   just run            # build, grant caps, configure, then launch everything
#                       # (just run rerun=live|record|both to add the Rerun sidecar)
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

# The GUI is built with the parallax H.264 live view (compiles openh264 from
# source — needs a C++ toolchain); drop the flag for a JPEG-preview-only,
# C++-free GUI.

# Build the GUI + the sensors + the identity correlator.
build:
    cargo build {{relflag}} \
        -p zensight --features zensight/h264 \
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
    ZENSIGHT_ZENOH_LISTEN="{{trim_start_match(listen, 'listen=')}}" {{bindir}}/zensight

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

# Optional Rerun sidecar (evaluation prototype, epic #415), standalone — or add
# it to the full stack with `just run rerun=live|record|both`.
# Feeds the live bus into Rerun; built on demand (pulls the arrow/tonic stack).
#   just rerun                # live → viewer at rerun+http://127.0.0.1:9876/proxy
#                             #   (start the viewer first: `rerun`)
#   just rerun mode=record    # headless → {{rundir}}/zensight.rrd (replay later)
rerun mode="live":
    cargo build {{relflag}} -p zensight-rerun
    mkdir -p {{rundir}}
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-rerun --config configs/rerun.json5 \
        --mode {{trim_start_match(mode, 'mode=')}} --rrd-path {{rundir}}/zensight.rrd

# ── Run (everything) ─────────────────────────────────────────────────────────

# Run the 6 sensors in the foreground, no GUI/correlator (Ctrl-C stops them).
# Point them at a remote GUI with: just sensors connect=tcp/<gui-host>:7447
sensors connect=hub: setup configure
    BINDIR="{{bindir}}" CONFDIR="{{rundir}}" LOGDIR="{{rundir}}" \
    CONNECT="{{trim_start_match(connect, 'connect=')}}" scripts/run-sensors.sh

# Build + caps + configure, then launch the sensors + GUI (close GUI to stop all).
# Optionally add the Rerun sidecar (evaluation, epic #415):
#   just run rerun=live     # stream to a Rerun viewer (auto-started if installed)
#   just run rerun=record   # headless → {{rundir}}/zensight.rrd (replay later)
#   just run rerun=both     # both at once
run rerun="": setup configure
    #!/usr/bin/env bash
    set -euo pipefail
    # Optional Rerun sidecar. `just` recipe args are positional, so accept both
    # `just run live` and the self-documenting `just run rerun=live`.
    rerun_mode="{{trim_start_match(rerun, 'rerun=')}}"
    case "$rerun_mode" in
        ""|live|record|both) ;;
        *) echo "error: rerun mode must be live|record|both, got '$rerun_mode'" >&2; exit 1 ;;
    esac
    # Build it up front (on-demand — it pulls the arrow/tonic stack) so the
    # sensors and GUI start together afterwards.
    if [[ -n "$rerun_mode" ]]; then
        cargo build {{relflag}} -p zensight-rerun
    fi
    # Sensors + correlator via the shared spawner (same process group, so the
    # trap below reaps them when the GUI exits or on Ctrl-C). They connect to
    # the GUI's loopback rendezvous (no multicast needed); logs in {{rundir}}/.
    BINDIR="{{bindir}}" CONFDIR="{{rundir}}" LOGDIR="{{rundir}}" \
    CONNECT="{{hub}}" WITH_CORRELATOR=1 scripts/run-sensors.sh &
    # Stop all sensors when the GUI exits (or on Ctrl-C).
    trap 'echo; echo "Stopping sensors…"; kill 0' EXIT
    if [[ -n "$rerun_mode" ]]; then
        case "$rerun_mode" in
        live|both)
            if command -v rerun >/dev/null; then
                echo "Starting Rerun viewer (log → {{rundir}}/rerun-viewer.log)…"
                rerun --port 9876 > {{rundir}}/rerun-viewer.log 2>&1 &
            else
                echo "note: 'rerun' viewer not installed — the sidecar still streams to" \
                     "rerun+http://127.0.0.1:9876/proxy; install it (cargo binstall rerun-cli)" \
                     "and start it with: rerun --port 9876"
            fi ;;
        esac
        echo "Starting Rerun sidecar, mode=$rerun_mode (log → {{rundir}}/rerun.log)…"
        ZENSIGHT_ZENOH_CONNECT="{{hub}}" {{bindir}}/zensight-rerun --config configs/rerun.json5 \
            --mode "$rerun_mode" --rrd-path {{rundir}}/zensight.rrd > {{rundir}}/rerun.log 2>&1 &
    fi
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

# ── Router storage verification (#471) ───────────────────────────────────────

# Verify configs/router-*.json5 against a REAL zenohd: state docs outlive their
# publisher, DELETE tombstones retire, blob chunks persist, `*` cannot match
# @catalog, and a fleet @rpc GET still fans in (no `complete` storage).
#
# Needs zenohd AND its plugins, all at the workspace's zenoh version (1.9).
# The plugins are cdylibs, not binaries — `cargo install` refuses them, and the
# volume name `fs` is NOT the crate name (it is zenoh-backend-*filesystem*):
#
#   cargo install zenohd --version 1.9.0 --locked
#   just router-plugins        # builds the two .so files into ~/.zenoh/lib
#
# A version-mismatched plugin is the trap: zenohd loads, logs one line, and
# serves *no* storage — every test then fails as "nothing was stored".
#
# The suite spawns its own zenohd on loopback:17447 with multicast AND gossip
# off. It never touches 7447 and never joins your fleet.
router-verify:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v zenohd >/dev/null || {
      echo "zenohd not on PATH — cargo install zenohd --version 1.9.0 --locked" >&2
      exit 1
    }
    for so in libzenoh_plugin_storage_manager.so libzenoh_backend_fs.so; do
      [ -f ~/.zenoh/lib/"$so" ] || {
        echo "$so missing from ~/.zenoh/lib — run: just router-plugins" >&2
        exit 1
      }
    done
    echo "zenohd: $(zenohd --version 2>&1 | head -1)"
    cargo test -p zensight-common --test router_storage -- --ignored --nocapture --test-threads=1

# Build the storage-manager plugin + fs backend from crates.io into ~/.zenoh/lib,
# at the zenoh version this workspace pins. Both are cdylibs, so they must be
# built rather than installed (`cargo install` refuses a library crate).
#
# THREE TRAPS. Each one produces the same symptom: zenohd starts, logs a single
# ERROR line, and then serves no storage at all — so every router-verify test
# fails as "nothing was stored" and none of them tells you why.
#
#  1. **Build both plugins in ONE workspace.** zenoh-plugin-storage-manager takes
#     `zenoh_backend_traits` with `default-features = false`; zenoh-backend-
#     filesystem takes it with defaults. Built separately, their compiled feature
#     strings differ and zenoh's compatibility check rejects the pair
#     ("Incompatible Zenoh feature sets"). Building them in one cargo workspace
#     unifies the features. This is why the recipe below writes a workspace
#     manifest rather than building each crate in turn.
#  2. Zenoh checks plugin compatibility by **exact rustc version**, and
#     zenoh-backend-filesystem ships a `rust-toolchain.toml` pinning an older one
#     than you will have built zenohd with ("Incompatible rustc versions"). It is
#     removed below so everything is built with the host toolchain.
#  3. The fs backend vendors rocksdb, whose C++ predates GCC 13's stricter header
#     hygiene — hence CXXFLAGS. Do NOT also set CFLAGS: the `-include` lands on a
#     zstd .S assembly file and breaks the build.
router-plugins version="1.9.0":
    #!/usr/bin/env bash
    set -euo pipefail
    # OUTSIDE the repo, and not in /tmp.
    #
    # Not in the repo (not even under target/): `cargo new` walks up looking for a
    # workspace and, finding ours, helpfully ADDS the scratch crate to the real
    # `Cargo.toml`'s members and rewrites `Cargo.lock`. Not in /tmp: this is a
    # rocksdb build, and a tmpfs with a quota dies half-way through with a
    # "Disk quota exceeded" from rustc.
    work="${XDG_CACHE_HOME:-$HOME/.cache}/zensight/router-plugins"
    rm -rf "$work" && mkdir -p "$work" ~/.zenoh/lib
    echo "host toolchain: $(rustc --version)  (zenohd must be built with THIS)"
    crates="zenoh-plugin-storage-manager zenoh-backend-filesystem"
    for crate in $crates; do
      # A throwaway crate is only how we get cargo to populate its registry cache
      # (`cargo fetch`); the .crate tarball we actually build lands there.
      (cd "$work" && cargo new --quiet --lib fetch-$crate && cd fetch-$crate \
        && cargo add --quiet "$crate@{{version}}" && cargo fetch --quiet)
      tar xzf ~/.cargo/registry/cache/*/"$crate-{{version}}.crate" -C "$work"
      rm -f "$work/$crate-{{version}}/rust-toolchain.toml"      # trap 2
    done
    # trap 1: one workspace, one build, unified features.
    {
      echo '[workspace]'
      echo 'resolver = "2"'
      echo -n 'members = ['
      for crate in $crates; do echo -n "\"$crate-{{version}}\", "; done
      echo ']'
    } > "$work/Cargo.toml"
    (cd "$work" && CXXFLAGS="-include cstdint" cargo build --release)   # trap 3
    find "$work/target/release" -maxdepth 1 -name 'libzenoh_*.so' -exec cp -v {} ~/.zenoh/lib/ \;
    ls -l ~/.zenoh/lib
