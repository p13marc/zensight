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
# Phase 2 (#845) closes the same gap for the OTHER exporter: the OTLP exporter
# had never been executed by anything — its release smoke is `--help`, its
# integration tests are pure functions. Here it joins the same hub and must
# deliver at least one OTLP/HTTP metrics export to a local sink.
#
# ISOLATION (the project rule, same as scripts/image-verify.sh): this runs on its
# OWN rendezvous port and its OWN scrape port, with multicast OFF. It never
# touches 7447 and never joins a live fleet. No containers, no zenohd, no sudo —
# the exporter IS the rendezvous, so it is two processes and a curl.
set -euo pipefail

PORT="${PORT:-17447}"
SCRAPE_PORT="${SCRAPE_PORT:-19464}"
OTLP_PORT="${OTLP_PORT:-19465}"
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
cargo build $relflag --locked -p zensight-exporter-prometheus -p zensight-exporter-otel \
    -p zensight-sensor-sysinfo -p zensight-historian -p zensight-desired >/dev/null
# The one-shot @rpc client the historian phase queries with (#912). An
# example, not a binary: it is a test fixture with a `main`, and shipping it
# in the release tarball would suggest otherwise.
cargo build $relflag --locked -p zensight-historian --example historian-query >/dev/null

# `cargo build` says a binary exists somewhere. This says it exists HERE.
require_bins "$BIN/zensight-exporter-prometheus" "$BIN/zensight-exporter-otel" \
    "$BIN/zensight-sensor-sysinfo" "$BIN/zensight-historian" "$BIN/zensight-desired" \
    "$BIN/examples/historian-query"

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

# ---------------------------------------------------------------------------
# Phase 1b (#912): the historian, EXECUTED and ASKED A QUESTION.
#
# The same lesson as the exporters (#845): compiling a query path proves
# nothing about it. `cargo test` covers the aggregation and the paging as pure
# functions, and the crate's own round trips use an in-process session — the
# right shape for a unit test and the wrong one for a smoke test, because it
# never starts the real binary, never crosses a real socket and never reads a
# config.
#
# It joins the bus the exporters are already on, so the sysinfo sensor that is
# still publishing feeds it too.
# ---------------------------------------------------------------------------
echo
echo "==> starting historian (ingest + range queries)"
# STATE_DIRECTORY keeps the database inside this run's temp dir: a smoke test
# must not leave a file behind, and must not read one an earlier run left.
ZENSIGHT_ZENOH_CONNECT="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
    STATE_DIRECTORY="$tmp" \
    "$BIN/zensight-historian" --config "$tmp/historian.json5" \
    >"$tmp/historian.log" 2>&1 &
pids+=($!)

# Poll the RANGE query, not `series`.
#
# `series` answers from the interner as soon as the first sample is recorded,
# which is within a second — but the default `step=60` selects the MINUTE tier,
# and that lives in redb, so it holds nothing until a flush has run. Polling
# `series` and then querying `range` once looked like a smoke test and was a
# race: it passed on a warm store and failed on a cold one, which is exactly
# backwards from what a smoke test should do.
#
# Polling the range instead exercises the whole chain this phase exists for —
# wire, ingest, downsample, flush, redb, query — and its failure message can
# say which link is missing, because `series` answering while `range` does not
# is a specific, diagnosable state.
echo "==> asking the historian for a range over its own ingest window"
range_json=""
for _ in $(seq 1 60); do
    still_running "${pids[@]}" || die "a process exited while waiting for the historian.\
$(logs_note "$tmp" "$tmp/historian.log" "$tmp/sysinfo.log")"
    now_ms=$(( $(date +%s) * 1000 ))
    # `agg` is left unset on purpose so the server picks by kind — the default
    # path is the one every caller takes, and a smoke test that always named an
    # aggregate would never exercise it.
    if range_json=$("$BIN/examples/historian-query" -c "$HUB" --timeout 3 \
        "v1/*/@rpc/historian/range?producer=sysinfo;from=$((now_ms - 600000));to=$now_ms;step=60" \
        2>>"$tmp/historian-query.log"); then
        grep -q '"points"' <<<"$range_json" && break
    fi
    sleep 1
done

if ! grep -q '"points"' <<<"$range_json"; then
    # Ask the other two procedures so the failure names the link that broke
    # rather than only the one that was asked.
    series_json=$("$BIN/examples/historian-query" -c "$HUB" --timeout 3 \
        'v1/*/@rpc/historian/series?producer=sysinfo' 2>/dev/null || echo "<no reply>")
    stats_json=$("$BIN/examples/historian-query" -c "$HUB" --timeout 3 \
        'v1/*/@rpc/historian/stats' 2>/dev/null || echo "<no reply>")
    die "the historian returned no range points over its own ingest window after 60s.
  series: $(head -c 200 <<<"$series_json")
  stats:  $(head -c 300 <<<"$stats_json")
A non-empty series list with an empty range means ingest reached the store but the
minute tier did not — a flush that is not running, or a downsample that produced
nothing.$(logs_note "$tmp" "$tmp/historian.log" "$tmp/sysinfo.log")"
fi

# The reply must say what it did, not only what it holds: `step_s` is the
# resolution actually served after the tier clamp, and a caller that cannot
# read it back cannot tell a coarse chart from a wrong one.
grep -q '"step_s":60' <<<"$range_json" \
    || die "the range reply did not state the step it served: $range_json"

# And `series` must agree that those points belong to something it holds.
series_json=$("$BIN/examples/historian-query" -c "$HUB" --timeout 5 \
    'v1/*/@rpc/historian/series?producer=sysinfo') \
    || die "the historian answered a range but not a series listing.\
$(logs_note "$tmp" "$tmp/historian.log")"
series_count=$(grep -o '"subject"' <<<"$series_json" | wc -l)
[[ "$series_count" -gt 0 ]] || die "series reply parsed to zero subjects: $series_json"

points=$(grep -o '\[[0-9]\{13\},' <<<"$range_json" | wc -l)
echo
echo "OK — sysinfo -> Zenoh -> historian -> @rpc/historian/range"
echo "     $series_count series held, $points point(s) returned over a 10-minute window."

# ---------------------------------------------------------------------------
# Phase 2 (#845): the OTLP exporter, executed. Nothing anywhere had ever run
# it: the release smoke is `--help`, `cargo test` covers pure conversion
# functions, and `just demo-otel` is manual and needs a container. This is
# honest smoke, not schema validation: a stdlib-python sink accepts OTLP/HTTP
# POSTs and the assertion is that at least one metrics export ARRIVES with
# the protobuf content-type and a non-empty body. The sink answers 200 with
# an empty body — a valid encoding of the empty ExportMetricsServiceResponse.
#
# The exporter CONNECTS to the same hub the prometheus exporter is listening
# on, so the running sysinfo sensor feeds both and this phase adds one
# process and one sink.
# ---------------------------------------------------------------------------
echo "==> phase 2: starting an OTLP/HTTP sink on 127.0.0.1:$OTLP_PORT"
python3 - "$OTLP_PORT" "$tmp/otlp-sink.log" >"$tmp/otlp-sink.stdout" 2>&1 <<'PY' &
import sys, http.server

port, log_path = int(sys.argv[1]), sys.argv[2]

class Sink(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length)
        with open(log_path, "a") as f:
            f.write(f"{self.path} {self.headers.get('Content-Type','')} {len(body)}\n")
        self.send_response(200)
        self.send_header("Content-Type", "application/x-protobuf")
        self.send_header("Content-Length", "0")
        self.end_headers()
    def log_message(self, *a):
        pass

http.server.HTTPServer(("127.0.0.1", port), Sink).serve_forever()
PY
pids+=($!)

# Point the generated otel config at the sink over http. Both keys exist in
# the committed example (the gen-configs.sh rule: a sed can only flip a key
# that is really in configs/*.json5).
sed -E -e "s|endpoint: \"[^\"]+\"|endpoint: \"http://127.0.0.1:${OTLP_PORT}\"|" \
       -e "s|protocol: \"grpc\"|protocol: \"http\"|" \
       "$tmp/otel-exporter.json5" > "$tmp/otel-exporter-ci.json5"

echo "==> starting otel exporter (connecting to $HUB, exporting to the sink)"
ZENSIGHT_ZENOH_CONNECT="$HUB" ZENSIGHT_ZENOH_SCOUTING=false \
    "$BIN/zensight-exporter-otel" --config "$tmp/otel-exporter-ci.json5" \
    >"$tmp/otel.log" 2>&1 &
pids+=($!)

echo "==> waiting for a metrics export to reach the sink"
# export_interval_secs is 10 in the shipped config; give it four intervals.
got=""
for _ in $(seq 40); do
    if [[ -f "$tmp/otlp-sink.log" ]] && grep -q '^/v1/metrics ' "$tmp/otlp-sink.log"; then
        got=1; break
    fi
    still_running "${pids[@]:-}" || break
    sleep 1
done
if [[ -z "$got" ]]; then
    keep_logs_on_failure
    dead=$(dead_children "${pids[@]:-}")
    if [[ -n "$dead" ]]; then
        die "no OTLP export reached the sink, and a process this script started is \
already gone. Its log says why.$(logs_note "$tmp" "$tmp/otel.log" "$tmp/otlp-sink.stdout")"
    fi
    die "no OTLP metrics export reached the sink in 40s with every process alive. \
The exporter either received no telemetry (discovery) or cannot speak OTLP/HTTP \
to the sink.$(logs_note "$tmp" "$tmp/otel.log" "$tmp/otlp-sink.log")"
fi

# The export is real protobuf with content, not an empty keep-alive.
bad=$(awk '$1 == "/v1/metrics" && ($2 !~ /protobuf/ || $3 == 0)' "$tmp/otlp-sink.log")
[[ -z "$bad" ]] || die "an OTLP metrics POST arrived malformed (path content-type bytes):
$bad"

exports=$(grep -c '^/v1/metrics ' "$tmp/otlp-sink.log" || true)
echo
echo "OK — sysinfo -> Zenoh -> otel exporter -> OTLP/HTTP sink"
echo "     $exports metrics export(s) delivered, protobuf content-type, non-empty body."

# ---------------------------------------------------------------------------
# Phase 3 (#938): the fleet policy compiler, EXECUTED against the policy this
# repository ships.
#
# The same lesson a third time. `cargo test` covers the overlay rules and the
# publish diff, and the e2e covers the bus properties — but neither runs the
# BINARY, neither parses `demo/fleet-policy.json5`, and neither would notice
# the file going stale. A shipped policy that no longer validates is exactly
# the thing an operator copies first.
#
# `plan --offline` opens no session, so this costs a subsecond and needs no
# bus: it parses the daemon config, parses the policy, and runs every check —
# class names, extends cycles, topic spellings against the live registry, and
# the never-list over every fragment. A topic renamed out from under the demo
# policy fails here.
echo
echo "==> phase 3: the fleet policy compiler, on the shipped demo policy"
if ! plan_out=$("$BIN/zensight-desired" \
        --config "$tmp/desired.json5" \
        --policy "$ROOT/demo/fleet-policy.json5" \
        plan --offline 2>&1); then
    die "demo/fleet-policy.json5 no longer validates:
$plan_out"
fi
echo "$plan_out" | grep -q ": valid" \
    || die "plan --offline exited 0 without reporting the policy valid: $plan_out"

# And the compiler must still REFUSE a bad policy — a validator that passes
# everything passes a shipped policy too, and this phase would be theatre.
cat > "$tmp/bad-policy.json5" <<'BADPOLICY'
{ classes: [ { name: "leaky", matches: { always: true },
    docs: { "sysinfo/thresholds": { zenoh: { connect: ["tcp/10.0.0.1:7447"] } } } } ],
  hosts: {} }
BADPOLICY
if "$BIN/zensight-desired" --config "$tmp/desired.json5" \
        --policy "$tmp/bad-policy.json5" plan --offline >/dev/null 2>&1; then
    die "the policy compiler accepted a document carrying a bus endpoint — the \
@desired never-list is not being enforced (#816)"
fi

echo "OK — demo/fleet-policy.json5 validates, and a never-list violation is refused."
