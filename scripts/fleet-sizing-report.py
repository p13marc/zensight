#!/usr/bin/env python3
"""fleet-sizing-report.py — turn a fleet-sizing run into the table #944 asks for.

Kept separate from `fleet-sizing.sh` on purpose, the same way
`media-loss-report.py` is kept separate from `media-loss-lab.sh`: a run that
took fourteen days is re-analysable without re-running it, and the captured
health documents stay the evidence rather than the evidence plus a verdict.

    scripts/fleet-sizing-report.py <rundir>

WHAT IT WILL NOT DO IS GUESS.

Every quadlet unit in `packaging/quadlet/` carries the same line — "MemoryMax
below is a STARTING POINT (reference-fleet sizing); measure yours" — and the
reason no measured table ever replaced it is that the numbers have to come off a
real fleet over real time. So this reports what the health documents said and
nothing else:

  * a producer that never published `self_stats` is listed as **not measured**,
    never as zero. An older sensor and an idle one must not render identically.
  * a producer present for only part of the window is reported with the span it
    was actually seen over, because a percentile across time it was dead for is
    a number about nothing.
  * `memory.max` and `oom_kills` come from the sensor's own cgroup reading, so
    the suggestion is compared against what the host *actually* allows rather
    than against what a unit file is supposed to say.
  * the suggested cap is `max x headroom` rounded up to a whole 16 MiB. It is
    only as good as the window it came from — a fourteen-day soak and a
    ten-minute smoke run produce the same table shape and very different advice
    — so the window is printed with it, every time.
"""

import json
import math
import sys
from pathlib import Path

MIB = 1024 * 1024


def read_ndjson(path):
    """Yield parsed lines, skipping anything unparseable rather than dying.

    A run is append-only and may have been killed mid-write, so the last line
    can be a fragment. Losing it is correct; losing the fourteen days in front
    of it because of it is not.
    """
    bad = 0
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                bad += 1
    if bad:
        print(f"note: {bad} unparseable line(s) in {path.name}", file=sys.stderr)


def parse_rpc_replies(text):
    """Yield (key, value) from `historian-query`/`rpc_get` output.

    Those print a key line followed by pretty JSON. Rather than pattern-matching
    braces at column 0 — a guess about a formatter — this walks the text with
    `raw_decode`, which consumes exactly one JSON value and says where it ended.
    """
    dec = json.JSONDecoder()
    i, n = 0, len(text)
    while i < n:
        j = text.find("{", i)
        if j < 0:
            return
        head = text[i:j].strip().splitlines()
        key = head[-1].strip() if head else "<unknown>"
        try:
            value, end = dec.raw_decode(text, j)
        except json.JSONDecodeError:
            i = j + 1
            continue
        yield key, value
        i = end


def producer_of(key):
    """`.../v1/<origin>/state/<producer>/health` -> (origin, producer)."""
    parts = key.split("/")
    try:
        k = parts.index("state")
    except ValueError:
        return ("?", "?")
    return (
        parts[k - 1] if k >= 1 else "?",
        parts[k + 1] if k + 1 < len(parts) else "?",
    )


def pct(values, q):
    """Nearest-rank percentile. Small-n honest: p95 of 3 samples is the max."""
    if not values:
        return None
    s = sorted(values)
    return s[max(1, math.ceil(q / 100 * len(s))) - 1]


def mib(v):
    return "—" if v is None else f"{v / MIB:.1f}"


def human_secs(s):
    s = int(s)
    if s >= 172800:
        return f"{s // 86400}d {(s % 86400) // 3600}h"
    if s >= 3600:
        return f"{s // 3600}h {(s % 3600) // 60}m"
    return f"{s // 60}m {s % 60}s"


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} <rundir>")
    run = Path(sys.argv[1])
    meta = json.loads((run / "run.json").read_text())
    headroom = float(meta["headroom"])

    health = run / "health.ndjson"
    if not health.exists():
        sys.exit(f"FAIL: {health} does not exist — this is not a fleet-sizing run.")

    by_producer = {}
    for rec in read_ndjson(health):
        doc = rec.get("v")
        if not isinstance(doc, dict) or "sensor" not in doc:
            continue
        by_producer.setdefault(producer_of(rec["key"]), []).append(rec)

    if not by_producer:
        sys.exit(
            "FAIL: the run captured no health documents at all.\n"
            f"Nothing published `v1/*/state/*/health` on {meta['hub']} during "
            f"{meta['window_human']}. Every ZenSight sensor publishes that document "
            "every five seconds, so this is a bus with no sensors on it, or an "
            "endpoint that reaches a different one — never a fleet that happens to "
            "use no memory.\n"
            f"  is a sensor running?   just sensors\n"
            f"  right endpoint?        HUB=tcp/<host>:7447 scripts/fleet-sizing.sh\n"
            f"  raw capture:           {health}"
        )

    out = []
    w = out.append
    hosts = {o for o, _ in by_producer}
    w("## Per-sensor memory, measured")
    w("")
    w(
        f"{len(by_producer)} producer(s) on {len(hosts)} host(s), watched for "
        f"**{meta['window_human']}** ending {meta['ended_utc']}, at most one sample "
        f"per producer per {meta['sample_interval_secs']}s."
    )
    w("")
    w(
        "| host | producer | samples | seen for | RSS p50 | RSS p95 | RSS max | "
        "CPU p95 | declared budget | cgroup memory.max | suggested MemoryMax | "
        "ladder | OOM |"
    )
    w("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|")

    notes = []
    window = float(meta["elapsed_secs"]) or 1.0
    # A producer's observed span can never reach the full window: the throttle
    # means the first sample lands up to one interval in and the last one up to
    # one interval before the end. Allowing two intervals of slack keeps a short
    # window from flagging every healthy producer as absent — which the first
    # version did, and a warning that fires on everything is read as noise.
    slack = 2 * float(meta["sample_interval_secs"])
    for (origin, producer), recs in sorted(by_producer.items()):
        docs = [r["v"] for r in recs]
        span = (recs[-1]["t"] - recs[0]["t"]) / 1000.0
        span_txt = human_secs(span)
        if span < window - slack:
            span_txt = f"**{span_txt}**"
            notes.append(
                f"`{producer}` on `{origin}` was only seen for {human_secs(span)} of a "
                f"{meta['window_human']} window. It started late, stopped early, or "
                "restarted — size it from a window it was present for."
            )

        stats = [d["self_stats"] for d in docs if d.get("self_stats")]
        if not stats:
            w(
                f"| `{origin}` | {producer} | {len(docs)} | {span_txt} | "
                "**not measured** | | | | | | | | |"
            )
            notes.append(
                f"`{producer}` on `{origin}` published {len(docs)} health document(s) "
                "with no `self_stats` at all — a build predating #811, or health "
                "published through `snapshot` rather than `snapshot_with_self`. It is "
                "not sized by this run."
            )
            continue

        rss = [s["rss_bytes"] for s in stats if s.get("rss_bytes") is not None]
        cpu = [s["cpu_percent"] for s in stats if s.get("cpu_percent") is not None]
        budget = next((s["budget_bytes"] for s in stats if s.get("budget_bytes")), None)
        cgs = [s["cgroup"] for s in stats if s.get("cgroup")]
        cg_max = next((c["memory_max_bytes"] for c in cgs if c.get("memory_max_bytes")), None)
        ooms = max((c.get("oom_kills") or 0 for c in cgs), default=0)
        steps = [s["ladder"].get("step", 0) for s in stats if s.get("ladder")]
        peak_step = max(steps) if steps else 0
        ladder = "no ladder" if not steps else (
            "nominal" if peak_step == 0 else f"**peaked at step {peak_step}**"
        )
        futile = any(s["ladder"].get("futile") for s in stats if s.get("ladder"))

        peak = max(rss) if rss else None
        cap = (
            int(math.ceil(peak * headroom / (16 * MIB))) * 16 * MIB if peak else None
        )
        w(
            f"| `{origin}` | {producer} | {len(docs)} | {span_txt} | "
            f"{mib(pct(rss, 50))} | {mib(pct(rss, 95))} | {mib(peak)} | "
            f"{('%.1f%%' % pct(cpu, 95)) if cpu else '—'} | {mib(budget)} | "
            f"{mib(cg_max)} | **{mib(cap)}** | {ladder} | {ooms or ''} |"
        )

        if ooms:
            notes.append(
                f"`{producer}` on `{origin}` has **{ooms} OOM kill(s)** in its cgroup. "
                "Every number in its row comes from the survivors of that, so read the "
                "peak as a floor and not as a peak."
            )
        if peak_step > 0:
            notes.append(
                f"`{producer}` on `{origin}` reached shed-ladder step {peak_step}"
                + (" and reported eviction **futile**" if futile else "")
                + ". It was staying inside its budget by dropping work, so its RSS is "
                "what the budget forced and not what the workload wanted. Raise "
                "`budget_rss_mb` and measure again before sizing from this row."
            )
        if cap and cg_max and cap > cg_max:
            notes.append(
                f"`{producer}` on `{origin}`: the suggested cap ({mib(cap)} MiB) is "
                f"above what its cgroup allows today ({mib(cg_max)} MiB) — the "
                "2026-08-17 shape. Raise the unit's `MemoryMax`, or it is killed at "
                "the next peak."
            )
        if not budget:
            notes.append(
                f"`{producer}` on `{origin}` declares **no memory budget**, so it arms "
                "no `sensor-budget` alert and no shed ladder: it grows silently until "
                "the cgroup kills it. Set `budget_rss_mb` below the unit's `MemoryMax`."
            )

    w("")
    w(
        f"All sizes MiB. `suggested MemoryMax` = observed max x {headroom:g} headroom, "
        "rounded up to 16 MiB — and worth exactly as much as the window above."
    )

    if notes:
        w("")
        w("### What this run wants you to look at")
        w("")
        for note in dict.fromkeys(notes):
            w(f"- {note}")

    hist = run / "historian-stats.txt"
    if hist.exists():
        w("")
        w("## Historian")
        w("")
        pairs = list(parse_rpc_replies(hist.read_text()))
        if not pairs:
            w(
                "No historian answered `@rpc/historian/stats`. A fleet without one is "
                "a supported deployment; if you expected one, it is not running or not "
                "reachable from this endpoint."
            )
        for key, doc in pairs:
            w(f"- `{key}`")
            for k, v in sorted(doc.items()):
                if isinstance(v, (int, float, str, bool)):
                    w(f"  - `{k}`: {v}")
        if pairs:
            w("")
            w(
                "Growth per day needs two of these separated by real time; one run "
                "reports the file as it stands. `zensight-historian/docs/storage.md` "
                "holds the acceptance numbers these are measured against (#911)."
            )

    print("\n".join(out))


if __name__ == "__main__":
    main()
