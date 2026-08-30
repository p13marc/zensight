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
    }
    assert!(
        !degraded_flag.load(Ordering::SeqCst),
        "recovery must restore the degradable"
    );
    // Reaching this line IS the final assertion: the process never exited.
}

/// The literal done-when numbers, RSS-strict: after the ladder is done, the
/// real process RSS is back under the budget. Ignored by default — raw RSS
/// depends on allocator behavior and parallel test threads, so this runs
/// nightly/manually: `cargo test -p zensight-sensor-core --test
/// governor_ladder -- --ignored --test-threads=1`.
#[test]
#[ignore = "RSS-strict; run manually with --test-threads=1"]
fn rss_returns_under_budget_after_shedding() {
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
