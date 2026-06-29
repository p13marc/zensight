//! Userspace side of the opt-in eBPF saturation histograms (#99).
//!
//! Loads the bytecode compiled by `build.rs`, attaches the runqlat/biolatency
//! tracepoint programs, then runs a poller that periodically sums the per-CPU
//! histogram arrays, computes a per-window delta, and stores a
//! [`LatencyReport`](crate::map::LatencyReport) into shared state that the
//! `@/query/latency` queryable replies with.
//!
//! Gated on `cfg(all(target_os = "linux", feature = "ebpf"))` — the rest of the
//! crate stays aya-free. Any load/attach failure (no `CAP_BPF`/`CAP_PERFMON`,
//! unsupported kernel) is returned as an `Err` to the caller, which logs a
//! single warning and leaves the unprivileged baseline untouched.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use aya::{
    Ebpf, EbpfLoader,
    maps::{MapData, PerCpuArray},
    programs::TracePoint,
};
use tokio::task::JoinHandle;
use zensight_sensor_sysinfo_ebpf_common::MAX_SLOTS;

use crate::map::{LatencyReport, build_histogram, windowed_delta};

/// Load + attach the eBPF programs and spawn the histogram poller.
///
/// On success the shared `report` is marked `available` and refreshed every
/// `poll_interval_secs`. On failure (returned `Err`) the caller keeps the
/// report `available: false` and logs the fallback warning.
pub fn start(report: Arc<Mutex<LatencyReport>>, poll_interval_secs: u64) -> Result<JoinHandle<()>> {
    // Best-effort: kernels < 5.11 need an RLIMIT_MEMLOCK bump for BPF maps;
    // newer kernels use memcg accounting and ignore it.
    bump_memlock();

    let mut bpf = EbpfLoader::new()
        .load(aya::include_bytes_aligned!(concat!(
            env!("OUT_DIR"),
            "/zensight-sensor-sysinfo-ebpf"
        )))
        .context("load eBPF bytecode")?;

    // aya-log is best-effort; absence of the log map is not fatal.
    if let Err(e) = aya_log::EbpfLogger::init(&mut bpf) {
        tracing::debug!(error = %e, "eBPF: aya-log init skipped");
    }

    attach_tp(&mut bpf, "sched_wakeup", "sched", "sched_wakeup")?;
    attach_tp(&mut bpf, "sched_wakeup_new", "sched", "sched_wakeup_new")?;
    attach_tp(&mut bpf, "sched_switch", "sched", "sched_switch")?;
    attach_tp(&mut bpf, "block_rq_issue", "block", "block_rq_issue")?;
    attach_tp(&mut bpf, "block_rq_complete", "block", "block_rq_complete")?;

    // Take the histogram maps out so the poller can read them; `bpf` itself is
    // moved into the task to keep the attached program links alive.
    let runq: PerCpuArray<MapData, u64> =
        PerCpuArray::try_from(bpf.take_map("RUNQ_HIST").context("RUNQ_HIST map missing")?)?;
    let bio: PerCpuArray<MapData, u64> =
        PerCpuArray::try_from(bpf.take_map("BIO_HIST").context("BIO_HIST map missing")?)?;

    if let Ok(mut r) = report.lock() {
        r.available = true;
        r.window_secs = poll_interval_secs;
    }

    let interval = poll_interval_secs.max(1);
    let handle = tokio::spawn(async move {
        // Keep the loaded programs attached for the lifetime of the poller.
        let _bpf = bpf;
        let mut prev_runq = [0u64; MAX_SLOTS];
        let mut prev_bio = [0u64; MAX_SLOTS];
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(interval));
        loop {
            tick.tick().await;
            let cur_runq = read_hist(&runq);
            let cur_bio = read_hist(&bio);
            let d_runq = windowed_delta(&cur_runq, &prev_runq);
            let d_bio = windowed_delta(&cur_bio, &prev_bio);
            prev_runq = cur_runq;
            prev_bio = cur_bio;
            if let Ok(mut r) = report.lock() {
                r.available = true;
                r.window_secs = interval;
                r.runqlat = build_histogram(&d_runq, "microseconds");
                r.biolatency = build_histogram(&d_bio, "microseconds");
            }
        }
    });

    Ok(handle)
}

fn attach_tp(bpf: &mut Ebpf, prog: &str, category: &str, name: &str) -> Result<()> {
    let p: &mut TracePoint = bpf
        .program_mut(prog)
        .with_context(|| format!("program {prog} missing"))?
        .try_into()
        .with_context(|| format!("program {prog} is not a tracepoint"))?;
    p.load().with_context(|| format!("load {prog}"))?;
    p.attach(category, name)
        .with_context(|| format!("attach {category}/{name}"))?;
    Ok(())
}

/// Sum a per-CPU histogram array across all CPUs into a flat `[u64; MAX_SLOTS]`.
fn read_hist(arr: &PerCpuArray<MapData, u64>) -> [u64; MAX_SLOTS] {
    let mut out = [0u64; MAX_SLOTS];
    for (i, slot) in out.iter_mut().enumerate() {
        if let Ok(vals) = arr.get(&(i as u32), 0) {
            *slot = vals.iter().copied().sum();
        }
    }
    out
}

fn bump_memlock() {
    let lim = libc::rlimit {
        rlim_cur: libc::RLIM_INFINITY,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: setrlimit with a valid rlimit pointer; failure is ignored.
    unsafe {
        libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim);
    }
}
