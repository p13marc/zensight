// The JPEG preview tile (#707): the low-cost default, the tile you show in a
// grid. Every JPEG is independently decodable, so there is no keyframe gate,
// no reference chain and no decode queue (`decoder_queue_depth` is OMITTED,
// not zero). What it does have is the **latest-wins drain**: while one
// picture is being decoded, a newer JPEG replaces the one waiting, and the
// superseded one is counted as a `backlog` shed. That drain is for JPEG only
// — on H.264 every AU is a reference and the video tile never copies it.
import { frameMetaOf } from "./cbor.js";
import { NO_FIRST_FRAME_MS, REPORT_INTERVAL_MS, ReceiverStats, consumerId, observedFrameAgeMs } from "./receiver.js";
import type { MediaSample } from "./tile.js";
import type { MediaReceiverReport } from "./types.gen.js";

/** Paints one JPEG. Resolves when the picture is on screen; rejects if it could not be decoded. */
export type Painter = (jpeg: Uint8Array, width: number, height: number) => Promise<void>;

export interface PreviewEvents {
  report(report: MediaReceiverReport): void;
  ended(reason: string | undefined): void;
}

export interface PreviewOptions {
  stream: string;
  paint: Painter;
  events: PreviewEvents;
  now?: () => number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
}

interface Pending {
  jpeg: Uint8Array;
  sequence: bigint;
  width: number;
  height: number;
}

export class PreviewTile {
  readonly stats: ReceiverStats;
  private painting = false;
  private pending: Pending | undefined;
  private anySample = false;
  private over = false;
  private readonly now: () => number;
  private readonly setTimer: (fn: () => void, ms: number) => unknown;
  private readonly clearTimer: (handle: unknown) => void;
  private firstFrameTimer: unknown;
  private reportTimer: unknown;
  private readonly paint: Painter;
  private readonly events: PreviewEvents;

  constructor(opts: PreviewOptions) {
    this.paint = opts.paint;
    this.events = opts.events;
    this.now = opts.now ?? (() => performance.now());
    this.setTimer = opts.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = opts.clearTimer ?? ((h) => clearTimeout(h as ReturnType<typeof setTimeout>));
    this.stats = new ReceiverStats(opts.stream, "mjpeg", undefined, consumerId(), this.now());
    this.firstFrameTimer = this.setTimer(() => {
      if (!this.anySample) this.end("no preview on this stream — the camera may be busy or unavailable");
    }, NO_FIRST_FRAME_MS);
    this.scheduleReport();
  }

  private scheduleReport(): void {
    this.reportTimer = this.setTimer(() => {
      if (this.over) return;
      this.events.report(this.stats.snapshot(this.now()));
      this.scheduleReport();
    }, REPORT_INTERVAL_MS);
  }

  onSample(sample: MediaSample): void {
    if (this.over) return;
    this.anySample = true;
    const ageMs = observedFrameAgeMs(sample.publishedMs, Date.now());
    const meta = frameMetaOf(sample.attachment);
    if (!meta) {
      this.stats.onUnreadableSample(ageMs);
      return;
    }
    this.stats.onSample(meta, ageMs);
    const next: Pending = { jpeg: sample.payload, sequence: meta.sequence, width: meta.width, height: meta.height };
    if (this.painting) {
      // Latest wins: the one waiting is superseded, and that is a shed.
      if (this.pending) this.stats.onShed("backlog");
      this.pending = next;
      return;
    }
    void this.paintLoop(next);
  }

  private async paintLoop(first: Pending): Promise<void> {
    this.painting = true;
    let job: Pending | undefined = first;
    while (job && !this.over) {
      try {
        await this.paint(job.jpeg, job.width, job.height);
        this.stats.onDecoded(job.sequence, true, this.now());
      } catch {
        this.stats.onShed("decode_failed");
      }
      job = this.pending;
      this.pending = undefined;
    }
    this.painting = false;
  }

  close(): void {
    this.end(undefined);
  }

  private end(reason: string | undefined): void {
    if (this.over) return;
    this.over = true;
    this.clearTimer(this.firstFrameTimer);
    this.clearTimer(this.reportTimer);
    this.pending = undefined;
    this.events.ended(reason);
  }

  get isOver(): boolean {
    return this.over;
  }
}
