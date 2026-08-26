//! What is left of our Annex-B helpers now that parallax owns them (#730).
//!
//! This module used to be 230 lines: a start-code scanner, `has_idr`,
//! `has_param_sets`, `extract_param_sets`, `prepend_param_sets` and the
//! extract/cache/prepend dance the egress drove by hand. parallax 0.8 ships all
//! of it in [`parallax::codec::annexb`] — compiled **unconditionally** (the
//! `codec` module deliberately depends on no codec library, so a consumer can
//! ask "is this a keyframe" without linking an encoder) and, unlike ours,
//! codec-aware: [`is_entry_point`] and [`has_param_sets`] take a
//! [`NalCodec`] and answer correctly for H.265 too, where the NAL header is two
//! bytes and our `& 0x1F` returned nonsense.
//!
//! The whole extract-cache-prepend loop is now
//! [`ParamSetCache::prepare`], which also does better than what it replaced: it
//! returns `Cow::Borrowed` for every delta frame and every keyframe that already
//! carries its sets, so only an actually-repaired keyframe copies.
//!
//! [`is_entry_point`]: parallax::codec::annexb::is_entry_point
//! [`has_param_sets`]: parallax::codec::annexb::has_param_sets
//! [`NalCodec`]: parallax::codec::annexb::NalCodec
//! [`ParamSetCache::prepare`]: parallax::codec::annexb::ParamSetCache::prepare
//!
//! One helper has no upstream equivalent and stays here, reimplemented over
//! upstream's scanner rather than over a private copy of it.

use parallax::codec::annexb::nal_units;

/// The `profile-level-id` of the first SPS in an H.264 access unit, packed as
/// `profile_idc << 16 | constraint_flags << 8 | level_idc` — the six hex digits
/// a WebCodecs client needs for `avc1.<6 hex>` (#707).
///
/// Re-exported rather than surfaced on the `@rpc/parallax/streams` catalogue,
/// which is the obvious-looking home and the wrong one: `Catalog` is built from
/// **config** at startup and answers for closed streams too, while this value
/// only exists once a keyframe has been encoded. A `StreamDescriptor` field
/// would therefore be `None` in exactly the case a viewer consults the
/// catalogue for — before opening anything. Upstream says the same from its
/// side: nothing on the `@media` wire transports it and `FrameMeta`
/// deliberately does not, because a consumer derives it from the first keyframe
/// — which the parameter-set promise the egress keeps (see `ParamSetCache`
/// above) is what makes possible. #707 decides where, if anywhere, it is
/// published; this is the primitive it needs.
pub use parallax::codec::annexb::h264_profile_level_id;

/// NAL unit type: coded slice of a non-IDR picture.
const NAL_SLICE: u8 = 1;
/// NAL unit type: coded slice of an IDR picture.
const NAL_IDR: u8 = 5;

/// How many *coded slice* NALs (IDR or non-IDR) an access unit carries —
/// parameter sets and SEI do not count. One per frame unless the encoder was
/// given a slice-size cap.
///
/// No upstream equivalent: parallax asks about NAL *kinds* ("is this an entry
/// point", "does it carry its sets"), never about how many slices a picture was
/// cut into. We need the count to check that `encoder.max_slice_len` (#509)
/// actually reached OpenH264 — with a cap set, one large keyframe comes back as
/// several coded slices instead of one — and the knob has no other observable
/// effect from outside the encoder.
pub fn coded_slice_count(data: &[u8]) -> usize {
    nal_units(data)
        .filter(|n| matches!(n.nal_type(), NAL_SLICE | NAL_IDR))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake NAL unit: start code + header byte (type) + payload.
    fn nal(four_byte: bool, ty: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = if four_byte {
            vec![0, 0, 0, 1]
        } else {
            vec![0, 0, 1]
        };
        v.push(ty & 0x1F);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn counts_coded_slices_only() {
        // Mixed 3- and 4-byte start codes; parameter sets must not count.
        let mut au = nal(true, 7, &[0xAA, 0xAA]); // SPS
        au.extend(nal(false, 8, &[0xBB])); // PPS
        au.extend(nal(true, 5, &[0xCC; 10])); // IDR slice
        assert_eq!(coded_slice_count(&au), 1);

        // A sliced keyframe: several coded slices in one access unit.
        let mut sliced = nal(true, 7, &[0xAA]);
        sliced.extend(nal(true, 5, &[0xCC; 4]));
        sliced.extend(nal(true, 5, &[0xCC; 4]));
        sliced.extend(nal(true, 1, &[0xDD; 4]));
        assert_eq!(coded_slice_count(&sliced), 3);
    }

    #[test]
    fn tolerates_garbage_and_truncation() {
        assert_eq!(coded_slice_count(b""), 0);
        assert_eq!(coded_slice_count(b"no start codes here"), 0);
        // A start code at the very end (no header byte) must not panic.
        assert_eq!(coded_slice_count(&[0, 0, 0, 1]), 0);
        // JPEG bytes (not Annex-B at all).
        assert_eq!(coded_slice_count(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 1]), 0);
    }
}
