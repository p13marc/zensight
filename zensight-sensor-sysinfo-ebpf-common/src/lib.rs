//! Shared constants/helpers for the sysinfo eBPF saturation histograms (#99).
//!
//! Used by both the kernel programs (`zensight-sensor-sysinfo-ebpf`, no_std,
//! `bpfel-unknown-none`) and the userspace loader (`zensight-sensor-sysinfo`
//! `ebpf` feature). Keeping `MAX_SLOTS` and the bucketing function here
//! guarantees the two sides agree on the log2 histogram layout.
#![no_std]

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

#[cfg(test)]
mod tests {
    use super::*;

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
