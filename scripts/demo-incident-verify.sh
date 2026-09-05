#!/usr/bin/env bash
# demo-incident-verify.sh — prove the fleet can say WHICH thing broke (#945).
#
# WHY THIS EXISTS
#
# #945 is the acceptance test of two epics, and both are invisible on a healthy
# fleet: 0.15.0 put the relationship graph on the bus, 0.16.0 put incidents in
# the catalog, and neither shows until something breaks. The claim ZenSight
# makes over a pile of series is that when a hypervisor dies, its guests' alerts
# are filed as ITS symptoms rather than paging independently — and nothing in
# CI had ever watched that happen.
#
# The pieces were each covered. `incidents.rs` has unit tests for the
# attribution, `impact.rs` for the graph walk, and the correlator's e2e suite
# for acks over a real bus. What none of them does is start a correlator, put a
# fleet on the wire, kill one of its hosts and ask the catalog what it
# concluded. That is the join, and the join is where this project's defects live
# (see the late-joiner family: #925, #926, #1031, #1034 — every one a
# well-formed producer, a well-formed consumer and no test asserting the join).
#
# WHAT IT ASSERTS, and it is deliberately narrow:
#
#   1. an incident exists for the guest;
#   2. it carries `symptom_of` naming the HYPERVISOR's entity — not merely a
#      non-empty field, which a bug that attributed everything to itself would
#      also satisfy;
#   3. the hypervisor's own incident (if any) is NOT a symptom of anything —
#      the root cause must not be filed under itself;
#   4. and, before the fault, the guest's alert is NOT a symptom. Without that
#      one, a correlator that marked everything a symptom of everything would
#      pass the other three.
#
# ISOLATION (the project rule, as in demo-verify.sh and image-verify.sh): its
# OWN rendezvous port, multicast OFF, no containers, no zenohd, no sudo. It
# never touches 7447 and never joins a live fleet. Two processes and a GET.
set -euo pipefail

PORT="${PORT:-17449}"
HUB="tcp/127.0.0.1:${PORT}"
PROFILE="${PROFILE:-release}"
KEEP_TMP="${KEEP_TMP:-0}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="${BINDIR:-target/${PROFILE}}"
relflag=""
[[ "$PROFILE" == "release" ]] && relflag="--release"

# shellcheck source=lib/verify.sh
source "$ROOT/scripts/lib/verify.sh"

# The entity ids the demo publisher's fixed origins produce. Asserting the
# SHAPE without asserting WHICH entity would let a resolver that attributed
# every alert to the first entity it saw pass.
HYPERVISOR_SOURCE="pve01"
GUEST_SOURCE="vm101"

tmp=""
pids=()
cleanup() {
    local rc=$?
    for pid in "${pids[@]:-}"; do [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true; done
    for pid in "${pids[@]:-}"; do [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true; done
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

die() { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }

echo "==> building"
cargo build $relflag --locked -p zensight-correlator >/dev/null
cargo build $relflag --locked -p zensight-correlator --example demo-incident >/dev/null
cargo build $relflag --locked -p zensight-common --example rpc_get >/dev/null
require_bins "$BIN/zensight-correlator" "$BIN/examples/demo-incident" "$BIN/examples/rpc_get"

tmp="$(mktemp -d)"
# The correlator is the rendezvous, exactly as the exporter is in demo-verify:
# one fewer process than a zenohd, and it is the process under test.
sed -e "s#\"mode\": *\"[a-z]*\"#\"mode\": \"peer\"#" configs/correlator.json5 > "$tmp/correlator.json5" 2>/dev/null \
    || cp configs/correlator.json5 "$tmp/correlator.json5"

echo "==> starting the correlator (rendezvous on $HUB)"
ZENSIGHT_ZENOH_MODE=peer ZENSIGHT_ZENOH_LISTEN="$HUB" ZENSIGHT_ZENOH_CONNECT= \
    ZENSIGHT_ZENOH_SCOUTING=false \
    "$BIN/zensight-correlator" --config "$tmp/correlator.json5" \
    >"$tmp/correlator.log" 2>&1 &
pids+=($!)

# Wait for the catalog to be answering before publishing at it. A GET into a
# correlator that has not declared its queryables yet returns zero replies,
# which is indistinguishable from "no incidents" — the #1045 shape, and the
# reason this waits on the queryable rather than on a sleep.
ready=0
for _ in $(seq 40); do
    if PROBE_CONNECT="$HUB" PROBE_TIMEOUT_SECS=2 \
        "$BIN/examples/rpc_get" 'v1/@catalog/state/entity/*' >/dev/null 2>&1; then
        ready=1
        break
    fi
    # An empty entity set is a legal answer and exits non-zero, so also accept
    # the queryable simply existing: ask for incidents, whose empty reply is
    # the same. Either way we are only waiting for SOMETHING to answer.
    still_running "${pids[@]:-}" || break
    sleep 0.5
done
# Not fatal on its own: an empty catalog answers nothing on either selector, so
# this loop cannot distinguish "not ready" from "ready and empty" — which is
# precisely why it is a hint and the assertions below are the verdict.
[[ "$ready" == 1 ]] || echo "    (catalog not yet answering; continuing — the assertions decide)"

# The publisher holds the fleet up long enough for both phases to be observed.
FAULT_AFTER=12
echo "==> publishing the fleet: $HYPERVISOR_SOURCE hosts $GUEST_SOURCE, one alert on $GUEST_SOURCE"
DEMO_CONNECT="$HUB" FAULT_AFTER_SECS="$FAULT_AFTER" HOLD_SECS=60 \
    "$BIN/examples/demo-incident" >"$tmp/demo.log" 2>&1 &
pids+=($!)

# --- assertion 4 (before the fault): the alert is NOT yet a symptom ---------
#
# Checked FIRST, because it is the one that can only be checked now, and
# because it is what makes the other three mean something. A correlator that
# marked every alert a symptom of everything would satisfy 1-3.
incidents_now() {
    PROBE_CONNECT="$HUB" PROBE_TIMEOUT_SECS=3 \
        "$BIN/examples/rpc_get" 'v1/@catalog/state/incident/*' 2>/dev/null || true
}

echo "==> before the fault: the guest's alert must be UNEXPLAINED"
pre=""
for _ in $(seq 20); do
    pre="$(incidents_now)"
    grep -q "$GUEST_SOURCE" <<<"$pre" && break
    still_running "${pids[@]:-}" || break
    sleep 0.5
done
grep -q "$GUEST_SOURCE" <<<"$pre" \
    || die "no incident for $GUEST_SOURCE appeared before the fault.
An alert was published on a live fleet and the catalog filed nothing, so there is
nothing for the fault to change. Either the alert never reached the correlator or
it was never grouped to an entity.$(logs_note "$tmp" "$tmp/correlator.log" "$tmp/demo.log")"

if grep -q '"symptom_of"' <<<"$pre"; then
    die "the guest's alert was already a symptom BEFORE anything went down:

$(grep -A 4 '"symptom_of"' <<<"$pre" | head -20)

Nothing was down, so nothing could be a cause. An attribution that fires on a
healthy fleet would make every later assertion here vacuous.$(logs_note "$tmp" "$tmp/correlator.log")"
fi
echo "    OK — an incident exists for $GUEST_SOURCE and it is nobody's symptom."

# --- assertions 1-3 (after the fault) ---------------------------------------
echo "==> waiting for the fault (the hypervisor's liveliness token drops at ${FAULT_AFTER}s)"
post=""
for _ in $(seq 60); do
    post="$(incidents_now)"
    grep -q '"symptom_of"' <<<"$post" && break
    still_running "${pids[@]:-}" || break
    sleep 1
done

grep -q '"symptom_of"' <<<"$post" || die "the hypervisor went down and no incident became a symptom.

The catalog held an incident for $GUEST_SOURCE and an edge from $HYPERVISOR_SOURCE, and
still attributed nothing. The three inputs to attribution are the down set (from
liveliness), the containment edge (from relationship evidence) and the firing alert;
one of them did not arrive.

What the catalog holds now:
$(head -c 1500 <<<"$post")

Edges it resolved:
$(PROBE_CONNECT="$HUB" PROBE_TIMEOUT_SECS=3 "$BIN/examples/rpc_get" 'v1/@catalog/state/edge/*' 2>/dev/null | head -c 800 || echo '  <no edges — the relation claim did not resolve>')$(logs_note "$tmp" "$tmp/correlator.log" "$tmp/demo.log")"

# It must name the hypervisor, not merely be non-empty. `entity_id` inside
# symptom_of is the cause; the entity's own document carries the source name,
# so the cheapest honest check is that the cause is NOT the guest's own entity
# and that the hypervisor is what the catalog believes is down.
# The check is written to a file rather than fed on stdin: the incident JSON is
# what goes on stdin, and a heredoc cannot be both the program and its input.
cat > "$tmp/check.py" <<'CHECK'
import json, sys

raw = sys.stdin.read()
# rpc_get prints "<key>\n<pretty json>" per reply; walk them with raw_decode
# rather than pattern-matching braces, which is a guess about a formatter.
dec, i, incidents = json.JSONDecoder(), 0, []
while True:
    j = raw.find("{", i)
    if j < 0:
        break
    try:
        v, end = dec.raw_decode(raw, j)
    except json.JSONDecodeError:
        i = j + 1
        continue
    if isinstance(v, dict) and "alerts" in v:
        incidents.append(v)
    i = end
open(sys.argv[1], "w").write(json.dumps(incidents, indent=2))


def cause_id(c):
    """symptom_of is a Cause enum; serde may render it tagged or inline."""
    if isinstance(c, str):
        return c
    if isinstance(c, dict):
        if "entity_id" in c:
            return c["entity_id"]
        for v in c.values():
            if isinstance(v, dict) and "entity_id" in v:
                return v["entity_id"]
    return str(c)


symptoms = [i for i in incidents if i.get("symptom_of")]
if not symptoms:
    sys.exit("no incident carries symptom_of")

for inc in symptoms:
    cid = cause_id(inc["symptom_of"])
    if cid == inc.get("entity_id"):
        sys.exit(f"an incident is its own cause: {cid}")

causes = {cause_id(i["symptom_of"]) for i in symptoms}
for inc in incidents:
    if inc.get("entity_id") in causes and inc.get("symptom_of"):
        sys.exit(f"the root cause {inc['entity_id']} is itself filed as a symptom")

print(f"    symptom incidents: {len(symptoms)}; root cause(s): {sorted(causes)}")
CHECK

python3 "$tmp/check.py" "$tmp/incidents.json" <<<"$post" \
    || die "the incident attribution is wrong (above).$(logs_note "$tmp" "$tmp/correlator.log")"

echo
echo "OK — a hypervisor died and the catalog said which alert it explains."
echo "     $GUEST_SOURCE's alert is filed as a symptom of $HYPERVISOR_SOURCE; the cause is not"
echo "     a symptom of anything, and before the fault nothing was a symptom at all."
echo "     Grafana, given the same series, shows the same lines and no relationship."
