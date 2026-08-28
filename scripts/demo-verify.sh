#!/usr/bin/env bash
# demo-verify.sh — prove the exporter path actually EXPORTS, end to end.
#
# WHY THIS EXISTS
#
# Nothing in CI had ever executed an exporter. `.forgejo/workflows/release.yml`
# builds and pushes twelve component images, including both exporters, and
# smoke-runs `--help` on exactly two of them (sysinfo, correlator). And while
# `cargo test --workspace` covers the exporters' unit tests, nothing anywhere
# started a hub, a sensor and an exporter and scraped `/metrics`.
#
# That gap is how two scrape-killers shipped: a `# TYPE ... info` token that made
# Prometheus discard every sample in the body (#752), and a duplicate `device`
# label that made it reject the sample outright (#753). Both were invisible to
# the entire suite, because the suite only ever asserted substrings of a body it
# never validated.
#
# ISOLATION (the project rule, same as scripts/image-verify.sh): this runs on its
# OWN rendezvous port and its OWN scrape port, with multicast OFF. It never
# touches 7447 and never joins a live fleet. No containers, no zenohd, no sudo —
# the exporter IS the rendezvous, so it is two processes and a curl.
set -euo pipefail

PORT="${PORT:-17447}"
SCRAPE_PORT="${SCRAPE_PORT:-19464}"
HUB="tcp/127.0.0.1:${PORT}"
SCRAPE="127.0.0.1:${SCRAPE_PORT}"
PROFILE="${PROFILE:-release}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN="${BINDIR:-target/${PROFILE}}"
relflag=""
[[ "$PROFILE" == "release" ]] && relflag="--release"

# The three fixes from #790 — a preflight that names the path it looked at,
# logs that outlive the exit trap, and a timeout message that can tell a dead
# child from an undiscovered one. Shared with conformance-verify.sh because
# both had the same three defects.
# shellcheck source=lib/verify.sh
source "$ROOT/scripts/lib/verify.sh"

tmp=""
# PIDs of the processes we start, so cleanup can be surgical.
#
# NOT `kill 0`: that signals the whole process group, which includes this
# script and whatever invoked it (a `just` recipe, a CI step, an interactive
# shell). It reliably wedges the run at exit even after every assertion has
# passed. Kill our own children by pid and nothing else.
pids=()
cleanup() {
    local rc=$?
    for pid in "${pids[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
    for pid in "${pids[@]:-}"; do
        [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true
    done
    # Keep the evidence when a failure pointed at it (#790).
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

echo "==> building"
cargo build $relflag -p zensight-exporter-prometheus -p zensight-sensor-sysinfo >/dev/null

# `cargo build` says a binary exists somewhere. This says it exists HERE.
require_bins "$BIN/zensight-exporter-prometheus" "$BIN/zensight-sensor-sysinfo"

tmp="$(mktemp -d)"
echo "==> generating configs into $tmp"
scripts/gen-configs.sh \
    --iface lo \
    --outdir "$tmp" \
    --configs-dir "$ROOT/configs" \
    --exporters >/dev/null

# ---------------------------------------------------------------------------
# The exporter LISTENS: it is the rendezvous, exactly as `just demo-prometheus`
# arranges it. SCOUTING=false because multicast on loopback triggers a
# CONNECTION_TO_SELF error storm, and because pinned endpoints make it noise.
# ---------------------------------------------------------------------------
echo "==> starting exporter (rendezvous on $HUB, scrape on $SCRAPE)"
ZENSIGHT_ZENOH_LISTEN="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
    "$BIN/zensight-exporter-prometheus" \
        --config "$tmp/prometheus-exporter.json5" \
        --listen "$SCRAPE" >"$tmp/exporter.log" 2>&1 &
pids+=($!)

for _ in $(seq 30); do
    curl -sf -o /dev/null "http://$SCRAPE/health" && break
    sleep 0.5
done
if ! curl -sf -o /dev/null "http://$SCRAPE/health"; then
    keep_logs_on_failure
    if still_running "${pids[@]:-}"; then
        die "the exporter is running and not serving /health on $SCRAPE.$(logs_note "$tmp" "$tmp/exporter.log")"
    fi
    die "the exporter exited before it could serve /health.$(logs_note "$tmp" "$tmp/exporter.log")"
fi

# /ready is 503 until the FIRST telemetry point. Asserting that here is not
# pedantry: it is what makes /ready a meaningful gate below, AND it documents
# why /ready is the wrong thing to put behind a compose `depends_on:
# condition: service_healthy` — such a gate would wait on telemetry that only
# arrives once the sensors reach the exporter it is gating.
code=$(curl -s -o /dev/null -w '%{http_code}' "http://$SCRAPE/ready")
[[ "$code" == 503 ]] \
    || die "expected /ready 503 before any telemetry, got $code"

echo "==> starting sysinfo sensor"
ZENSIGHT_ZENOH_CONNECT="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
    "$BIN/zensight-sensor-sysinfo" --config "$tmp/sysinfo.json5" \
    >"$tmp/sysinfo.log" 2>&1 &
pids+=($!)

echo "==> waiting for telemetry to reach the exporter"
for _ in $(seq 60); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://$SCRAPE/ready")" == 200 ]] && break
    # Do not wait out 60s for a process that has already exited (#790).
    still_running "${pids[@]:-}" || break
    sleep 1
done
if [[ "$(curl -s -o /dev/null -w '%{http_code}' "http://$SCRAPE/ready")" != 200 ]]; then
    keep_logs_on_failure
    # A dead child and an undiscovered one are different failures and used to
    # render identically (#790). Ask before diagnosing.
    dead=$(dead_children "${pids[@]:-}")
    if [[ -n "$dead" ]]; then
        die "no telemetry reached the exporter, and $(wc -l <<<"$dead") of the two \
processes this script started is/are already gone.

This is NOT a discovery problem — a process that has exited publishes nothing
and discovers nothing. Its log says why.$(logs_note "$tmp" "$tmp/exporter.log" "$tmp/sysinfo.log")"
    fi
    die "no telemetry reached the exporter in 60s, and both processes are still
alive.

Everything is running and nothing arrived, which is the discovery failure: the
shipped configs say mode:\"peer\" with \`connect\` commented out, which means
multicast — and every demo path turns multicast off. Check that
ZENSIGHT_ZENOH_{LISTEN,CONNECT,SCOUTING} are set on both processes.$(logs_note "$tmp" "$tmp/exporter.log" "$tmp/sysinfo.log")"
fi

echo "==> scraping /metrics"
metrics=$(curl -sf "http://$SCRAPE/metrics") || die "/metrics did not answer"

# --- Every `# TYPE` token must be legal (the #752 invariant) ----------------
while IFS= read -r line; do
    token="${line##* }"
    case "$token" in
        counter|gauge|histogram|summary|untyped) ;;
        *) die "illegal '# TYPE' token '$token' in: $line
Prometheus rejects the ENTIRE scrape body on an unknown type." ;;
    esac
done < <(grep '^# TYPE ' <<<"$metrics" || true)

# --- No series may carry a duplicate label name (the #753 invariant) --------
dupes=$(python3 - "$metrics" <<'PY'
import re, sys
bad = []
for line in sys.argv[1].splitlines():
    if line.startswith("#") or "{" not in line:
        continue
    inner = line[line.index("{") + 1 : line.rindex("}")]
    names = re.findall(r'([A-Za-z_][A-Za-z0-9_]*)=', inner)
    if len(names) != len(set(names)):
        bad.append(line)
print("\n".join(bad))
PY
)
[[ -z "$dupes" ]] || die "duplicate label name in series:
$dupes"

# --- The semconv path fired, not merely "some series exist" ----------------
grep -q '^zensight_system_cpu_utilization' <<<"$metrics" \
    || die "no zensight_system_cpu_utilization — the semconv mapping did not fire"
grep -q '^zensight_system_memory_usage_bytes{.*state="used"' <<<"$metrics" \
    || die "memory did not factor its state into a LABEL (semconv regression)"

# The registry-driven rename put per-entity subjects in LABELS (#764). Before
# it, each mount was its own metric family and `sum by (mount)` could not be
# written at all.
grep -q '^zensight_sysinfo_disk_inodes_total{.*mount=' <<<"$metrics" \
    || die "disk inodes did not factor the mount into a LABEL — the registry-driven \
naming regressed, and per-entity subjects are back in metric names"

# --- the provisioned dashboards must query names that actually exist --------
#
# Metric names come from the registry now (#764) and carry unit/_total
# conventions (#767), so a rename lands in the exposition and the dashboards rot
# silently — a panel with no data reads as "the exporter is broken". This is the
# check that keeps demo/ honest.
#
# This harness runs sysinfo only and fires no alerts, so families from other
# sensors are deferred rather than asserted. Everything a provisioned dashboard
# names that sysinfo CAN produce must exist.
stale=$(python3 - "$metrics" <<'PY'
import glob, re, sys

live = set(re.findall(r'(?m)^(zensight_[a-z_0-9]+)', sys.argv[1]))

used = set()
for f in glob.glob("demo/prometheus/dashboards/*.json"):
    used |= set(re.findall(r'zensight_[a-z_0-9]+', open(f).read()))

OTHER_SENSORS = ("zensight_netlink_", "zensight_systemd_", "zensight_netring_")
deferred = {m for m in used if m.startswith(OTHER_SENSORS)}
deferred.add("zensight_alert")

print("\n".join(sorted(used - live - deferred)))
PY
)
[[ -z "$stale" ]] || die "provisioned dashboards query metrics that no longer exist:
$stale

The exposition renamed something and demo/prometheus/dashboards/ was not updated."

accepted=$(awk '/^zensight_exporter_points_accepted_total /{print $2}' <<<"$metrics")
[[ "${accepted:-0}" -gt 0 ]] \
    || die "points_accepted_total is 0 — everything was filtered out"

series=$(grep -c '^zensight_' <<<"$metrics" || true)
echo
echo "OK — sysinfo -> Zenoh -> exporter -> /metrics"
echo "     $accepted points accepted, $series exported lines, all TYPE tokens legal,"
echo "     no duplicate label names."
