#!/usr/bin/env bash
# run-sensors.sh — spawn and supervise the local sensor set. THE BUNDLE IS A
# DEMO (#813): the fleet unit is one container/unit per sensor, each with its
# own MemoryMax and restart policy (packaging/quadlet/, docs/DEPLOYMENT.md);
# this script exists for `just run`, `just sensors` and the all-in-one demo
# image, and is built so one sensor's crash can never blank the rest.
#
# Starts the selected sensors (default: every one whose binary exists —
# sysinfo, netlink, netring, logs, systemd, hostspec, plus parallax where
# built) and optionally the identity correlator, anchors them to this
# process, supervises each child with restart-and-backoff, and tears
# everything down on TERM/INT/EXIT.
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
#   ZENSIGHT_SENSORS comma/space-separated subset to run (#813) — e.g.
#                    "sysinfo,systemd,logs" on a box where netring holds
#                    319 MB to watch no traffic. Default: all present.
#   MAX_RESTARTS     per-child restart budget before that child is given up
#                    on (the REST keep running)          (default 5)
#
# FAIL_FAST is GONE (#813): it made the first crash take down the four
# sensors that would have explained it — vm-edge, 2026-08-17. Supervision is
# per-child now: exponential backoff (2,4,8,… capped 60s), restarts logged,
# a child that exhausts MAX_RESTARTS is dropped while the rest carry on.

set -euo pipefail

BINDIR="${BINDIR:-target/release}"
CONFDIR="${CONFDIR:-.run}"
LOGDIR="${LOGDIR:-.run}"
CONNECT="${CONNECT:-tcp/127.0.0.1:7447}"
WITH_CORRELATOR="${WITH_CORRELATOR:-0}"
ZENSIGHT_SENSORS="${ZENSIGHT_SENSORS:-}"
MAX_RESTARTS="${MAX_RESTARTS:-5}"

# Is this sensor selected? Empty selection = all.
selected() {
    [[ -z "$ZENSIGHT_SENSORS" ]] && return 0
    local want
    for want in ${ZENSIGHT_SENSORS//,/ }; do
        [[ "$want" == "$1" ]] && return 0
    done
    return 1
}

# Sensors connect to the hub (`ZenohConfig::with_env_overrides` replaces the
# config file's `connect` list) instead of relying on multicast discovery,
# which is unreliable on hosts with a VPN or extra interfaces.
export ZENSIGHT_ZENOH_CONNECT="$CONNECT"

# Endpoints are pinned via CONNECT, so multicast scouting is pure noise here (it
# also risks joining a neighbour's live hub, and on loopback it triggers a
# CONNECTION_TO_SELF error storm). Gossip stays on, so hub spokes still discover
# each other. Overridable for anyone who genuinely wants multicast.
export ZENSIGHT_ZENOH_SCOUTING="${ZENSIGHT_ZENOH_SCOUTING:-false}"

# Supervise one child: run, and on unexpected exit restart with exponential
# backoff (2,4,8,… capped 60s) up to MAX_RESTARTS — then give up on THIS
# child while the rest carry on. One sensor's crash must never blank the
# four that would explain it (#813).
supervise() {
    local bin="$1" cfg="$2" name attempt=0 delay rc
    name="${cfg%.json5}"
    while true; do
        # `set -e` is script-global and would kill THIS supervisor at the
        # child's first non-zero exit — the exact opposite of supervision.
        # Suspend it around the child only.
        if [[ "$LOGDIR" == "-" ]]; then
            # Interleave on stdout with a per-sensor prefix (container mode).
            set +e
            "$BINDIR/$bin" --config "$CONFDIR/$cfg" 2>&1 | sed -u "s/^/[$name] /"
            rc=${PIPESTATUS[0]}
            set -e
        else
            set +e
            "$BINDIR/$bin" --config "$CONFDIR/$cfg" >> "$LOGDIR/$name.log" 2>&1
            rc=$?
            set -e
        fi
        attempt=$((attempt + 1))
        if (( attempt > MAX_RESTARTS )); then
            echo "run-sensors: $name exited (rc=$rc) — restart budget ($MAX_RESTARTS) spent, giving up on it (the rest keep running)" >&2
            return 1
        fi
        delay=$(( 2 ** attempt )); (( delay > 60 )) && delay=60
        echo "run-sensors: $name exited (rc=$rc) — restart $attempt/$MAX_RESTARTS in ${delay}s" >&2
        sleep "$delay"
    done
}

spawn() {
    local bin="$1" cfg="$2" name
    name="${cfg%.json5}"
    if ! selected "$name"; then
        echo "run-sensors: $name not in ZENSIGHT_SENSORS — skipped"
        return 0
    fi
    supervise "$bin" "$cfg" &
}

# Kill the whole process group on TERM/INT/EXIT so no sensor outlives the
# spawner (the `trap -` prevents re-entry when kill 0 signals ourselves).
trap 'trap - TERM INT EXIT; kill 0 2>/dev/null' TERM INT EXIT

extra=""
[[ "$WITH_CORRELATOR" == 1 ]] && extra=" + correlator"
sel="${ZENSIGHT_SENSORS:-all}"
echo "Starting sensors [$sel]$extra (connecting to $CONNECT)…"
spawn zensight-sensor-sysinfo sysinfo.json5
spawn zensight-sensor-netlink netlink.json5
spawn zensight-sensor-netring netring.json5
spawn zensight-sensor-logs    logs.json5
spawn zensight-sensor-systemd systemd.json5
spawn zensight-sensor-hostspec hostspec.json5
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

# Each child is supervised individually; the script itself only ends on
# TERM/INT (the trap) or when every supervisor has given up.
wait
