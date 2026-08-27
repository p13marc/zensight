#!/usr/bin/env bash
# media-loss-lab.sh — put a real @media stream on a link we control, and record
# what arrives.
#
# WHY THIS EXISTS (#713)
#
# `QosClass::LiveVideo` asks for `BestEffort` + `Drop`. Every config in
# `configs/` is a `tcp/` peer. Over TCP, best-effort cannot lose a sample in
# flight — it only permits the SENDER to drop under congestion. So every
# sequence gap the GUI has ever recovered from was a sender-side drop or a
# pipeline restart, and the frame-age deadline (#716), the report's loss field
# (#714) and the controller's thresholds (#720) are all tuned against a
# distribution nobody has produced, let alone measured.
#
# This script produces it. Two legs:
#
#   --leg tcp    today's deployment, `tbf`-throttled below the tier bitrate.
#                The expected result is that the WIRE loses nothing and every
#                gap is the sensor's own AppSink shedding. That is the point.
#   --leg quic   `quic/…?mixed_rel=1`, where best-effort rides UNRELIABLE QUIC
#                datagrams, at a 1200 B MTU with `netem loss`. Zenoh fragments
#                to give the illusion of an unlimited MTU and defragmentation
#                is all-or-nothing, so a ~50 KB IDR is ~45 datagrams and ONE
#                lost datagram loses the whole keyframe.
#
# ISOLATION. Two network namespaces joined by one veth pair. **No qdisc is ever
# attached to `lo` or to a real interface**, and everything — namespaces, veth,
# certificates, the sensor process — is torn down by the EXIT trap, including on
# failure. Nothing outside the two namespaces can observe the run, and the
# bus lives entirely inside them (scouting off, explicit endpoints).
#
# ROOT. `ip netns` and `tc` need it; the sensor and the probe do not, so they
# are dropped back to the invoking user inside the namespace.
set -euo pipefail

LEG=quic
LOSS=1            # netem loss %, quic leg
RATE=""           # tbf rate (e.g. 600kbit), tcp leg; default = 60% of the tier
DELAY=""          # optional one-way netem delay, e.g. 20ms
# 1280, not the 1200 #713 asks for: QUIC mandates a path that carries a
# 1200-byte UDP *payload*, so an interface MTU of 1200 cannot carry a QUIC
# handshake at all (IPv4 adds 28 bytes and the packet is sent DF). 1280 is the
# IPv6 minimum MTU and the smallest honest floor — it leaves 1252 bytes of QUIC
# payload, which is the number the fragment arithmetic actually uses.
MTU=""            # default: 1280 on the quic leg, 1500 on the tcp leg
SECONDS_RUN=60
TIER=high
SLICE=""          # --slice N sets the encoder's max_slice_len (#509)
# The test pattern decides the ONE thing leg 2 turns on: how many datagrams an
# access unit is. A flat `smpte` chart at 720p compresses to ~3 KB — three
# datagrams — and cannot exhibit an effect that only bites on ~45. `snow` is
# incompressible noise, the other extreme. A real camera sits between them, so
# the report runs both and brackets reality rather than picking one.
PATTERN=snow
WIDTH=1280
HEIGHT=720
FPS=30
BITRATE=4000
# Quantiser. The independent variable of leg 2 is ACCESS-UNIT SIZE, and on a
# synthetic pattern the honest way to move it is picture quality, not a weirder
# pattern: a low `qp` is what a real camera at high quality looks like on the
# wire. Unset leaves OpenH264's own choice.
QP=""
OUT=""
KEEP=0

usage() { sed -n '2,32p' "$0"; exit "${1:-0}"; }
while [ $# -gt 0 ]; do
  case "$1" in
    --leg)     LEG=$2; shift 2 ;;
    --loss)    LOSS=$2; shift 2 ;;
    --rate)    RATE=$2; shift 2 ;;
    --delay)   DELAY=$2; shift 2 ;;
    --mtu)     MTU=$2; shift 2 ;;
    --seconds) SECONDS_RUN=$2; shift 2 ;;
    --tier)    TIER=$2; shift 2 ;;
    --slice)   SLICE=$2; shift 2 ;;
    --pattern) PATTERN=$2; shift 2 ;;
    --width)   WIDTH=$2; shift 2 ;;
    --height)  HEIGHT=$2; shift 2 ;;
    --fps)     FPS=$2; shift 2 ;;
    --bitrate) BITRATE=$2; shift 2 ;;
    --qp)      QP=$2; shift 2 ;;
    --out)     OUT=$2; shift 2 ;;
    --keep)    KEEP=1; shift ;;
    -h|--help) usage 0 ;;
    *) echo "unknown argument: $1" >&2; usage 1 ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
PROFILE="${PROFILE:-release}"
BIN="$ROOT/target/$PROFILE/zensight-sensor-parallax"
PROBE="$ROOT/target/$PROFILE/examples/media_loss_probe"
TC=/sbin/tc
NS_TX=zs-tx
NS_RX=zs-rx
IP_TX=10.77.0.1
IP_RX=10.77.0.2
PORT=7447
: "${MTU:=$([ "$LEG" = quic ] && echo 1280 || echo 1500)}"
: "${OUT:=$ROOT/target/media-loss/$LEG-$PATTERN-$(date +%Y%m%dT%H%M%S)}"

for f in "$BIN" "$PROBE"; do
  [ -x "$f" ] || { echo "missing $f — cargo build --$PROFILE -p zensight-sensor-parallax --example media_loss_probe --bin zensight-sensor-parallax" >&2; exit 2; }
done
[ -x "$TC" ] || { echo "no $TC (iproute2)" >&2; exit 2; }

mkdir -p "$OUT"
RUNUSER="$(id -un)"
SENSOR_PID=""

cleanup() {
  set +e
  [ -n "$SENSOR_PID" ] && sudo kill "$SENSOR_PID" 2>/dev/null
  if [ "$KEEP" = 0 ]; then
    sudo ip netns del "$NS_TX" 2>/dev/null
    sudo ip netns del "$NS_RX" 2>/dev/null
  fi
}
trap cleanup EXIT

# ── the link ────────────────────────────────────────────────────────────────
sudo ip netns del "$NS_TX" 2>/dev/null || true
sudo ip netns del "$NS_RX" 2>/dev/null || true
sudo ip netns add "$NS_TX"
sudo ip netns add "$NS_RX"
sudo ip link add zst type veth peer name zsr
sudo ip link set zst netns "$NS_TX"
sudo ip link set zsr netns "$NS_RX"
sudo ip -n "$NS_TX" addr add "$IP_TX/30" dev zst
sudo ip -n "$NS_RX" addr add "$IP_RX/30" dev zsr
for ns in "$NS_TX" "$NS_RX"; do sudo ip -n "$ns" link set lo up; done
sudo ip -n "$NS_TX" link set zst mtu "$MTU" up
sudo ip -n "$NS_RX" link set zsr mtu "$MTU" up

# The impairment goes on the SENSOR's egress: this measures the media
# direction, and putting it on the receiver's egress would only impair acks.
NETEM=""
[ "$LEG" = quic ] && NETEM="loss ${LOSS}%"
[ -n "$DELAY" ]   && NETEM="$NETEM delay $DELAY"
if [ -n "$NETEM" ]; then
  # shellcheck disable=SC2086
  sudo ip netns exec "$NS_TX" $TC qdisc add dev zst root netem $NETEM
fi
if [ "$LEG" = tcp ]; then
  : "${RATE:=600kbit}"
  # A shallow burst/latency on purpose: a deep token bucket would absorb the
  # congestion this leg exists to create and hand us a queue instead.
  sudo ip netns exec "$NS_TX" $TC qdisc add dev zst root tbf \
      rate "$RATE" burst 32kbit latency 50ms
fi
echo "== link: $NS_TX($IP_TX) -> $NS_RX($IP_RX), mtu $MTU, ${NETEM:-${RATE:+tbf $RATE}} =="
sudo ip netns exec "$NS_TX" $TC -s qdisc show dev zst | sed 's/^/   /'

# ── certificates (quic leg): the PROBE is the listener ──────────────────────
# In a deployment the listening side is the zenohd router and ZenSight
# processes are TLS clients only, which is why `zensight_common::session` sets
# connect-side material only. The lab has no router, so the probe listens and
# builds its own config — the one thing an example is explicitly allowed to do.
SCHEME=tcp
EP_META=""
CA_ARGS=()
PROBE_TLS=()
if [ "$LEG" = quic ]; then
  SCHEME=quic
  EP_META='?mixed_rel=1'   # both ends must offer it: it is ALPN-negotiated
  # A real two-level chain, not a self-signed leaf handed round as its own
  # trust anchor: rustls wants `serverAuth` EKU on the end entity and
  # `CA:TRUE` on the anchor, and one certificate cannot honestly be both.
  openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=zs-lab-ca" \
      -keyout "$OUT/ca.key" -out "$OUT/ca.crt" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -subj "/CN=$IP_RX" \
      -keyout "$OUT/probe.key" -out "$OUT/probe.csr" 2>/dev/null
  openssl x509 -req -in "$OUT/probe.csr" -days 2 \
      -CA "$OUT/ca.crt" -CAkey "$OUT/ca.key" -CAcreateserial \
      -out "$OUT/probe.crt" \
      -extfile <(printf 'subjectAltName=IP:%s\nextendedKeyUsage=serverAuth\n' "$IP_RX") 2>/dev/null
  chmod 644 "$OUT/probe.key" "$OUT/ca.key"
  CA_ARGS=(--root-ca "$OUT/ca.crt")
  PROBE_TLS=(--listen-cert "$OUT/probe.crt" --listen-key "$OUT/probe.key")
fi
LISTEN="$SCHEME/$IP_RX:$PORT$EP_META"
CONNECT="$SCHEME/$IP_RX:$PORT$EP_META"

# ── the sensor ──────────────────────────────────────────────────────────────
# 720p rather than the shipped 640x360 test source, for the same reason the
# pattern is a knob: access-unit SIZE is the independent variable of leg 2.
SLICE_LINE=""
[ -n "$SLICE" ] && SLICE_LINE="max_slice_len: $SLICE,"
[ -n "$QP" ] && SLICE_LINE="$SLICE_LINE qp: $QP,"
cat > "$OUT/sensor.json5" <<J5
{ zenoh: { mode: "peer", connect: ["$CONNECT"], listen: [] },
  parallax: {
    source: "auto", enumerate_v4l2: false, rtsp: [],
    test_sources: [ { name: "test0", pattern: "$PATTERN", width: $WIDTH, height: $HEIGHT, fps: $FPS } ],
    preview: { fps: 1, quality: 60, max_height: 180 },
    video: {
      gop_frames: 30, default_tier: "$TIER",
      encoder: { $SLICE_LINE },
      tiers: [ { name: "$TIER", max_height: null, fps: $FPS, bitrate_kbps: $BITRATE } ],
    },
    idle_timeout_secs: 30, stats_interval_secs: 5,
  },
  logging: { level: "info", format: "text" } }
J5

TLS_ENV=()
[ "$LEG" = quic ] && TLS_ENV=(ZENSIGHT_ZENOH_TLS_CA="$OUT/ca.crt")
sudo ip netns exec "$NS_TX" sudo -u "$RUNUSER" env \
    ZENSIGHT_ZENOH_SCOUTING=false ZENSIGHT_ZENOH_GOSSIP=false "${TLS_ENV[@]}" \
    "$BIN" --config "$OUT/sensor.json5" > "$OUT/sensor.log" 2>&1 &
SENSOR_PID=$!

# The origin is minted from the host, not the namespace, but read it from the
# log rather than recomputing it — a probe pointed at the wrong origin
# subscribes to a key nobody publishes and reports a clean 100% loss.
# Matched as a bare `h-<12 hex>` rather than `origin=…`: the text formatter
# puts ANSI colour escapes between the field name and its value.
ORIGIN=""
for _ in $(seq 1 60); do
  ORIGIN=$(grep -oE 'h-[0-9a-f]{12}' "$OUT/sensor.log" 2>/dev/null | head -1 || true)
  [ -n "$ORIGIN" ] && break
  sleep 0.5
done
[ -n "$ORIGIN" ] || { echo "sensor never minted an origin:"; tail -20 "$OUT/sensor.log"; exit 1; }
echo "== sensor origin $ORIGIN, tier $TIER, ${PATTERN} ${WIDTH}x${HEIGHT}@${FPS} ${BITRATE}kbps${QP:+ qp$QP}${SLICE:+, max_slice_len $SLICE} =="

# ── the probe ───────────────────────────────────────────────────────────────
sudo ip netns exec "$NS_RX" sudo -u "$RUNUSER" env RUST_LOG="${RUST_LOG:-warn}" \
    "$PROBE" --origin "$ORIGIN" --stream test0 --tier "$TIER" \
    --listen "$LISTEN" --mode peer "${CA_ARGS[@]}" "${PROBE_TLS[@]}" \
    --seconds "$SECONDS_RUN" --out "$OUT/frames.csv" 2>&1 | tee "$OUT/probe.log"

# The sender's own account of the same seconds is in `sender-stats.csv`, which
# the probe recorded off `telemetry/parallax/test0/stats/*` — see its module
# doc for why a receiver-only CSV cannot answer leg 1 at all.
{
  echo "leg=$LEG loss=${LOSS} rate=${RATE:-} delay=${DELAY:-} mtu=$MTU tier=$TIER slice=${SLICE:-none} seconds=$SECONDS_RUN"
  echo "pattern=$PATTERN size=${WIDTH}x${HEIGHT} fps=$FPS bitrate_kbps=$BITRATE qp=${QP:-default}"
  echo "origin=$ORIGIN"
  sudo ip netns exec "$NS_TX" $TC -s qdisc show dev zst
} > "$OUT/run.txt"
echo "== wrote $OUT =="
ls -la "$OUT"
