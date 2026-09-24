// What a browser needs to know about an Annex-B H.264 access unit, and
// nothing more (#707).
//
// WebCodecs takes Annex-B natively — configure `VideoDecoder` WITHOUT a
// `description` and the bitstream is assumed Annex-B rather than avcC — but
// it needs the codec string, `avc1.<profile-level-id as 6 hex>`, and nothing
// on the wire carries that: `FrameMeta` deliberately does not, because RFC 07
// keeps parameter sets in the bitstream. So the consumer parses the three
// bytes after the SPS NAL header of the first keyframe — `profile_idc`, the
// constraint-set flags, `level_idc` ARE the profile-level-id — which is only
// possible because the sensor's keyframe promise is byte-level: `keyframe:
// true` means an IDR is present and the parameter sets are there, prepended
// from cache if the camera announced them only in its SDP. This is the
// browser's copy of parallax's `codec::annexb::h264_profile_level_id`.

const NAL_SPS = 7;

/** Iterate the NAL units of an Annex-B byte stream (3- or 4-byte start codes), yielding each unit's bytes from its header. */
export function* nalUnits(data: Uint8Array): Generator<Uint8Array> {
  let i = 0;
  let start = -1;
  const n = data.length;
  while (i + 2 < n) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      // A 4-byte start code is 00 00 00 01; the unit before it ends one byte earlier.
      const end = i > 0 && data[i - 1] === 0 ? i - 1 : i;
      if (start >= 0 && end > start) yield data.subarray(start, end);
      i += 3;
      start = i;
    } else {
      i++;
    }
  }
  if (start >= 0 && start < n) yield data.subarray(start, n);
}

/** `profile_idc << 16 | constraint_flags << 8 | level_idc` of the first SPS, or `undefined` if the access unit carries none. */
export function profileLevelId(accessUnit: Uint8Array): number | undefined {
  for (const nal of nalUnits(accessUnit)) {
    if ((nal[0]! & 0x1f) !== NAL_SPS) continue;
    if (nal.length < 4) return undefined;
    return (nal[1]! << 16) | (nal[2]! << 8) | nal[3]!;
  }
  return undefined;
}

/** The WebCodecs codec string for a profile-level-id: `avc1.640C14`. */
export function codecString(profileLevelId: number): string {
  return `avc1.${profileLevelId.toString(16).toUpperCase().padStart(6, "0")}`;
}

/**
 * Why a profile is unusable in a browser, or `undefined` when it is safe.
 * Deliberately narrow, like parallax's `h264_browser_compatible`: only the
 * profiles that are categorically undecodable — High 10 (110), High 4:2:2
 * (122), High 4:4:4 predictive (244) — not the merely uncommon.
 */
export function browserIncompatibility(profileLevelId: number): string | undefined {
  const profileIdc = (profileLevelId >> 16) & 0xff;
  switch (profileIdc) {
    case 110:
    case 122:
    case 244:
      return `H.264 profile_idc ${profileIdc} (high-bit-depth or non-4:2:0) is not decodable in any browser; Baseline (66), Main (77) and High (100) are`;
    default:
      return undefined;
  }
}
