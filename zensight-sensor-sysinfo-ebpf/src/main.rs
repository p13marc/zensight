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
//! NOTE (blind implementation, #99): the tracepoint field OFFSETS below are the
//! commonly-documented ones but ARE kernel-version dependent — they must be
//! validated against `/sys/kernel/tracing/events/{sched,block}/*/format` on the
//! target kernel (or switched to BTF/CO-RE field access). Flagged in the PR.
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
    use zensight_sensor_sysinfo_ebpf_common::{log2_bucket, MAX_SLOTS};

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

    // -- tracepoint field offsets (VERIFY against the kernel's format files) --
    // sched/sched_wakeup:  ... char comm[16]@8, pid_t pid@24
    const OFF_WAKEUP_PID: usize = 24;
    // sched/sched_switch:  ... next_comm[16]@40, pid_t next_pid@56
    const OFF_SWITCH_NEXT_PID: usize = 56;
    // block/block_rq_issue & block_rq_complete: dev_t dev@8, sector_t sector@16
    const OFF_BLK_DEV: usize = 8;
    const OFF_BLK_SECTOR: usize = 16;

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
