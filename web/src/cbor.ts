// A minimal CBOR decoder (RFC 8949) for the one thing on the `@media` plane
// that is strictly CBOR: the `FrameMeta` attachment on every sample. Maps
// with text keys, unsigned/negative integers, byte and text strings, arrays,
// bools, null and the three float widths — what serde/ciborium emits for a
// small struct, and nothing a decoder needs a dependency for.
//
// Integers come back as `number` when they fit in 2^53 and as `bigint`
// otherwise; `sequence`, `pts_ns` and friends are u64 on the wire and a
// pipeline clock in nanoseconds passes 2^53 after 104 days, so
// `frameMetaOf` keeps every u64 as a `bigint`.

export type CborValue =
  | number
  | bigint
  | string
  | Uint8Array
  | boolean
  | null
  | undefined
  | CborValue[]
  | { [key: string]: CborValue };

export class CborError extends Error {}

/** Decode one CBOR item; trailing bytes are an error (an attachment is one item). */
export function decodeCbor(bytes: Uint8Array): CborValue {
  const r = new Reader(bytes);
  const v = r.item();
  if (r.pos !== bytes.length) throw new CborError(`${bytes.length - r.pos} trailing byte(s)`);
  return v;
}

class Reader {
  pos = 0;
  private readonly view: DataView;
  constructor(private readonly buf: Uint8Array) {
    this.view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  }

  private need(n: number): void {
    if (this.pos + n > this.buf.length) throw new CborError("truncated");
  }

  private u8(): number {
    this.need(1);
    return this.buf[this.pos++]!;
  }

  /** The argument of a head byte: `ai` < 24 is the value itself; 24–27 follow in 1/2/4/8 bytes. */
  private arg(ai: number): number | bigint {
    if (ai < 24) return ai;
    if (ai === 24) return this.u8();
    if (ai === 25) {
      this.need(2);
      const v = this.view.getUint16(this.pos);
      this.pos += 2;
      return v;
    }
    if (ai === 26) {
      this.need(4);
      const v = this.view.getUint32(this.pos);
      this.pos += 4;
      return v;
    }
    if (ai === 27) {
      this.need(8);
      const v = this.view.getBigUint64(this.pos);
      this.pos += 8;
      return v <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(v) : v;
    }
    throw new CborError(`indefinite length or reserved additional info ${ai}`);
  }

  private length(ai: number): number {
    const n = this.arg(ai);
    if (typeof n === "bigint") throw new CborError("length beyond 2^53");
    return n;
  }

  item(): CborValue {
    const head = this.u8();
    const major = head >> 5;
    const ai = head & 0x1f;
    switch (major) {
      case 0:
        return this.arg(ai);
      case 1: {
        const n = this.arg(ai);
        return typeof n === "bigint" ? -1n - n : -1 - n;
      }
      case 2: {
        const n = this.length(ai);
        this.need(n);
        const out = this.buf.slice(this.pos, this.pos + n);
        this.pos += n;
        return out;
      }
      case 3: {
        const n = this.length(ai);
        this.need(n);
        const out = utf8.decode(this.buf.subarray(this.pos, this.pos + n));
        this.pos += n;
        return out;
      }
      case 4: {
        const n = this.length(ai);
        const out: CborValue[] = [];
        for (let i = 0; i < n; i++) out.push(this.item());
        return out;
      }
      case 5: {
        const n = this.length(ai);
        const out: { [key: string]: CborValue } = {};
        for (let i = 0; i < n; i++) {
          const k = this.item();
          if (typeof k !== "string") throw new CborError("non-text map key");
          out[k] = this.item();
        }
        return out;
      }
      case 6: {
        // A tag: skip it and return the tagged item (none are expected).
        this.arg(ai);
        return this.item();
      }
      case 7:
        switch (ai) {
          case 20:
            return false;
          case 21:
            return true;
          case 22:
            return null;
          case 23:
            return undefined;
          case 25: {
            this.need(2);
            const v = half(this.view.getUint16(this.pos));
            this.pos += 2;
            return v;
          }
          case 26: {
            this.need(4);
            const v = this.view.getFloat32(this.pos);
            this.pos += 4;
            return v;
          }
          case 27: {
            this.need(8);
            const v = this.view.getFloat64(this.pos);
            this.pos += 8;
            return v;
          }
          default:
            throw new CborError(`unsupported simple value ${ai}`);
        }
      default:
        throw new CborError(`major type ${major}`);
    }
  }
}

const utf8 = new TextDecoder("utf-8", { fatal: true });

/** IEEE 754 binary16 → number (RFC 8949 appendix D). */
function half(h: number): number {
  const exp = (h >> 10) & 0x1f;
  const mant = h & 0x3ff;
  let v: number;
  if (exp === 0) v = mant * 2 ** -24;
  else if (exp !== 31) v = (mant + 1024) * 2 ** (exp - 25);
  else v = mant === 0 ? Infinity : NaN;
  return h & 0x8000 ? -v : v;
}

/**
 * `zensight_common::stream::FrameMeta`, the per-frame sidecar (RFC 07 §1).
 * Absent timing fields are ABSENT (not null): an unstamped encoder omits
 * them, and `dts_ns` is omitted when it equals `pts_ns`.
 */
export interface FrameMeta {
  keyframe: boolean;
  sequence: bigint;
  width: number;
  height: number;
  pts_ns?: bigint;
  dts_ns?: bigint;
  duration_ns?: bigint;
}

/** Decode a FrameMeta attachment, or `undefined` for anything that is not one. */
export function frameMetaOf(bytes: Uint8Array | undefined): FrameMeta | undefined {
  if (!bytes) return undefined;
  let v: CborValue;
  try {
    v = decodeCbor(bytes);
  } catch {
    return undefined;
  }
  if (v === null || typeof v !== "object" || Array.isArray(v) || v instanceof Uint8Array) return undefined;
  const m = v as { [key: string]: CborValue };
  if (typeof m["keyframe"] !== "boolean") return undefined;
  const sequence = u64(m["sequence"]);
  const width = u32(m["width"]);
  const height = u32(m["height"]);
  if (sequence === undefined || width === undefined || height === undefined) return undefined;
  const out: FrameMeta = { keyframe: m["keyframe"], sequence, width, height };
  for (const k of ["pts_ns", "dts_ns", "duration_ns"] as const) {
    if (k in m) {
      const n = u64(m[k]);
      if (n === undefined) return undefined;
      out[k] = n;
    }
  }
  return out;
}

function u64(v: CborValue): bigint | undefined {
  if (typeof v === "bigint") return v >= 0n ? v : undefined;
  if (typeof v === "number" && Number.isInteger(v) && v >= 0) return BigInt(v);
  return undefined;
}

function u32(v: CborValue): number | undefined {
  return typeof v === "number" && Number.isInteger(v) && v >= 0 && v <= 0xffff_ffff ? v : undefined;
}
