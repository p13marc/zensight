//! #812's done-when, driven for real: a sensor given a budget and an
//! adversarial workload evicts, degrades, reports — and does not exit.
//!
//! No bus: [`governed_snapshot`] is the exact tick body the runner publishes,
//! and a [`SensorHealth`] without a publisher publishes nothing. The ballast
//! table is real memory (touched 64 KiB chunks), and `rss_bytes` is the real
//! process RSS from `/proc/self/status` — so what the ladder observes is what
//! the kernel would OOM on.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use zensight_common::TableStats;
use zensight_sensor_core::governor::governed_snapshot;
use zensight_sensor_core::{EvictOutcome, MemoryGovernor, SensorHealth, TableHandle};

const CHUNK: usize = 64 * 1024;
const MIB: u64 = 1024 * 1024;

/// RSS is process-global: two ladder tests reading `/proc/self/status`
/// concurrently shift each other's ratios (one test's allocations are the
/// other's mystery pressure), so every test that measures real RSS holds
/// this for its whole body.
static RSS_LOCK: Mutex<()> = Mutex::new(());

type Ballast = Arc<Mutex<Vec<Vec<u8>>>>;

fn touched_chunk(i: usize) -> Vec<u8> {
    // Touch every page so the allocation is resident, not just reserved.
    vec![(i % 251) as u8; CHUNK]
}

fn inflate(ballast: &Ballast, bytes: u64) {
    let mut v = ballast.lock().unwrap();
    let start = v.len();
    for i in 0..(bytes as usize / CHUNK) {
        v.push(touched_chunk(start + i));
    }
}

fn register_ballast(governor: &MemoryGovernor, ballast: &Ballast) {
    let stats_b = ballast.clone();
    let evict_b = ballast.clone();
    governor.register_table(TableHandle {
        name: "ballast".into(),
        stats: Box::new(move || {
            let v = stats_b.lock().unwrap();
            TableStats {
                name: "ballast".into(),
                entries: v.len() as u64,
                bytes: Some((v.len() * CHUNK) as u64),
                ..Default::default()
            }
        }),
        evict: Some(Box::new(move |target| {
            let mut v = evict_b.lock().unwrap();
            let n = (target as usize).div_ceil(CHUNK).min(v.len());
            let keep = v.len() - n;
            v.truncate(keep);
            v.shrink_to_fit();
            EvictOutcome {
                entries: n as u64,
                bytes: (n * CHUNK) as u64,
            }
        })),
    });
}

/// The core done-when: over budget → the ladder arms, evicts the ballast by
/// name, escalates to Degrade under sustained pressure, reports Degraded —
/// and the process is still here to assert all of it.
#[test]
fn ladder_evicts_names_the_table_degrades_and_never_exits() {
    let _rss = RSS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let health = SensorHealth::new("test");
    let governor = MemoryGovernor::default();
    let ballast: Ballast = Arc::default();
    register_ballast(&governor, &ballast);
    let degraded_flag = Arc::new(AtomicBool::new(false));
    {
        let f = degraded_flag.clone();
        governor.register_degradable(
            "test_collector",
            Box::new(move |apply| f.store(apply, Ordering::SeqCst)),
        );
    }

    // Baseline RSS, then a budget 8 MiB above it: the adversarial 24 MiB of
    // ballast below puts the process far over.
    let baseline = governed_snapshot(&health, &governor)
        .self_stats
        .and_then(|s| s.rss_bytes)
        .expect("self-measured RSS on Linux");
    health.set_budget_bytes(baseline + 8 * MIB);
    inflate(&ballast, 24 * MIB);

    let mut saw_evict_of_ballast = false;
    let mut saw_degraded_status = false;
    for _ in 0..20 {
        // Keep the pressure adversarial: re-inflate part of what was evicted
        // so the ladder cannot win by evicting once.
        if ballast.lock().unwrap().len() * CHUNK < 16 * MIB as usize {
            inflate(&ballast, 8 * MIB);
        }
        let snap = governed_snapshot(&health, &governor);
        let stats = snap.self_stats.as_ref().expect("measured");
        let ladder = stats.ladder.as_ref().expect("budget set => ladder armed");
        if ladder
            .evicted
            .iter()
            .any(|e| e.table == "ballast" && e.entries > 0)
        {
            saw_evict_of_ballast = true;
            // The report names the table — the difference between a page
            // and a fix.
            assert!(
                ladder.reason.as_deref().unwrap_or("").contains("ballast"),
                "reason must name the evicted table: {:?}",
                ladder.reason
            );
        }
        if ladder.step >= 2 {
            assert!(
                degraded_flag.load(Ordering::SeqCst),
                "step 2 must have applied the degradable"
            );
            assert!(
                ladder.degraded.contains(&"test_collector".to_string()),
                "the degraded list names what was stopped"
            );
            assert_eq!(
                snap.status,
                zensight_common::HealthStatus::Degraded,
                "a degraded sensor must not report Healthy"
            );
            saw_degraded_status = true;
        }
        assert!(ladder.step <= 3, "there is no step past Saturated");
    }
    assert!(saw_evict_of_ballast, "the ladder never evicted the ballast");
    assert!(
        saw_degraded_status,
        "sustained pressure never reached Degrade"
    );

    // Relief: stop re-inflating, let the ladder drain and walk down.
    //
    // The pause is load-bearing, not politeness. Recovery is measured from
    // *self-reported RSS*, and RSS is what the kernel currently attributes to
    // the process — not what the allocator has released. `malloc_trim` hands
    // pages back, but the accounting catches up on its own schedule, so a
    // tight spin of 30 snapshots can complete in a few milliseconds and never
    // observe the drop it is waiting for. That made the assertion depend on
    // machine load rather than on the ladder: it passed on an idle box and
    // failed under a parallel `cargo test --workspace`, which is the run that
    // matters.
    //
    // 20 ms × 30 is 600 ms of patience for a property that takes RECOVER_TICKS
    // ticks to establish, and the loop still exits the moment it sees step 0.
    for _ in 0..30 {
        let snap = governed_snapshot(&health, &governor);
        if snap
            .self_stats
            .as_ref()
            .and_then(|s| s.ladder.as_ref())
            .is_some_and(|l| l.step == 0)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !degraded_flag.load(Ordering::SeqCst),
        "recovery must restore the degradable"
    );
    // Reaching this line IS the final assertion: the process never exited.
}

/// #864's done-when: a budget below the process baseline must not become
/// scorched-earth LRU forever. The target is unreachable by construction
/// (the RSS is the test binary itself, not the table), so the governor gets
/// exactly one honest round, latches futile, holds Saturated loudly — and
/// the table keeps its data. Raising the budget recovers without a restart.
#[test]
fn budget_below_process_baseline_saturates_loudly_without_thrashing() {
    let _rss = RSS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let health = SensorHealth::new("test");
    let governor = MemoryGovernor::default();
    let evict_calls = Arc::new(Mutex::new(0u32));
    {
        let c = evict_calls.clone();
        // A tiny inventory table, the #864 shape: a few entries, a few KiB —
        // nothing next to the process baseline the budget ignores.
        governor.register_table(TableHandle {
            name: "tls_inventory".into(),
            stats: Box::new(|| TableStats {
                name: "tls_inventory".into(),
                entries: 2,
                bytes: Some(2048),
                ..Default::default()
            }),
            evict: Some(Box::new(move |_ask| {
                *c.lock().unwrap() += 1;
                EvictOutcome {
                    entries: 2,
                    bytes: 2048,
                }
            })),
        });
    }
    let degraded_flag = Arc::new(AtomicBool::new(false));
    {
        let f = degraded_flag.clone();
        governor.register_degradable(
            "test_collector",
            Box::new(move |apply| f.store(apply, Ordering::SeqCst)),
        );
    }

    let baseline = governed_snapshot(&health, &governor)
        .self_stats
        .and_then(|s| s.rss_bytes)
        .expect("self-measured RSS on Linux");
    // Half the baseline: instant Critical, target unreachable forever.
    health.set_budget_bytes(baseline / 2);

    let mut last = None;
    for _ in 0..15 {
        last = governed_snapshot(&health, &governor).self_stats;
    }
    let stats = last.expect("measured");
    let ladder = stats.ladder.as_ref().expect("budget set => ladder armed");
    assert!(
        *evict_calls.lock().unwrap() <= 2,
        "eviction must stop after futility, not repeat every tick (called {}x)",
        *evict_calls.lock().unwrap()
    );
    assert_eq!(
        (ladder.step, ladder.futile),
        (3, true),
        "an impossible budget holds Saturated with the futility latch set"
    );
    let reason = ladder.reason.as_deref().unwrap_or("");
    assert!(
        reason.contains("raise budget_rss_mb"),
        "the report must tell the operator the actual fix: {reason}"
    );
    assert!(
        degraded_flag.load(Ordering::SeqCst),
        "degrading is free and still applies on the futile path"
    );

    // The operator's fix — a budget above RSS — recovers without a restart.
    let rss = stats.rss_bytes.expect("rss");
    health.set_budget_bytes(rss * 2);
    let mut recovered = false;
    for _ in 0..40 {
        let snap = governed_snapshot(&health, &governor);
        let l = snap.self_stats.as_ref().and_then(|s| s.ladder.as_ref());
        if l.is_some_and(|l| l.step == 0 && !l.futile) {
            recovered = true;
            break;
        }
    }
    assert!(recovered, "a resized budget must walk the ladder back down");
    assert!(
        !degraded_flag.load(Ordering::SeqCst),
        "recovery must restore the degradable"
    );
}

/// The literal done-when numbers, RSS-strict: after the ladder is done, the
/// real process RSS is back under the budget. Ignored by default — raw RSS
/// depends on allocator behavior and parallel test threads, so this runs
/// nightly/manually: `cargo test -p zensight-sensor-core --test
/// governor_ladder -- --ignored --test-threads=1`.
#[test]
#[ignore = "RSS-strict; run manually with --test-threads=1"]
fn rss_returns_under_budget_after_shedding() {
    let _rss = RSS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let health = SensorHealth::new("test");
    let governor = MemoryGovernor::default();
    let ballast: Ballast = Arc::default();
    register_ballast(&governor, &ballast);

    let baseline = governed_snapshot(&health, &governor)
        .self_stats
        .and_then(|s| s.rss_bytes)
        .expect("rss");
    let budget = baseline + 8 * MIB;
    health.set_budget_bytes(budget);
    inflate(&ballast, 32 * MIB);

    for _ in 0..10 {
        let snap = governed_snapshot(&health, &governor);
        let rss = snap.self_stats.and_then(|s| s.rss_bytes).expect("rss");
        if rss < budget {
            return; // shed back under budget — done-when met
        }
    }
    let final_rss = governed_snapshot(&health, &governor)
        .self_stats
        .and_then(|s| s.rss_bytes)
        .unwrap();
    // 2 MiB slack for allocator noise.
    assert!(
        final_rss < budget + 2 * MIB,
        "rss {final_rss} never returned near budget {budget}"
    );
}
