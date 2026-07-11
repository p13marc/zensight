//! Minimal H.264 Annex-B helpers for the media egress.
//!
//! The egress treats every published keyframe access unit as a promise: "a
//! fresh decoder can start here" (keyframe-on-subscribe, KEYSPACE §3.3).
//! These helpers let it *keep* that promise at the byte level — derive the
//! keyframe flag from the actual bitstream instead of trusting upstream
//! metadata, and prepend cached SPS/PPS to a keyframe that arrived without
//! its parameter sets (e.g. an RTSP camera that only announced them
//! out-of-band in the SDP).
//!
//! Scope is deliberately tiny: start-code scanning and NAL *types* only —
//! no bitstream parsing beyond the one header byte per NAL unit.

/// NAL unit type: coded slice of an IDR picture.
const NAL_IDR: u8 = 5;
/// NAL unit type: sequence parameter set.
const NAL_SPS: u8 = 7;
/// NAL unit type: picture parameter set.
const NAL_PPS: u8 = 8;

/// Iterate the NAL units of an Annex-B stream as `(start, payload_start)`
/// byte offsets, where `start` points at the start code (`00 00 01` or
/// `00 00 00 01`) and `payload_start` at the NAL header byte.
fn nal_offsets(data: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i + 3 <= data.len() {
            if data[i..].starts_with(&[0, 0, 0, 1]) {
                let at = (i, i + 4);
                i += 4;
                if at.1 < data.len() {
                    return Some(at);
                }
            } else if data[i..].starts_with(&[0, 0, 1]) {
                let at = (i, i + 3);
                i += 3;
                if at.1 < data.len() {
                    return Some(at);
                }
            } else {
                i += 1;
            }
        }
        None
    })
}

/// NAL unit types (header & 0x1F) present in an Annex-B access unit.
fn nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    nal_offsets(data).map(|(_, p)| data[p] & 0x1F)
}

/// Does the access unit contain an IDR slice (i.e. is it a keyframe)?
pub fn has_idr(data: &[u8]) -> bool {
    nal_types(data).any(|t| t == NAL_IDR)
}

/// Does the access unit carry SPS *and* PPS parameter sets?
pub fn has_param_sets(data: &[u8]) -> bool {
    let mut sps = false;
    let mut pps = false;
    for t in nal_types(data) {
        sps |= t == NAL_SPS;
        pps |= t == NAL_PPS;
        if sps && pps {
            return true;
        }
    }
    false
}

/// Extract the SPS/PPS NAL units (with their start codes) from an access
/// unit, concatenated in stream order — ready to prepend to a later
/// parameter-set-less keyframe. Returns `None` unless both an SPS and a PPS
/// are present.
pub fn extract_param_sets(data: &[u8]) -> Option<Vec<u8>> {
    if !has_param_sets(data) {
        return None;
    }
    let offsets: Vec<(usize, usize)> = nal_offsets(data).collect();
    let mut out = Vec::new();
    for (idx, &(start, payload)) in offsets.iter().enumerate() {
        let t = data[payload] & 0x1F;
        if t == NAL_SPS || t == NAL_PPS {
            let end = offsets.get(idx + 1).map_or(data.len(), |&(next, _)| next);
            out.extend_from_slice(&data[start..end]);
        }
    }
    Some(out)
}

/// Prepend `param_sets` (as produced by [`extract_param_sets`]) to a keyframe
/// access unit that lacks its own.
pub fn prepend_param_sets(param_sets: &[u8], au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(param_sets.len() + au.len());
    out.extend_from_slice(param_sets);
    out.extend_from_slice(au);
    out
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
    fn detects_idr_and_param_sets() {
        let mut au = nal(true, 7, &[0xAA]);
        au.extend(nal(true, 8, &[0xBB]));
        au.extend(nal(true, 5, &[0xCC, 0xDD]));
        assert!(has_idr(&au));
        assert!(has_param_sets(&au));

        let delta = nal(true, 1, &[0xEE]);
        assert!(!has_idr(&delta));
        assert!(!has_param_sets(&delta));

        // IDR without params — the case the egress repairs.
        let bare_idr = nal(true, 5, &[0xCC]);
        assert!(has_idr(&bare_idr));
        assert!(!has_param_sets(&bare_idr));

        // SPS alone is not enough.
        let sps_only = nal(true, 7, &[0xAA]);
        assert!(!has_param_sets(&sps_only));
    }

    #[test]
    fn extracts_and_prepends_param_sets() {
        // Mixed 3- and 4-byte start codes, params sandwiched around an IDR.
        let sps = nal(true, 7, &[0x11, 0x22]);
        let pps = nal(false, 8, &[0x33]);
        let idr = nal(true, 5, &[0x44; 8]);
        let mut au = sps.clone();
        au.extend(&pps);
        au.extend(&idr);

        let params = extract_param_sets(&au).expect("SPS+PPS present");
        let mut expected = sps;
        expected.extend(&pps);
        assert_eq!(params, expected, "params kept verbatim with start codes");

        let bare_idr = nal(true, 5, &[0x55; 4]);
        assert!(extract_param_sets(&bare_idr).is_none());

        let repaired = prepend_param_sets(&params, &bare_idr);
        assert!(has_param_sets(&repaired));
        assert!(has_idr(&repaired));
        assert!(repaired.ends_with(&bare_idr));
    }

    #[test]
    fn tolerates_garbage_and_truncation() {
        assert!(!has_idr(&[]));
        assert!(!has_param_sets(&[0, 0]));
        // A start code at the very end (no header byte) must not panic.
        assert!(!has_idr(&[0, 0, 0, 1]));
        assert!(extract_param_sets(&[0, 0, 1]).is_none());
        // JPEG bytes (not Annex-B at all).
        assert!(!has_idr(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 1]));
    }
}
