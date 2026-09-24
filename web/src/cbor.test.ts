import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { CborError, decodeCbor, frameMetaOf } from "./cbor.js";

// The SAME corpus the Rust side pins (zensight-common/tests/framemeta_corpus.rs,
// copied verbatim from parallax-pipeline): three vectors, one per wire rule.
// Reading them from the Rust tree keeps it one corpus.
const corpus = join(import.meta.dirname, "..", "..", "zensight-common", "tests", "fixtures", "framemeta");
const vector = (name: string) => new Uint8Array(readFileSync(join(corpus, name)));

describe("FrameMeta from parallax's canonical CBOR corpus", () => {
  it("the corpus is the three vectors the Rust test names", () => {
    expect(readdirSync(corpus).filter((f) => f.endsWith(".cbor")).sort()).toEqual([
      "full.cbor",
      "minimal-no-timing.cbor",
      "pts-only.cbor",
    ]);
  });

  it("full: every field, dts genuinely distinct from pts, a sequence past 2^32", () => {
    const m = frameMetaOf(vector("full.cbor"))!;
    expect(m.keyframe).toBe(false);
    expect(m.pts_ns).toBe(1_234_567_890n);
    expect(m.dts_ns).toBe(1_234_000_000n);
    expect(m.duration_ns).toBe(16_683_333n);
    expect(m.sequence).toBe(4_294_967_296n);
    expect([m.width, m.height]).toEqual([3840, 2160]);
  });

  it("minimal: absent is ABSENT — the timing keys are not in the map, not null", () => {
    const raw = decodeCbor(vector("minimal-no-timing.cbor")) as Record<string, unknown>;
    expect(Object.keys(raw).sort()).toEqual(["height", "keyframe", "sequence", "width"]);
    const m = frameMetaOf(vector("minimal-no-timing.cbor"))!;
    expect(m.keyframe).toBe(true);
    expect(m.sequence).toBe(1n);
    expect("pts_ns" in m).toBe(false);
    expect([m.width, m.height]).toEqual([16, 16]);
  });

  it("pts-only: dts omitted when it equals pts", () => {
    const m = frameMetaOf(vector("pts-only.cbor"))!;
    expect(m.pts_ns).toBe(1_000_000n);
    expect(m.dts_ns).toBeUndefined();
    expect(m.sequence).toBe(0n);
    expect([m.width, m.height]).toEqual([1920, 1080]);
  });
});

describe("the decoder itself", () => {
  const hex = (s: string) => new Uint8Array(s.match(/../g)!.map((b) => parseInt(b, 16)));

  it("integers of every width, negatives, and bigints past 2^53", () => {
    expect(decodeCbor(hex("17"))).toBe(23);
    expect(decodeCbor(hex("1818"))).toBe(24);
    expect(decodeCbor(hex("1903e8"))).toBe(1000);
    expect(decodeCbor(hex("1a000f4240"))).toBe(1_000_000);
    expect(decodeCbor(hex("1b0000000100000000"))).toBe(4_294_967_296);
    expect(decodeCbor(hex("1bffffffffffffffff"))).toBe(18_446_744_073_709_551_615n);
    expect(decodeCbor(hex("20"))).toBe(-1);
    expect(decodeCbor(hex("3903e7"))).toBe(-1000);
  });

  it("strings, bytes, arrays, bools, null, and the three float widths", () => {
    expect(decodeCbor(hex("6161"))).toBe("a");
    expect(decodeCbor(hex("420102"))).toEqual(new Uint8Array([1, 2]));
    expect(decodeCbor(hex("83010203"))).toEqual([1, 2, 3]);
    expect(decodeCbor(hex("f4"))).toBe(false);
    expect(decodeCbor(hex("f5"))).toBe(true);
    expect(decodeCbor(hex("f6"))).toBe(null);
    expect(decodeCbor(hex("f93e00"))).toBe(1.5);
    expect(decodeCbor(hex("fa47c35000"))).toBe(100000);
    expect(decodeCbor(hex("fb3ff199999999999a"))).toBe(1.1);
  });

  it("refuses truncation, trailing bytes and indefinite lengths rather than guessing", () => {
    expect(() => decodeCbor(hex("1903"))).toThrow(CborError);
    expect(() => decodeCbor(hex("0101"))).toThrow(/trailing/);
    expect(() => decodeCbor(hex("9fff"))).toThrow(CborError);
  });

  it("a FrameMeta must have its four required fields with the right types", () => {
    expect(frameMetaOf(undefined)).toBeUndefined();
    expect(frameMetaOf(hex("a0"))).toBeUndefined();
    // keyframe as an int is not a FrameMeta
    expect(frameMetaOf(hex("a4686b65796672616d65016873657175656e636501657769647468106668656967687410"))).toBeUndefined();
    expect(frameMetaOf(hex("ff"))).toBeUndefined();
  });
});
