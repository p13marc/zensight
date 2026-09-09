# ZenSight — build / configure / run the GUI + sensors + correlator
#   (netring, netlink, sysinfo, logs, systemd, hostspec, parallax + the identity correlator)
#
#   just run            # build, grant caps, configure, then launch everything
#                       # (just run rerun=live|record|both to add the Rerun sidecar)
#   just demo           # run the GUI in demo mode (simulated data, no sensors)
#   just setup          # build + grant capabilities only
#   just gui            # run just the GUI    (just gui listen=tcp/0.0.0.0:7447 for remote sensors)
#   just sensors        # run just the 6 sensors, no GUI/correlator (Ctrl-C stops them)
#                       # (just sensors connect=tcp/<gui-host>:7447 to feed a remote GUI)
#   just <name>         # run one piece (netring | netlink | sysinfo | logs | systemd | hostspec | parallax | correlator | historian)
#   just desired-plan   # validate the fleet policy and show what it would publish
#   just desired        # run the fleet policy compiler (#938) — it WRITES @desired
#   just container      # the container sensor (#819) — needs a runtime socket
#   just probe          # the outside-in probe sensor (#820) — needs targets
#   just pve            # the Proxmox VE sensor (#818) — needs a PVE endpoint
#   just bmc            # the BMC sensor (#953) — needs a Redfish endpoint
#                       # and a read-only API token, so it is not in `just run`
#   just rerun          # optional Rerun sidecar (evaluation, epic #415) — see the recipe
#   just demo-actions   # install the inert unit + polkit rule that make gated
#                       # service control demonstrable (#866), then:
#                       #   just actions=1 run
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
# (`actions`), which is left off because it stops/restarts real units. `just
# actions=1 run` arms it for ONE inert unit — see `just demo-actions` and #866.
#
# SNMP outlet control (#956) has no `just` demo and cannot have one: it cycles
# power on a real PDU, and a fake that pretends to would be a demo of the wrong
# thing. What IS demonstrable without hardware is the half that matters most —
# that the gate refuses and says which switch refused — and
# `a_default_sensor_answers_the_probe_and_refuses_the_write_surface` in
# zensight-sensor-snmp does exactly that over a real bus. The opt-in path for a
# deployment that has a PDU is the commented `actions:` block in
# configs/snmp.json5, which spells out all four gates.

# Build profile: "release" (default) or "dev".
profile := "release"

# Network interface for netring capture (defaults to the default-route iface).
iface := `ip route show default 2>/dev/null | awk '{print $5; exit}' | grep -m1 . || ip -o link show up 2>/dev/null | awk -F': ' '$2 != "lo" {print $2; exit}' | grep -m1 . || echo lo`

# Derived: where cargo puts the binaries, and the --release flag.
bindir := if profile == "release" { "target/release" } else { "target/debug" }
relflag := if profile == "release" { "--release" } else { "" }

# Run configs are generated here (gitignored), so committed examples stay clean.
rundir := ".run"

# Gated systemd service control (#866). "0" (default) leaves `actions` off in
# the generated config, which is what every run has always done — and is why
# the whole allowlist / arm-confirm / in-flight-lock / audit-ring surface had
# never once been watched working. "1" arms it for exactly ONE inert unit,
# `zensight-demo.service`, which `sudo scripts/demo-actions.sh install` creates
# along with a polkit rule scoped to that unit and your user:
#
#   sudo scripts/demo-actions.sh install     # the unit + the one-unit polkit rule
#   just actions=1 run                       # then start/stop it from the GUI
#   sudo scripts/demo-actions.sh remove      # put the machine back
#
# Without the polkit rule (or root) the sensor still refuses — correctly, and
# now with a reason string the GUI shows. Point it somewhere else by editing
# .run/systemd.json5; the default demo never touches a unit anything needs.
actions := "0"

# eBPF collectors (#99): "auto" builds them iff this host has the toolchain, and
# silently goes without otherwise, so `just run` stays portable. "1" forces them
# on (the build fails loudly if the toolchain is missing); "0" forces them off.
#   just ebpf=1 run   /   just ebpf=0 run
ebpf := "auto"

# aya-build shells out to `rustup run <toolchain> cargo build -Z
# build-std=core`. That toolchain is the DATED nightly the program crates pin
# (#1094) — `build.rs` reads it out of their `rust-toolchain.toml`, so there is
# one pin and `rustup run` honours it. It used to be the literal string
# `nightly`, which meant the object was built by whatever nightly the machine
# happened to have; on a box whose plain `nightly` predates the workspace's
# `rust-version` that fails with a message about the *ebpf* crates rather than
# about the toolchain, which is a confusing way to learn you need `rustup
# update`.
#
# So detection asks for the pinned one by name, plus rust-src for build-std,
# plus bpf-linker for the link step.
_ebpf_toolchain := `grep -oP '^channel = "\K[^"]+' zensight-sensor-sysinfo-ebpf/rust-toolchain.toml 2>/dev/null || echo nightly`
_ebpf_detected := ```
    tc=$(grep -oP '^channel = "\K[^"]+' zensight-sensor-sysinfo-ebpf/rust-toolchain.toml 2>/dev/null || echo nightly)
    if command -v bpf-linker >/dev/null 2>&1 \
       && rustup toolchain list 2>/dev/null \
            | grep -q "^${tc}-$(rustc -vV | awk '/^host:/{print $2}')" \
       && rustup component list --toolchain "$tc" 2>/dev/null \
            | grep -q '^rust-src.*(installed)'
    then echo 1; else echo 0; fi
```

ebpf_on := if ebpf == "auto" { _ebpf_detected } else if ebpf == "1" { "1" } else if ebpf == "0" { "0" } else { error("ebpf must be auto|1|0, got '" + ebpf + "'") }

# Only sysinfo, still — but for a smaller reason than before. netlink's
# connect-latency probe used to measure the connect() call path rather than the
# SYN→SYN-ACK handshake, publishing microseconds to any host on earth; that is
# fixed and host-validated (#114). What keeps netlink out of the demo now is
# that its tcplife byte/segment counters are still hardcoded zero, and a zero
# that reads as "idle connection" is worse in a demo than an absent panel.
# sysinfo's runqlat/biolatency are self-validating: each joins a key written by
# one tracepoint against one read by another, so a bad offset yields an empty
# histogram rather than a wrong one.
#
# Cargo merges every --features flag into ONE global set; `pkg/feature` is what
# binds a feature to a package. (`-p a --features x -p b --features y` is NOT
# positional — the flags do not attach to the preceding -p, despite how `build`
# below reads.)
ebpf_features := if ebpf_on == "1" { "--features zensight-sensor-sysinfo/ebpf" } else { "" }

# Local Zenoh rendezvous: the GUI listens here and sensors connect to it, so the
# pieces always find each other on loopback without relying on multicast peer
# discovery (which is unreliable on hosts with a VPN or extra interfaces, e.g.
# tailscale/docker). Honored via the ZENSIGHT_ZENOH_* env vars.
hub := "tcp/127.0.0.1:7447"

# The Prometheus exporter's scrape port. NOT 9090 — that is the Prometheus
# *server's* own port, and the demo stack runs both on the host network. 9464 is
# the conventional OpenTelemetry/Prometheus-exporter port and is now the shipped
# default in configs/prometheus-exporter.json5 too.
exporter_port := "9464"

# Compose front-end for the demo stacks. `docker compose` is canonical for this
# repo's compose files (docker/docker-compose.yml documents itself that way);
# `podman compose` is accepted because `just image` already builds with podman
# and some hosts have only that. Detected once so the recipes don't each decide.
_compose := ```
    if docker compose version >/dev/null 2>&1; then echo "docker compose"
    elif podman compose version >/dev/null 2>&1; then echo "podman compose"
    elif command -v podman-compose >/dev/null 2>&1; then echo "podman-compose"
    else echo ""; fi
```

_default:
    @just --list

# ── Build ────────────────────────────────────────────────────────────────────

# The GUI is built with the parallax H.264 live view (compiles openh264 from
# source — needs a C++ toolchain); drop the flag for a JPEG-preview-only,
# C++-free GUI.

# Build the GUI + the sensors + the identity correlator.
#
# The eBPF note below carries NO backticks on purpose. Recipe lines are handed to
# sh, which reads a backtick as command substitution — and a backticked
# `just caps` there recurses through this recipe's own dependency and hangs the
# build with no output whatsoever. (This comment lives out here rather than in the
# recipe body because just echoes recipe lines, comments included.)
build:
    @echo '{{ if ebpf_on == "1" { "eBPF: ON (sysinfo runqlat/biolatency) — grant the caps with: just caps" } else { "eBPF: off — needs bpf-linker, plus the nightly pinned in zensight-sensor-sysinfo-ebpf/rust-toolchain.toml with rust-src. See: just ebpf-setup" } }}'
    cargo build {{relflag}} \
        -p zensight --features zensight/h264 \
        -p zensight-sensor-netring \
        -p zensight-sensor-netlink \
        -p zensight-sensor-sysinfo \
        -p zensight-sensor-logs \
        -p zensight-sensor-systemd \
        -p zensight-sensor-hostspec \
        -p zensight-sensor-pve \
        -p zensight-sensor-bmc \
        -p zensight-sensor-container \
        -p zensight-sensor-probe \
        -p zensight-sensor-parallax \
        -p zensight-correlator \
        -p zensight-historian \
        -p zensight-desired \
        {{ebpf_features}}

# Install exactly what an eBPF build needs: the DATED nightly the program crates
# pin (#1094) plus rust-src, and bpf-linker.
#
# The pin is read from the crate rather than written here, so this recipe cannot
# drift from what `build.rs` actually asks `rustup run` for.
ebpf-setup:
    #!/usr/bin/env bash
    set -euo pipefail
    tc=$(grep -oP '^channel = "\K[^"]+' zensight-sensor-sysinfo-ebpf/rust-toolchain.toml)
    echo "pinned eBPF toolchain: $tc"
    rustup toolchain install "$tc" --component rust-src
    command -v bpf-linker >/dev/null 2>&1 || cargo install bpf-linker
    echo "ready — build with: just ebpf=1 build"

# ── Capabilities ─────────────────────────────────────────────────────────────

# Grant capture/admin capabilities via sudo (re-run after each rebuild):
#   netring → CAP_NET_RAW,CAP_IPC_LOCK  (AF_PACKET/AF_XDP capture)
#   netlink → CAP_NET_ADMIN             (optional nftables/conntrack + XFRM monitor)
# netlink's baseline reads work without this; the cap only unlocks the extras.
#
# With an eBPF build ({{ebpf_on}}=1), sysinfo also gets:
#   CAP_BPF + CAP_PERFMON  — load tracing programs and perf_event_open. NOT
#                            CAP_NET_ADMIN: that gates *networking* program types
#                            (XDP, cgroup/skb), none of which we load.
#   CAP_DAC_READ_SEARCH    — aya resolves a tracepoint by reading
#                            <tracefs>/events/<cat>/<name>/id from userspace, and
#                            /sys/kernel/tracing is mode 0700 root:root, so every
#                            attach fails EACCES without it — even with CAP_BPF.
#                            This is a broad grant (read any file on the host); it
#                            buys the runqlat/biolatency panel. `just ebpf=0 caps`
#                            skips it. The narrower alternative is chmod 755 on
#                            /sys/kernel/tracing, which is worse: it exposes
#                            tracing to every user and resets each boot.
caps: build _sysinfo-caps
    #!/usr/bin/env bash
    set -euo pipefail
    echo "Granting CAP_NET_RAW,CAP_IPC_LOCK to {{bindir}}/zensight-sensor-netring (sudo)…"
    sudo setcap 'cap_net_raw,cap_ipc_lock=+ep' {{bindir}}/zensight-sensor-netring
    echo "Granting CAP_NET_ADMIN to {{bindir}}/zensight-sensor-netlink (sudo)…"
    sudo setcap 'cap_net_admin=+ep' {{bindir}}/zensight-sensor-netlink
    if [[ "{{ebpf_on}}" == "1" ]]; then
        echo "sysinfo's eBPF caps were granted above; logs + parallax need none."
    else
        echo "sysinfo + logs + parallax need no capabilities."
    fi

# sysinfo's eBPF capabilities, on their own so `just sysinfo` can depend on them
# without dragging in netring's and netlink's sudo setcaps (#685).
#
# `just ebpf=1 sysinfo` used to depend on `build configure` only, while netring
# and netlink depend on `caps`. So it built an eBPF binary, wrote
# `collect.ebpf: true` into the generated config, and then ran that binary with
# no capabilities — the one combination guaranteed to log the warning and serve
# `available: false`. Depending on `caps` outright would have been worse: a
# plain `just sysinfo` would then sudo-setcap two binaries it never runs.
#
# A no-op unless ebpf_on == "1", so the unprivileged path never prompts for sudo.
_sysinfo-caps: build
    #!/usr/bin/env bash
    set -euo pipefail
    if [[ "{{ebpf_on}}" != "1" ]]; then exit 0; fi
    echo "Granting CAP_BPF,CAP_PERFMON,CAP_DAC_READ_SEARCH to {{bindir}}/zensight-sensor-sysinfo (sudo)…"
    sudo setcap 'cap_bpf,cap_perfmon,cap_dac_read_search=+ep' {{bindir}}/zensight-sensor-sysinfo
    # A file capability only grants privilege in the user namespace that set
    # it, but the kernel checks bpf_capable() against the *initial* userns —
    # so inside a rootless container setcap is void and every load is EPERM.
    # Say so here rather than let it surface as a mystery in the sensor log.
    if [[ -e /run/.containerenv || -e /.dockerenv ]]; then
        echo
        echo "  WARNING: this is a rootless container. BPF loads are checked against the"
        echo "  initial user namespace, so the caps above cannot take effect here and"
        echo "  sysinfo will log 'eBPF latency collector unavailable'. The rest of the"
        echo "  demo is unaffected. Run from a host terminal for the eBPF panel."
        echo
    fi
    # The caps above are necessary and not sufficient (#683). Debian and
    # Ubuntu ship perf_event_paranoid=3 — above upstream's maximum of 2 —
    # which denies perf_event_open beyond what CAP_PERFMON relaxes. The
    # programs LOAD and every attach then fails EACCES, which reads as a
    # capability problem and is not one. Report it here, where the caps are
    # granted, rather than let it surface as one line in the sensor log.
    paranoid=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "?")
    if [[ "$paranoid" =~ ^[0-9]+$ ]] && (( paranoid > 2 )); then
        echo
        echo "  WARNING: kernel.perf_event_paranoid=$paranoid (Debian/Ubuntu default is 3,"
        echo "  above upstream's maximum of 2). The programs will load and every attach"
        echo "  will fail with 'Permission denied', whatever the caps above say."
        echo "    this session: sudo sysctl kernel.perf_event_paranoid=2"
        echo "    persistent:   echo 'kernel.perf_event_paranoid = 2' | sudo tee /etc/sysctl.d/60-zensight-ebpf.conf"
        echo "  It relaxes perf_event_open for every unprivileged process on the host, so"
        echo "  it is your call to make, not something 'just caps' should do for you."
        echo
    else
        echo "kernel.perf_event_paranoid=$paranoid — permits the attach (needs <= 2)."
    fi
    echo "logs + parallax need no capabilities."

# Build + grant capabilities.
setup: build caps

# ── Configure ────────────────────────────────────────────────────────────────

# Generate run configs in {{rundir}} (netring capture interface = {{iface}}).
# The demo-max profile itself lives in scripts/gen-configs.sh, shared with the
# sensors container image — edit it there.
configure:
    scripts/gen-configs.sh --iface "{{iface}}" --outdir "{{rundir}}" \
        --configs-dir "{{justfile_directory()}}/configs" \
        --snapshot-dir "{{justfile_directory()}}/docs" \
        --pcap-dir "{{justfile_directory()}}/{{rundir}}/pcap" \
        --exporters \
        {{ if actions == "1" { "--actions zensight-demo.service" } else { "" } }} \
        {{ if ebpf_on == "1" { "--ebpf" } else { "" } }}

# ── Run (individual) ─────────────────────────────────────────────────────────

# Run the desktop GUI.
# The GUI listens on the hub so separately-run sensors can connect. For
# sensors on OTHER machines, listen on all interfaces:
#   just gui listen=tcp/0.0.0.0:7447
gui listen=hub: build
    ZENSIGHT_ZENOH_LISTEN="{{trim_start_match(listen, 'listen=')}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight

# A built-in simulator feeds realistic telemetry, health, liveness and anomaly
# alerts for every sensor type — no real sensors, capabilities or Zenoh hub.
# Run the GUI in demo mode (great for a quick look at the UI).
demo: build
    {{bindir}}/zensight --demo

# Run the netring sensor (wire flows + anomaly alerts).
netring: caps configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-sensor-netring --config {{rundir}}/netring.json5

# Run the netlink sensor (kernel interfaces/sockets + expectation alerts).
netlink: caps configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-sensor-netlink --config {{rundir}}/netlink.json5

# Run the sysinfo sensor (CPU/memory/disk/network).
sysinfo: build _sysinfo-caps configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-sensor-sysinfo --config {{rundir}}/sysinfo.json5

# Run the logs sensor (systemd journal via journald + known-event alerts).
logs: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-sensor-logs --config {{rundir}}/logs.json5

# Run the systemd sensor (unit/boot telemetry + threshold alerts + sentinel).
systemd: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-sensor-systemd --config {{rundir}}/systemd.json5

# Run the outside-in probe sensor (#820). NOT part of `just run`: its whole
# value is the targets an operator names, and a probe with no targets checks
# nothing. Fill in configs/probe.json5 first — every target there is commented
# out, because a demo cannot invent a URL worth watching.
probe config="configs/probe.json5": build
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false \
        {{bindir}}/zensight-sensor-probe --config "{{trim_start_match(config, 'config=')}}"

# Run the container sensor (#819). NOT part of `just run`: it needs a container
# runtime socket, and on a host with none it would be a sensor reporting a
# failure every cycle for something that host simply does not do. On a host WITH
# podman it works with the shipped config unchanged.
container config="configs/container.json5": build
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false \
        {{bindir}}/zensight-sensor-container --config "{{trim_start_match(config, 'config=')}}"

# Run the BMC sensor (#953). NOT part of `just run`: there is no baseboard
# management controller on a dev box, and a sensor whose every endpoint is
# unreachable would report a failure a minute for something the machine simply
# does not have. Copy configs/bmc.json5, fill in an endpoint and a credential,
# then:
#   just bmc                       # uses .run/bmc.json5 if you put one there,
#                                  # else configs/bmc.json5
bmc config="configs/bmc.json5": build
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false \
        {{bindir}}/zensight-sensor-bmc --config "{{trim_start_match(config, 'config=')}}"

# Run the Proxmox VE sensor (#818). NOT part of `just run`: it needs a PVE API
# endpoint and a read-only PVEAuditor token, which no demo can invent. Copy
# configs/pve.json5, fill in host + token, then:
#   just pve                       # uses .run/pve.json5 if you put one there,
#                                  # else configs/pve.json5
pve config="configs/pve.json5": build
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false \
        {{bindir}}/zensight-sensor-pve --config "{{trim_start_match(config, 'config=')}}"

# Make gated systemd service control demonstrable (#866). Installs an inert
# `zensight-demo.service` (a `sleep infinity` under DynamicUser, no network)
# plus a polkit rule granting manage-units on THAT UNIT to THIS USER and
# nothing else. Both need root, which is the honest cost of demonstrating a
# privileged surface — the script asks for it rather than hiding a sudo here.
# Then: `just actions=1 run`, systemd device → Units, filter `zensight-demo`.
# `just demo-actions-remove` puts the machine back.
demo-actions:
    sudo scripts/demo-actions.sh install "$USER"

# Remove the #866 demo unit and its polkit rule.
demo-actions-remove:
    sudo scripts/demo-actions.sh remove

# Run the hostspec sensor (desired-state assertions; the shipped set is empty —
# uncomment examples in configs/hostspec.json5 to hold this host to something).
hostspec: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-sensor-hostspec --config {{rundir}}/hostspec.json5

# Run the parallax sensor (live video: synthetic test pattern + local cameras).
# Open the parallax device in the GUI and "Load streams" → preview tiles.
parallax: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-sensor-parallax --config {{rundir}}/parallax.json5

# Run the identity correlator (fuses sensor evidence into one HostEntity per host).
correlator: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-correlator --config {{rundir}}/correlator.json5

# Run the historian (the fleet's telemetry history: ingest + range queries).
# Its store lands in ~/.local/state/zensight/history.redb unless the config
# names a path, so it survives a restart of this recipe.
historian: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-historian --config {{rundir}}/historian.json5

# Run the fleet policy compiler (#938): compile fleet-policy.json5 against the
# catalog and publish the per-host @desired documents.
#
# Opt-in in `just run`, and not by accident. This daemon WRITES the desired
# state every sensor reconciles, so starting it with a policy you have not read
# would reconfigure the whole demo fleet. Look first:
#
#   just desired-plan     # validate + show what would change, publishes nothing
#   just desired          # actually publish, standalone
#   just run desired=1    # the whole stack, controller included
#
# The policy is {{rundir}}/fleet-policy.json5, copied there from demo/ by
# `just configure` — which also points the generated desired.json5 at that
# copy, so ONE file is in force and it is the one the daemon names. It sets one
# sysinfo threshold on every host, which is enough to watch a document land on
# a sensor's state/<producer>/applied/<topic> marker.
desired: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-desired --config {{rundir}}/desired.json5 run

# Validate the demo policy and print what it would publish. Needs no bus for
# the policy half; with one, it also lists the documents per host.
desired-plan: build configure
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-desired --config {{rundir}}/desired.json5 plan

# Optional Rerun sidecar (evaluation prototype, epic #415), standalone — or add
# it to the full stack with `just run rerun=live|record|both`.
# Feeds the live bus into Rerun; built on demand (pulls the arrow/tonic stack).
#   just rerun                # live → viewer at rerun+http://127.0.0.1:9876/proxy
#                             #   (start the viewer first: `rerun`)
#   just rerun mode=record    # headless → {{rundir}}/zensight.rrd (replay later)
rerun mode="live":
    cargo build {{relflag}} -p zensight-rerun
    mkdir -p {{rundir}}
    ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-rerun --config configs/rerun.json5 \
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
# Or the fleet policy controller (#941):
#   just run desired=1      # also start zensight-desired against demo/fleet-policy.json5
run rerun="" desired="": setup configure
    #!/usr/bin/env bash
    set -euo pipefail
    # Optional Rerun sidecar. `just` recipe args are positional, so accept both
    # `just run live` and the self-documenting `just run rerun=live`.
    rerun_mode="{{trim_start_match(rerun, 'rerun=')}}"
    case "$rerun_mode" in
        ""|live|record|both) ;;
        *) echo "error: rerun mode must be live|record|both, got '$rerun_mode'" >&2; exit 1 ;;
    esac
    # The policy controller. OPT-IN, and it stays that way: this daemon writes
    # the desired state every sensor in the run reconciles, so starting it by
    # default would reconfigure the demo fleet from a file nobody had read.
    # {{rundir}}/fleet-policy.json5 is that file — `just configure` copies it
    # from demo/ and points the generated desired.json5 at the copy.
    with_desired="{{trim_start_match(desired, 'desired=')}}"
    case "$with_desired" in
        ""|0) with_desired=0 ;;
        1)   with_desired=1
             echo "Policy controller ON — it will publish @desired from" \
                  "{{rundir}}/fleet-policy.json5 to every host the catalog knows." ;;
        *) echo "error: desired must be 0 or 1, got '$with_desired'" >&2; exit 1 ;;
    esac
    # Build it up front (on-demand — it pulls the arrow/tonic stack) so the
    # sensors and GUI start together afterwards.
    if [[ -n "$rerun_mode" ]]; then
        cargo build {{relflag}} -p zensight-rerun
    fi
    # Sensors + correlator + historian via the shared spawner (same process
    # group, so the
    # trap below reaps them when the GUI exits or on Ctrl-C). They connect to
    # the GUI's loopback rendezvous (no multicast needed); logs in {{rundir}}/.
    BINDIR="{{bindir}}" CONFDIR="{{rundir}}" LOGDIR="{{rundir}}" \
    CONNECT="{{hub}}" WITH_CORRELATOR=1 WITH_HISTORIAN=1 WITH_DESIRED="$with_desired" \
    scripts/run-sensors.sh &
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
        ZENSIGHT_ZENOH_CONNECT="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight-rerun --config configs/rerun.json5 \
            --mode "$rerun_mode" --rrd-path {{rundir}}/zensight.rrd > {{rundir}}/rerun.log 2>&1 &
    fi
    sleep 1
    echo "Launching GUI (listening on {{hub}}; close it to stop everything)…"
    echo "GUI log → {{rundir}}/gui.log"
    # Capture the GUI's logs to a file we can inspect afterward, while still
    # echoing to the terminal. Override verbosity with RUST_LOG if needed.
    export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
    export RUST_LOG="${RUST_LOG:-info}"
    ZENSIGHT_ZENOH_LISTEN="{{hub}}" ZENSIGHT_ZENOH_SCOUTING=false {{bindir}}/zensight 2>&1 | tee {{rundir}}/gui.log

# ── Demo: exporters + a real TSDB / dashboard stack ──────────────────────────
#
# `just demo-prometheus` and `just demo-otel` are the two "one command, working
# demo" entry points for the exporters — which, until these landed, had NO run
# path at all: zero mentions in this justfile, one service in
# docker/docker-compose.yml, and not one occurrence of the word "exporter" in
# docs/DEPLOYMENT.md.
#
# THE TOPOLOGY. `just run` makes the GUI the Zenoh rendezvous (it LISTENS on
# {{hub}}; everything else CONNECTS). These demos are headless, so the EXPORTER
# plays that role instead: it listens on {{hub}} and scripts/run-sensors.sh
# points the full sensor set at it. Same shape, one fewer process, and no "start
# the GUI in another terminal first". If a hub is already up (you ran `just run`
# elsewhere), the recipe detects it and attaches as a spoke instead of fighting
# for the port.
#
# THE FOOTGUN THESE RECIPES DISARM. configs/{prometheus,otel}-exporter.json5
# both say `mode: "peer"` with `connect` COMMENTED OUT. Per
# zensight-common/src/config.rs a peer with no explicit connect gets multicast
# scouting ON — but every ZenSight demo path turns multicast OFF, deliberately
# (VPNs and extra interfaces make it unreliable, and on loopback it triggers a
# CONNECTION_TO_SELF error storm). Run the shipped config naively next to
# `just run` and the exporter finds nothing, silently, forever. Hence
# ZENSIGHT_ZENOH_{LISTEN,CONNECT,SCOUTING} on every line below.
#
# Both stacks bind host TCP 3000 and 9090, so they are MUTUALLY EXCLUSIVE.

# Deliberately NOT folded into `build`: `just run` does not need them, and they
# pull the OTLP/tonic stack.
#
# Build both exporters.
build-exporters:
    cargo build {{relflag}} -p zensight-exporter-prometheus -p zensight-exporter-otel

# Prometheus + Grafana demo: the full `just run` sensor set on the host, the
# Prometheus exporter on the host as the Zenoh rendezvous, Prometheus + Grafana
# in containers on the host network.
#
#   Grafana   http://127.0.0.1:3000   (anonymous; opens on ZenSight — Host overview)
#   Prom      http://127.0.0.1:9090   (target zensight-exporter must be UP)
#   /metrics  http://127.0.0.1:{{exporter_port}}/metrics
#
# Ctrl-C stops the exporter, the sensors AND the containers.
#
# Demo: sensors + Prometheus exporter on the host, Prometheus + Grafana in containers.
demo-prometheus: setup configure build-exporters
    #!/usr/bin/env bash
    set -euo pipefail
    compose="{{_compose}}"
    [[ -n "$compose" ]] || {
      echo "error: no compose front-end found (docker compose | podman compose | podman-compose)" >&2
      exit 1
    }
    # Is something already listening on the hub? (a `just run` GUI, or a
    # previous demo that did not tear down). If so, attach as a spoke.
    if (exec 3<>/dev/tcp/127.0.0.1/7447) 2>/dev/null; then
        exec 3>&-
        echo "Hub {{hub}} is already up — attaching the exporter as a spoke (not spawning sensors)."
        zenoh_env=(ZENSIGHT_ZENOH_CONNECT="{{hub}}")
        own_hub=0
    else
        echo "No hub on {{hub}} — the exporter will BE the rendezvous and spawn the sensors."
        zenoh_env=(ZENSIGHT_ZENOH_LISTEN="{{hub}}")
        own_hub=1
    fi
    $compose -f demo/prometheus/compose.yml up -d
    trap 'echo; echo "Stopping…"; '"$compose"' -f demo/prometheus/compose.yml down >/dev/null 2>&1 || true; kill 0' EXIT
    echo
    echo "  Grafana   http://127.0.0.1:3000   (ZenSight folder — provisioned)"
    echo "  Prom      http://127.0.0.1:9090   (target zensight-exporter must be UP)"
    echo "  /metrics  http://127.0.0.1:{{exporter_port}}/metrics"
    echo
    # The exporter first, so the listener exists before the sensors dial it.
    env "${zenoh_env[@]}" ZENSIGHT_ZENOH_SCOUTING=false \
        {{bindir}}/zensight-exporter-prometheus \
            --config {{rundir}}/prometheus-exporter.json5 \
            --listen 127.0.0.1:{{exporter_port}} 2>&1 | sed -u 's/^/[exporter] /' &
    sleep 1
    if [[ "$own_hub" == 1 ]]; then
        BINDIR="{{bindir}}" CONFDIR="{{rundir}}" LOGDIR="{{rundir}}" \
        CONNECT="{{hub}}" WITH_CORRELATOR=1 scripts/run-sensors.sh
    else
        wait
    fi

# OpenTelemetry demo: the same sensor set and the same rendezvous trick, with
# the OTel exporter pushing OTLP/gRPC to grafana/otel-lgtm (Collector +
# Prometheus + Tempo + Loki + Grafana in one container).
#
#   Grafana   http://127.0.0.1:3000   (Explore → Prometheus / Tempo / Loki)
#   OTLP      127.0.0.1:4317 (gRPC) · 127.0.0.1:4318 (HTTP)
#
# The exporter's shipped endpoint (configs/otel-exporter.json5) is already
# http://localhost:4317 with protocol "grpc" — nothing to override.
#
# Demo: sensors + OTel exporter on the host, grafana/otel-lgtm in one container.
demo-otel: setup configure build-exporters
    #!/usr/bin/env bash
    set -euo pipefail
    compose="{{_compose}}"
    [[ -n "$compose" ]] || { echo "error: no compose front-end found" >&2; exit 1; }
    if (exec 3<>/dev/tcp/127.0.0.1/7447) 2>/dev/null; then
        exec 3>&-
        echo "Hub {{hub}} is already up — attaching the exporter as a spoke (not spawning sensors)."
        zenoh_env=(ZENSIGHT_ZENOH_CONNECT="{{hub}}"); own_hub=0
    else
        echo "No hub on {{hub}} — the exporter will BE the rendezvous and spawn the sensors."
        zenoh_env=(ZENSIGHT_ZENOH_LISTEN="{{hub}}"); own_hub=1
    fi
    $compose -f demo/otel/compose.yml up -d
    trap 'echo; echo "Stopping…"; '"$compose"' -f demo/otel/compose.yml down >/dev/null 2>&1 || true; kill 0' EXIT
    echo
    echo "  Grafana   http://127.0.0.1:3000   (Explore → Prometheus / Tempo / Loki)"
    echo "  OTLP      127.0.0.1:4317 gRPC"
    echo
    # otel-lgtm needs a few seconds before its OTLP receiver binds. The exporter
    # retries anyway, but starting into a refused connection makes the log look
    # broken when it is merely early.
    sleep 5
    env "${zenoh_env[@]}" ZENSIGHT_ZENOH_SCOUTING=false \
        {{bindir}}/zensight-exporter-otel \
            --config {{rundir}}/otel-exporter.json5 2>&1 | sed -u 's/^/[exporter] /' &
    sleep 1
    if [[ "$own_hub" == 1 ]]; then
        BINDIR="{{bindir}}" CONFDIR="{{rundir}}" LOGDIR="{{rundir}}" \
        CONNECT="{{hub}}" WITH_CORRELATOR=1 scripts/run-sensors.sh
    else
        wait
    fi

# Safe when nothing is up. `just stop` handles the sensors; this adds the
# exporters and the containers.
#
# Tear down both demo stacks and anything they left running.
demo-stop: stop
    #!/usr/bin/env bash
    set -euo pipefail
    compose="{{_compose}}"
    if [[ -n "$compose" ]]; then
        $compose -f demo/prometheus/compose.yml down >/dev/null 2>&1 || true
        $compose -f demo/otel/compose.yml down >/dev/null 2>&1 || true
    fi
    pkill -f 'zensight-exporter-(prometheus|otel)' 2>/dev/null || true
    echo "Demo stacks stopped."

# Isolated ports, no containers, no sudo. This is what CI runs.
#
# Prove sensor -> Zenoh -> exporter -> /metrics works end to end.
demo-verify:
    scripts/demo-verify.sh

# ── The incident demo (#945) ─────────────────────────────────────────────────

# Prove the incident story in CI: a hypervisor dies, and the catalog says which alert that explains (#945)
demo-incident-verify:
    scripts/demo-incident-verify.sh

# WHAT THE INCIDENT DEMO IS FOR
#
# It is the one demo that shows what ZenSight does that a pile of series does
# not. On a healthy fleet the two look identical; the difference only appears
# when something breaks and one of them can say WHICH thing broke.
#
# Two synthetic hosts go on the bus — pve01 hosting vm101 — with a firing alert
# on the guest. For the first twenty seconds that alert is unexplained, which is
# exactly what Grafana would show you forever. Then pve01's liveliness token
# drops, and the catalog re-files the guest's alert as a `symptom_of` pve01.
#
# Run the GUI beside it to watch that happen (needs a display):
#
#   just demo-incident              # terminal 1: correlator + historian + the fault
#   just gui listen=tcp/127.0.0.1:17450   # terminal 2 — wrong: it must CONNECT
#   ZENSIGHT_ZENOH_CONNECT=tcp/127.0.0.1:17450 ZENSIGHT_ZENOH_SCOUTING=false \
#     just gui                      # terminal 2, joining the demo bus
#
# and `just demo-prometheus` in a third to see the same series with no
# relationship at all. That comparison is the demo.
#
# For the assertion rather than the picture, use `just demo-incident-verify` —
# it is the same fault with no GUI and an exit code, and it is what CI runs.

# A hypervisor dies and takes a guest with it — the five-minute demo (#945)
demo-incident: build
    #!/usr/bin/env bash
    set -euo pipefail
    hub="tcp/127.0.0.1:17450"
    echo "==> building the scripted fault"
    cargo build --locked -p zensight-correlator --example demo-incident >/dev/null
    trap 'kill $(jobs -p) 2>/dev/null || true' EXIT INT TERM
    echo "==> correlator (the catalog, and the rendezvous) on $hub"
    ZENSIGHT_ZENOH_MODE=peer ZENSIGHT_ZENOH_LISTEN="$hub" ZENSIGHT_ZENOH_CONNECT= \
        ZENSIGHT_ZENOH_SCOUTING=false \
        {{bindir}}/zensight-correlator --config {{rundir}}/correlator.json5 &
    sleep 3
    echo "==> historian (so the incident has a timeline behind it)"
    ZENSIGHT_ZENOH_CONNECT="$hub" ZENSIGHT_ZENOH_SCOUTING=false \
        {{bindir}}/zensight-historian --config {{rundir}}/historian.json5 &
    sleep 2
    echo
    echo "    Open the GUI against this bus to watch it happen:"
    echo "      ZENSIGHT_ZENOH_CONNECT=$hub ZENSIGHT_ZENOH_SCOUTING=false just gui"
    echo
    DEMO_CONNECT="$hub" FAULT_AFTER_SECS="${FAULT_AFTER_SECS:-30}" \
        HOLD_SECS="${HOLD_SECS:-300}" \
        cargo run --locked -q -p zensight-correlator --example demo-incident

# ── Fleet sizing ─────────────────────────────────────────────────────────────

# Eleven quadlet units say "MemoryMax below is a STARTING POINT … measure
# yours", and every sensor has published exactly those numbers in its health
# document since #811. This is the reading of them, as one command.
#
# Defaults to ten minutes against a local bus, which is a smoke test of the
# harness rather than a sizing run. The window is the whole point — #944 asks
# for fourteen days on the reference fleet:
#
#   WINDOW_SECS=1209600 HUB=tcp/<router>:7447 just fleet-sizing
#
# Run a soak under something that outlives your shell (systemd-run --user,
# tmux, nohup); the capture is written line by line, so a run killed on day
# nine still reports nine days.

# Measure what each sensor actually uses and print the sizing table (#944)
fleet-sizing *ARGS:
    scripts/fleet-sizing.sh {{ARGS}}

# ── Tests that need a flag you would not guess ───────────────────────────────

# The GUI test suite, on a renderer that survives 169 concurrent wgpu devices.
#
# WHY THIS RECIPE EXISTS (#687)
#
# `cargo test -p zensight` segfaults on a headless Linux box with Mesa
# installed and prints NOTHING while doing it: the process dies before libtest
# writes a result line, so there is no FAILED and no panic to grep for. The only
# available reading is "my change broke something", and it is wrong.
#
# It is NOT only the ui_tests target, which is what #687 and zensight/docs/
# testing.md originally recorded. The crate's OWN lib tests take the same path
# and crash MORE often: measured on master at 3 crashes in 10 runs of
# `cargo test -p zensight --lib`, against the ~1-in-7 the doc records for
# ui_tests. Hence `-p zensight` here rather than `--test ui_tests`: a recipe
# that covered half the affected targets would send someone chasing a phantom
# in the other half.
#
# `iced_test::simulator` stands up a real wgpu device; wgpu picks Vulkan; a
# GPU-less host resolves that to lavapipe, Mesa's software Vulkan; and many
# tests doing it at once crash inside the Vulkan loader, under
# `wgpu_core::snatch::SnatchLock`. Measured: ui_tests, 6 crashes in 40 runs by
# default and 0 in 40 with WGPU_BACKEND=gl; --lib, 3 in 10 by default and 0 in
# 10 with it.
#
# Deliberately NOT in .cargo/config.toml's [env] block: that applies to
# `cargo run` too, and downgrading the real GUI's renderer on every developer
# machine to fix a test-only problem is the wrong trade.
#
# CI is unaffected — the runner image ships no Vulkan ICD, so wgpu never takes
# this path there. Which also means a red `test` job on CI is NOT this, and
# should be read as a real failure.

# Since #829 the test binaries set WGPU_BACKEND=gl themselves (pre-main ctor
# guards in src/lib.rs and tests/ui_tests.rs), so a plain `cargo test -p
# zensight` is already safe; this recipe stays as the discoverable name for
# the story above, and as the belt to the guards' braces.

# The zensight crate's tests, on a renderer that survives concurrent wgpu devices (#687)
test-ui *ARGS:
    WGPU_BACKEND=gl cargo test -p zensight {{ARGS}}

# ── Container image ──────────────────────────────────────────────────────────

# Build the all-in-one sensors image (see docs/DEPLOYMENT.md for running it).
image:
    podman build -t zensight-sensors -f docker/Dockerfile.sensors .

# Verify the sensors image against a REAL bus (#472): the container joins an
# isolated hub under ONE `h-<12hex>` origin, answers `introspect` on every
# procedure it advertises (alive => callable, RFC 04 §5), and — the claim
# multi-machine deployment actually rests on — comes back under the SAME origin
# after `podman rm -f` + restart.
#
# That last one is why the `/etc/machine-id` mount in docs/DEPLOYMENT.md and
# docker-compose.yml is not optional: without it every start mints a fresh
# random origin and the catalog silently fills with ghost hosts.
#
# Needs rootful podman (the script defaults to `sudo podman` — host namespaces,
# CAP_NET_RAW and the journal mounts all need it) and zenohd at the workspace's
# zenoh version:
#
#   cargo install zenohd --version 1.10.0 --locked
#
# No storage plugins here, unlike `router-verify` — this hub only routes. The
# script builds the image itself from docker/Dockerfile.sensors (a full release
# build of five sensors inside the container: budget tens of minutes cold), and
# stands its own zenohd up on loopback:17447 — it never touches 7447.
image-verify:
    scripts/image-verify.sh

# Stop any running sensors + correlator started by `just run`.
stop:
    -pkill -f 'zensight-sensor-(netring|netlink|sysinfo|logs|systemd|hostspec|parallax)' || true
    -pkill -f 'zensight-correlator' || true
    -pkill -f 'zensight-rerun' || true

# Remove generated run configs and logs.
clean-run:
    rm -rf {{rundir}}

# ── Router storage verification (#471) ───────────────────────────────────────

# Verify configs/router-*.json5 against a REAL zenohd: state docs outlive their
# publisher, DELETE tombstones retire, blob chunks persist, every event record
# survives because each owns its ULID key (#583 — the claim behind that config
# choosing an `fs` volume over InfluxDB), `*` cannot match @catalog, and a fleet
# @rpc GET still fans in (no `complete` storage).
#
# Needs zenohd AND its plugins, all at the workspace's zenoh version (1.10).
# The plugins are cdylibs, not binaries — `cargo install` refuses them, and the
# volume name `fs` is NOT the crate name (it is zenoh-backend-*filesystem*):
#
#   cargo install zenohd --version 1.10.0 --locked
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
      echo "zenohd not on PATH — cargo install zenohd --version 1.10.0 --locked" >&2
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
router-plugins version="1.10.0":
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
