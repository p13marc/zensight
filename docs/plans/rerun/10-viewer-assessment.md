# 10 — Hands-on viewer assessment (#448)

> Status: **in progress** (started 2026-07-12). Environment: the local development
> workstation (Fedora 44, Wayland/GNOME) — the machine previously referred to as the
> pending "GPU box". CLI-verifiable items below are done; viewer-visual items are being
> worked through interactively.

## Install (feeds #427)

- Route: `cargo binstall -y rerun-cli@0.34.1` — fetched the **prebuilt**
  `x86_64-unknown-linux-gnu` binary from GitHub releases in ~49 s (no compile). This is the
  offline-mirrorable artifact for #427.
- `rerun --version` → `rerun-cli 0.34.1 … built 2026-07-07`, features include
  `native_viewer`, `web_viewer`, `map_view`, and **video: `av1 ffmpeg nasm`** — the ffmpeg
  video feature ships in the stock binary, relevant to the H.264 spike (#451).
- Perf telemetry/analytics: startup logs `Telemetry initialized enabled=false` — the CLI
  build's telemetry is off by default (cf. the `re_analytics` supply-chain note in
  [01-capabilities.md](01-capabilities.md)).

## Recording generation (repro)

Three recordings from the committed demos, isolated sessions (scouting off, loopback 7449):

```bash
# adapter (record mode), then demo, per scenario:
ZENSIGHT_ZENOH_LISTEN=tcp/127.0.0.1:7449 target/release/zensight-rerun \
    --config configs/rerun.json5 --isolate --mode record --rrd-path <name>.rrd &
target/release/zensight-rerun-demo metrics --duration-secs 30 --interval-ms 500
target/release/zensight-rerun-demo events
target/release/zensight-rerun-demo incident --base-ts 1782864000000 --pace-ms 100
```

All three pass `rerun rrd verify` ("verified without error"). Sizes: metrics 480 000 B,
events 80 150 B, incident 253 171 B.

## CLI-verifiable checklist items — verdicts

### `rerun rrd optimize` — **major positive finding**

| File | Before | After | Reduction |
|---|---|---|---|
| metrics.rrd (~366 pts, 6 series) | 480 000 B (~1.3 KiB/pt) | 36 877 B (**~100 B/pt**) | **13×** |
| incident.rrd | 253 171 B | 66 473 B | 3.8× |

The ~1.3 KiB/point live-write cost measured in [07-record-replay.md](07-record-replay.md)
is a *streaming* artifact (one chunk per log call); offline compaction reclaims it almost
entirely. The "~110 MB/day per modest host, untenable" extrapolation becomes ~8–9 MB/day
for *archived* recordings. Live-write and in-viewer memory costs still stand — but the
archival reject-signal is substantially weakened. Recommended practice: always `optimize`
recordings before storing/sharing. (#426 should measure optimize throughput; #430 should
weigh the softened archival story.)

### `rerun rrd merge` — works

`merge metrics.rrd events.rrd incident.rrd > merged.rrd` (813 226 B) verifies clean.
Multiple adapter recordings can be consolidated after the fact.

### Crash truncation (kill -9 mid-record) — **recoverable**

- `kill -9` the adapter 6 s into a 30 s recording → 97 280 B file.
- Strict `rerun rrd verify` **fails** ("Missing RRD footer / no RRD manifests") — expected,
  the footer is written on close.
- But the data stream itself decodes: `rrd stats` reads it (7 entity paths), and
  **`rerun rrd optimize crashed.rrd > repaired.rrd` produces a fully valid file**
  (verify passes, 13 chunks / 7 entity paths retained).
- Verdict: a crash mid-record loses only unflushed tail data, not the recording;
  `optimize` doubles as the repair tool. Closes the open item in
  [07-record-replay.md](07-record-replay.md).

### `--serve-web` — works; **insecure default bind (feeds #428)**

- `rerun --serve-web --port 9877 --web-viewer-port 9091 incident.rrd`: HTTP 200 on the web
  viewer, gRPC proxy up.
- **Both ports bind `0.0.0.0` by default.** `--bind 127.0.0.1` exists and works (verified
  with the native viewer: `ss` shows `127.0.0.1:9876`). #428's "forbidden configurations"
  list should lead with this: the *default* invocation exposes the viewer and proxy to the
  network. Secure baseline: always pass `--bind 127.0.0.1`.

### Native viewer launch — works on this machine

`rerun --bind 127.0.0.1 --port 9876 --memory-limit 4GB metrics.rrd events.rrd incident.opt.rrd`
opens the viewer with all three recordings loaded (Wayland, no wgpu errors). `both`-mode
adapter (`--mode both`) streams into the running viewer while writing the `.rrd` —
first live-mode run against a real viewer succeeded (previously untested headless).

### Viewer MCP — present, not yet exercised

`rerun viewer-mcp` subcommand exists in 0.34.1 ("Run an MCP server that controls a running
Rerun Viewer"). Deferred to a follow-up session; candidate for automating demo dry-runs
(#452) and screenshot capture.

## Viewer-visual checklist — pending interactive pass

The items below need eyes on the viewer (session running as of this writing). Record a
verdict + screenshot per item:

From 04/05 (metrics + events):
- [ ] TextLog level colors + selection-panel rendering of `AnyValues` attributes
- [ ] Same-millisecond AnyValues overwrite: is the 50-event burst lossy in practice?
- [ ] Event-burst readability in the TextLog view
- [ ] Dataframe view: `correlation_id` filtering — usable drill-down?

From 06 (incident):
- [ ] Blueprint-free load: does the incident tell its story without manual view setup?
- [ ] Scrubbing the cause→effect→alert chain (RSSI→loss→retransmits→RTT→alert)
- [ ] Alert `/state` step-series bracketing the incident
- [ ] Cross-view cursor correlation on hover

From 08 (live):
- [ ] Viewer kill/restart under live stream: reconnect behavior, data gap
- [ ] Memory panel under sustained live stream; purge behavior at `--memory-limit`

From 09 (topology):
- [ ] Graph force-layout stability while scrubbing
- [ ] Fleet-scale readability (≥ 20 hosts)
- [ ] Node-selection → linked series navigation

## Artifacts

Recordings + logs under `.run/rerun-assessment/` (gitignored; regenerate with the commands
above — incident is timeline-deterministic via `--base-ts 1782864000000`).
