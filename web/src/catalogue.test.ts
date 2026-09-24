import { describe, expect, it } from "vitest";
import { fetchCatalogue, nativeLabel, tierChoices, watchStatus } from "./catalogue.js";
import { FakeBus, err, ok } from "./fake.js";
import { Origin } from "./keys.js";
import type { StreamStatus } from "./types.gen.js";

const origin = Origin.of("h-3fa9c2d41b7e");

const descriptor = {
  stream: "test0",
  codecs: ["h264", "mjpeg"],
  active: false,
  width: 640,
  height: 360,
  fps: 15,
  tiers: [
    { name: "low", max_height: 240, fps: 10, bitrate_kbps: 400 },
    { name: "medium", max_height: 480, fps: 20, bitrate_kbps: 1200 },
    { name: "high", fps: 30, bitrate_kbps: 4000 },
  ],
};

describe("the catalogue", () => {
  it("is one GET on the origin's streams key, decoded from JSON", async () => {
    const bus = new FakeBus();
    bus.answer = () => [ok([descriptor])];
    const cat = await fetchCatalogue(bus, origin);
    expect(bus.gets[0]?.selector).toBe("v1/h-3fa9c2d41b7e/@rpc/parallax/streams");
    expect(bus.gets[0]?.opts.payload).toBeUndefined();
    expect(cat).toEqual([descriptor]);
  });

  it("is undefined when nothing answers, never an empty list", async () => {
    const bus = new FakeBus();
    bus.answer = () => [];
    expect(await fetchCatalogue(bus, origin)).toBeUndefined();
    bus.answer = () => [err({ error: "x", message: "y" })];
    expect(await fetchCatalogue(bus, origin)).toBeUndefined();
  });

  it("builds the tier selector from the descriptor, before anything is opened", () => {
    expect(tierChoices(descriptor)).toEqual(["low", "medium", "high"]);
    expect(nativeLabel(descriptor)).toBe("640×360 @ 15");
    expect(nativeLabel({ stream: "x", codecs: [], active: false })).toBe("unknown");
  });
});

describe("status", () => {
  const status = (stream: string, open: boolean): StreamStatus => ({
    stream,
    open,
    tiers: open ? [{ tier: "low", applied: { width: 426, height: 240, fps: 10, bitrate_kbps: 400 }, viewers: 1 }] : [],
  });

  it("subscribes to the host's stream/* selector and keeps one document per stream", async () => {
    const bus = new FakeBus();
    const seen: ReadonlyMap<string, StreamStatus>[] = [];
    const undeclare = await watchStatus(bus, origin, (m) => seen.push(m));
    const sel = "v1/h-3fa9c2d41b7e/state/parallax/stream/*";
    expect(bus.subscriptions.has(sel)).toBe(true);
    bus.publish(sel, "v1/h-3fa9c2d41b7e/state/parallax/stream/test0", status("test0", true));
    bus.publish(sel, "v1/h-3fa9c2d41b7e/state/parallax/stream/cam1", status("cam1", false));
    expect(seen).toHaveLength(2);
    expect([...seen[1]!.keys()]).toEqual(["test0", "cam1"]);
    expect(seen[1]!.get("test0")?.tiers?.[0]?.viewers).toBe(1);
    // A DELETE drops the stream; the maps are distinct objects.
    bus.publish(sel, "v1/h-3fa9c2d41b7e/state/parallax/stream/cam1", {}, false);
    expect([...seen[2]!.keys()]).toEqual(["test0"]);
    expect(seen[2]).not.toBe(seen[1]);
    await undeclare();
    expect(bus.undeclared).toEqual([sel]);
  });

  it("the key names the stream, not the document, and an unreadable document is dropped", async () => {
    const bus = new FakeBus();
    const seen: ReadonlyMap<string, StreamStatus>[] = [];
    await watchStatus(bus, origin, (m) => seen.push(m));
    const sel = "v1/h-3fa9c2d41b7e/state/parallax/stream/*";
    bus.publish(sel, "v1/h-3fa9c2d41b7e/state/parallax/stream/test0", status("WRONG", true));
    expect(seen[0]!.get("test0")?.stream).toBe("test0");
    bus.subscriptions.get(sel)!({
      key: "v1/h-3fa9c2d41b7e/state/parallax/stream/test0",
      payload: new Uint8Array([0xa1, 0x00]), // CBOR, not JSON
      alive: true,
    });
    expect(seen).toHaveLength(1);
  });
});
