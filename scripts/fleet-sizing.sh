#!/usr/bin/env bash
# fleet-sizing.sh — watch a live fleet's health documents and turn them into the
# sizing table `docs/ops/SIZING.md` is waiting for.
#
# WHY THIS EXISTS (#944)
#
# Eleven quadlet units in `packaging/quadlet/` carry the identical comment:
#
#     # MemoryMax below is a STARTING POINT (reference-fleet sizing); measure
#     # via each sensor's health doc `self_stats`
#
# Every sensor has published exactly that since #811 — RSS, CPU, the declared
# budget, per-table occupancy, the shed ladder's step, and the process's own
# cgroup reading including `memory.max` and `oom_kills`. The reason no measured
# table ever replaced the starting points is not that the data is missing; it is
# that collecting it meant reading a document per host, by hand, repeatedly, for
# as long as the window you wanted. So nobody did, and on 2026-08-17 a VM was
# OOM-killed under numbers that had been chosen on a laptop.
#
# This is that collection, as one command. It measures; it decides nothing.
# `fleet-sizing-report.py` turns a run into markdown and is deliberately a
# separate step, so a fourteen-day run is re-analysable without being re-run.
#
# WHY IT SUBSCRIBES RATHER THAN POLLS
#
# The obvious shape — GET `v1/*/state/*/health` on a timer — does not work, and
# fails in the quiet direction: **the health document has no late-joiner seed**,
# so a GET on that selector returns zero replies against a perfectly healthy
# fleet. It does not need one; the runner republishes health every five seconds,
# so a subscriber converges in five seconds. `state_watch` is that subscriber,
# and it sees every tick rather than whichever instants a poller woke on.
#
# WHAT IT IS NOT
#
# Not a monitor and not a service: a client that subscribes to one state
# selector and writes what arrives. It publishes nothing, joins no fleet beyond
# the endpoint it dials (scouting fully off), and keeps no state between runs.
# Point it at production; it is a reader.
#
# THE WINDOW IS THE WHOLE POINT. #944 asks for fourteen days on the six-VM
# reference fleet. A ten-minute run produces the same table with advice worth
# far less, so the window is recorded in the run and printed in the report — a
# table can never be read without the window it came from.
#
# USAGE
#
#   scripts/fleet-sizing.sh                          # 10 minutes, local bus
#   WINDOW_SECS=1209600 scripts/fleet-sizing.sh      # the #944 fourteen-day soak
#   HUB=tcp/10.0.0.1:7447 scripts/fleet-sizing.sh    # a fleet that is not local
#
#   HUB                    router/peer to dial          (default tcp/127.0.0.1:7447)
#   WINDOW_SECS            how long to watch            (default 600)
#   SAMPLE_INTERVAL_SECS   per-key throttle             (default 60)
#   HEADROOM               peak multiplier for the cap  (default 1.5)
#   OUTDIR                 where the run is written     (default target/fleet-sizing/<ts>)
#   PROFILE                debug | release              (default release)
#   SKIP_HISTORIAN         1 to not ask for historian stats
#
# Health ticks every 5 s, so the throttle is what keeps a long soak small:
# fourteen days at one sample per minute is ~20 000 lines per producer, where
# every tick would be a quarter of a million. It throttles per KEY, so the
# fleet's rarest publisher is never crowded out by its noisiest.
#
# Run a long soak under something that outlives your shell — `systemd-run --user
# --unit=fleet-sizing`, tmux, or nohup. The capture is written line by line, so a
# run killed on day nine still reports nine days.

set -euo pipefail

cd "$(dirname "$0")/.."
# shellcheck source=lib/verify.sh
source scripts/lib/verify.sh

HUB="${HUB:-tcp/127.0.0.1:7447}"
WINDOW_SECS="${WINDOW_SECS:-600}"
SAMPLE_INTERVAL_SECS="${SAMPLE_INTERVAL_SECS:-60}"
HEADROOM="${HEADROOM:-1.5}"
PROFILE="${PROFILE:-release}"
SKIP_HISTORIAN="${SKIP_HISTORIAN:-0}"
OUTDIR="${OUTDIR:-target/fleet-sizing/$(date -u +%Y%m%dT%H%M%SZ)}"

relflag=""
[[ "$PROFILE" == "release" ]] && relflag="--release"
BIN="${BINDIR:-target/${PROFILE}}"

echo "==> building the readers"
# Examples, not binaries: one-shot clients that are test fixtures with a `main`.
# Shipping them in the release tarball would suggest otherwise — the same
# reasoning demo-verify.sh applies to the same two tools.
cargo build $relflag --locked -p zensight-common --example state_watch >/dev/null
want=("$BIN/examples/state_watch")
if [[ "$SKIP_HISTORIAN" != "1" ]]; then
    cargo build $relflag --locked -p zensight-historian --example historian-query >/dev/null
    want+=("$BIN/examples/historian-query")
fi
require_bins "${want[@]}"

mkdir -p "$OUTDIR"
started_epoch=$(date -u +%s)
started_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)

echo "==> watching v1/*/state/*/health on $HUB for ${WINDOW_SECS}s"
echo "    at most one sample per producer per ${SAMPLE_INTERVAL_SECS}s -> $OUTDIR/health.ndjson"

watch_rc=0
PROBE_CONNECT="$HUB" \
    WATCH_SECS="$WINDOW_SECS" \
    WATCH_MIN_INTERVAL_SECS="$SAMPLE_INTERVAL_SECS" \
    "$BIN/examples/state_watch" 'v1/*/state/*/health' \
    >"$OUTDIR/health.ndjson" 2>"$OUTDIR/state_watch.log" || watch_rc=$?

tail -1 "$OUTDIR/state_watch.log" || true

# state_watch exits non-zero when NOTHING arrived. That is the failure worth
# stopping on and worth explaining here rather than in a report the operator has
# to reach: every sensor publishes health every five seconds, so an empty
# capture is a bus with no sensors or the wrong endpoint — never a fleet that
# happens to use no memory.
if (( watch_rc == 2 )); then
    cat >&2 <<EOF

FAIL: could not open a Zenoh session to $HUB — nothing is listening there.

This is the wrong-endpoint failure, not the quiet-fleet one: the run never
reached a bus at all. Check the address and that the router or peer is up.

state_watch's own output: $OUTDIR/state_watch.log
EOF
    exit 1
fi
if (( watch_rc != 0 )); then
    cat >&2 <<EOF

FAIL: reached $HUB, but nothing published 'v1/*/state/*/health' in ${WINDOW_SECS}s.

  - is a sensor running?    just sensors    (or: systemctl status 'zensight-sensor-*')
  - is this the right hub?  HUB=tcp/<host>:7447 scripts/fleet-sizing.sh
  - what IS on the bus?     PROBE_CONNECT=$HUB WATCH_SECS=15 \\
                              cargo run -p zensight-common --example state_watch -- 'v1/**'

state_watch's own output: $OUTDIR/state_watch.log
EOF
    exit 1
fi

if [[ "$SKIP_HISTORIAN" != "1" ]]; then
    echo "==> asking the historian for its stats"
    # Not fatal: a fleet without a historian is a supported deployment. The file
    # is left behind even when empty, because the report distinguishes "asked,
    # nobody answered" from "not asked".
    "$BIN/examples/historian-query" -c "$HUB" --timeout 5 \
        'v1/*/@rpc/historian/stats' >"$OUTDIR/historian-stats.txt" \
        2>"$OUTDIR/historian-query.log" || true
fi

ended_epoch=$(date -u +%s)
elapsed=$(( ended_epoch - started_epoch ))
if (( elapsed >= 172800 )); then
    window_human="$(( elapsed / 86400 ))d $(( (elapsed % 86400) / 3600 ))h"
elif (( elapsed >= 3600 )); then
    window_human="$(( elapsed / 3600 ))h $(( (elapsed % 3600) / 60 ))m"
else
    window_human="$(( elapsed / 60 ))m $(( elapsed % 60 ))s"
fi

cat >"$OUTDIR/run.json" <<EOF
{
  "hub": "$HUB",
  "window_secs": $WINDOW_SECS,
  "sample_interval_secs": $SAMPLE_INTERVAL_SECS,
  "headroom": $HEADROOM,
  "started_utc": "$started_utc",
  "ended_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "elapsed_secs": $elapsed,
  "window_human": "$window_human"
}
EOF

echo "==> report"
echo
scripts/fleet-sizing-report.py "$OUTDIR" | tee "$OUTDIR/REPORT.md"
echo
echo "==> run kept at $OUTDIR — re-analyse without re-running:"
echo "    scripts/fleet-sizing-report.py $OUTDIR"
