// Origin resolution (#706, "the second trap"): a browser has no fleet model,
// so before it can spell a single key it must find out WHICH hosts run a
// parallax sensor. The sensor's liveliness token
// (`v1/<origin>/state/parallax/alive`) is the answer — a token, not a data
// surface, so the fleet selector is legal here and only here.
import type { Bus, Undeclare } from "./bus.js";
import { Origin, aliveSelector, originOfAliveKey } from "./keys.js";

export interface OriginsView {
  /** Hosts whose parallax sensor is alive, sorted. */
  readonly alive: readonly Origin[];
}

/**
 * Keep a live set of parallax origins. `onChange` fires after every edge,
 * including the initial replay, so a UI can render the picker once and
 * repaint on change. Returns the subscription's undeclare.
 */
export async function watchOrigins(
  bus: Bus,
  onChange: (view: OriginsView) => void,
): Promise<Undeclare> {
  const set = new Map<string, Origin>();
  const emit = () =>
    onChange({
      alive: [...set.values()].sort((a, b) => a.value.localeCompare(b.value)),
    });
  const undeclare = await bus.livelinessSubscribe(aliveSelector(), (s) => {
    const origin = originOfAliveKey(s.key);
    if (!origin) return;
    if (s.alive) set.set(origin.value, origin);
    else set.delete(origin.value);
    emit();
  });
  // A liveliness subscriber with `history` replays the tokens already up,
  // but a fleet with none replays nothing; say so rather than leave the
  // picker blank with no signal.
  emit();
  return undeclare;
}

/** One-shot: the parallax origins alive right now. */
export async function listOrigins(bus: Bus): Promise<Origin[]> {
  const keys = await bus.livelinessGet(aliveSelector());
  const out = new Map<string, Origin>();
  for (const k of keys) {
    const o = originOfAliveKey(k);
    if (o) out.set(o.value, o);
  }
  return [...out.values()].sort((a, b) => a.value.localeCompare(b.value));
}
