// The catalogue and the per-stream status — the read half of the control
// plane (#706).
//
//   catalogue   GET  v1/<origin>/@rpc/parallax/streams       → StreamDescriptor[] (JSON)
//   status      SUB  v1/<origin>/state/parallax/stream/*      → StreamStatus (JSON)
//
// `StreamDescriptor` is capability-bearing on purpose: native width/height/
// fps plus the offered tiers let a viewer build a sensible tier selector
// BEFORE opening anything.
import type { Bus, Undeclare } from "./bus.js";
import { json } from "./json.js";
import { Origin, streamOfStatusKey, streamStatusKey, streamsKey } from "./keys.js";
import type { StreamDescriptors, StreamStatus } from "./types.gen.js";

export type StreamDescriptor = StreamDescriptors[number];

/**
 * The catalogue of one host. Every replier is a queryable on that host's own
 * key, so the first `ok` reply is the answer; an `err` is the sensor refusing
 * (it has no reason to on a read) and an empty list means nothing answered.
 */
export async function fetchCatalogue(
  bus: Bus,
  origin: Origin,
  timeoutMs?: number,
): Promise<StreamDescriptor[] | undefined> {
  const opts = timeoutMs === undefined ? {} : { timeoutMs };
  const replies = await bus.get(streamsKey(origin), opts);
  for (const r of replies) {
    if (r.kind === "ok") return json<StreamDescriptors>(r.payload);
  }
  return undefined;
}

/**
 * Watch every stream's status on one host. The map is replaced, never
 * mutated in place, so a renderer can compare by identity.
 */
export async function watchStatus(
  bus: Bus,
  origin: Origin,
  onChange: (statuses: ReadonlyMap<string, StreamStatus>) => void,
): Promise<Undeclare> {
  let current = new Map<string, StreamStatus>();
  return bus.subscribe(streamStatusKey(origin, "*"), (s) => {
    const stream = streamOfStatusKey(s.key, origin);
    if (!stream) return;
    const next = new Map(current);
    if (!s.alive) {
      next.delete(stream);
    } else {
      let status: StreamStatus;
      try {
        status = json<StreamStatus>(s.payload);
      } catch {
        return; // a document this client cannot read is not a status
      }
      // The key names the stream; a document claiming another name is a
      // publisher bug, and the key wins because it is what was subscribed.
      next.set(stream, { ...status, stream });
    }
    current = next;
    onChange(current);
  });
}

/** The tiers a descriptor offers for `codec`, best first — what a selector lists. */
export function tierChoices(d: StreamDescriptor): readonly string[] {
  return (d.tiers ?? []).map((t) => t.name);
}

/** `640×360 @ 15` from the native capability, or `unknown`. */
export function nativeLabel(d: StreamDescriptor): string {
  if (d.width == null || d.height == null) return "unknown";
  const fps = d.fps == null ? "" : ` @ ${Math.round(d.fps)}`;
  return `${d.width}×${d.height}${fps}`;
}
