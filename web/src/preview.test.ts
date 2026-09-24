import { describe, expect, it } from "vitest";
import { PreviewTile } from "./preview.js";
import { NO_FIRST_FRAME_MS, REPORT_INTERVAL_MS } from "./receiver.js";
import type { MediaSample } from "./tile.js";
import type { MediaReceiverReport } from "./types.gen.js";

const utf8 = new TextEncoder();
const text = (s: string) => [0x60 | s.length, ...utf8.encode(s)];
const meta = (sequence: number): Uint8Array =>
  new Uint8Array([0xa4, ...text("keyframe"), 0xf5, ...text("sequence"), 0x18, sequence, ...text("width"), 0x18, 64, ...text("height"), 0x18, 48]);
const jpeg = (n: number): MediaSample => ({ payload: new Uint8Array([0xff, 0xd8, n]), attachment: meta(n), publishedMs: undefined });

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

/** A painter the test releases by hand, to hold a decode "in progress". */
function harness() {
  const clock = new Clock();
  const painted: number[] = [];
  const pending: (() => void)[] = [];
  const reports: MediaReceiverReport[] = [];
  const ended: (string | undefined)[] = [];
  const tile = new PreviewTile({
    stream: "cam0",
    paint: (bytes) =>
      new Promise<void>((resolve) => {
        pending.push(() => {
          painted.push(bytes[2]!);
          resolve();
        });
      }),
    events: { report: (r) => reports.push(r), ended: (r) => ended.push(r) },
    now: clock.now,
    setTimer: clock.set,
    clearTimer: clock.clear,
  });
  const release = async () => {
    pending.shift()?.();
    await new Promise((r) => setTimeout(r, 0));
  };
  return { clock, tile, painted, reports, ended, release };
}

describe("the JPEG preview tile", () => {
  it("has no keyframe gate — every JPEG paints", async () => {
    const h = harness();
    h.tile.onSample(jpeg(1));
    await h.release();
    expect(h.painted).toEqual([1]);
    expect(h.tile.stats.counts().decoded).toBe(1n);
  });

  it("latest wins while a paint is in progress: the superseded JPEG is a backlog shed", async () => {
    const h = harness();
    h.tile.onSample(jpeg(1)); // painting…
    h.tile.onSample(jpeg(2)); // waiting
    h.tile.onSample(jpeg(3)); // replaces 2
    h.tile.onSample(jpeg(4)); // replaces 3
    await h.release(); // 1 done → 4 starts
    await h.release(); // 4 done
    expect(h.painted).toEqual([1, 4]);
    expect(h.tile.stats.shedCounts().backlog).toBe(2n);
    expect(h.tile.stats.counts()).toMatchObject({ received: 4n, decoded: 2n, dropped: 2n });
  });

  it("reports without a decode queue depth, and ends with a reason when nothing ever arrives", () => {
    const h = harness();
    h.clock.advance(REPORT_INTERVAL_MS);
    expect(h.reports[0]).toMatchObject({ stream: "cam0", codec: "mjpeg", received_frames: 0 });
    expect(h.reports[0]?.decoder_queue_depth).toBeUndefined();
    expect(h.reports[0]?.tier).toBeUndefined();
    h.clock.advance(NO_FIRST_FRAME_MS);
    expect(h.ended).toEqual(["no preview on this stream — the camera may be busy or unavailable"]);
  });

  it("a paint that fails is a decode-failed shed, not a crash", async () => {
    const clock = new Clock();
    const tile = new PreviewTile({
      stream: "cam0",
      paint: () => Promise.reject(new Error("not a jpeg")),
      events: { report: () => {}, ended: () => {} },
      now: clock.now,
      setTimer: clock.set,
      clearTimer: clock.clear,
    });
    tile.onSample(jpeg(1));
    await new Promise((r) => setTimeout(r, 0));
    expect(tile.stats.shedCounts().decode_failed).toBe(1n);
  });
});
