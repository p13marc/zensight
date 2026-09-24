import { describe, expect, it } from "vitest";
import { browserIncompatibility, codecString, nalUnits, profileLevelId } from "./h264.js";

describe("the profile-level-id from the first SPS (parallax's own example)", () => {
  it("SPS profile_idc 0x64 (High), constraints 0x0c, level_idc 0x14 (2.0) → avc1.640C14", () => {
    // parallax `codec::annexb::h264_profile_level_id`'s doctest vector, byte for byte.
    const au = new Uint8Array([0, 0, 0, 1, 0x67, 0x64, 0x0c, 0x14, 0xac]);
    expect(profileLevelId(au)).toBe(0x640c14);
    expect(codecString(0x640c14)).toBe("avc1.640C14");
  });

  it("finds the SPS behind other NAL units and a 3-byte start code", () => {
    const au = new Uint8Array([
      0, 0, 1, 0x09, 0xf0, // AUD
      0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1e, 0xd9, // SPS Baseline 3.0
      0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, // PPS
      0, 0, 1, 0x65, 0x88, 0x84, // IDR
    ]);
    expect([...nalUnits(au)].map((n) => n[0]! & 0x1f)).toEqual([9, 7, 8, 5]);
    expect(codecString(profileLevelId(au)!)).toBe("avc1.42C01E");
  });

  it("a keyframe with no SPS yields nothing — the keyframe promise is byte-level", () => {
    expect(profileLevelId(new Uint8Array([0, 0, 0, 1, 0x65, 0x88, 0x84]))).toBeUndefined();
    expect(profileLevelId(new Uint8Array([0, 0, 0, 1, 0x67, 0x64]))).toBeUndefined();
    expect(profileLevelId(new Uint8Array([]))).toBeUndefined();
  });

  it("only the categorically undecodable profiles are refused", () => {
    expect(browserIncompatibility(0x640c14)).toBeUndefined(); // High
    expect(browserIncompatibility(0x42c01e)).toBeUndefined(); // Baseline
    expect(browserIncompatibility(0x4d401f)).toBeUndefined(); // Main
    expect(browserIncompatibility(0x6e0028)).toMatch(/profile_idc 110/); // High 10
    expect(browserIncompatibility(0x7a0028)).toMatch(/profile_idc 122/); // High 4:2:2
    expect(browserIncompatibility(0xf40028)).toMatch(/profile_idc 244/); // High 4:4:4
  });

  it("pads the six hex digits", () => {
    expect(codecString(0x42000a)).toBe("avc1.42000A");
  });
});
