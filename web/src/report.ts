// Sending a tile's `MediaReceiverReport` to its own producer (#718's browser
// half): a write on `v1/<origin>/@rpc/parallax/stream/report`, JSON, every 3 s
// for as long as the tile is open. Addressed, never broadcast — the origin is
// the tile's own. A refusal is said ONCE: a producer that refuses (an older
// sensor with no report queryable) would otherwise put a red line in the log
// every 3 s per tile, forever.
import type { Bus } from "./bus.js";
import { Origin } from "./keys.js";
import { describe, outcomeOf, type Outcome } from "./rpc.js";
import type { MediaReceiverReport } from "./types.gen.js";

export function streamReportKey(origin: Origin): string {
  return `v1/${origin.value}/@rpc/parallax/stream/report`;
}

export class Reporter {
  private refusedSaid = false;
  private inFlight = false;

  constructor(
    private readonly bus: Bus,
    readonly origin: Origin,
    private readonly log: (line: string) => void,
    private readonly timeoutMs = 2500,
  ) {}

  /** Fire and forget; a report that overlaps the previous one still in flight is dropped (the next cadence carries the same cumulative counters). */
  send(report: MediaReceiverReport): void {
    if (this.inFlight) return;
    this.inFlight = true;
    void this.bus
      .get(streamReportKey(this.origin), { payload: JSON.stringify(report), timeoutMs: this.timeoutMs })
      .then((replies) => this.settle(report, outcomeOf(replies)))
      .catch((e: unknown) => this.settle(report, { ok: false, error: { error: "transport", message: String(e) } }))
      .finally(() => {
        this.inFlight = false;
      });
  }

  private settle(report: MediaReceiverReport, outcome: Outcome): void {
    if (outcome.ok) {
      this.refusedSaid = false;
      return;
    }
    if (this.refusedSaid) return;
    this.refusedSaid = true;
    this.log(`${this.origin.value}: report for ${report.stream}/${report.tier ?? report.codec ?? "?"} refused — ${describe(outcome)} (said once)`);
  }
}
