//! Minimal read-only BTF parser: enough to find a kernel struct by name and
//! report its members' byte offsets.
//!
//! # Why this exists
//!
//! The eBPF programs read tracepoint and `struct` fields at hardcoded byte
//! offsets, which are kernel-version dependent. Getting one wrong raises no
//! error — it reads the wrong field, and the histograms fill with plausible
//! nonsense. BTF member offsets are emitted by the same `offsetof` the
//! tracepoint's `format` file is generated from, so they are the authority for
//! those constants, and `/sys/kernel/btf/vmlinux` is world-readable where
//! `/sys/kernel/tracing` is mode `0700` — so this validates unprivileged, in a
//! container, and in CI.
//!
//! CO-RE would be the usual answer and is not available to us: its field
//! relocations come from clang's `__builtin_preserve_access_index`, which
//! rustc/bpf-linker do not emit. The substitute is to resolve offsets here and
//! inject them at load time with `aya::EbpfLoader::set_global` (#681).
//!
//! # Why a separate crate
//!
//! Two consumers with different needs. The `-ebpf-common` crates want it as a
//! **dev-dependency**, to check their hardcoded constants against the running
//! kernel in a host test; cargo never builds a dependency's dev-dependencies,
//! so nothing here can reach the `bpfel-unknown-none` object. The userspace
//! sensors want it as a **regular dependency**, to resolve offsets at load
//! time. One crate serves both because the *kind* of dependency is a property
//! of each consumer, not of this crate. It is dependency-free so that being
//! pulled into either build costs nothing.
//!
//! # Robustness
//!
//! This parses an untrusted-length file inside a running sensor, so it never
//! panics and never indexes unchecked: a malformed, truncated, or simply
//! unexpected blob yields `None`. That is the one substantive difference from
//! the test-only reader this replaces, which asserted its magic and sliced
//! freely — acceptable in a test, not in a loader.
//!
//! Format reference: `Documentation/bpf/btf.rst`.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

/// Where the kernel exposes its own BTF. World-readable when
/// `CONFIG_DEBUG_INFO_BTF=y`.
pub const VMLINUX_PATH: &str = "/sys/kernel/btf/vmlinux";

const BTF_MAGIC: u16 = 0xEB9F;
const KIND_INT: u32 = 1;
const KIND_ARRAY: u32 = 3;
const KIND_STRUCT: u32 = 4;
const KIND_UNION: u32 = 5;
const KIND_ENUM: u32 = 6;
const KIND_FUNC_PROTO: u32 = 13;
const KIND_VAR: u32 = 14;
const KIND_DATASEC: u32 = 15;
const KIND_DECL_TAG: u32 = 17;
const KIND_ENUM64: u32 = 19;

/// Read the running kernel's BTF blob.
///
/// `None` when the file is absent or unreadable — a kernel built without
/// `CONFIG_DEBUG_INFO_BTF`, or a non-Linux host. Callers treat that as "cannot
/// validate here", never as an error: it is the normal state on some hosts.
#[cfg(feature = "std")]
pub fn read_vmlinux() -> Option<std::vec::Vec<u8>> {
    std::fs::read(VMLINUX_PATH).ok()
}

/// A cursor over the blob that returns `None` rather than panicking.
struct Reader<'a> {
    blob: &'a [u8],
}

impl<'a> Reader<'a> {
    fn u16_at(&self, off: usize) -> Option<u16> {
        let bytes = self.blob.get(off..off.checked_add(2)?)?;
        Some(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32_at(&self, off: usize) -> Option<u32> {
        let bytes = self.blob.get(off..off.checked_add(4)?)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// The NUL-terminated string at `off` within the string section.
    fn name_of(&self, strs: usize, off: u32) -> Option<&'a str> {
        if off == 0 {
            return Some("");
        }
        let start = strs.checked_add(off as usize)?;
        let rest = self.blob.get(start..)?;
        let end = rest.iter().position(|&b| b == 0)?;
        core::str::from_utf8(&rest[..end]).ok()
    }
}

/// Header fields we need, validated.
struct Header {
    /// Absolute offset of the type section.
    types: usize,
    /// Length of the type section, in bytes.
    types_len: usize,
    /// Absolute offset of the string section.
    strs: usize,
}

fn header(r: &Reader) -> Option<Header> {
    if r.u16_at(0)? != BTF_MAGIC {
        return None;
    }
    let hdr_len = r.u32_at(4)? as usize;
    let type_off = r.u32_at(8)? as usize;
    let type_len = r.u32_at(12)? as usize;
    let str_off = r.u32_at(16)? as usize;
    let types = hdr_len.checked_add(type_off)?;
    // A type section that runs past the blob is a truncated file, not a
    // parse we should attempt.
    types.checked_add(type_len).filter(|e| *e <= r.blob.len())?;
    Some(Header {
        types,
        types_len: type_len,
        strs: hdr_len.checked_add(str_off)?,
    })
}

/// How many bytes of kind-dependent payload trail this type's 12-byte header.
fn payload_len(kind: u32, vlen: usize) -> Option<usize> {
    Some(match kind {
        KIND_INT | KIND_VAR | KIND_DECL_TAG => 4,
        KIND_ARRAY => 12,
        KIND_STRUCT | KIND_UNION | KIND_DATASEC | KIND_ENUM64 => 12usize.checked_mul(vlen)?,
        KIND_ENUM | KIND_FUNC_PROTO => 8usize.checked_mul(vlen)?,
        _ => 0,
    })
}

/// Walk the type section, calling `visit` for each struct/union whose name
/// matches one the caller wants. Stops at the first `Some` the visitor returns.
fn walk_structs<T>(
    blob: &[u8],
    mut visit: impl FnMut(&Reader, &Header, &str, usize, usize, u32) -> Option<T>,
) -> Option<T> {
    let r = Reader { blob };
    let h = header(&r)?;

    let mut pos = h.types;
    let end = h.types.checked_add(h.types_len)?;
    while pos < end {
        let name_off = r.u32_at(pos)?;
        let info = r.u32_at(pos.checked_add(4)?)?;
        let vlen = (info & 0xFFFF) as usize;
        let kind = (info >> 24) & 0x1F;
        let kind_flag = (info >> 31) & 1;
        let body = pos.checked_add(12)?;

        if kind == KIND_STRUCT || kind == KIND_UNION {
            let name = r.name_of(h.strs, name_off)?;
            if let Some(found) = visit(&r, &h, name, body, vlen, kind_flag) {
                return Some(found);
            }
        }

        pos = body.checked_add(payload_len(kind, vlen)?)?;
    }
    None
}

/// Byte offset of `member` in `struct <name>`, or `None` if either is absent.
///
/// A union is accepted under the same name, matching how the kernel's own
/// tracepoint structs are emitted.
pub fn member_offset(blob: &[u8], name: &str, member: &str) -> Option<usize> {
    walk_structs(blob, |r, h, sname, body, vlen, kind_flag| {
        if sname != name {
            return None;
        }
        member_offset_in(r, h, body, vlen, kind_flag, member)
    })
}

/// Byte offset of `member` in the first of `candidates` that this kernel has.
///
/// Returns `(index into candidates, byte offset)`.
///
/// This exists because the portability hazard here is not a field moving — it
/// is a **tracepoint changing its event class**, which renames the whole struct
/// and moves every field at once. `tcp:tcp_retransmit_skb`, for instance, is a
/// `DEFINE_EVENT` of the `tcp_event_sk_skb` class on some kernels and carries
/// its own struct on others. A single hardcoded name cannot survive that; a
/// candidate list can.
pub fn member_offset_of_any(
    blob: &[u8],
    candidates: &[&str],
    member: &str,
) -> Option<(usize, usize)> {
    for (index, candidate) in candidates.iter().enumerate() {
        if let Some(off) = member_offset(blob, candidate, member) {
            return Some((index, off));
        }
    }
    None
}

/// Declared size of `struct <name>` in bytes, or `None` if absent.
///
/// Worth asserting alongside the offsets: a same-name struct of a different
/// size is a kernel whose layout moved wholesale, and catching that as one
/// failure beats catching it as several confusing ones.
pub fn struct_size(blob: &[u8], name: &str) -> Option<usize> {
    walk_structs(blob, |r, _h, sname, body, _vlen, _kf| {
        if sname != name {
            return None;
        }
        // `size` is the third u32 of the type header, i.e. 4 bytes before the
        // member array starts.
        r.u32_at(body.checked_sub(4)?).map(|s| s as usize)
    })
}

fn member_offset_in(
    r: &Reader,
    h: &Header,
    body: usize,
    vlen: usize,
    kind_flag: u32,
    member: &str,
) -> Option<usize> {
    for i in 0..vlen {
        let m = body.checked_add(12usize.checked_mul(i)?)?;
        if r.name_of(h.strs, r.u32_at(m)?)? == member {
            let raw = r.u32_at(m.checked_add(8)?)?;
            // With `kind_flag` the low 24 bits are the bit offset and the high
            // 8 are the bitfield size; without it the whole word is the bit
            // offset.
            let bit_off = if kind_flag == 1 {
                raw & 0x00FF_FFFF
            } else {
                raw
            };
            return Some(bit_off as usize / 8);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a BTF blob with one struct, so the parser is covered on any host —
    /// including one with no `/sys/kernel/btf/vmlinux` at all.
    ///
    /// Layout: 24-byte header, then one STRUCT type, then the string section.
    fn synthetic(struct_name: &str, members: &[(&str, u32)], size: u32) -> Vec<u8> {
        let mut strs = vec![0u8]; // index 0 is the empty string
        let str_off = |s: &str, strs: &mut Vec<u8>| -> u32 {
            let at = strs.len() as u32;
            strs.extend_from_slice(s.as_bytes());
            strs.push(0);
            at
        };
        let name_idx = str_off(struct_name, &mut strs);
        let member_idx: Vec<u32> = members.iter().map(|(n, _)| str_off(n, &mut strs)).collect();

        let mut types = Vec::new();
        types.extend_from_slice(&name_idx.to_le_bytes());
        // info: vlen in the low 16 bits, kind (STRUCT = 4) in bits 24..29.
        let info = (members.len() as u32) | (KIND_STRUCT << 24);
        types.extend_from_slice(&info.to_le_bytes());
        types.extend_from_slice(&size.to_le_bytes());
        for (idx, (_, byte_off)) in member_idx.iter().zip(members) {
            types.extend_from_slice(&idx.to_le_bytes());
            types.extend_from_slice(&0u32.to_le_bytes()); // type id, unused here
            types.extend_from_slice(&(byte_off * 8).to_le_bytes()); // bit offset
        }

        let hdr_len = 24u32;
        let mut blob = Vec::new();
        blob.extend_from_slice(&BTF_MAGIC.to_le_bytes());
        blob.push(1); // version
        blob.push(0); // flags
        blob.extend_from_slice(&hdr_len.to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes()); // type_off
        blob.extend_from_slice(&(types.len() as u32).to_le_bytes());
        blob.extend_from_slice(&(types.len() as u32).to_le_bytes()); // str_off
        blob.extend_from_slice(&(strs.len() as u32).to_le_bytes());
        blob.extend_from_slice(&types);
        blob.extend_from_slice(&strs);
        blob
    }

    #[test]
    fn finds_a_member_offset() {
        let blob = synthetic("thing", &[("a", 0), ("b", 8), ("c", 24)], 32);
        assert_eq!(member_offset(&blob, "thing", "a"), Some(0));
        assert_eq!(member_offset(&blob, "thing", "b"), Some(8));
        assert_eq!(member_offset(&blob, "thing", "c"), Some(24));
        assert_eq!(struct_size(&blob, "thing"), Some(32));
    }

    #[test]
    fn absent_struct_or_member_is_none_not_a_panic() {
        let blob = synthetic("thing", &[("a", 0)], 8);
        assert_eq!(member_offset(&blob, "other", "a"), None);
        assert_eq!(member_offset(&blob, "thing", "zzz"), None);
        assert_eq!(struct_size(&blob, "other"), None);
    }

    /// The property the loader depends on: a hostile or truncated blob must not
    /// take the process down. The old test-only reader asserted its magic and
    /// sliced unchecked, which is exactly what cannot ship inside a sensor.
    #[test]
    fn malformed_input_never_panics() {
        assert_eq!(member_offset(&[], "thing", "a"), None);
        assert_eq!(member_offset(&[0xFF, 0xFF], "thing", "a"), None);
        assert_eq!(member_offset(b"not btf at all", "thing", "a"), None);

        let good = synthetic("thing", &[("a", 0), ("b", 8)], 16);
        // Every truncation of a valid blob.
        for cut in 0..good.len() {
            let _ = member_offset(&good[..cut], "thing", "b");
            let _ = struct_size(&good[..cut], "thing");
        }
        // And a valid header whose declared type section overruns the file.
        let mut lying = good.clone();
        lying[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(member_offset(&lying, "thing", "a"), None);
    }

    #[test]
    fn candidate_list_takes_the_first_kernel_has() {
        let blob = synthetic("second_name", &[("family", 32)], 80);
        assert_eq!(
            member_offset_of_any(&blob, &["first_name", "second_name"], "family"),
            Some((1, 32)),
            "reports which candidate matched, not just the offset"
        );
        assert_eq!(
            member_offset_of_any(&blob, &["nope", "also_nope"], "family"),
            None
        );
    }

    /// Not `#[ignore]`d: fast, needs no privilege, and skips where BTF genuinely
    /// is not exposed. If it can read the kernel's own BTF, the parser must
    /// agree with it on a struct every Linux kernel has.
    #[test]
    fn parses_this_kernel_if_it_exposes_btf() {
        let Some(blob) = read_vmlinux() else {
            eprintln!("skipping: {VMLINUX_PATH} unreadable (no CONFIG_DEBUG_INFO_BTF?)");
            return;
        };
        let size = struct_size(&blob, "task_struct");
        assert!(
            size.is_some_and(|s| s > 0),
            "every Linux kernel has a non-empty `struct task_struct`; \
             this parser could not find one, so it is not reading real BTF"
        );
        assert!(
            member_offset(&blob, "task_struct", "pid").is_some(),
            "`task_struct.pid` must resolve"
        );
    }
}
