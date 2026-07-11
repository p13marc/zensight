#!/usr/bin/env bash
# Storage-cost measurement for the Rerun adapter (docs/plans/rerun/07-record-replay.md).
#
# For each duration: run the adapter in record mode on an isolated loopback
# session, drive the metrics demo (6 series, 500 ms ticks), and report the
# resulting .rrd size. Run from the repo root after `cargo build -p zensight-rerun --bins`.
set -euo pipefail

ENDPOINT="${ENDPOINT:-tcp/127.0.0.1:7459}"
OUT_DIR="${OUT_DIR:-$(mktemp -d)}"
mkdir -p "$OUT_DIR"
BIN=./target/debug
DURATIONS=("${@:-10 30 60}")
# shellcheck disable=SC2206 # intentional word split of the default list
DURATIONS=(${DURATIONS[@]})

echo "| duration (s) | points | .rrd bytes | bytes/point |"
echo "|---|---|---|---|"
for secs in "${DURATIONS[@]}"; do
    rrd="$OUT_DIR/cost-${secs}s.rrd"
    rm -f "$rrd"
    ZENSIGHT_ZENOH_LISTEN="$ENDPOINT" \
        "$BIN/zensight-rerun" --mode record --rrd-path "$rrd" --isolate \
        >"$OUT_DIR/adapter-${secs}s.log" 2>&1 &
    adapter=$!
    sleep 3
    points=$("$BIN/zensight-rerun-demo" --connect "$ENDPOINT" metrics \
        --duration-secs "$secs" --interval-ms 500 2>&1 |
        sed 's/\x1b\[[0-9;]*m//g' | grep -oE 'published=[0-9]+' | cut -d= -f2)
    sleep 2
    kill -TERM "$adapter" 2>/dev/null || true
    wait "$adapter" 2>/dev/null || true
    size=$(stat -c%s "$rrd")
    echo "| $secs | $points | $size | $((size / points)) |"
done
echo
echo "rrd files + adapter logs left in: $OUT_DIR"
