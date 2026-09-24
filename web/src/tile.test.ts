import { describe, expect, it } from "vitest";
import {
  DECODE_QUEUE_CAP,
  NO_DECODE_MS,
  NO_FIRST_FRAME_MS,
  REPORT_INTERVAL_MS,
  RESYNC_MIN_INTERVAL_MS,
} from "./receiver.js";
import { VideoTile, type Chunk, type Decoder, type MediaSample } from "./tile.js";
import type { MediaReceiverReport } from "./types.gen.js";

// ── a deterministic clock with timers ────────────────────────────────────
class Clock {
  t = 0;
  private timers: { at: number; fn: () => void; id: number }[] = [];
  private next = 1;
  now = () => this.t;
  set = (fn: () => void, ms: number) => {
    const id = this.next++;
    this.timers.push({ at: this.t + ms, fn, id });
    return id;
  };
  clear = (h: unknown) => {
    this.timers = this.timers.filter((x) => x.id !== h);
  };
  advance(ms: number): void {
    const until = this.t + ms;
    for (;;) {
      const due = this.timers.filter((x) => x.at <= until).sort((a, b) => a.at - b.at)[0];
      if (!due) break;
      this.t = due.at;
      this.timers = this.timers.filter((x) => x.id !== due.id);
      due.fn();
    }
    this.t = until;
  }
}

// ── a decoder that records everything and can be told to be slow ─────────
class FakeDecoder implements Decoder {
  configured: string[] = [];
  decoded: Chunk[] = [];
  resets = 0;
  closed = false;
  queue = 0;
  throwOnDecode = false;
  configure(codec: string): void {
    this.configured.push(codec);
  }
  decode(chunk: Chunk): void {
    if (this.throwOnDecode) throw new Error("bad NAL");
    this.decoded.push(chunk);
    this.queue += 1;
  }
  reset(): void {
    this.resets += 1;
    this.queue = 0;
  }
  queueSize(): number {
    return this.queue;
  }
  close(): void {
    this.closed = true;
  }
}

// ── CBOR FrameMeta by hand: {keyframe, sequence, width, height[, pts_ns]} ──
const utf8 = new TextEncoder();
function cborText(s: string): number[] {
  const b = utf8.encode(s);
  return [0x60 | b.length, ...b];
}
function cborUint(n: bigint): number[] {
  if (n < 24n) return [Number(n)];
  if (n < 256n) return [0x18, Number(n)];
  if (n < 65536n) return [0x19, Number(n >> 8n), Number(n & 0xffn)];
  const out = [0x1b];
  for (let i = 7; i >= 0; i--) out.push(Number((n >> BigInt(i * 8)) & 0xffn));
  return out;
}
function frameMeta(keyframe: boolean, sequence: bigint, ptsNs?: bigint): Uint8Array {
  const entries: number[] = [];
  let n = 4;
  entries.push(...cborText("keyframe"), keyframe ? 0xf5 : 0xf4);
  if (ptsNs !== undefined) {
    n++;
    entries.push(...cborText("pts_ns"), ...cborUint(ptsNs));
  }
  entries.push(...cborText("sequence"), ...cborUint(sequence));
  entries.push(...cborText("width"), 0x18, 64);
  entries.push(...cborText("height"), 0x18, 48);
  return new Uint8Array([0xa0 | n, ...entries]);
}
// A keyframe carries an SPS (High 4.0) + PPS + IDR; a delta is a single slice.
const KEY_AU = new Uint8Array([0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xac, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 1, 0x65, 0x88]);
const DELTA_AU = new Uint8Array([0, 0, 0, 1, 0x41, 0x9a, 0x22]);
const NO_SPS_KEY_AU = new Uint8Array([0, 0, 1, 0x65, 0x88]);

function sample(keyframe: boolean, sequence: bigint, opts: { ageMs?: number; payload?: Uint8Array; pts?: bigint } = {}): MediaSample {
  const s: MediaSample = {
    payload: opts.payload ?? (keyframe ? KEY_AU : DELTA_AU),
    attachment: frameMeta(keyframe, sequence, opts.pts),
    publishedMs: opts.ageMs === undefined ? undefined : Date.now() - opts.ageMs,
  };
  return s;
}

function harness(maxLiveLatencyMs: number | undefined = 1500) {
  const clock = new Clock();
  const decoder = new FakeDecoder();
  const keyframes: number[] = [];
  const reports: MediaReceiverReport[] = [];
  const ended: (string | undefined)[] = [];
  const configured: string[] = [];
  const tile = new VideoTile({
    stream: "cam0",
    tier: "low",
    decoder,
    maxLiveLatencyMs,
    now: clock.now,
    setTimer: clock.set,
    clearTimer: clock.clear,
    events: {
      requestKeyframe: () => keyframes.push(clock.t),
      report: (r) => reports.push(r),
      ended: (reason) => ended.push(reason),
      configured: (c) => configured.push(c),
    },
  });
  return { clock, decoder, tile, keyframes, reports, ended, configured };
}

describe("the keyframe gate and the codec string", () => {
  it("never feeds the decoder before the first keyframe, then configures from that keyframe's SPS", () => {
    const h = harness();
    h.tile.onSample(sample(false, 1n));
    h.tile.onSample(sample(false, 2n));
    expect(h.decoder.decoded).toHaveLength(0);
    expect(h.decoder.configured).toHaveLength(0);
    h.tile.onSample(sample(true, 3n, { pts: 5_000_000n }));
    expect(h.decoder.configured).toEqual(["avc1.640028"]);
    expect(h.configured).toEqual(["avc1.640028"]);
    expect(h.decoder.decoded.map((c) => [c.type, c.timestamp])).toEqual([["key", 5000]]);
    h.tile.onSample(sample(false, 4n));
    expect(h.decoder.decoded.map((c) => c.type)).toEqual(["key", "delta"]);
    expect(h.tile.stats.shedCounts().unsynced).toBe(2n);
  });

  it("a keyframe with no SPS is malformed: shed, and a fresh IDR asked for", () => {
    const h = harness();
    h.tile.onSample(sample(true, 1n, { payload: NO_SPS_KEY_AU }));
    expect(h.decoder.decoded).toHaveLength(0);
    expect(h.tile.stats.shedCounts().malformed).toBe(1n);
    expect(h.keyframes).toHaveLength(1);
  });

  it("a profile no browser decodes ends the tile with that sentence", () => {
    const h = harness();
    const high10 = new Uint8Array([0, 0, 0, 1, 0x67, 0x6e, 0x00, 0x28, 0xac, 0, 0, 1, 0x65]);
    h.tile.onSample(sample(true, 1n, { payload: high10 }));
    expect(h.ended).toEqual([expect.stringMatching(/profile_idc 110/)]);
    expect(h.decoder.closed).toBe(true);
  });
});

describe("resync on a sequence gap, through one rate-limited gate", () => {
  it("a gap drops sync, resets and reconfigures the decoder before the next keyframe, and asks once per 2 s", () => {
    const h = harness();
    h.tile.onSample(sample(true, 1n));
    h.tile.onSample(sample(false, 2n));
    h.tile.onSample(sample(false, 5n)); // 3 and 4 missing
    expect(h.keyframes).toEqual([0]);
    expect(h.tile.stats.counts().lost).toBe(2n);
    expect(h.decoder.decoded).toHaveLength(2); // the gapped delta was shed as unsynced
    expect(h.tile.stats.shedCounts().unsynced).toBe(1n);
    // More deltas before the IDR: shed, and NOT asked again inside the gate.
    h.clock.advance(500);
    h.tile.onSample(sample(false, 6n));
    expect(h.keyframes).toEqual([0]);
    // The IDR arrives: the pending reset lands first, then the keyframe decodes.
    h.tile.onSample(sample(true, 7n));
    expect(h.decoder.resets).toBe(1);
    expect(h.decoder.configured).toEqual(["avc1.640028", "avc1.640028"]);
    expect(h.decoder.decoded.at(-1)?.type).toBe("key");
  });

  it("a counter regression past 300 is a producer restart: re-anchor, do not freeze", () => {
    const h = harness();
    h.tile.onSample(sample(true, 1000n));
    h.tile.onSample(sample(false, 1001n));
    h.tile.onSample(sample(true, 2n)); // the sensor's pipeline restarted
    expect(h.tile.stats.counts().lost).toBe(0n);
    expect(h.keyframes).toHaveLength(1);
    expect(h.decoder.decoded.at(-1)?.sequence).toBe(2n);
    h.tile.onSample(sample(false, 3n));
    expect(h.decoder.decoded.at(-1)?.sequence).toBe(3n);
  });

  it("the gate clears only on a HEALTHY decode — never on the keyframe the tile just asked for", () => {
    const h = harness();
    h.tile.onSample(sample(true, 1n));
    h.tile.onDecoded({ sequence: 1n, keyframe: true });
    h.tile.onSample(sample(false, 3n)); // gap → ask at t=0
    expect(h.keyframes).toEqual([0]);
    h.clock.advance(100);
    h.tile.onSample(sample(true, 4n)); // the IDR asked for
    h.tile.onDecoded({ sequence: 4n, keyframe: true }); // a decode after a shed: NOT healthy
    h.tile.onSample(sample(false, 9n)); // another gap right away
    // still inside the 2 s gate; clearing it here would rebuild #435
    expect(h.keyframes).toEqual([0]);
    h.clock.advance(RESYNC_MIN_INTERVAL_MS);
    h.tile.onSample(sample(true, 10n));
    h.tile.onDecoded({ sequence: 10n, keyframe: true });
    h.tile.onSample(sample(false, 11n));
    h.tile.onDecoded({ sequence: 11n, keyframe: false }); // healthy: nothing shed since
    h.tile.onSample(sample(false, 20n));
    expect(h.keyframes).toEqual([0, RESYNC_MIN_INTERVAL_MS + 100]);
  });
});

describe("the frame-age deadline (#716)", () => {
  it("sheds a late delta and asks for an IDR; NEVER sheds a late keyframe", () => {
    const h = harness(1500);
    h.tile.onSample(sample(true, 1n, { ageMs: 10 }));
    h.tile.onSample(sample(false, 2n, { ageMs: 3000 }));
    expect(h.tile.stats.shedCounts().deadline).toBe(1n);
    expect(h.keyframes).toEqual([0]);
    h.tile.onSample(sample(true, 3n, { ageMs: 3000 }));
    expect(h.decoder.decoded.map((c) => c.sequence)).toEqual([1n, 3n]);
  });

  it("an unstamped sample never trips it — unknown age is not age zero", () => {
    const h = harness(1500);
    h.tile.onSample(sample(true, 1n));
    h.tile.onSample(sample(false, 2n));
    expect(h.tile.stats.shedCounts().deadline).toBe(0n);
    expect(h.decoder.decoded).toHaveLength(2);
  });

  it("a deadline no frame can ever meet disarms itself after 30 stamped samples, and the age is still reported raw", () => {
    const h = harness(1500);
    // Every frame 3 s old: a clock offset. First the gate: keyframes decode, deltas shed…
    h.tile.onSample(sample(true, 0n, { ageMs: 3000 }));
    for (let i = 1; i < 30; i++) h.tile.onSample(sample(i % 10 === 0, BigInt(i), { ageMs: 3000 }));
    const shedBefore = h.tile.stats.shedCounts().deadline;
    expect(shedBefore).toBeGreaterThan(0n);
    // …then, with 30 stamped samples and a floor of 3000 ms, the deadline is off.
    h.tile.onSample(sample(true, 30n, { ageMs: 3000 }));
    h.tile.onSample(sample(false, 31n, { ageMs: 3000 }));
    expect(h.tile.stats.shedCounts().deadline).toBe(shedBefore);
    expect(h.decoder.decoded.at(-1)?.sequence).toBe(31n);
    h.clock.advance(REPORT_INTERVAL_MS);
    expect(h.reports[0]?.frame_age_ms).toBeGreaterThanOrEqual(2990);
  });
});

describe("decode queue backpressure (#717)", () => {
  it("at DECODE_QUEUE_CAP the AU is shed, sync dropped, an IDR asked, and the depth is reported", () => {
    const h = harness(undefined);
    h.tile.onSample(sample(true, 1n));
    for (let i = 2n; i <= BigInt(DECODE_QUEUE_CAP); i++) h.tile.onSample(sample(false, i));
    expect(h.decoder.queue).toBe(DECODE_QUEUE_CAP);
    h.tile.onSample(sample(false, 9n));
    expect(h.tile.stats.shedCounts().queue_full).toBe(1n);
    expect(h.keyframes).toEqual([0]);
    h.clock.advance(REPORT_INTERVAL_MS);
    expect(h.reports[0]?.decoder_queue_depth).toBe(DECODE_QUEUE_CAP);
    expect(h.reports[0]?.dropped_frames).toBe(1);
  });

  it("a decoder error is a shed, a reset, and an ask", () => {
    const h = harness(undefined);
    h.tile.onSample(sample(true, 1n));
    h.decoder.throwOnDecode = true;
    h.tile.onSample(sample(false, 2n));
    expect(h.tile.stats.shedCounts().decode_failed).toBe(1n);
    expect(h.keyframes).toEqual([0]);
    h.decoder.throwOnDecode = false;
    h.tile.onSample(sample(false, 3n)); // unsynced now
    expect(h.tile.stats.shedCounts().unsynced).toBe(1n);
    h.tile.onSample(sample(true, 4n));
    expect(h.decoder.resets).toBe(1);
  });
});

describe("three end conditions with a reason, never a black rectangle", () => {
  it("no sample at all within 10 s", () => {
    const h = harness();
    h.clock.advance(NO_FIRST_FRAME_MS - 1);
    expect(h.ended).toEqual([]);
    h.clock.advance(1);
    expect(h.ended).toEqual(["no video on this tier — the camera may be busy or unavailable"]);
    expect(h.decoder.closed).toBe(true);
    expect(h.tile.isOver).toBe(true);
  });

  it("samples arriving but nothing decoding within 12 s", () => {
    const h = harness();
    h.tile.onSample(sample(false, 1n)); // never a keyframe
    h.clock.advance(NO_DECODE_MS);
    h.tile.onSample(sample(false, 2n));
    expect(h.ended).toEqual(["receiving 64×48 video but could not decode this tier"]);
  });

  it("samples with no readable metadata: received, shed, and the watchdog still fires", () => {
    const h = harness();
    h.tile.onSample({ payload: DELTA_AU, attachment: new Uint8Array([0xff]), publishedMs: undefined });
    expect(h.tile.stats.counts()).toMatchObject({ received: 1n, dropped: 1n });
    h.clock.advance(NO_DECODE_MS);
    h.tile.onSample({ payload: DELTA_AU, attachment: undefined, publishedMs: undefined });
    expect(h.ended).toEqual(["receiving samples with no readable frame metadata"]);
  });

  it("the sensor closing the stream before a frame decoded", () => {
    const h = harness();
    h.tile.onSample(sample(true, 1n));
    h.tile.onStreamClosed();
    expect(h.ended).toEqual(["the sensor closed this stream before a frame decoded"]);
    // …but not once a picture is up: the status can lag a reopen.
    const h2 = harness();
    h2.tile.onSample(sample(true, 1n));
    h2.tile.onDecoded({ sequence: 1n, keyframe: true });
    h2.tile.onStreamClosed();
    expect(h2.ended).toEqual([]);
  });

  it("reports every 3 s on its own clock, even with nothing arriving; close stops it and the decoder", () => {
    const h = harness();
    h.clock.advance(REPORT_INTERVAL_MS * 2);
    expect(h.reports).toHaveLength(2);
    expect(h.reports[0]).toMatchObject({ stream: "cam0", codec: "h264", tier: "low", received_frames: 0 });
    expect(h.reports[0]?.frame_age_ms).toBeUndefined();
    h.tile.close();
    expect(h.ended).toEqual([undefined]);
    expect(h.decoder.closed).toBe(true);
    h.clock.advance(REPORT_INTERVAL_MS * 2);
    expect(h.reports).toHaveLength(2);
  });
});
