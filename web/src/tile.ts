// The H.264 video tile's receive loop (#707): a port of the iced tile's
// `parallax_h264.rs`, driven by samples instead of a select loop, with the
// decoder behind an interface so the loop is testable without WebCodecs.
//
// The rules it keeps, each of which the iced twin learned the hard way:
//
// - **Keyframe gate.** Never feed the decoder before the first
//   `FrameMeta.keyframe`; a decoder fed a mid-GOP AU produces nothing useful.
// - **Sequence gap ⇒ resync.** Any break — missing AUs, a backwards jump, a
//   pipeline restart (a regression past `SEQ_RESTART_GAP`) — means the
//   reference chain is gone: drop sync, reset the decoder, ask for an IDR.
// - **One gate for keyframe requests** (`RESYNC_MIN_INTERVAL_MS`), cleared
//   only by a HEALTHY decode (nothing shed since the last picture) — clearing
//   on any decode rebuilds #435's keyframe storm out of #716's parts.
// - **A late keyframe is never shed**; it is the only frame that can restart
//   decoding. A late delta is shed and asks (through the gate).
// - **A deadline no frame can meet disarms itself** after
//   `MIN_SAMPLES_FOR_SKEW` stamped samples (a clock offset, not a backlog).
//   The reported age is never corrected.
// - **`decodeQueueSize` is the backpressure.** At `DECODE_QUEUE_CAP` the AU is
//   shed, sync dropped, an IDR asked — never block, never let latency grow
//   invisibly.
// - **Three end conditions with a reason**, never a black rectangle.
// - **Every `VideoFrame` is closed on every path** — the decoder adapter's
//   job, and the fake asserts it.
import type { FrameMeta } from "./cbor.js";
import { frameMetaOf } from "./cbor.js";
import { browserIncompatibility, codecString, profileLevelId } from "./h264.js";
import {
  DECODE_QUEUE_CAP,
  NO_DECODE_MS,
  NO_FIRST_FRAME_MS,
  REPORT_INTERVAL_MS,
  RESYNC_MIN_INTERVAL_MS,
  ReceiverStats,
  consumerId,
  observedFrameAgeMs,
} from "./receiver.js";
import type { MediaReceiverReport } from "./types.gen.js";

/** What the tile hands the decoder — `EncodedVideoChunk`'s shape without the class. */
export interface Chunk {
  type: "key" | "delta";
  /** Microseconds; from `pts_ns` when present, else 0. */
  timestamp: number;
  data: Uint8Array;
  sequence: bigint;
}

/** A picture the decoder produced, already painted; the tile only accounts for it. */
export interface Decoded {
  sequence: bigint;
  keyframe: boolean;
}

/** The decoder as the tile sees it. `WebCodecsDecoder` (in `webcodecs.ts`) is the real one. */
export interface Decoder {
  /** (Re)configure for a codec string; called on the first keyframe and after a reset. */
  configure(codec: string): void;
  /** Queue one access unit. Throws on a decoder error; the tile treats that as a decode failure. */
  decode(chunk: Chunk): void;
  /** Drop every queued AU and all reference state; the next chunk must be a keyframe. */
  reset(): void;
  /** Access units waiting for the decoder (`VideoDecoder.decodeQueueSize`). */
  queueSize(): number;
  close(): void;
}

/** One sample off the tier's key, as `Bus` delivers it. */
export interface MediaSample {
  payload: Uint8Array;
  attachment: Uint8Array | undefined;
  /** The publisher's HLC stamp in ms since the epoch, or `undefined` when unstamped. */
  publishedMs: number | undefined;
}

export interface TileEvents {
  /** Ask the sensor for a fresh IDR (already rate-limited by the tile). */
  requestKeyframe(): void;
  report(report: MediaReceiverReport): void;
  /** The tile is over. `reason` is a sentence for the operator, or `undefined` for a clean close. */
  ended(reason: string | undefined): void;
  /** The decoder was configured — the codec string, for the caption. */
  configured?(codec: string): void;
}

export interface TileOptions {
  stream: string;
  tier: string;
  decoder: Decoder;
  events: TileEvents;
  /** The frame-age deadline in ms; `undefined` disables it. */
  maxLiveLatencyMs: number | undefined;
  /** Injectable clocks for the tests. */
  now?: () => number;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
}

export class VideoTile {
  readonly stats: ReceiverStats;
  private synced = false;
  private lastResyncAt: number | undefined;
  private shedSinceDecode = false;
  private pendingReset = false;
  private firstSampleAt: number | undefined;
  private everDecoded = false;
  private anySample = false;
  private codec: string | undefined;
  private deadlineDisarmed = false;
  private over = false;
  private readonly now: () => number;
  private readonly setTimer: (fn: () => void, ms: number) => unknown;
  private readonly clearTimer: (handle: unknown) => void;
  private firstFrameTimer: unknown;
  private reportTimer: unknown;
  private readonly decoder: Decoder;
  private readonly events: TileEvents;
  private readonly maxLiveLatencyMs: number | undefined;

  constructor(opts: TileOptions) {
    this.decoder = opts.decoder;
    this.events = opts.events;
    this.maxLiveLatencyMs = opts.maxLiveLatencyMs;
    this.now = opts.now ?? (() => performance.now());
    this.setTimer = opts.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
    this.clearTimer = opts.clearTimer ?? ((h) => clearTimeout(h as ReturnType<typeof setTimeout>));
    this.stats = new ReceiverStats(opts.stream, "h264", opts.tier, consumerId(), this.now());
    // Bound the wait for the first sample so a tier that never publishes
    // (open failed on the sensor) ends with a reason.
    this.firstFrameTimer = this.setTimer(() => {
      if (!this.anySample) this.end("no video on this tier — the camera may be busy or unavailable");
    }, NO_FIRST_FRAME_MS);
    // The report cadence runs on its own clock, not off arriving frames: a
    // tile receiving nothing still reports, and that report is the useful one.
    this.scheduleReport();
  }

  private scheduleReport(): void {
    this.reportTimer = this.setTimer(() => {
      if (this.over) return;
      this.stats.setQueueDepth(this.decoder.queueSize());
      this.events.report(this.stats.snapshot(this.now()));
      this.scheduleReport();
    }, REPORT_INTERVAL_MS);
  }

  /** The sensor said the stream is closed while this tile has no picture: an end with a reason (#707's third condition). */
  onStreamClosed(): void {
    if (!this.everDecoded) this.end("the sensor closed this stream before a frame decoded");
  }

  onSample(sample: MediaSample): void {
    if (this.over) return;
    const now = this.now();
    this.anySample = true;
    this.firstSampleAt ??= now;
    // Armed on arrival, before the attachment parses: a producer emitting
    // samples nothing can read still trips the watchdog.
    const expired = !this.everDecoded && now - this.firstSampleAt >= NO_DECODE_MS;
    const ageMs = observedFrameAgeMs(sample.publishedMs, Date.now());

    const meta = frameMetaOf(sample.attachment);
    if (!meta) {
      this.stats.onUnreadableSample(ageMs);
      this.shedSinceDecode = true;
      if (expired) this.end("receiving samples with no readable frame metadata");
      return;
    }
    if (expired) {
      this.end(`receiving ${meta.width}×${meta.height} video but could not decode this tier`);
      return;
    }
    const gap = this.stats.onSample(meta, ageMs);
    if (this.synced && gap.kind !== "none") {
      this.synced = false;
      this.pendingReset = true;
      this.askForKeyframe(now);
    }

    // A deadline no frame can ever meet is not a deadline.
    let armed = this.maxLiveLatencyMs;
    if (armed !== undefined && !this.stats.deadlineIsReachable(armed)) {
      armed = undefined;
      if (!this.deadlineDisarmed) {
        this.deadlineDisarmed = true;
        console.warn(
          `${this.stats.stream}: frame-age deadline disarmed — no frame has ever been younger than ${this.maxLiveLatencyMs} ms (floor ${this.stats.minFrameAgeMs()?.toFixed(0)} ms); a clock offset, not a backlog`,
        );
      }
    }
    const late = armed !== undefined && ageMs !== undefined && ageMs > armed;
    if (late && !meta.keyframe) {
      this.shed("deadline");
      this.askForKeyframe(now);
      return;
    }
    if (!this.synced && !meta.keyframe) {
      // The reference chain is gone; this AU is undecodable, not lost. The
      // gap path already asked for a keyframe.
      this.shed("unsynced");
      return;
    }

    // Admitted: in sequence, or the keyframe that re-anchors us.
    if (meta.keyframe && this.codec === undefined) {
      if (!this.configure(sample.payload)) return;
    }
    if (this.pendingReset) {
      this.decoder.reset();
      if (this.codec !== undefined) this.decoder.configure(this.codec);
      this.pendingReset = false;
    }
    if (this.decoder.queueSize() >= DECODE_QUEUE_CAP) {
      // The decoder is behind. Shed to the next keyframe rather than let the
      // backlog grow where nothing can see it.
      this.shed("queue_full");
      this.synced = false;
      this.pendingReset = true;
      this.askForKeyframe(now);
      return;
    }
    this.synced = true;
    try {
      this.decoder.decode({
        type: meta.keyframe ? "key" : "delta",
        timestamp: meta.pts_ns === undefined ? 0 : Number(meta.pts_ns / 1000n),
        data: sample.payload,
        sequence: meta.sequence,
      });
    } catch (e) {
      this.onDecodeError(e instanceof Error ? e.message : String(e));
    }
    this.stats.setQueueDepth(this.decoder.queueSize());
  }

  /** The codec string from the first keyframe's SPS; a profile no browser decodes ends the tile with that sentence. */
  private configure(payload: Uint8Array): boolean {
    const plid = profileLevelId(payload);
    if (plid === undefined) {
      // The keyframe promise is byte-level; a keyframe with no SPS is a
      // producer defect this tile cannot work around.
      this.shed("malformed");
      this.askForKeyframe(this.now());
      return false;
    }
    const why = browserIncompatibility(plid);
    if (why) {
      this.end(why);
      return false;
    }
    this.codec = codecString(plid);
    this.decoder.configure(this.codec);
    this.events.configured?.(this.codec);
    return true;
  }

  /** The decoder produced (and the adapter painted and closed) a picture. */
  onDecoded(d: Decoded): void {
    if (this.over) return;
    this.everDecoded = true;
    if (!this.shedSinceDecode) this.lastResyncAt = undefined;
    this.shedSinceDecode = false;
    this.stats.onDecoded(d.sequence, d.keyframe, this.now());
    this.stats.setQueueDepth(this.decoder.queueSize());
  }

  /** The decoder refused something (its `error` callback, or `decode` throwing). */
  onDecodeError(message: string): void {
    if (this.over) return;
    this.stats.onShed("decode_failed");
    this.shedSinceDecode = true;
    this.synced = false;
    this.pendingReset = true;
    console.warn(`${this.stats.stream}: h264 decode failed; resyncing: ${message}`);
    this.askForKeyframe(this.now());
  }

  private shed(why: "deadline" | "unsynced" | "queue_full" | "malformed"): void {
    this.stats.onShed(why);
    this.shedSinceDecode = true;
    if (this.synced && why !== "malformed") {
      this.synced = false;
      this.pendingReset = true;
    }
  }

  private askForKeyframe(now: number): void {
    if (this.lastResyncAt === undefined || now - this.lastResyncAt >= RESYNC_MIN_INTERVAL_MS) {
      this.lastResyncAt = now;
      this.events.requestKeyframe();
    }
  }

  /** Close the tile: stops the timers and the decoder. The subscriber's undeclare is the caller's (it is the sensor's teardown signal). */
  close(): void {
    this.end(undefined);
  }

  private end(reason: string | undefined): void {
    if (this.over) return;
    this.over = true;
    this.clearTimer(this.firstFrameTimer);
    this.clearTimer(this.reportTimer);
    this.decoder.close();
    this.events.ended(reason);
  }

  get isOver(): boolean {
    return this.over;
  }
}
