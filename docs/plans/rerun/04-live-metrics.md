# Live metric mapping, counter-rate policy, sampling (#420)

Implementation notes + observed behavior for the metric half of the pipeline
(design contract: [02-mapping.md](02-mapping.md) §2; seam: [03-sink-design.md](03-sink-design.md)).

## 1. What was built

- `mapping::RateConverter` — per-series counter→rate:
  - first sample of a series → `None` (nothing to differentiate against);
  - reset (`v1 < v0`, restart/wrap) → `None` and re-arm on the new baseline;
  - non-advancing clock (`t1 <= t0`) → `None`;
  - otherwise `(v1 - v0) * 1000 / (t1 - t0)` — timestamps are epoch **ms**, rates are per
    **second**.
  Absorbed samples are counted (`WorkerStats.rate_absorbed`), not silently lost.
- `mapping::Sampler` — per-series token check *before* conversion: global
  `sampling.max_hz_per_series` plus `sampling.per_prefix` overrides on the metric path
  (longest matching prefix wins). Because the rate converter runs *after* the sampler, a
  sub-sampled counter series still produces correct rates over the longer window (the
  converter differentiates the samples that actually pass).
- `SinkWorker` wiring for `CounterPolicy`:
  - `rate` (default): plot the rate on the metric path;
  - `raw`: plot the counter as-is;
  - `both`: rate on `<path>`, raw on `<path>/raw`.
- `zensight-rerun-demo metrics` — a synthetic publisher exercising all of it on
  **real-shaped keys** through the real bus contract (`PublisherRegistry`, declared
  publishers, CBOR, `QosClass::Telemetry` — never `session.put`):

  | Subject (under `zensight/v1/<origin>/telemetry/<producer>/`) | Value shape | Exercises |
  |---|---|---|
  | `sysinfo` · `cpu/usage` | Gauge, sine 20–50 % | plain gauge |
  | `sysinfo` · `memory/usage_percent` | Gauge, slow ramp | plain gauge |
  | `netlink` · `iface/eth0/rx_bytes` | Counter, ~1.2 MB/s + **mid-run reset** | rate + reset re-arm |
  | `netring` · `flow/red/p95_ms` | Gauge, noisy 20 ms | latency lane |
  | `netring` · `flow/red/error_ratio` | Gauge, 0 with bursts | error lane |
  | `netlink` · `ethtool/wlan0/speed_mbps` | Gauge, ≈ 300 Mb/s noise | link-quality lane |

  All six are **real registered subjects** — metric names the sysinfo/netlink/netring
  sensors actually emit. They used to be three real ones plus three invented lanes
  (`path/gateway/{rtt_ms,loss_percent}`, `wifi/wlan0/rssi_dbm`), which the subject
  registry (RFC 08, issue #468) caught the moment it stopped being a `{metric...}`
  catch-all: publishing an unregistered telemetry key now panics in debug. A demo that
  invents keys teaches the viewer — and any blueprint built on it — a keyspace that
  does not exist.

## 2. Running it (isolated end-to-end, headless)

```bash
# adapter: lone listener, scouting off, recording to a file
ZENSIGHT_ZENOH_LISTEN=tcp/127.0.0.1:7449 \
cargo run -p zensight-rerun -- --mode record --rrd-path /tmp/metrics.rrd --isolate &

# demo publisher: connects explicitly to the adapter (also scouting-off)
cargo run -p zensight-rerun --bin zensight-rerun-demo -- \
    metrics --connect tcp/127.0.0.1:7449 --duration-secs 30
```

## 3. Observed (2026-07-11, this worktree, debug build)

10 s run, 500 ms ticks (20 ticks × 6 series), record mode, isolated loopback session:

```text
demo:    published=120
adapter: telemetry_received=120  telemetry_dropped=0
         metrics=119  sink_errors=0        # 1 absorbed = rx_bytes first sample
metrics.rrd: 172146 bytes
header:  52 52 46 32 00 22 01 00           # "RRF2" + binary semver 0.34.1
```

- The 4-byte magic is `RRF2` as pinned in 01-capabilities §3, followed by the
  binary-encoded writer version `00 22 01 00` = 0.34.1 — the `.rrd` self-identifies its
  writer, which the compat policy (§3) makes operationally important.
- ~172 KiB for 119 scalar points is **~1.4 KiB/point at this scale** — chunk/schema overhead
  dominates tiny recordings (each series carries schema + static `SeriesLines` chunks; the
  batcher had little to batch at 2 Hz). Longer-run amortization is measured in
  [07-record-replay.md](07-record-replay.md).
- Incident-scenario equivalent: [06-incident.md](06-incident.md).

## 4. Findings

- **Counter policy is mandatory, not cosmetic.** Raw `rx_bytes` renders as a monotonic ramp
  whose slope is unreadable next to gauges; the viewer has no built-in derivative transform
  for scalars (assessed against the 0.34 docs — time series show logged values). Prometheus
  gets this from `rate()` at query time; with Rerun the *producer* must pre-compute it. This
  is a real ergonomic gap vs. PromQL-style backends: changing your mind later (raw↔rate)
  requires re-recording, not re-querying. (`counters: "both"` is the hedge, at 2× points.)
- **First-sample/reset absorption is visible.** A series born mid-recording starts one poll
  late, and a counter reset produces a one-interval gap instead of a negative spike — both
  the correct trade, both observable in the `.rrd`.
- **Sampling belongs in the adapter.** Rerun has viewer-side `--memory-limit` but no
  producer-side rate limiting; without the `Sampler`, a 1 s-poll netlink sensor with 40
  per-interface counters dominates the recording. Per-prefix overrides make that a config
  decision, mirroring the exporters' filter philosophy.
- Boolean-as-0/1 step series read fine in a time-series lane; no dedicated boolean archetype
  exists (nothing lost).
