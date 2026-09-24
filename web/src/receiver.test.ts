import { describe, expect, it } from "vitest";
import type { FrameMeta } from "./cbor.js";
import {
  DECODE_QUEUE_CAP,
  MIN_SAMPLES_FOR_SKEW,
  REPORT_INTERVAL_MS,
  RESYNC_MIN_INTERVAL_MS,
  SEQ_RESTART_GAP,
  ReceiverStats,
  maxLiveLatencyFrom,
  medianOf,
  observedFrameAgeMs,
} from "./receiver.js";

const meta = (sequence: bigint, keyframe = false): FrameMeta => ({ keyframe, sequence, width: 16, height: 16 });

describe("the constants match the iced tile, so both clients' reports mean the same thing", () => {
  it("DECODE_QUEUE_CAP 8, RESYNC 2 s, SEQ_RESTART_GAP 300, report cadence 3 s, 30 samples before a floor", () => {
    expect(DECODE_QUEUE_CAP).toBe(8);
    expect(RESYNC_MIN_INTERVAL_MS).toBe(2000);
    expect(SEQ_RESTART_GAP).toBe(300n);
    expect(REPORT_INTERVAL_MS).toBe(3000);
    expect(MIN_SAMPLES_FOR_SKEW).toBe(30);
  });

  it("maxLiveLatencyMs: default 1500, 0 disables, clamped 100…30000, garbage is the default", () => {
    expect(maxLiveLatencyFrom(undefined)).toBe(1500);
    expect(maxLiveLatencyFrom("x")).toBe(1500);
    expect(maxLiveLatencyFrom(0)).toBeUndefined();
    expect(maxLiveLatencyFrom(10)).toBe(100);
    expect(maxLiveLatencyFrom(99_999)).toBe(30_000);
    expect(maxLiveLatencyFrom(800)).toBe(800);
  });
});

describe("sequence gaps", () => {
  it("in order is no gap; a hole is `missing` and counts as lost (the network's doing)", () => {
    const s = new ReceiverStats("cam0", "h264", "low", "zs-web-1", 0);
    expect(s.onSample(meta(10n), undefined).kind).toBe("none");
    expect(s.onSample(meta(11n), undefined).kind).toBe("none");
    expect(s.onSample(meta(14n), undefined)).toEqual({ kind: "missing", count: 2n });
    expect(s.counts()).toMatchObject({ received: 3n, lost: 2n });
  });

  it("a regression past SEQ_RESTART_GAP is a producer restart, not loss, and re-anchors last_sequence", () => {
    const s = new ReceiverStats("cam0", "h264", "low", "zs-web-1", 0);
    s.onSample(meta(1000n), undefined);
    expect(s.onSample(meta(3n), undefined).kind).toBe("restart");
    expect(s.counts().lost).toBe(0n);
    expect(s.snapshot(1).last_sequence).toBe(3);
  });

  it("a small backwards jump is a reorder: not loss, not a restart, and the high-water mark stays", () => {
    const s = new ReceiverStats("cam0", "h264", "low", "zs-web-1", 0);
    s.onSample(meta(50n), undefined);
    expect(s.onSample(meta(48n), undefined).kind).toBe("backward");
    expect(s.counts().lost).toBe(0n);
    expect(s.snapshot(1).last_sequence).toBe(50);
  });
});

describe("the frame-age clock and the deadline floor", () => {
  it("unstamped is NOT ASKED, never zero; a negative age is not clamped", () => {
    expect(observedFrameAgeMs(undefined, 1000)).toBeUndefined();
    expect(observedFrameAgeMs(750, 1000)).toBe(250);
    expect(observedFrameAgeMs(1040, 1000)).toBe(-40);
  });

  it("the deadline stays armed until 30 stamped samples, then disarms when no frame was ever under it", () => {
    const s = new ReceiverStats("cam0", "h264", "low", "zs-web-1", 0);
    for (let i = 0; i < MIN_SAMPLES_FOR_SKEW - 1; i++) s.onSample(meta(BigInt(i)), 3000);
    expect(s.deadlineIsReachable(1500)).toBe(true);
    s.onSample(meta(100n), 2900);
    expect(s.deadlineIsReachable(1500)).toBe(false);
    expect(s.minFrameAgeMs()).toBe(2900);
    // One fresh frame ever and the deadline is reachable again: a backlog, not an offset.
    s.onSample(meta(101n), 12);
    expect(s.deadlineIsReachable(1500)).toBe(true);
  });
});

describe("the snapshot", () => {
  it("counters are cumulative, the window resets, and absent is never zero", () => {
    const s = new ReceiverStats("cam0", "h264", "low", "zs-web-7", 1000);
    s.onSample(meta(1n, true), 20);
    s.onSample(meta(2n), 40);
    s.onSample(meta(4n), 30); // one lost
    s.onShed("deadline");
    s.onDecoded(1n, true, 1500);
    s.setQueueDepth(3);
    const r1 = s.snapshot(4000);
    expect(r1).toMatchObject({
      stream: "cam0",
      codec: "h264",
      tier: "low",
      consumer_id: "zs-web-7",
      interval_ms: 3000,
      received_frames: 3,
      lost_frames: 1,
      dropped_frames: 1,
      decoded_frames: 1,
      last_sequence: 4,
      frame_age_ms: 30, // lower median of [20, 40, 30]
      frame_age_max_ms: 40,
      decoder_queue_depth: 3,
      last_keyframe_sequence: 1,
      since_last_keyframe_ms: 2500,
    });
    expect(r1.interarrival_jitter_ms).toBeCloseTo(20 + (10 - 20) / 16, 6);
    // Next window: nothing arrived. Counters persist, timing is absent.
    const r2 = s.snapshot(7000);
    expect(r2).toMatchObject({ received_frames: 3, lost_frames: 1, interval_ms: 3000 });
    expect(r2.frame_age_ms).toBeUndefined();
    expect(r2.interarrival_jitter_ms).toBeUndefined();
    expect(r2.frame_age_max_ms).toBeUndefined();
  });

  it("an unstamped stream reports no age and no jitter; a preview reports no queue depth", () => {
    const s = new ReceiverStats("cam0", "mjpeg", undefined, "zs-web-2", 0);
    s.onSample(meta(1n, true), undefined);
    s.onSample(meta(2n, true), undefined);
    const r = s.snapshot(3000);
    expect(r.frame_age_ms).toBeUndefined();
    expect(r.interarrival_jitter_ms).toBeUndefined();
    expect(r.decoder_queue_depth).toBeUndefined();
    expect(r.tier).toBeUndefined();
    expect(r.codec).toBe("mjpeg");
    expect(s.unstamped()).toBe(0); // reset by the snapshot
  });

  it("interval_ms is never 0 — the sensor refuses a zero-span snapshot", () => {
    const s = new ReceiverStats("cam0", "h264", "low", "zs-web-1", 5);
    expect(s.snapshot(5).interval_ms).toBe(1);
  });

  it("a malformed sample is received and shed (every frame that arrives has exactly one cause)", () => {
    const s = new ReceiverStats("cam0", "h264", "low", "zs-web-1", 0);
    s.onUnreadableSample(12);
    expect(s.counts()).toMatchObject({ received: 1n, dropped: 1n });
    expect(s.shedCounts().malformed).toBe(1n);
  });

  it("medianOf takes the lower middle on an even count, like the sensor", () => {
    expect(medianOf([4, 1, 3, 2])).toBe(2);
    expect(medianOf([7])).toBe(7);
  });
});
