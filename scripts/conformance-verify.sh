#!/usr/bin/env bash
# conformance-verify.sh — stand a real deployment up and let the RFC judges at it.
#
# WHY THIS EXISTS
#
# `cargo test --workspace` proves ZenSight's code agrees with itself. It cannot
# say whether what a *running* sensor puts on the wire agrees with the
# keyspace-v2 RFCs and with the registry TOMLs the same binary claims to serve:
# the served-vs-declared slice diff, `alive ⇒ callable` (RFC 04 §5), schema
# drift, declared-vs-observed QoS, cardinality budgets. Those are properties of
# a deployment, and only a deployment can be asked.
#
# This is the same argument demo-verify.sh makes about the exporters ("nothing
# in CI had ever executed an exporter"), one layer up: nothing in CI had ever
# asked a running ZenSight fleet whether it obeys its own contract.
#
# ISOLATION (the project rule, same as scripts/demo-verify.sh and
# scripts/image-verify.sh): this runs on its OWN rendezvous port with multicast
# OFF, on both sides. It never touches 7447 and never joins a live fleet. No
# containers, no zenohd, no sudo — the first sensor IS the rendezvous, so it is
# N+1 processes and no network beyond loopback.
#
# The judge is `zensight-conformance`; see zensight-conformance/README.md for
# what it gates on and what it deliberately does not.
set -euo pipefail

PORT="${PORT:-17447}"
HUB="tcp/127.0.0.1:${PORT}"
PROFILE="${PROFILE:-release}"
# The deployment: sysinfo + logs + systemd (producer slices) and the correlator
# (the `@catalog` service origin, whose verbatim `@` chunk is a structurally
# different introspect key — RFC 08 §6's property D4 — so running both covers
# both halves of the slice diff). Override to judge a bigger or smaller one.
#
# Widened from sysinfo-only in #815: every live producer is a producer whose
# served schemas, seeds and payloads actually get judged — a 2-producer
# deployment left 9 of 11 slices as describe-missing counts and most state
# families never exercised. logs and systemd are the safe additions: both are
# built for degraded hosts (logs runs without a journal; systemd without a
# system bus declares every procedure and says why — pinned by its
# no_system_bus test), so a CI container that has neither still keeps them on
# the roster. netring/netlink need capture/netlink privileges; snmp/gnmi/
# modbus/netflow/parallax need devices or protoc — they stay out of CI and in
# reach of a local `SENSORS=… scripts/conformance-verify.sh`.
SENSORS="${SENSORS:-sysinfo logs systemd}"
# The correlator (the `@catalog` service origin) is ON, as of #782.
#
# It was off, and that was a finding rather than a preference: its entities seed
# queryable replied untimestamped, which the deep freshness check correctly
# reported as `unstamped-state`. Both this comment and
# zensight-conformance/README.md promised that the day the correlator stamped
# its seed replies it would join the CI deployment with no change to the gate.
# #782 stamped them, and this is that day — `unstamped-state` was never in
# `gate::DEFAULT_EXCLUDED`, so nothing needed un-excluding.
#
# It earns its place beyond closing that loop: `@catalog` is a **service**
# origin, whose verbatim `@` chunk makes it a structurally different introspect
# key (RFC 08 §6, property D4), so running both covers both halves of the slice
# diff.
CORRELATOR="${CORRELATOR:-1}"
# The passive listening window: how long the doctor watches the data planes
# before judging what rode. Shorter in CI than a human would use — every
# second here is a second of a 2-lane runner.
FOR_SECS="${FOR_SECS:-12}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="${BINDIR:-target/${PROFILE}}"
relflag=""
[[ "$PROFILE" == "release" ]] && relflag="--release"

# The three fixes from #790 — a preflight that names the path it looked at,
# logs that outlive the exit trap, and a timeout message that can tell a dead
# child from an undiscovered one. Shared with demo-verify.sh because both had
# the same three defects.
# shellcheck source=lib/verify.sh
source "$ROOT/scripts/lib/verify.sh"

tmp=""
# PIDs of the processes we start, so cleanup can be surgical.
#
# NOT `kill 0`: that signals the whole process group, which includes this
# script and whatever invoked it (a `just` recipe, a CI step, an interactive
# shell). Kill our own children by pid and nothing else.
pids=()
cleanup() {
    local rc=$?
    for pid in "${pids[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
    for pid in "${pids[@]:-}"; do
        [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true
    done
    # Keep the evidence when a failure pointed at it (#790): every failure
    # message ends with "logs: $tmp/…", and deleting the directory on the way
    # out made that line a lie.
    if [[ -n "$tmp" ]]; then
        if [[ "$KEEP_TMP" == 1 ]]; then
            printf '\n(logs kept: %s)\n' "$tmp" >&2
        else
            rm -rf "$tmp"
        fi
    fi
    exit "$rc"
}
trap cleanup EXIT INT TERM

die() {
    printf '\nFAIL: %s\n' "$*" >&2
    exit 1
}

pkgs=(-p zensight-conformance)
for s in $SENSORS; do pkgs+=(-p "zensight-sensor-$s"); done
[[ "$CORRELATOR" == "1" ]] && pkgs+=(-p zensight-correlator)

echo "==> building ${pkgs[*]}"
cargo build $relflag --locked "${pkgs[@]}" >/dev/null

# `cargo build` says a binary exists somewhere. This says it exists HERE.
required=("$BIN/zensight-conformance")
for s in $SENSORS; do required+=("$BIN/zensight-sensor-$s"); done
[[ "$CORRELATOR" == "1" ]] && required+=("$BIN/zensight-correlator")
require_bins "${required[@]}"

tmp="$(mktemp -d)"
echo "==> generating configs into $tmp"
# The same generator every other run path uses — a hand-written config here
# would be a second definition of what a ZenSight deployment looks like, and
# the conformance judgement would then be about that instead.
scripts/gen-configs.sh \
    --iface lo \
    --outdir "$tmp" \
    --configs-dir "$ROOT/configs" >/dev/null

# ---------------------------------------------------------------------------
# The FIRST sensor is the rendezvous, exactly as the exporter is in
# demo-verify.sh. SCOUTING=false because multicast on loopback triggers a
# CONNECTION_TO_SELF error storm, and because pinned endpoints make it noise.
# Gossip stays on (peer mode, #626): it is what lets the spokes route to each
# other through the hub without a router in the picture.
# ---------------------------------------------------------------------------
first=1
for s in $SENSORS; do
    if [[ $first == 1 ]]; then
        echo "==> starting $s sensor (rendezvous on $HUB)"
        ZENSIGHT_ZENOH_LISTEN="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
            "$BIN/zensight-sensor-$s" --config "$tmp/$s.json5" \
            >"$tmp/$s.log" 2>&1 &
        first=0
    else
        echo "==> starting $s sensor"
        ZENSIGHT_ZENOH_CONNECT="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
            "$BIN/zensight-sensor-$s" --config "$tmp/$s.json5" \
            >"$tmp/$s.log" 2>&1 &
    fi
    pids+=($!)
done

if [[ "$CORRELATOR" == "1" ]]; then
    echo "==> starting correlator (the @catalog service origin)"
    ZENSIGHT_ZENOH_CONNECT="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
        "$BIN/zensight-correlator" --config "$tmp/correlator.json5" \
        >"$tmp/correlator.log" 2>&1 &
    pids+=($!)
fi

expected=$(wc -w <<<"$SENSORS")
[[ "$CORRELATOR" == "1" ]] && expected=$((expected + 1))

# ---------------------------------------------------------------------------
# Wait for the roster, not for a fixed sleep. The judge itself is the probe:
# a shallow, listen-less run is one liveliness query and costs nothing, and
# using it here means the wait fails for exactly the reasons the real run
# would.
# ---------------------------------------------------------------------------
judge() {
    "$BIN/zensight-conformance" \
        --connect "$HUB" \
        --registry "$ROOT/zensight-common/registry" \
        "$@"
}

echo "==> waiting for $expected producer(s) on the liveliness roster"
live=0
for _ in $(seq 40); do
    # Two statements, not one pipeline: `set -o pipefail` is on, and the judge
    # exits 2 while the roster is still empty — piping it straight into python
    # would take that 2 as the *pipeline's* status and fire the `|| echo 0`
    # fallback on top of the number python had already printed.
    probe=$(judge --shallow --for 0 --timeout 1 --json 2>/dev/null) || true
    live=$(python3 -c \
        'import json,sys; print(json.load(sys.stdin)["report"]["live_producers"])' \
        <<<"$probe" 2>/dev/null) || live=0
    [[ "$live" -ge "$expected" ]] && break
    # Do not wait out 40s for a process that has already exited (#790). The
    # roster can only grow while something is alive to join it.
    still_running "${pids[@]:-}" || break
    sleep 1
done
if [[ "$live" -lt "$expected" ]]; then
    keep_logs_on_failure
    # A dead child and an undiscovered one are different failures and used to
    # render identically (#790). Ask before diagnosing.
    dead=$(dead_children "${pids[@]:-}")
    if [[ -n "$dead" ]]; then
        die "only $live of $expected producer(s) reached the roster, and \
$(wc -l <<<"$dead") of the processes this script started is/are already gone.

This is NOT a discovery problem — a process that has exited cannot be
discovered. Its log says why.
$(logs_note "$tmp" "$tmp"/*.log)"
    fi
    die "only $live of $expected producer(s) reached the roster in 40s, and \
every process this script started is still alive.

Everything is running and nothing found anything, which is the discovery
failure: the shipped configs say mode:\"peer\" with \`connect\` commented out,
which means multicast — and every isolated run path turns multicast off. Check
that ZENSIGHT_ZENOH_{LISTEN,CONNECT,SCOUTING} are set on every process.
$(logs_note "$tmp" "$tmp"/*.log)"
fi

# ---------------------------------------------------------------------------
# The real run. Deep checks on, a passive listen window on, --strict-window
# so a window that shed samples is unobservable rather than quietly clean
# (#845), and the gate at its default floor (`warning`).
# ---------------------------------------------------------------------------
echo "==> judging the deployment (deep, ${FOR_SECS}s listen window)"
set +e
judge --for "$FOR_SECS" --strict-window | tee "$tmp/report.txt"
rc=${PIPESTATUS[0]}
set -e

# A non-conforming deployment's logs and report are evidence too, and the
# report was being tee'd into the directory the exit trap deletes (#790).
[[ "$rc" == 0 ]] || keep_logs_on_failure

case "$rc" in
    0) ;;
    1) die "the deployment does not conform — see the GATED section above.
       The full report and every producer log are in the directory named below." ;;
    2) die "the run could not carry a verdict (unobservable, or the checks never ran).
       The full report and every producer log are in the directory named below." ;;
    *) die "zensight-conformance exited $rc." ;;
esac

echo
echo "OK — $live producer(s) judged against the keyspace-v2 RFCs and the"
echo "     in-tree registry; no gated findings."
