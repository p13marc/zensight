#!/usr/bin/env bash
# record-fixtures.sh — regenerate the GUI's `.zrec` decode-fixture corpus (#747).
#
# Stands up the same isolated deployment as conformance-verify.sh (its stanza,
# deliberately: one definition of what a ZenSight deployment looks like), then
# runs `zensight-conformance --record-zrec` once per fixture instead of the
# judge. The captures land in zensight/tests/fixtures/zrec/ and are consumed by
# zensight/tests/zrec_fixtures.rs and the replay tests in zensight/src/app.rs.
#
# MANUAL ONLY — never wired into CI. The corpus is a *pinned* regression input:
# CI replays the checked-in bytes; a capture that regenerated per run would be
# testing the weather. The regeneration contract (zensight/docs/testing.md):
# run this script, then `cargo test -p zensight` must pass unchanged — the
# fixture tests assert only capture-stable facts.
#
# ETIQUETTE: the recorded traffic is real sensor output on an isolated
# loopback port, so RFC 09 §5.3's synthetic marker does not apply to the
# capture. Anything that ever *re-publishes* these rows onto a bus owes that
# marker and the rest of the replay obligations; the GUI's replay module is
# session-less and cannot.
#
# ISOLATION: own rendezvous port, multicast off on both sides, never 7447 —
# the project rule, same as conformance-verify.sh where it is explained.
set -euo pipefail

PORT="${PORT:-17447}"
HUB="tcp/127.0.0.1:${PORT}"
PROFILE="${PROFILE:-release}"
SENSORS="${SENSORS:-sysinfo logs systemd}"
CORRELATOR="${CORRELATOR:-1}"
# The capture window. Health refresh cadence is what paces the state plane, so
# give it long enough to see at least one full refresh from every producer.
FOR_SECS="${FOR_SECS:-20}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

OUTDIR="${OUTDIR:-$ROOT/zensight/tests/fixtures/zrec}"

BIN="${BINDIR:-target/${PROFILE}}"
relflag=""
[[ "$PROFILE" == "release" ]] && relflag="--release"

# shellcheck source=lib/verify.sh
source "$ROOT/scripts/lib/verify.sh"

tmp=""
pids=()
cleanup() {
    local rc=$?
    for pid in "${pids[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
    for pid in "${pids[@]:-}"; do
        [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true
    done
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
cargo build $relflag "${pkgs[@]}" >/dev/null

required=("$BIN/zensight-conformance")
for s in $SENSORS; do required+=("$BIN/zensight-sensor-$s"); done
[[ "$CORRELATOR" == "1" ]] && required+=("$BIN/zensight-correlator")
require_bins "${required[@]}"

tmp="$(mktemp -d)"
echo "==> generating configs into $tmp"
scripts/gen-configs.sh \
    --iface lo \
    --outdir "$tmp" \
    --configs-dir "$ROOT/configs" >/dev/null

# The rendezvous first, then the RECORDERS, then everything else. Order is
# load-bearing: the correlator recovers the sensors' cached self-evidence via
# `history()` the moment it starts and publishes its entity docs ~1s later —
# once, not on a cadence (re-emit is 60s). A recorder that connects after the
# roster settles has already missed that publication, which is exactly how the
# first cut of this script recorded an empty catalog capture. So the captures
# open before the fleet finishes coming up, and the corpus deliberately
# includes the start-up burst — the richest decode traffic there is.
first_sensor=$(awk '{print $1}' <<<"$SENSORS")
rest_sensors=$(cut -d' ' -f2- <<<"$SENSORS ")

echo "==> starting $first_sensor sensor (rendezvous on $HUB)"
ZENSIGHT_ZENOH_LISTEN="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
    "$BIN/zensight-sensor-$first_sensor" --config "$tmp/$first_sensor.json5" \
    >"$tmp/$first_sensor.log" 2>&1 &
pids+=($!)

# ---------------------------------------------------------------------------
# The captures. Concurrent, one recorder per fixture. `*` never matches the
# verbatim `@catalog` origin (grammar D4), which is why the catalog needs its
# own capture rather than falling out of `v1/*/state/**`.
# ---------------------------------------------------------------------------
mkdir -p "$OUTDIR"
echo "==> recording 3 captures (${FOR_SECS}s window) into $OUTDIR"
rec_pids=()
"$BIN/zensight-conformance" --connect "$HUB" --for "$FOR_SECS" \
    --record-zrec "$OUTDIR/sysinfo-telemetry.zrec" \
    --record-selector 'v1/*/telemetry/sysinfo/**' \
    --record-max 150 >"$tmp/rec-telemetry.log" 2>&1 &
rec_pids+=($!)
"$BIN/zensight-conformance" --connect "$HUB" --for "$FOR_SECS" \
    --record-zrec "$OUTDIR/state-plane.zrec" \
    --record-selector 'v1/*/state/**' \
    --record-max 300 >"$tmp/rec-state.log" 2>&1 &
rec_pids+=($!)
"$BIN/zensight-conformance" --connect "$HUB" --for "$FOR_SECS" \
    --record-zrec "$OUTDIR/catalog-entities.zrec" \
    --record-selector 'v1/@catalog/state/**' \
    --record-max 100 >"$tmp/rec-catalog.log" 2>&1 &
rec_pids+=($!)

# Give the recorders a moment to declare their subscribers before the rest of
# the fleet starts publishing.
sleep 2

for s in $rest_sensors; do
    echo "==> starting $s sensor"
    ZENSIGHT_ZENOH_CONNECT="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
        "$BIN/zensight-sensor-$s" --config "$tmp/$s.json5" \
        >"$tmp/$s.log" 2>&1 &
    pids+=($!)
done

if [[ "$CORRELATOR" == "1" ]]; then
    echo "==> starting correlator (the @catalog service origin)"
    ZENSIGHT_ZENOH_CONNECT="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
        "$BIN/zensight-correlator" --config "$tmp/correlator.json5" \
        >"$tmp/correlator.log" 2>&1 &
    pids+=($!)
fi

rc=0
for pid in "${rec_pids[@]}"; do
    wait "$pid" || rc=1
done
if [[ "$rc" != 0 ]]; then
    keep_logs_on_failure
    cat "$tmp"/rec-*.log >&2 || true
    die "a capture failed or held no samples — the corpus was NOT fully regenerated.
$(logs_note "$tmp" "$tmp"/*.log)"
fi

cat "$tmp"/rec-*.log
echo
echo "OK — corpus regenerated. Now run: cargo test -p zensight"
echo "     (the fixture tests must pass unchanged — that is the contract)"
