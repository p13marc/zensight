// The write half of the control plane (#706): open / close / keyframe as a
// `Command<StreamControl>` on `v1/<origin>/@rpc/parallax/stream/set`.
//
// Two things the iced GUI learned the hard way, kept here on purpose:
//
// 1. **Close must be profile-correct.** A `CloseStream` without `codec`
//    resolves to the sensor's DEFAULT video tier and decrements the wrong
//    refcount; a preview or a non-default tier must name its own codec and
//    tier. `Profile.closeCommand()` is the only way this module spells a
//    close, and it always names both.
// 2. **Close-then-open, in order.** Switching tiers captures the outgoing
//    profile's close BEFORE the new open is built, and the two are sent on
//    one serial queue per origin — each command's reply is awaited before
//    the next is sent — so the sensor's actor sees them in arrival order.
import type { Bus } from "./bus.js";
import { Origin, streamSetKey } from "./keys.js";
import { outcomeOf, type Outcome } from "./rpc.js";
import type { StreamCommand, StreamControl } from "./types.gen.js";

/** The default video codec; the only one a browser decodes today (#707). */
export const VIDEO_CODEC = "h264";
/** The preview codec's name on the control plane. Its key is `…/preview/jpeg`. */
export const PREVIEW_CODEC = "mjpeg";

/**
 * One opened profile of a stream: exactly what was asked for, so its close
 * can name it. `tier` is `undefined` only for the preview, whose key has no
 * tier chunk.
 */
export class Profile {
  private constructor(
    readonly stream: string,
    readonly codec: string,
    readonly tier: string | undefined,
  ) {}

  static video(stream: string, tier: string): Profile {
    return new Profile(stream, VIDEO_CODEC, tier);
  }

  static preview(stream: string): Profile {
    return new Profile(stream, PREVIEW_CODEC, undefined);
  }

  openCommand(): StreamControl {
    return this.tier === undefined
      ? { type: "open_stream", stream: this.stream, codec: this.codec }
      : { type: "open_stream", stream: this.stream, codec: this.codec, tier: this.tier };
  }

  /** Names codec AND tier, always — see the module note. */
  closeCommand(): StreamControl {
    return this.tier === undefined
      ? { type: "close_stream", stream: this.stream, codec: this.codec }
      : { type: "close_stream", stream: this.stream, codec: this.codec, tier: this.tier };
  }

  /** RFC 07 §1: the Nth viewer gets no matching-listener edge and must ask. */
  keyframeCommand(): StreamControl {
    return this.tier === undefined
      ? { type: "request_keyframe", stream: this.stream }
      : { type: "request_keyframe", stream: this.stream, tier: this.tier };
  }

  equals(other: Profile): boolean {
    return this.stream === other.stream && this.codec === other.codec && this.tier === other.tier;
  }
}

/** What was sent and what came back, for a log pane. */
export interface Sent {
  origin: Origin;
  command: StreamControl;
  outcome: Outcome;
}

/**
 * The control plane of one host. Commands are serialised per instance —
 * one at a time, each awaited — which is the ordering guarantee above.
 */
export class ControlPlane {
  private queue: Promise<unknown> = Promise.resolve();
  private seq = 0;

  constructor(
    private readonly bus: Bus,
    readonly origin: Origin,
    private readonly onSent?: (s: Sent) => void,
    private readonly timeoutMs?: number,
  ) {}

  open(profile: Profile): Promise<Outcome> {
    return this.send(profile.openCommand());
  }

  close(profile: Profile): Promise<Outcome> {
    return this.send(profile.closeCommand());
  }

  requestKeyframe(profile: Profile): Promise<Outcome> {
    return this.send(profile.keyframeCommand());
  }

  /**
   * Replace `from` with `to`: the outgoing profile's close is captured first
   * and sent first. Returns the open's outcome; a refused close is reported
   * through `onSent` but does not stop the open, because the sensor's
   * refcount for the old profile is its own to reconcile on the subscriber's
   * falling edge (#707 undeclares on close).
   */
  async switchTo(from: Profile | undefined, to: Profile): Promise<Outcome> {
    if (from && from.equals(to)) return this.requestKeyframe(to);
    const close = from?.closeCommand();
    const open = to.openCommand();
    if (close) await this.send(close);
    return this.send(open);
  }

  private send(body: StreamControl): Promise<Outcome> {
    const command: StreamCommand = { id: `web-${++this.seq}`, body };
    const run = async (): Promise<Outcome> => {
      const opts = this.timeoutMs === undefined ? {} : { timeoutMs: this.timeoutMs };
      const replies = await this.bus.get(streamSetKey(this.origin), {
        payload: JSON.stringify(command),
        ...opts,
      });
      const outcome = outcomeOf(replies);
      this.onSent?.({ origin: this.origin, command: body, outcome });
      return outcome;
    };
    // Chain behind whatever is in flight; a failure earlier in the chain
    // must not poison later commands, hence the `catch` before the link.
    const next = this.queue.catch(() => undefined).then(run);
    this.queue = next;
    return next;
  }
}
