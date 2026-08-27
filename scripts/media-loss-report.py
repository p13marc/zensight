#!/usr/bin/env python3
"""media-loss-report.py — turn a media-loss-lab run into the numbers #713 asks for.

Kept separate from the lab on purpose: a run that took ten minutes behind a
netem qdisc is re-analysable without re-running it, and the CSV stays the wire
rather than the wire plus a verdict.

    scripts/media-loss-report.py target/media-loss/quic-snow-*/ [...]

What it will NOT do is guess. A sequence gap is only loss if the sender says it
sent the frames; `sender-stats.csv` carries `stats/drops` (the AppSink shedding
under a slow consumer) and `stats/rc_drops` (the rate controller skipping
frames), and those are SUBTRACTED before anything is called wire loss. Leg 1's
whole answer is that after that subtraction nothing remains.
"""

import csv
import statistics
import sys
from pathlib import Path

# A backwards jump this large is a pipeline restart re-anchoring at 0, not
# 4 billion lost frames. Same constant, same reasoning, as the GUI's
# `SEQ_RESTART_GAP` in zensight/src/view/specialized/parallax_receiver.rs.
SEQ_RESTART_GAP = 1000


def read_frames(path):
    rows = []
    with open(path, newline="") as fh:
        for r in csv.DictReader(fh):
            if not r["seq"]:
                # An unreadable attachment is still a sample that arrived.
                rows.append(None)
                continue
            rows.append(
                {
                    "recv_ns": int(r["recv_ns"]),
                    "seq": int(r["seq"]),
                    "key": r["keyframe"] == "1",
                    "bytes": int(r["bytes"]),
                    "age_ms": float(r["age_ms"]) if r["age_ms"] else None,
                }
            )
    return rows


def read_stats(path):
    """Per-metric (first, last) over the window, plus the last value.

    Counters are cumulative since the sensor started, and the sensor starts
    before the probe subscribes. Reading the LAST value as "what the sender
    shed during this run" credits the run with every frame shed while nobody
    was watching, which is how a clean run reports negative wire loss. The
    growth across the window is the only honest reading.
    """
    seen = {}
    if not path.exists():
        return seen
    with open(path, newline="") as fh:
        for r in csv.DictReader(fh):
            k = r["metric"].split("/")[-1]
            v = float(r["value"])
            if k in seen:
                seen[k] = (seen[k][0], v)
            else:
                seen[k] = (v, v)
    return seen


def growth(stats, key):
    lo, hi = stats.get(key, (0.0, 0.0))
    return max(0.0, hi - lo)


def latest(stats, key):
    return stats.get(key, (float("nan"), float("nan")))[1]


def gaps(rows):
    """Missing sequence numbers, as a list of burst lengths.

    Burst length, not a count: whether the controller's input should be frame
    loss or gap BURST length is one of the three things #713 exists to decide,
    and a mean rate cannot answer it.
    """
    bursts, prev = [], None
    for r in rows:
        if r is None:
            continue
        s = r["seq"]
        if prev is None:
            prev = s
            continue
        if s == prev + 1 or s <= prev:
            if s <= prev and prev - s >= SEQ_RESTART_GAP:
                pass  # restart: re-anchor, do not count 4e9 lost frames
            prev = max(prev, s)
            continue
        bursts.append(s - prev - 1)
        prev = s
    return bursts


def summarize(run_dir):
    run_dir = Path(run_dir)
    meta = (run_dir / "run.txt").read_text() if (run_dir / "run.txt").exists() else ""
    rows = read_frames(run_dir / "frames.csv")
    stats = read_stats(run_dir / "sender-stats.csv")
    got = [r for r in rows if r is not None]
    if not got:
        return f"## {run_dir.name}\n\nNo readable samples. {meta}\n"

    first, last = got[0]["seq"], max(r["seq"] for r in got)
    expected = last - first + 1
    received = len(got)
    burst = gaps(got)
    missing = sum(burst)

    # What the sender admits it never put on the wire.
    #
    # ONLY `drops`. The two counters are not interchangeable and adding them
    # was this analysis's first bug: `drops` is derived at egress from gaps in
    # the sequence the AppSink handed on (`zensight-sensor-parallax/src/
    # egress.rs:158`), so it is exactly the sender's share of the gaps the
    # receiver sees. `rc_drops` is the encoder skipping a frame under rate
    # control BEFORE a sequence number exists, so it lowers the frame rate and
    # creates no gap at all — subtracting it credits the wire with frames that
    # were never numbered, and turns a clean run into negative loss. The runs
    # here show it plainly: `rc_drops` grew by 10 on a run whose receiver saw a
    # contiguous 0..259.
    shed = growth(stats, "drops")
    skipped = growth(stats, "rc_drops")
    unexplained = missing - shed

    keys = [r for r in got if r["key"]]
    deltas = [r for r in got if not r["key"]]
    ages = [r["age_ms"] for r in got if r["age_ms"] is not None]
    unstamped = sum(1 for r in got if r["age_ms"] is None)

    def sizes(rs):
        if not rs:
            return "—"
        b = [r["bytes"] for r in rs]
        return f"{min(b)}/{int(statistics.median(b))}/{max(b)}"

    mtu = 1200
    for tok in meta.split():
        if tok.startswith("mtu="):
            mtu = int(tok.split("=")[1])
    key_frag = (
        max(1, -(-int(statistics.median([r["bytes"] for r in keys])) // mtu)) if keys else 0
    )

    # The prediction under test: zenoh fragments an access unit across
    # datagrams and defragmentation is all-or-nothing, so an AU survives only
    # if EVERY one of its datagrams does — P(survive) = (1-p)^n. Datagram
    # payload is taken as MTU minus IPv4+UDP+QUIC headers, which is an
    # estimate; it is stated rather than hidden because the model's shape, not
    # its third decimal, is what #720 would rely on.
    loss_pct = 0.0
    for tok in meta.split():
        if tok.startswith("loss="):
            loss_pct = float(tok.split("=")[1] or 0)
    payload = mtu - 20 - 8 - 30
    predicted = ""
    if loss_pct and received:
        p = loss_pct / 100.0
        med = statistics.median([r["bytes"] for r in got])
        n = max(1, -(-int(med) // payload))
        pred = (1 - (1 - p) ** n) * 100
        obs = 100.0 * unexplained / max(1, expected - shed)
        predicted = (
            f"| model: median AU is {n} datagrams @{payload}B → "
            f"1-(1-{p:g})^{n} = **{pred:.1f}%** lost, observed **{obs:.1f}%** |"
        )

    out = [f"## {run_dir.name}", "", "```", meta.strip(), "```", ""]
    out += [
        "| | |",
        "|---|---|",
        f"| samples received | {received} |",
        f"| sequence span | {first}..{last} ({expected} expected) |",
        f"| missing sequences | {missing} |",
        f"| sender shed at egress (`drops`, gap-forming) | {shed:.0f} |",
        f"| encoder skipped (`rc_drops`, no gap) | {skipped:.0f} |",
        f"| **unexplained by the sender** | **{unexplained:.0f}** |",
        f"| gap bursts (len) | {sorted(burst, reverse=True)[:12] or '—'} |",
        f"| longest burst | {max(burst) if burst else 0} |",
        f"| keyframes received | {len(keys)} |",
        f"| deltas received | {len(deltas)} |",
        f"| keyframe bytes min/med/max | {sizes(keys)} |",
        f"| delta bytes min/med/max | {sizes(deltas)} |",
        f"| median keyframe ≈ datagrams @{mtu}B | {key_frag} |",
        f"| frame age ms med/max | "
        + (
            f"{statistics.median(ages):.2f}/{max(ages):.2f} |"
            if ages
            else "not asked |"
        ),
        f"| unstamped samples | {unstamped} |",
        f"| sender fps / kbps (last) | {latest(stats, 'fps'):.1f} / "
        f"{latest(stats, 'kbps'):.0f} |",
    ]
    if predicted:
        out.append(predicted)
    out.append("")
    return "\n".join(out)


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    print("\n".join(summarize(d) for d in argv[1:]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
