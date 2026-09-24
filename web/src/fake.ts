// A scripted `Bus` for the unit tests: records every GET and lets a test
// answer it, and lets a test push samples and liveliness edges.
import type { Bus, BusReply, BusSample, GetOptions, Undeclare } from "./bus.js";

const utf8 = new TextEncoder();

export interface RecordedGet {
  selector: string;
  opts: GetOptions;
  /** The parsed JSON payload, if any. */
  body: unknown;
}

export class FakeBus implements Bus {
  readonly gets: RecordedGet[] = [];
  readonly subscriptions = new Map<string, (s: BusSample) => void>();
  readonly livelinessSubs = new Map<string, (s: BusSample) => void>();
  readonly undeclared: string[] = [];
  alive: string[] = [];
  /** Decide the replies to a GET; default: one empty OK (a write executed). */
  answer: (get: RecordedGet) => BusReply[] | Promise<BusReply[]> = () => [ok("")];
  /** Delay each GET's completion by this many ms; for ordering tests. */
  latencyMs = 0;
  closed = false;

  async get(selector: string, opts: GetOptions = {}): Promise<BusReply[]> {
    const rec: RecordedGet = {
      selector,
      opts,
      body: opts.payload === undefined ? undefined : JSON.parse(opts.payload),
    };
    this.gets.push(rec);
    if (this.latencyMs > 0) await new Promise((r) => setTimeout(r, this.latencyMs));
    return this.answer(rec);
  }

  async subscribe(keyexpr: string, onSample: (s: BusSample) => void): Promise<Undeclare> {
    this.subscriptions.set(keyexpr, onSample);
    return async () => {
      this.subscriptions.delete(keyexpr);
      this.undeclared.push(keyexpr);
    };
  }

  async livelinessGet(): Promise<string[]> {
    return [...this.alive];
  }

  async livelinessSubscribe(keyexpr: string, onToken: (s: BusSample) => void): Promise<Undeclare> {
    this.livelinessSubs.set(keyexpr, onToken);
    for (const key of this.alive) onToken({ key, payload: new Uint8Array(), alive: true });
    return async () => {
      this.livelinessSubs.delete(keyexpr);
      this.undeclared.push(keyexpr);
    };
  }

  async close(): Promise<void> {
    this.closed = true;
  }

  /** Push one sample to every subscriber whose key expression is exactly `keyexpr` (tests spell the selector they subscribed with). */
  publish(subscribedAs: string, key: string, value: unknown, alive = true): void {
    const cb = this.subscriptions.get(subscribedAs);
    if (!cb) throw new Error(`nothing subscribed as ${subscribedAs}`);
    cb({ key, payload: utf8.encode(JSON.stringify(value)), alive });
  }

  /** A liveliness edge for `key` on every liveliness subscriber. */
  token(key: string, alive: boolean): void {
    for (const cb of this.livelinessSubs.values()) cb({ key, payload: new Uint8Array(), alive });
  }
}

export function ok(payload: string | object, key = "reply"): BusReply {
  const text = typeof payload === "string" ? payload : JSON.stringify(payload);
  return { kind: "ok", key, payload: utf8.encode(text) };
}

export function err(payload: object): BusReply {
  return { kind: "err", payload: utf8.encode(JSON.stringify(payload)) };
}
