// What a write procedure answers (`zensight_common::served::WriteQuery`):
// an empty OK reply when the gate said yes and the producer acted, or a
// `reply_err` carrying `RpcError` as JSON. Not in the generated types because
// `RpcError` is not an entry of the fleet type table (it is the error
// envelope, not a payload).
import type { BusReply } from "./bus.js";
import { json } from "./json.js";

export interface RpcError {
  /** The error's name, e.g. `invalid_args`. */
  error: string;
  message: string;
  /** Which switch refused a write, when the gate set one (#866). */
  refused_by?: string;
}

export type Outcome =
  | { ok: true }
  | { ok: false; error: RpcError }
  /** Nobody answered within the timeout: the host's sensor is down, or it does not serve this procedure. */
  | { ok: false; error: undefined; unanswered: true };

/** Reduce the replies to a write call into one outcome. */
export function outcomeOf(replies: BusReply[]): Outcome {
  for (const r of replies) {
    if (r.kind === "err") {
      let error: RpcError;
      try {
        error = json<RpcError>(r.payload);
      } catch {
        error = { error: "malformed_error", message: new TextDecoder().decode(r.payload) };
      }
      return { ok: false, error };
    }
  }
  if (replies.length === 0) return { ok: false, error: undefined, unanswered: true };
  return { ok: true };
}

/** One sentence for a log line or a toast. */
export function describe(outcome: Outcome): string {
  if (outcome.ok) return "executed";
  if (outcome.error === undefined) return "no answer — is that host's parallax sensor up?";
  const by = outcome.error.refused_by ? ` (refused by ${outcome.error.refused_by})` : "";
  return `${outcome.error.error}: ${outcome.error.message}${by}`;
}
