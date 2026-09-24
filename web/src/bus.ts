// The narrow slice of a zenoh session this client uses, behind an interface
// so the client modules are testable against a fake (`FakeBus` in the tests)
// and so #723's second session for `@media` can be a second `Bus`.
import {
  Config,
  Encoding,
  Reply,
  ReplyError,
  Sample,
  SampleKind,
  Session,
  ZBytes,
} from "@eclipse-zenoh/zenoh-ts";
import { Duration } from "@eclipse-zenoh/zenoh-ts";

/** One reply to a GET: an answer (with its key) or the callee's error payload. */
export type BusReply =
  | { kind: "ok"; key: string; payload: Uint8Array }
  | { kind: "err"; payload: Uint8Array };

/** One sample on a subscription. `alive` is false for a DELETE (a liveliness token dropped). */
export interface BusSample {
  key: string;
  payload: Uint8Array;
  alive: boolean;
  /** The sample's attachment — on `@media`, the CBOR `FrameMeta` sidecar (#707). */
  attachment?: Uint8Array;
  /** The publisher's HLC stamp as ms since the epoch — the frame-age clock (RFC 07 §1.3). Absent when unstamped. */
  publishedMs?: number;
}

/** Undeclares the subscription. The falling edge is the sensor's teardown signal on `@media`, so it is never optional. */
export type Undeclare = () => Promise<void>;

export interface GetOptions {
  /** JSON body for a write procedure; sent as the query payload with `application/json`. */
  payload?: string;
  timeoutMs?: number;
}

export interface Bus {
  /** One GET, every reply collected until the callee(s) finish or the timeout passes. */
  get(selector: string, opts?: GetOptions): Promise<BusReply[]>;
  /** A plain subscriber on `keyexpr`. */
  subscribe(keyexpr: string, onSample: (s: BusSample) => void): Promise<Undeclare>;
  /** The liveliness tokens currently matching `keyexpr` (their keys). */
  livelinessGet(keyexpr: string, timeoutMs?: number): Promise<string[]>;
  /** A liveliness subscriber: `alive` true on a token appearing, false on it dropping. Replays the current set first. */
  livelinessSubscribe(keyexpr: string, onToken: (s: BusSample) => void): Promise<Undeclare>;
  close(): Promise<void>;
}

const DEFAULT_TIMEOUT_MS = 5000;

/** Connect to the remote-api bridge (#705) at `ws://host:10000` or `wss://…`. */
export async function connect(locator: string): Promise<Bus> {
  const session = await Session.open(new Config(locator));
  return zenohBus(session);
}

/** Wrap an open zenoh-ts session. */
export function zenohBus(session: Session): Bus {
  return {
    async get(selector, opts = {}) {
      const timeoutMs = opts.timeoutMs ?? DEFAULT_TIMEOUT_MS;
      const getOpts: Parameters<Session["get"]>[1] = {
        timeout: Duration.milliseconds.of(timeoutMs),
      };
      if (opts.payload !== undefined) {
        getOpts.payload = new ZBytes(opts.payload);
        getOpts.encoding = Encoding.APPLICATION_JSON;
      }
      const receiver = await session.get(selector, getOpts);
      const out: BusReply[] = [];
      if (!receiver) return out;
      for await (const reply of receiver) {
        out.push(intoBusReply(reply));
      }
      return out;
    },

    async subscribe(keyexpr, onSample) {
      const sub = await session.declareSubscriber(keyexpr, {
        handler: (s: Sample) => onSample(intoBusSample(s)),
      });
      return () => sub.undeclare();
    },

    async livelinessGet(keyexpr, timeoutMs = DEFAULT_TIMEOUT_MS) {
      const receiver = await session
        .liveliness()
        .get(keyexpr, { timeout: Duration.milliseconds.of(timeoutMs) });
      const keys: string[] = [];
      if (!receiver) return keys;
      for await (const reply of receiver) {
        const r = reply.result();
        if (r instanceof Sample) keys.push(r.keyexpr().toString());
      }
      return keys;
    },

    async livelinessSubscribe(keyexpr, onToken) {
      const sub = await session.liveliness().declareSubscriber(keyexpr, {
        history: true,
        handler: (s: Sample) => onToken(intoBusSample(s)),
      });
      return () => sub.undeclare();
    },

    close: () => session.close(),
  };
}

function intoBusReply(reply: Reply): BusReply {
  const r = reply.result();
  if (r instanceof ReplyError) {
    return { kind: "err", payload: r.payload().toBytes() };
  }
  return { kind: "ok", key: r.keyexpr().toString(), payload: r.payload().toBytes() };
}

function intoBusSample(s: Sample): BusSample {
  const out: BusSample = {
    key: s.keyexpr().toString(),
    payload: s.payload().toBytes(),
    alive: s.kind() !== SampleKind.DELETE,
  };
  const attachment = s.attachment();
  if (attachment) out.attachment = attachment.toBytes();
  const ts = s.timestamp();
  if (ts) out.publishedMs = ts.getMsSinceUnixEpoch();
  return out;
}

export { json } from "./json.js";
