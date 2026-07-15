#!/usr/bin/env bash
# run-sensors.sh — spawn and supervise the local sensor set.
#
# The single spawner behind `just sensors`, `just run`, and the all-in-one
# sensors container image (docker/entrypoint-sensors.sh). Starts the host
# sensors (sysinfo, netlink, netring, logs, systemd — plus parallax where the
# binary exists) and optionally the identity correlator, anchors them to this
# process, and tears them all down on TERM/INT/EXIT.
#
# Parameterized by environment (all with local-dev defaults):
#   BINDIR           where the binaries live            (default target/release)
#   CONFDIR          where the *.json5 run configs live (default .run)
#   LOGDIR           per-sensor log dir, or "-" to interleave all output on
#                    stdout with a [name] prefix (container mode)
#                                                       (default .run)
#   CONNECT          Zenoh endpoint the sensors connect to
#                                                       (default tcp/127.0.0.1:7447)
#   WITH_CORRELATOR  1 = also run zensight-correlator   (default 0)
#   FAIL_FAST        1 = exit non-zero as soon as any child dies (container
#                    mode; restart is the orchestrator's job). 0 = keep the
#                    rest running, like `just run` always did.  (default 0)

set -euo pipefail

BINDIR="${BINDIR:-target/release}"
CONFDIR="${CONFDIR:-.run}"
LOGDIR="${LOGDIR:-.run}"
CONNECT="${CONNECT:-tcp/127.0.0.1:7447}"
WITH_CORRELATOR="${WITH_CORRELATOR:-0}"
FAIL_FAST="${FAIL_FAST:-0}"

# Sensors connect to the hub (`ZenohConfig::with_env_overrides` replaces the
# config file's `connect` list) instead of relying on multicast discovery,
# which is unreliable on hosts with a VPN or extra interfaces.
export ZENSIGHT_ZENOH_CONNECT="$CONNECT"

# Endpoints are pinned via CONNECT, so multicast scouting is pure noise here (it
# also risks joining a neighbour's live hub, and on loopback it triggers a
# CONNECTION_TO_SELF error storm). Gossip stays on, so hub spokes still discover
# each other. Overridable for anyone who genuinely wants multicast.
export ZENSIGHT_ZENOH_SCOUTING="${ZENSIGHT_ZENOH_SCOUTING:-false}"

spawn() {
    local bin="$1" cfg="$2" name
    name="${cfg%.json5}"
    if [[ "$LOGDIR" == "-" ]]; then
        # Interleave on stdout with a per-sensor prefix (container mode).
        "$BINDIR/$bin" --config "$CONFDIR/$cfg" 2>&1 | sed -u "s/^/[$name] /" &
    else
        "$BINDIR/$bin" --config "$CONFDIR/$cfg" > "$LOGDIR/$name.log" 2>&1 &
    fi
}

# Kill the whole process group on TERM/INT/EXIT so no sensor outlives the
# spawner (the `trap -` prevents re-entry when kill 0 signals ourselves).
trap 'trap - TERM INT EXIT; kill 0 2>/dev/null' TERM INT EXIT

extra=""
[[ "$WITH_CORRELATOR" == 1 ]] && extra=" + correlator"
echo "Starting sensors$extra (connecting to $CONNECT)…"
spawn zensight-sensor-sysinfo sysinfo.json5
spawn zensight-sensor-netlink netlink.json5
spawn zensight-sensor-netring netring.json5
spawn zensight-sensor-logs    logs.json5
spawn zensight-sensor-systemd systemd.json5
# parallax (live video: synthetic test pattern + local cameras) ships in local
# builds but not (yet) in the sensors container image — spawn it only when the
# binary exists so the image keeps working unchanged.
if [[ -x "$BINDIR/zensight-sensor-parallax" ]]; then
    spawn zensight-sensor-parallax parallax.json5
fi
if [[ "$WITH_CORRELATOR" == 1 ]]; then
    # The correlator fuses the sensors' identity evidence into HostEntity docs
    # (needs no capabilities). netring/netlink evidence feeds are on by default.
    spawn zensight-correlator correlator.json5
fi

if [[ "$FAIL_FAST" == 1 ]]; then
    # Container mode: the first child to exit takes the pod down non-zero so
    # the orchestrator's restart policy kicks in.
    wait -n
    echo "run-sensors: a sensor exited — stopping the rest" >&2
    exit 1
else
    # Local mode: keep the survivors running; Ctrl-C (or the caller's trap)
    # stops everything.
    wait
fi
