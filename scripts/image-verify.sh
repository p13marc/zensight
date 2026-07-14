#!/usr/bin/env bash
# image-verify.sh — build the all-in-one sensors image and prove a container
# sensor lands on the v1 keyspace as ONE stable host (#472).
#
# RUN THIS ON A HOST WITH WORKING PODMAN. It cannot run in the dev toolbox:
# podman-in-toolbox fails with "unable to create a new pause process", which is
# why #472 is a handover rather than a merged verification.
#
#   ./scripts/image-verify.sh
#
# What it proves, and why each part earns its place:
#
#   1. The image BUILDS from source (docker/Dockerfile.sensors). Nothing else
#      built it until now — the release pipeline assembles its image from
#      prebuilt binaries — so rot was only discoverable by whoever next tried.
#
#   2. The container publishes under exactly ONE `h-<12hex>` origin, and that
#      origin SURVIVES A RESTART. This is what multi-machine deployment rests on:
#      the origin is the *host's* (from the mounted /etc/machine-id), not the
#      container's. Miss that mount and each start mints a fresh random id — so
#      every restart looks like a brand-new host and the catalog fills with
#      ghosts. Nothing errors. You just slowly accumulate hosts that never
#      existed, which is why this is worth a restart to check.
#
#   3. Every live producer answers `introspect` on its `@rpc` plane — "alive ⇒
#      callable" (RFC 04 §5). A host that publishes telemetry but serves no
#      procedures is one you can watch and cannot ask.
#
# ISOLATION (project rule): everything runs against a throwaway router on
# loopback:17447, multicast OFF. It never touches 7447 and never joins the fleet.

set -euo pipefail

PORT="${PORT:-17447}"
IMAGE="${IMAGE:-zensight-sensors:verify}"
PODMAN="${PODMAN:-sudo podman}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HUB="tcp/127.0.0.1:$PORT"

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die()  { printf '\n\033[31mFAIL: %s\033[0m\n' "$*" >&2; exit 1; }
pass() { printf '\033[32m  ok\033[0m %s\n' "$*"; }

command -v zenohd >/dev/null || die "zenohd not on PATH — cargo install zenohd --locked"

cleanup() {
  [[ -n "${ROUTER_PID:-}" ]] && kill "$ROUTER_PID" 2>/dev/null || true
  $PODMAN rm -f zensight-verify >/dev/null 2>&1 || true
}
trap cleanup EXIT

# zenctl stays un-namespaced by design: it sees the wire as it really is, which
# is the only way to check that what the container *claims* and what it *sends*
# agree. Scouting is off by default — it will not wander onto your fleet.
zenctl() { cargo run --quiet --release -p zenctl -- "$@" --connect "$HUB"; }

# The origins holding a liveliness token. `node list` prints each origin on its
# own line with its producers indented beneath.
origins_on_bus() { zenctl node list 2>/dev/null | grep -E '^h-[0-9a-f]{12}$' || true; }

start_container() {
  $PODMAN run -d --name zensight-verify \
    --net=host --uts=host --pid=host \
    --cap-add=NET_RAW --cap-add=NET_ADMIN --cap-add=IPC_LOCK \
    --security-opt label=disable \
    -v /etc/machine-id:/etc/machine-id:ro \
    -v /run/dbus/system_bus_socket:/run/dbus/system_bus_socket:ro \
    -v /var/log/journal:/var/log/journal:ro \
    -v /run/log/journal:/run/log/journal:ro \
    -e ZENSIGHT_ZENOH_CONNECT="$HUB" \
    "$IMAGE" >/dev/null
  sleep 20
  $PODMAN ps --filter name=zensight-verify --format '{{.Status}}' | grep -q Up \
    || { $PODMAN logs zensight-verify; die "the container exited"; }
}

# ── 1. It builds ────────────────────────────────────────────────────────────
say "building $IMAGE from docker/Dockerfile.sensors"
$PODMAN build -t "$IMAGE" -f "$ROOT/docker/Dockerfile.sensors" "$ROOT"

# The entrypoint must REFUSE to start with nowhere to publish (exit 64). A sensor
# that starts, finds no endpoint and stays quiet is the failure this image most
# invites — and it looks exactly like a healthy one.
say "the entrypoint refuses to start with nowhere to publish"
set +e
$PODMAN run --rm "$IMAGE" >/dev/null 2>&1
code=$?
set -e
[[ $code -eq 64 ]] || die "expected exit 64 with no ZENSIGHT_ZENOH_CONNECT, got $code"
pass "exits 64"

# ── 2. An isolated hub ──────────────────────────────────────────────────────
say "starting an isolated router on 127.0.0.1:$PORT (multicast OFF)"
zenohd --cfg "listen/endpoints:[\"tcp/0.0.0.0:$PORT\"]" \
       --cfg 'scouting/multicast/enabled:false' \
       >/tmp/zensight-verify-router.log 2>&1 &
ROUTER_PID=$!
sleep 2
kill -0 "$ROUTER_PID" 2>/dev/null || die "router died — see /tmp/zensight-verify-router.log"

# ── 3. One host, one origin ─────────────────────────────────────────────────
say "starting the sensors container against it"
start_container
zenctl node list 2>/dev/null || die "zenctl could not reach the hub"

origins=$(origins_on_bus)
[[ -n "$origins" ]] || die "no origins on the bus — the container published nothing.
Look at: $PODMAN logs zensight-verify"

count=$(echo "$origins" | wc -l)
[[ "$count" -eq 1 ]] || die "expected ONE origin (this is one host), saw $count:
$origins
Several origins from one container means a sensor minted its own id instead of
deriving it from the mounted /etc/machine-id."
origin="$origins"
pass "one origin: $origin"

# ── 4. alive ⇒ callable ─────────────────────────────────────────────────────
# `doctor` GETs `introspect` on every origin's @rpc plane and cross-checks the
# roster: a producer holding an `alive` token that does not answer is a finding,
# not a race (RFC 04 §5). That is exactly the telemetry-but-no-@rpc failure.
say "every live producer answers on its @rpc plane"
doctor=$(zenctl doctor 2>/dev/null) || true
echo "$doctor"
echo "$doctor" | grep -q "did not answer introspect" \
  && die "a producer holds an alive token but serves no @rpc — alive ⇒ callable (RFC 04 §5).
This host can be watched but not asked."
pass "all producers callable"

# ── 5. The origin is the HOST's, and survives a restart ─────────────────────
# The actual risk: without /etc/machine-id the sensors mint a *fresh random*
# origin at every start. Nothing errors — you just accumulate ghost hosts. A
# restart is the cheapest way to catch it, and it needs no knowledge of the salt.
say "restarting the container — the origin must NOT change"
$PODMAN rm -f zensight-verify >/dev/null
sleep 2
start_container

after=$(origins_on_bus | grep -Fx "$origin" || true)
[[ -n "$after" ]] || die "the origin changed across a restart.
Before: $origin
After:  $(origins_on_bus | tr '\n' ' ')
/etc/machine-id is not reaching the container, so it minted a fresh random id.
Every restart will look like a brand-new host and the catalog will fill with
ghosts — silently."
pass "still $origin"

say "OK — the container is ONE host, stable across restarts, callable, under $origin"
