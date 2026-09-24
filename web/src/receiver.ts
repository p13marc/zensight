// The receiver half's accounting (#707, the port of the iced tile's
// `parallax_receiver.rs` — `zensight/docs/media-receiver.md` is the write-up).
// Every number a tile reports comes from here, with the same meaning as the
// iced twin's so both clients' reports read the same.
//
// Constants match the iced tile exactly; a report field means the same thing
// on both clients or the producer's aggregate mixes apples and oranges.
import type { FrameMeta } from "./cbor.js";
import type { MediaReceiverReport } from "./types.gen.js";

/** Report cadence. RFC 07 §1.1's reference is one per few seconds; the registry's ceiling is one per second. */
export const REPORT_INTERVAL_MS = 3000;
/** Access units that may wait for the decoder — the `decodeQueueSize` ceiling (#717). */
export const DECODE_QUEUE_CAP = 8;
/** Every path that drops sync asks for an IDR through one gate (#435). */
export const RESYNC_MIN_INTERVAL_MS = 2000;
/** A regression this large is the producer's pipeline counter going back to ~0, not reordering. */
export const SEQ_RESTART_GAP = 300n;
/** Stamped samples before a tile will call its own deadline unreachable. */
export const MIN_SAMPLES_FOR_SKEW = 30;
/** No sample at all on the tier's key within this: "no video on this tier". */
export const NO_FIRST_FRAME_MS = 10_000;
/** Samples arriving but nothing decoded within this: the tier cannot be decoded here. */
export const NO_DECODE_MS = 12_000;
/** The frame-age deadline's default and bounds; `0` disables (#716). */
export const DEFAULT_MAX_LIVE_LATENCY_MS = 1500;
export const MAX_LIVE_LATENCY_RANGE = { min: 100, max: 30_000 } as const;
/** Above this frame age a losing Transport hop is a congested sender, not a lossy link (#801). */
export const CONGESTED_AGE_MS = 500;

const JITTER_GAIN = 16;

/** Validate a deadline read from settings: `0` off, otherwise clamped to the range, `undefined` for garbage. */
export function maxLiveLatencyFrom(value: unknown): number | undefined {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) return DEFAULT_MAX_LIVE_LATENCY_MS;
  if (value === 0) return undefined;
  return Math.min(MAX_LIVE_LATENCY_RANGE.max, Math.max(MAX_LIVE_LATENCY_RANGE.min, value));
}

/** A frame this consumer shed on purpose — `dropped_frames`, by cause (kept local for the log). */
export type Shed = "deadline" | "queue_full" | "unsynced" | "backlog" | "malformed" | "decode_failed";

export type Gap =
  | { kind: "none" }
  | { kind: "missing"; count: bigint }
  | { kind: "restart" }
  | { kind: "backward" };

/** `zs-web-<random>`: in the payload, never in a key (RFC 07 §1.1); stable for one tile incarnation. */
export function consumerId(): string {
  const r = Math.floor(Math.random() * 0xffff_ffff).toString(16).padStart(8, "0");
  return `zs-web-${r}`;
}

export class ReceiverStats {
  private received = 0n;
  private lost = 0n;
  private decoded = 0n;
  private readonly sheds: Record<Shed, bigint> = {
    deadline: 0n,
    queue_full: 0n,
    unsynced: 0n,
    backlog: 0n,
    malformed: 0n,
    decode_failed: 0n,
  };
  private lastSequence = 0n;
  private prevSequence: bigint | undefined;
  private intervalStarted: number;
  private agesMs: number[] = [];
  private unstampedCount = 0;
  private jitterMs: number | undefined;
  private prevTransitMs: number | undefined;
  private stamped = 0;
  private minAgeMs: number | undefined;
  private queueDepth: number | undefined;
  private lastKeyframeSequence: bigint | undefined;
  private lastKeyframeAt: number | undefined;

  constructor(
    readonly stream: string,
    readonly codec: string | undefined,
    readonly tier: string | undefined,
    readonly consumer: string,
    now: number,
  ) {
    this.intervalStarted = now;
  }

  /** One sample arrived with readable metadata. Returns the sequence verdict. */
  onSample(meta: FrameMeta, ageMs: number | undefined): Gap {
    this.received += 1n;
    this.foldAge(ageMs);
    const prev = this.prevSequence;
    let gap: Gap;
    if (prev === undefined || meta.sequence === prev + 1n) gap = { kind: "none" };
    else if (meta.sequence <= prev && prev - meta.sequence >= SEQ_RESTART_GAP) gap = { kind: "restart" };
    else if (meta.sequence <= prev) gap = { kind: "backward" };
    else gap = { kind: "missing", count: meta.sequence - prev - 1n };

    switch (gap.kind) {
      case "missing":
        this.lost += gap.count;
        this.lastSequence = meta.sequence;
        break;
      case "restart":
        // The producer's counter went back to ~0: its old high-water mark says
        // nothing about where this consumer is in the new domain.
        this.lastSequence = meta.sequence;
        break;
      default:
        if (meta.sequence > this.lastSequence) this.lastSequence = meta.sequence;
    }
    this.prevSequence = meta.sequence;
    return gap;
  }

  private foldAge(ageMs: number | undefined): void {
    if (ageMs === undefined) {
      this.unstampedCount += 1;
      return;
    }
    this.agesMs.push(ageMs);
    this.stamped += 1;
    this.minAgeMs = this.minAgeMs === undefined ? ageMs : Math.min(this.minAgeMs, ageMs);
    // RFC 3550 inter-arrival jitter over the transit times. Needs both
    // clocks, so an unstamped stream has none — and reports none.
    if (this.prevTransitMs !== undefined) {
      const d = Math.abs(ageMs - this.prevTransitMs);
      this.jitterMs = this.jitterMs === undefined ? d : this.jitterMs + (d - this.jitterMs) / JITTER_GAIN;
    }
    this.prevTransitMs = ageMs;
  }

  onShed(why: Shed): void {
    this.sheds[why] += 1n;
  }

  /** A sample with no readable FrameMeta: it arrived, so it has exactly one cause. */
  onUnreadableSample(ageMs: number | undefined): void {
    this.received += 1n;
    this.foldAge(ageMs);
    this.sheds.malformed += 1n;
  }

  onDecoded(sequence: bigint, keyframe: boolean, now: number): void {
    this.decoded += 1n;
    if (keyframe) {
      this.lastKeyframeSequence = sequence;
      this.lastKeyframeAt = now;
    }
  }

  setQueueDepth(depth: number | undefined): void {
    this.queueDepth = depth;
  }

  /**
   * Whether a deadline of `limitMs` is one any frame could ever meet here.
   * The smallest age ever observed separates a real backlog (some frames
   * arrive fresh, the floor is small) from a systematic clock offset (the
   * floor IS the offset). A deadline under the floor is disarmed — but only
   * after enough stamped samples that the floor has had a chance to fall.
   */
  deadlineIsReachable(limitMs: number): boolean {
    if (this.minAgeMs === undefined || this.stamped < MIN_SAMPLES_FOR_SKEW) return true;
    return this.minAgeMs <= limitMs;
  }

  minFrameAgeMs(): number | undefined {
    return this.minAgeMs;
  }

  shedCounts(): Readonly<Record<Shed, bigint>> {
    return this.sheds;
  }

  counts(): { received: bigint; lost: bigint; decoded: bigint; dropped: bigint } {
    return { received: this.received, lost: this.lost, decoded: this.decoded, dropped: this.totalSheds() };
  }

  private totalSheds(): bigint {
    let t = 0n;
    for (const v of Object.values(this.sheds)) t += v;
    return t;
  }

  /**
   * The report to send, and the start of a fresh timing window. Counters stay
   * cumulative (a resend is idempotent); only the window resets. Absent is
   * NEVER zero: unstamped means no `frame_age_ms`, no queue means no
   * `decoder_queue_depth` (RFC 07 §1.3).
   */
  snapshot(now: number): MediaReceiverReport {
    // The sensor refuses `interval_ms == 0`; one millisecond is a lie of at
    // most one millisecond, a refusal loses the whole report.
    const intervalMs = Math.min(0xffff_ffff, Math.max(1, Math.round(now - this.intervalStarted)));
    const ages = this.agesMs;
    const report: MediaReceiverReport = {
      stream: this.stream,
      consumer_id: this.consumer,
      interval_ms: intervalMs,
      received_frames: num(this.received),
      lost_frames: num(this.lost),
      dropped_frames: num(this.totalSheds()),
      decoded_frames: num(this.decoded),
      last_sequence: num(this.lastSequence),
    };
    if (this.codec !== undefined) report.codec = this.codec;
    if (this.tier !== undefined) report.tier = this.tier;
    if (ages.length > 0) {
      if (this.jitterMs !== undefined) report.interarrival_jitter_ms = this.jitterMs;
      report.frame_age_ms = medianOf(ages);
      report.frame_age_max_ms = Math.max(...ages);
    }
    if (this.queueDepth !== undefined) report.decoder_queue_depth = this.queueDepth;
    if (this.lastKeyframeSequence !== undefined) report.last_keyframe_sequence = num(this.lastKeyframeSequence);
    if (this.lastKeyframeAt !== undefined) {
      report.since_last_keyframe_ms = Math.min(0xffff_ffff, Math.max(0, Math.round(now - this.lastKeyframeAt)));
    }
    this.intervalStarted = now;
    this.agesMs = [];
    this.unstampedCount = 0;
    return report;
  }

  unstamped(): number {
    return this.unstampedCount;
  }
}

/** JSON has no u64; a counter past 2^53 is not a real concern for a tab, and the generated type says `number`. */
function num(b: bigint): number {
  return Number(b);
}

/** Lower median on an even count, matching the sensor's own fold. */
export function medianOf(v: readonly number[]): number {
  const s = [...v].sort((a, b) => a - b);
  return s[(s.length - 1) >> 1]!;
}

/**
 * The frame-age clock (RFC 07 §1.3, `zensight_common::media::observed_frame_age_ms`):
 * the publisher's HLC sample timestamp minus local arrival. Unstamped is NOT
 * ASKED, never zero; a negative age is the clock-skew evidence and is not
 * clamped.
 */
export function observedFrameAgeMs(publishedMs: number | undefined, nowMs: number): number | undefined {
  return publishedMs === undefined ? undefined : nowMs - publishedMs;
}
