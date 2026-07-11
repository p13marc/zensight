# Record mode & headless `.rrd` verification (#424)

The headless half of the evaluation: prove that a GPU-less box (sensor host, CI, this
sandbox) can capture a fully valid recording that a viewer elsewhere replays.

## 1. What is verified in CI (`tests/record_e2e.rs`)

One **lone** Zenoh session (scouting multicast+gossip off, no endpoints — the live-fleet
contamination guard), real declared-publisher CBOR publications (60 metric points across 6
series, an alert firing→resolved pair, one `HostEntity`), the real subscriber → worker →
`RerunSink(record)` pipeline, then:

- pipeline counters: 60 received, **59 metrics** (the counter series' first sample absorbed
  by the rate converter, `rate_absorbed == 1`), 2 alerts, 1 entity, 0 sink errors;
- file exists, `len > 1 KiB`;
- first 4 bytes are **`RRF2`** (the FourCC pinned in [01-capabilities.md](01-capabilities.md) §3);
- a **full decode pass**: `re_log_encoding::rrd::DecoderApp::decode_lazy` (same
  exact-pinned 0.34.1 as the sink) parses the stream end-to-end, with ≥ 1 `SetStoreInfo`
  and a healthy `ArrowMsg` chunk count. The planned timebox ("size+magic suffices if the
  decoder is disproportionate") was not needed — the decoder crate is already in the
  dependency tree and the API (`Decoder::decode_lazy(BufRead) -> iterator`) worked first try.

Observed (2026-07-11, debug build): the e2e run finishes in ~7 s wall, dominated by the
deliberate 5 s subscriber-readiness wait (entity-seed GET timeout headroom).

## 2. Storage cost

`zensight-rerun/scripts/storage-cost.sh` (adapter record + metrics demo, 6 series at 2 Hz,
500 ms ticks; debug build, 2026-07-11):

| duration (s) | points | .rrd bytes | bytes/point |
|---|---|---|---|
| 10 | 120 | 172,194 | 1,434 |
| 30 | 360 | 480,644 | 1,335 |
| 60 | 720 | 941,433 | 1,307 |

Reading: **~1.3 KiB per scalar point at 2 Hz, and it barely amortizes with duration** —
the marginal cost between the 30 s and 60 s runs is still ~1.28 KiB/point. The batcher
flushes on time, so at low per-series rates every chunk stays tiny and the per-chunk
envelope (Arrow IPC schema + `RowId`s + timeline columns) dominates; fixed overhead
(store info, `SeriesLines` styling) is only a small part. Extrapolated: a modest
6-series host at 2 Hz ≈ **110 MB/day**; a real fleet (thousands of series) is untenable
without batching-tuning (`ChunkBatcherConfig`), sampling, or offline `rerun rrd optimize`
(untested here — needs the CLI). ZenSight's own CBOR points are ~100 B on the wire — the
recording costs >10× the raw telemetry at this cadence. **Reject-signal for continuous
recording; acceptable for bounded incident captures.**

Note `rerun rrd optimize` / `merge` exist for offline compaction of many small recordings —
untested here (needs the `rerun` CLI, which is not part of the sdk-only build).

## 3. Crash truncation

The RRD format is a linear sequence of framed messages with an *optional* footer
(01-capabilities §3): a crash-truncated file loads up to the last complete message, losing
only the tail and the random-access index. The adapter additionally flushes
(`flush_blocking`) on clean shutdown. Not force-tested here; flagged for the GPU-box
session (kill -9 the adapter mid-recording, open the file).

## 4. Replay on a GPU box

```bash
# inspect without a GPU (any box with the rerun CLI):
rerun rrd stats  capture.rrd
rerun rrd verify capture.rrd

# replay (GPU box):
rerun capture.rrd

# or serve to a browser from a headless server:
rerun --serve-web capture.rrd    # then open the printed URL
```

`rerun rrd` also offers `filter`/`split`/`merge`/`migrate` (CLI manual, 01-capabilities §3);
`migrate` matters if a capture must outlive one Rerun minor release.

## 5. Live vs record vs both

- `record` is the headless workhorse (this doc).
- `live` (`connect_grpc` to `rerun+http://…:9876/proxy`) buffers in-process while the viewer
  is away — unbounded; adapter-side sampling is the control (03-sink-design §4).
- `both` (`set_sinks(GrpcSink, FileSink)`) compiles and constructs against 0.34.1 (verified
  at commit 4); its runtime behavior needs a reachable viewer → GPU-box item.
