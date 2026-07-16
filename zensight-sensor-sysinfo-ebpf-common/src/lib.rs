//! Shared constants/helpers for the sysinfo eBPF saturation histograms (#99).
//!
//! Used by both the kernel programs (`zensight-sensor-sysinfo-ebpf`, no_std,
//! `bpfel-unknown-none`) and the userspace loader (`zensight-sensor-sysinfo`
//! `ebpf` feature). Keeping `MAX_SLOTS` and the bucketing function here
//! guarantees the two sides agree on the log2 histogram layout.
//!
//! The tracepoint field offsets live here for the same reason, plus one more:
//! the program crate only compiles for the `bpf` target, so constants declared
//! inside it are invisible to a host test. Here they can be checked against the
//! running kernel's BTF — see `btf_offsets_match_this_kernel`.
#![cfg_attr(not(test), no_std)]

// ---------------------------------------------------------------------------
// Tracepoint field offsets
// ---------------------------------------------------------------------------
//
// These are a kernel ABI contract, hand-maintained and **kernel-version
// dependent**. Validated against `/sys/kernel/btf/vmlinux` on 7.1.3-200.fc44
// (2026-07-16); the test below re-checks them against whatever kernel you are
// on, because getting one wrong is a silent-garbage bug, not a loud one.
//
// (The portable fix is `aya::EbpfLoader::set_global` offset injection at load
// time. CO-RE is not an option: its field relocations come from clang's
// `__builtin_preserve_access_index`, which rustc/bpf-linker do not emit.)

/// `pid` in `struct trace_event_raw_sched_wakeup_template`.
pub const OFF_WAKEUP_PID: usize = 24;

/// `next_pid` in `struct trace_event_raw_sched_switch`.
pub const OFF_SWITCH_NEXT_PID: usize = 56;

/// `dev` in both `struct trace_event_raw_block_rq` (block_rq_issue) and
/// `struct trace_event_raw_block_rq_completion` (block_rq_complete).
pub const OFF_BLK_DEV: usize = 8;

/// `sector` in both of the above.
///
/// The two structs are NOT the same (size 64 vs 48) and diverge at offset 28
/// (`bytes` vs `error`); they agree only up to `nr_sector`. So a shared accessor
/// is safe for `dev`/`sector` and nothing beyond.
pub const OFF_BLK_SECTOR: usize = 16;

/// Number of log2 latency buckets.
///
/// Bucket 0 = `[0, 1)` µs; bucket `i` (i ≥ 1) covers `[2^(i-1), 2^i)` µs. With
/// 27 slots the top bucket's upper bound is `2^26` µs ≈ 67 s, which caps any
/// run-queue / block-I/O latency we care about. (`runqlat`/`biolatency` use the
/// same log2 scheme.)
pub const MAX_SLOTS: usize = 27;

/// log2 bucket index for a microsecond value, clamped to `MAX_SLOTS - 1`.
///
/// Shared so the kernel side (which increments) and the userspace side (which
/// labels bucket boundaries) agree exactly. `log2_bucket(0) == 0`,
/// `log2_bucket(1) == 1`, `log2_bucket(2) == 2`, `log2_bucket(3) == 2`, …
#[inline]
pub fn log2_bucket(us: u64) -> u32 {
    if us == 0 {
        return 0;
    }
    // Position of the highest set bit + 1 (classic bpf `log2l`).
    let b = 64 - us.leading_zeros();
    if b >= MAX_SLOTS as u32 {
        MAX_SLOTS as u32 - 1
    } else {
        b
    }
}

/// Minimal BTF reader, test-only: enough to find a struct by name and report its
/// members' byte offsets.
///
/// BTF member offsets are emitted by the same `offsetof` the tracepoint's
/// `format` file is generated from, so they are the authority for the constants
/// above — and `/sys/kernel/btf/vmlinux` is world-readable where
/// `/sys/kernel/tracing` is mode 0700, so this validates unprivileged, in a
/// container, and in CI. Format: `Documentation/bpf/btf.rst`.
#[cfg(test)]
mod btf {
    /// Byte offset of `member` in `struct <name>`, or `None` if either is absent.
    pub fn member_offset(blob: &[u8], name: &str, member: &str) -> Option<usize> {
        let u16_at = |o: usize| u16::from_le_bytes(blob[o..o + 2].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(blob[o..o + 4].try_into().unwrap());

        assert_eq!(u16_at(0), 0xEB9F, "bad BTF magic");
        let hdr_len = u32_at(4) as usize;
        let type_off = u32_at(8) as usize;
        let type_len = u32_at(12) as usize;
        let str_off = u32_at(16) as usize;

        let strs = hdr_len + str_off;
        let name_of = |off: u32| -> &str {
            if off == 0 {
                return "";
            }
            let start = strs + off as usize;
            let end = start + blob[start..].iter().position(|&b| b == 0).unwrap();
            core::str::from_utf8(&blob[start..end]).unwrap_or("")
        };

        let mut pos = hdr_len + type_off;
        let end = pos + type_len;
        while pos < end {
            let name_off = u32_at(pos);
            let info = u32_at(pos + 4);
            let vlen = (info & 0xFFFF) as usize;
            let kind = (info >> 24) & 0x1F;
            let kind_flag = (info >> 31) & 1;
            let body = pos + 12;

            const STRUCT: u32 = 4;
            const UNION: u32 = 5;
            if (kind == STRUCT || kind == UNION) && name_of(name_off) == name {
                for i in 0..vlen {
                    let m = body + 12 * i;
                    if name_of(u32_at(m)) == member {
                        let raw = u32_at(m + 8);
                        // With kind_flag the low 24 bits are the bit offset and
                        // the high 8 are the bitfield size; without it the whole
                        // word is the bit offset.
                        let bit_off = if kind_flag == 1 {
                            raw & 0x00FF_FFFF
                        } else {
                            raw
                        };
                        return Some(bit_off as usize / 8);
                    }
                }
                return None;
            }

            // Skip this type's trailing payload, which is kind-dependent.
            pos = body
                + match kind {
                    1 | 14 | 17 => 4,                      // INT, VAR, DECL_TAG
                    3 => 12,                               // ARRAY
                    STRUCT | UNION | 15 | 19 => 12 * vlen, // STRUCT/UNION/DATASEC/ENUM64
                    6 | 13 => 8 * vlen,                    // ENUM, FUNC_PROTO
                    _ => 0,
                };
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hardcoded tracepoint offsets must match the kernel we are running on.
    ///
    /// Getting one wrong does not raise an error — it silently reads the wrong
    /// field, and the histograms fill with plausible nonsense. That is the whole
    /// reason this test exists rather than a comment saying "verify these".
    ///
    /// Not `#[ignore]`d: it is fast, needs no privilege, and a failure here is
    /// real news on any kernel. It skips only where BTF genuinely is not exposed
    /// (non-Linux, or a kernel built without CONFIG_DEBUG_INFO_BTF).
    #[test]
    fn btf_offsets_match_this_kernel() {
        const VMLINUX: &str = "/sys/kernel/btf/vmlinux";
        let Ok(blob) = std::fs::read(VMLINUX) else {
            eprintln!("skipping: {VMLINUX} unreadable (no CONFIG_DEBUG_INFO_BTF?)");
            return;
        };

        // (struct, member, our constant). block_rq_issue and block_rq_complete
        // carry different structs, so assert dev/sector against *both*.
        let cases: &[(&str, &str, usize)] = &[
            (
                "trace_event_raw_sched_wakeup_template",
                "pid",
                OFF_WAKEUP_PID,
            ),
            (
                "trace_event_raw_sched_switch",
                "next_pid",
                OFF_SWITCH_NEXT_PID,
            ),
            ("trace_event_raw_block_rq", "dev", OFF_BLK_DEV),
            ("trace_event_raw_block_rq", "sector", OFF_BLK_SECTOR),
            ("trace_event_raw_block_rq_completion", "dev", OFF_BLK_DEV),
            (
                "trace_event_raw_block_rq_completion",
                "sector",
                OFF_BLK_SECTOR,
            ),
        ];

        let mut checked = 0;
        for (strukt, member, ours) in cases {
            let Some(truth) = btf::member_offset(&blob, strukt, member) else {
                panic!("BTF has no {strukt}.{member} — the tracepoint ABI moved");
            };
            assert_eq!(
                truth, *ours,
                "{strukt}.{member} is at {truth} on this kernel, but we hardcode {ours} — \
                 the histograms would fill with garbage. Update the constant."
            );
            checked += 1;
        }
        assert_eq!(checked, cases.len(), "every offset must be checked");
    }

    #[test]
    fn bucket_edges() {
        assert_eq!(log2_bucket(0), 0);
        assert_eq!(log2_bucket(1), 1);
        assert_eq!(log2_bucket(2), 2);
        assert_eq!(log2_bucket(3), 2);
        assert_eq!(log2_bucket(4), 3);
    }

    #[test]
    fn bucket_clamps_to_max() {
        assert_eq!(log2_bucket(u64::MAX), MAX_SLOTS as u32 - 1);
        // Anything at/above 2^(MAX_SLOTS-1) µs saturates the top bucket.
        assert_eq!(log2_bucket(1 << 40), MAX_SLOTS as u32 - 1);
    }

    #[test]
    fn bucket_is_monotonic() {
        let mut prev = 0;
        for shift in 0..30u32 {
            let b = log2_bucket(1u64 << shift);
            assert!(b >= prev, "bucket must be monotonic in input");
            prev = b;
        }
    }
}
