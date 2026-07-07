#!/usr/bin/env bash
# entrypoint-sensors.sh — entrypoint of the all-in-one sensors image
# (docker/Dockerfile.sensors). Validates the one supported knob
# (ZENSIGHT_ZENOH_CONNECT), warns about missing host mounts, detects the
# capture interface in the HOST network namespace (--net=host), generates the
# demo-max configs, then hands off to the shared spawner in fail-fast mode.
#
# See docs/DEPLOYMENT.md for the full `podman run` invocation.

set -euo pipefail

# ── 1. The one supported knob ────────────────────────────────────────────────
if [[ -z "${ZENSIGHT_ZENOH_CONNECT:-}" ]]; then
    cat >&2 <<'EOF'
ERROR: ZENSIGHT_ZENOH_CONNECT is required — the Zenoh endpoint the sensors
connect to (the machine running the ZenSight GUI, listening via
`just gui listen=tcp/0.0.0.0:7447`).

    podman run ... -e ZENSIGHT_ZENOH_CONNECT=tcp/<gui-host>:7447 ...

(For a single-host demo with --net=host: tcp/127.0.0.1:7447.)
EOF
    exit 64
fi
# ZENSIGHT_ZENOH_MODE / ZENSIGHT_ZENOH_LISTEN also pass through to the sensors
# (zensight-common honors them) but are unsupported for this image.

# ── 2. Host-mount / identity preflight (warn, don't fail — sensors degrade
#       gracefully; a missing mount only silences its sensor) ─────────────────
if [[ ! -s /etc/machine-id ]]; then
    echo "WARN: /etc/machine-id is missing or empty — host_id will be absent and" >&2
    echo "      the correlator can't anchor this host's identity." >&2
    echo "      Add: -v /etc/machine-id:/etc/machine-id:ro" >&2
fi
if [[ "$(hostname)" =~ ^[0-9a-f]{12}$ ]]; then
    echo "WARN: hostname '$(hostname)' looks like a container id — telemetry will" >&2
    echo "      carry it as the <source> segment. Add: --uts=host" >&2
fi
if [[ ! -S /run/dbus/system_bus_socket ]]; then
    echo "WARN: no /run/dbus/system_bus_socket — the systemd sensor will see no units." >&2
    echo "      Add: -v /run/dbus/system_bus_socket:/run/dbus/system_bus_socket:ro" >&2
fi
if [[ ! -d /var/log/journal && ! -d /run/log/journal ]]; then
    echo "WARN: no journal directory — the logs sensor will see nothing." >&2
    echo "      Add: -v /var/log/journal:/var/log/journal:ro (and /run/log/journal for volatile journals)" >&2
fi

# ── 3. Capture interface, detected at CONTAINER START in the host netns.
#       bookworm-slim has no iproute2, so parse /proc/net/route: the first
#       route with destination 00000000 is the default route. ──────────────────
IFACE=$(awk '$2 == "00000000" { print $1; exit }' /proc/net/route || true)
if [[ -z "$IFACE" ]]; then
    IFACE=$(ls /sys/class/net 2>/dev/null | grep -v '^lo$' | head -1 || true)
fi
IFACE="${IFACE:-lo}"
echo "netring capture interface: $IFACE"

# ── 4. Generate the demo-max configs (same profile as `just configure`;
#       no --snapshot-dir — the repo's docs/ doesn't exist in the image) ──────
/usr/local/bin/gen-configs.sh \
    --iface "$IFACE" \
    --outdir /run/zensight \
    --configs-dir /usr/share/zensight/configs

# ── 5. Hand off to the shared spawner: 5 sensors, no correlator (it runs once,
#       beside the GUI), interleaved logs on stdout, fail-fast so podman's
#       --restart policy handles recovery. ────────────────────────────────────
exec env \
    BINDIR=/usr/local/bin \
    CONFDIR=/run/zensight \
    LOGDIR=- \
    CONNECT="$ZENSIGHT_ZENOH_CONNECT" \
    WITH_CORRELATOR=0 \
    FAIL_FAST=1 \
    /usr/local/bin/run-sensors.sh
