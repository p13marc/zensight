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
# The deployment: the sysinfo sensor (a producer slice) and the correlator (the
# `@catalog` service origin, whose verbatim `@` chunk is a structurally
# different introspect key — RFC 08 §6's property D4 — so running both covers
# both halves of the slice diff). Override to judge a bigger one.
SENSORS="${SENSORS:-sysinfo}"
# The correlator (the `@catalog` service origin) is OFF by default, and that is
# a finding rather than a preference — see zensight-conformance/README.md,
# "Known findings": its entities seed queryable replies untimestamped, which the
# deep freshness check correctly reports as `unstamped-state`. It is not
# excluded from the gate, so `CORRELATOR=1` reproduces the finding and the day
# the correlator stamps its seed replies this becomes the CI default with no
# change to the gate.
CORRELATOR="${CORRELATOR:-0}"
# The passive listening window: how long the doctor watches the data planes
# before judging what rode. Shorter in CI than a human would use — every
# second here is a second of a 2-lane runner.
FOR_SECS="${FOR_SECS:-12}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="${BINDIR:-target/${PROFILE}}"
relflag=""
[[ "$PROFILE" == "release" ]] && relflag="--release"

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
    [[ -n "$tmp" ]] && rm -rf "$tmp"
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
cargo build $relflag "${pkgs[@]}" >/dev/null

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
    sleep 1
done
[[ "$live" -ge "$expected" ]] || die \
"only $live of $expected producer(s) reached the roster in 40s.

This is the demo's #1 failure and it is ALMOST ALWAYS Zenoh discovery: the
shipped configs say mode:\"peer\" with \`connect\` commented out, which means
multicast — and every isolated run path turns multicast off. Check that
ZENSIGHT_ZENOH_{LISTEN,CONNECT,SCOUTING} are set on every process.

  logs: $tmp/*.log"

# ---------------------------------------------------------------------------
# The real run. Deep checks on, a passive listen window on, and the gate at
# its default floor (`warning`, minus the exclusions the crate documents).
# ---------------------------------------------------------------------------
echo "==> judging the deployment (deep, ${FOR_SECS}s listen window)"
set +e
judge --for "$FOR_SECS" | tee "$tmp/report.txt"
rc=${PIPESTATUS[0]}
set -e

case "$rc" in
    0) ;;
    1) die "the deployment does not conform — see the GATED section above." ;;
    2) die "the run could not carry a verdict (unobservable, or the checks never ran)." ;;
    *) die "zensight-conformance exited $rc." ;;
esac

echo
echo "OK — $live producer(s) judged against the keyspace-v2 RFCs and the"
echo "     in-tree registry; no gated findings."
