//! eBPF kernel programs for sysinfo saturation histograms (#99).
//!
//! Two classic latency histograms, log2-bucketed in per-CPU arrays that
//! userspace sums and turns into percentiles:
//!
//! * **runqlat** — scheduler run-queue latency (enqueue → on-CPU). `sched_wakeup`
//!   / `sched_wakeup_new` stamp an enqueue time keyed by pid; `sched_switch`
//!   measures `now - enqueue` for the task coming on-CPU.
//! * **biolatency** — block-I/O latency (issue → complete). `block_rq_issue`
//!   stamps a start time keyed by `(dev, sector)`; `block_rq_complete` measures
//!   the delta.
//!
//! ## Status: offsets validated (#99)
//!
//! The tracepoint field offsets below were validated against
//! `/sys/kernel/btf/vmlinux` on 7.1.3-200.fc44 (2026-07-16) and all four were
//! already correct. They remain **kernel-version dependent**, so the
//! `btf_offsets_match_this_kernel` test in `zensight-sensor-sysinfo-ebpf-common`
//! re-checks them
//! against the running kernel's BTF. ("Switch to CO-RE", as an earlier note here
//! suggested, is not available: CO-RE field relocations come from clang's
//! `__builtin_preserve_access_index`, which rustc/bpf-linker do not emit. The
//! portable fix would be `aya::EbpfLoader::set_global` offset injection.)
//!
//! Both histograms are **self-validating**, which is why this is the half we turn
//! on by default: each joins a key written by one tracepoint against a key read
//! by another (`pid` for runqlat, `(dev, sector)` for biolatency). A wrong offset
//! makes the lookup miss, so the histogram comes out **empty** rather than
//! plausibly wrong. A non-empty histogram is itself evidence the offsets are
//! right.
//!
//! Fidelity note: runqlat only measures wakeup→switch. bcc's `runqlat` also
//! stamps tasks preempted while still runnable (`prev_state == TASK_RUNNING` in
//! `sched_switch`), so our tail reads lower than its under heavy CPU load. The
//! numbers are not identical to `runqlat.py` by construction.
#![cfg_attr(target_arch = "bpf", no_std)]
#![cfg_attr(target_arch = "bpf", no_main)]

// Host stub: the real programs only compile for the bpf target (aya-ebpf is
// bpf-only). This keeps the crate a normal workspace member so `aya-build` can
// resolve it by `--package`, while `cargo build --workspace` on stable builds a
// trivial empty binary. aya-build compiles the bpf target separately.
#[cfg(not(target_arch = "bpf"))]
fn main() {}

#[cfg(target_arch = "bpf")]
mod prog {
    use aya_ebpf::{
        helpers::bpf_ktime_get_ns,
        macros::{map, tracepoint},
        maps::{HashMap, PerCpuArray},
        programs::TracePointContext,
    };
    // The tracepoint offsets live in the shared crate, not here: this module only
    // compiles for the `bpf` target, so a constant declared here is invisible to
    // the host test that checks it against the running kernel's BTF.
    use zensight_sensor_sysinfo_ebpf_common::{
        log2_bucket, MAX_SLOTS, OFF_BLK_DEV, OFF_BLK_SECTOR, OFF_SWITCH_NEXT_PID, OFF_WAKEUP_PID,
    };

    // -- histograms (per-CPU → lock-free in-kernel; userspace sums across CPUs) -
    #[map]
    static RUNQ_HIST: PerCpuArray<u64> = PerCpuArray::with_max_entries(MAX_SLOTS as u32, 0);
    #[map]
    static BIO_HIST: PerCpuArray<u64> = PerCpuArray::with_max_entries(MAX_SLOTS as u32, 0);

    // -- scratch start-timestamp maps ----------------------------------------
    // pid -> enqueue timestamp (ns)
    #[map]
    static RUNQ_START: HashMap<u32, u64> = HashMap::with_max_entries(10240, 0);
    // (dev<<32 | sector_low) -> issue timestamp (ns)
    #[map]
    static BIO_START: HashMap<u64, u64> = HashMap::with_max_entries(10240, 0);

    // Field offsets: see `zensight-sensor-sysinfo-ebpf-common`, where they are
    // declared and checked against the running kernel's BTF.
    //
    // One subtlety `bio_key()` below depends on: block_rq_issue and
    // block_rq_complete carry DIFFERENT structs (trace_event_raw_block_rq, size
    // 64, vs trace_event_raw_block_rq_completion, size 48). They agree on
    // dev@8/sector@16 and diverge at 28 (`bytes` vs `error`), so one accessor is
    // safe for these two fields and nothing past offset 24.

    #[inline(always)]
    fn record(hist: &PerCpuArray<u64>, delta_ns: u64) {
        let us = delta_ns / 1_000;
        let idx = log2_bucket(us);
        if let Some(slot) = hist.get_ptr_mut(idx) {
            // SAFETY: idx < MAX_SLOTS (clamped by log2_bucket) and the per-CPU
            // array has MAX_SLOTS entries, so the slot is in bounds and
            // exclusively owned by this CPU.
            unsafe {
                *slot += 1;
            }
        }
    }

    // -- runqlat -------------------------------------------------------------
    #[tracepoint]
    pub fn sched_wakeup(ctx: TracePointContext) -> u32 {
        try_wakeup(&ctx).unwrap_or(0)
    }

    #[tracepoint]
    pub fn sched_wakeup_new(ctx: TracePointContext) -> u32 {
        try_wakeup(&ctx).unwrap_or(0)
    }

    fn try_wakeup(ctx: &TracePointContext) -> Result<u32, i64> {
        // SAFETY: reading a fixed-width field at a known tracepoint offset.
        let pid: u32 = unsafe { ctx.read_at(OFF_WAKEUP_PID)? };
        if pid == 0 {
            return Ok(0);
        }
        let now = unsafe { bpf_ktime_get_ns() };
        // Last writer wins if a task is woken multiple times before running.
        let _ = RUNQ_START.insert(&pid, &now, 0);
        Ok(0)
    }

    #[tracepoint]
    pub fn sched_switch(ctx: TracePointContext) -> u32 {
        try_switch(&ctx).unwrap_or(0)
    }

    fn try_switch(ctx: &TracePointContext) -> Result<u32, i64> {
        let next_pid: u32 = unsafe { ctx.read_at(OFF_SWITCH_NEXT_PID)? };
        if next_pid == 0 {
            return Ok(0);
        }
        // SAFETY: pointer comes from a successful map lookup.
        if let Some(&start) = unsafe { RUNQ_START.get(&next_pid) } {
            let now = unsafe { bpf_ktime_get_ns() };
            let delta = now.saturating_sub(start);
            record(&RUNQ_HIST, delta);
            let _ = RUNQ_START.remove(&next_pid);
        }
        Ok(0)
    }

    // -- biolatency ----------------------------------------------------------
    #[inline(always)]
    fn bio_key(ctx: &TracePointContext) -> Result<u64, i64> {
        let dev: u32 = unsafe { ctx.read_at(OFF_BLK_DEV)? };
        let sector: u64 = unsafe { ctx.read_at(OFF_BLK_SECTOR)? };
        Ok(((dev as u64) << 32) | (sector & 0xffff_ffff))
    }

    #[tracepoint]
    pub fn block_rq_issue(ctx: TracePointContext) -> u32 {
        try_bio_issue(&ctx).unwrap_or(0)
    }

    fn try_bio_issue(ctx: &TracePointContext) -> Result<u32, i64> {
        let key = bio_key(ctx)?;
        let now = unsafe { bpf_ktime_get_ns() };
        let _ = BIO_START.insert(&key, &now, 0);
        Ok(0)
    }

    #[tracepoint]
    pub fn block_rq_complete(ctx: TracePointContext) -> u32 {
        try_bio_complete(&ctx).unwrap_or(0)
    }

    fn try_bio_complete(ctx: &TracePointContext) -> Result<u32, i64> {
        let key = bio_key(ctx)?;
        if let Some(&start) = unsafe { BIO_START.get(&key) } {
            let now = unsafe { bpf_ktime_get_ns() };
            let delta = now.saturating_sub(start);
            record(&BIO_HIST, delta);
            let _ = BIO_START.remove(&key);
        }
        Ok(0)
    }
}

#[cfg(target_arch = "bpf")]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // eBPF programs cannot unwind; the verifier also rejects panics in practice.
    loop {}
}
