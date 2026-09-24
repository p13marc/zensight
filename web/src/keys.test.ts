import { describe, expect, it } from "vitest";
import {
  Origin,
  aliveSelector,
  mediaPreviewKey,
  mediaVideoKey,
  originOfAliveKey,
  streamOfStatusKey,
  streamSetKey,
  streamStatusKey,
  streamsKey,
} from "./keys.js";

// The origin the iced GUI's own key tests pin (parallax_detail.rs).
const origin = Origin.of("h-3fa9c2d41b7e");

describe("the key spellings match the sensor's, chunk for chunk", () => {
  it("catalogue, control and status are origin-scoped and base-less", () => {
    expect(streamsKey(origin)).toBe("v1/h-3fa9c2d41b7e/@rpc/parallax/streams");
    expect(streamSetKey(origin)).toBe("v1/h-3fa9c2d41b7e/@rpc/parallax/stream/set");
    expect(streamStatusKey(origin, "cam0")).toBe("v1/h-3fa9c2d41b7e/state/parallax/stream/cam0");
    expect(streamStatusKey(origin)).toBe("v1/h-3fa9c2d41b7e/state/parallax/stream/*");
  });

  it("media keys are EXACT tier keys, the literal the sensor publishes on (RFC 07 §1)", () => {
    expect(mediaVideoKey(origin, "cam0", "h264", "high")).toBe(
      "v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/high",
    );
    expect(mediaPreviewKey(origin, "cam0")).toBe(
      "v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg",
    );
  });

  it("the only fleet-wide selector is the liveliness token", () => {
    expect(aliveSelector()).toBe("v1/*/state/parallax/alive");
    expect(originOfAliveKey("v1/h-3fa9c2d41b7e/state/parallax/alive")?.value).toBe(
      "h-3fa9c2d41b7e",
    );
    // A device token (`…/device/<d>/alive`) and another producer's token are not origins here.
    expect(originOfAliveKey("v1/h-3fa9c2d41b7e/state/parallax/device/cam0/alive")).toBeUndefined();
    expect(originOfAliveKey("v1/h-3fa9c2d41b7e/state/sysinfo/alive")).toBeUndefined();
  });

  it("a status key yields its stream, and only a direct child", () => {
    expect(streamOfStatusKey("v1/h-3fa9c2d41b7e/state/parallax/stream/cam0", origin)).toBe("cam0");
    expect(streamOfStatusKey("v1/h-3fa9c2d41b7e/state/parallax/stream/cam0/x", origin)).toBeUndefined();
    expect(streamOfStatusKey("v1/h-000000000000/state/parallax/stream/cam0", origin)).toBeUndefined();
  });
});

describe("a `*` origin is unrepresentable (RFC 07 §3)", () => {
  it("refuses the wildcard, a service origin and the wrong hex length", () => {
    for (const bad of ["*", "@catalog", "h-3fa9c2d41b7", "h-3FA9C2D41B7E", "", "h-3fa9c2d41b7e/x"]) {
      expect(Origin.parse(bad), bad).toBeUndefined();
      expect(() => Origin.of(bad), bad).toThrow(/not a host origin/);
    }
  });

  it("accepts exactly h- plus twelve lowercase hex, trimmed", () => {
    expect(Origin.parse("  h-3fa9c2d41b7e\n")?.value).toBe("h-3fa9c2d41b7e");
  });
});
