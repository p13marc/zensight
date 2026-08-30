//! The memory governor and its shed ladder (#812).
//!
//! #811 taught a sensor to *see* itself (RSS, budget, per-table occupancy);
//! this module teaches it to *act*: a sensor over its declared budget sheds
//! instead of dying, because a monitoring agent that exits during resource
//! pressure removes the evidence at the exact moment it becomes interesting.
//!
//! The ladder, in order — and dying is not on it:
//!
//! | Step | Meaning |
//! |---|---|
//! | 0 | nominal |
//! | 1 | **Evict** — LRU from the largest registered table first |
//! | 2 | **Degrade** — registered optional work stopped |
//! | 3 | **Saturated** — everything shed, still over budget: the loudest possible report |
//!
//! The thresholds deliberately agree with [`crate::health::budget_level`]
//! (Warning ≥ 80 %, Critical ≥ 95 %, clear < 75 %), so the `sensor-budget`
//! alert and the ladder describe one condition, never two.
//!
//! The governor owns its own registry and runs on the health tick, **outside
//! every `SensorHealth` lock** — eviction takes the tables' own hot-path
//! mutexes, which the table-stats providers' must-not-block contract forbids.

use std::collections::HashMap;
use std::sync::Mutex;

use zensight_common::{LadderEviction, LadderState, SelfStats, TableStats};

/// What one governor-driven eviction actually freed — honest counts, not the
/// requested target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictOutcome {
    pub entries: u64,
    /// Owner's estimate; 0 = "cannot say".
    pub bytes: u64,
}

/// One registered table: a cheap occupancy read plus (for evictable tables)
/// an eviction handle. `stats` must be cheap (a lock and a length); `evict`
/// may take the table's hot-path mutex — the governor guarantees it is never
/// called under the [`crate::SensorHealth`] lock.
pub struct TableHandle {
    pub name: String,
    pub stats: Box<dyn Fn() -> TableStats + Send + Sync>,
    /// Free approximately this many bytes, LRU-first, returning what was
    /// actually freed. `None` = report-only table (rings, foreign caches).
    pub evict: Option<Box<dyn Fn(u64) -> EvictOutcome + Send + Sync>>,
}

/// Stop (`true`) or restore (`false`) one piece of optional work. Must be
/// idempotent — the governor fans transitions, but a restart or hot-swap may
/// replay one.
pub type DegradeFn = Box<dyn Fn(bool) + Send + Sync>;

/// The fraction of cgroup `memory.max` taken as the discovered budget when
/// the config declares none (#812). 0.75 puts the ladder's Warning line at
/// 60 % and its Critical line at ~71 % of the cgroup limit — comfortably
/// ahead of the OOM killer, which is the entire point.
pub const CGROUP_BUDGET_FRACTION: f64 = 0.75;

/// Ladder thresholds — kept textually beside [`crate::health::budget_level`]'s
/// 0.95/0.80/0.75 so a change to one is visibly a change to both.
const ENTER_RATIO: f64 = 0.80;
const CRITICAL_RATIO: f64 = 0.95;
const CLEAR_RATIO: f64 = 0.75;
/// Ticks step 1 may run without relief before escalating to Degrade.
const STUCK_TICKS: u32 = 3;
/// Consecutive under-`CLEAR_RATIO` ticks before de-escalating one step.
const RECOVER_TICKS: u32 = 6;
/// Minimum ticks between any two transitions — one spike must not slam
/// collectors off and on.
const COOLDOWN_TICKS: u32 = 2;

/// What one tick decided. Side effects are the caller's ([`MemoryGovernor::step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Decision {
    step: u8,
    /// Bytes to try to free this tick (down to `CLEAR_RATIO × budget`).
    evict_target: Option<u64>,
    /// `Some(true)` = apply degradables, `Some(false)` = restore them —
    /// emitted only on the transition, like the netring shed controller.
    apply_degrade: Option<bool>,
}

/// The pure ladder state machine: `(rss, budget)` per tick in, a [`Decision`]
/// out. No I/O, no clock — time is the tick count, which is what makes every
/// escalation rule unit-testable without sleeping.
#[derive(Debug)]
struct LadderMachine {
    step: u8,
    ticks_in_step: u32,
    ticks_since_transition: u32,
    under_clear_streak: u32,
}

impl Default for LadderMachine {
    fn default() -> Self {
        Self {
            step: 0,
            ticks_in_step: 0,
            // Born past the cool-down: the first arming must not wait out a
            // transition that never happened.
            ticks_since_transition: COOLDOWN_TICKS + 1,
            under_clear_streak: 0,
        }
    }
}

impl LadderMachine {
    fn transition(&mut self, to: u8) -> u8 {
        self.step = to;
        self.ticks_in_step = 0;
        self.ticks_since_transition = 0;
        self.under_clear_streak = 0;
        to
    }

    fn decide(&mut self, rss: u64, budget: u64) -> Decision {
        let ratio = if budget == 0 {
            0.0
        } else {
            rss as f64 / budget as f64
        };
        self.ticks_in_step = self.ticks_in_step.saturating_add(1);
        self.ticks_since_transition = self.ticks_since_transition.saturating_add(1);
        if ratio < CLEAR_RATIO {
            self.under_clear_streak = self.under_clear_streak.saturating_add(1);
        } else {
            self.under_clear_streak = 0;
        }
        let cooled = self.ticks_since_transition > COOLDOWN_TICKS;

        let mut apply_degrade = None;
        match self.step {
            0 => {
                if ratio >= ENTER_RATIO && cooled {
                    self.transition(1);
                }
            }
            1 => {
                // Escalate on Critical pressure, or on being stuck: three
                // ticks of eviction that never brought relief below the
                // entry line means eviction alone is not enough.
                if (ratio >= CRITICAL_RATIO
                    || (ratio >= ENTER_RATIO && self.ticks_in_step > STUCK_TICKS))
                    && cooled
                {
                    self.transition(2);
                    apply_degrade = Some(true);
                } else if self.under_clear_streak >= RECOVER_TICKS && cooled {
                    self.transition(0);
                }
            }
            2 => {
                // A full post-transition tick still at Critical: nothing left
                // to shed — saturated, and reporting is all that remains.
                if ratio >= CRITICAL_RATIO && self.ticks_in_step > 1 && cooled {
                    self.transition(3);
                } else if self.under_clear_streak >= RECOVER_TICKS && cooled {
                    self.transition(1);
                    apply_degrade = Some(false);
                }
            }
            _ => {
                if ratio < CRITICAL_RATIO && cooled {
                    // Pressure relented below Critical: back to Degraded
                    // (degradables stay applied until the full recovery path
                    // walks down through step 2).
                    self.transition(2);
                } else if self.under_clear_streak >= RECOVER_TICKS && cooled {
                    self.transition(1);
                    apply_degrade = Some(false);
                }
            }
        }

        // Evict every tick the ladder is armed and over the entry line —
        // aiming at the alert's clear line so the ladder and the
        // `sensor-budget` rule stop worrying at the same place.
        let evict_target = (self.step >= 1 && ratio >= ENTER_RATIO)
            .then(|| rss.saturating_sub((budget as f64 * CLEAR_RATIO) as u64))
            .filter(|&t| t > 0);

        Decision {
            step: self.step,
            evict_target,
            apply_degrade,
        }
    }
}

/// Cumulative ladder bookkeeping published as [`LadderState`].
#[derive(Debug, Default)]
struct LadderLog {
    /// table → (entries, bytes) evicted since the ladder last left step 0.
    evicted: HashMap<String, (u64, u64)>,
    degraded: Vec<String>,
    since_ms: Option<i64>,
    last_step: u8,
}

/// The memory governor: registered tables + degradables, the ladder machine,
/// and the per-tick [`step`](Self::step) the runner drives.
#[derive(Default)]
pub struct MemoryGovernor {
    tables: Mutex<Vec<TableHandle>>,
    degradables: Mutex<Vec<(String, DegradeFn)>>,
    machine: Mutex<LadderMachine>,
    log: Mutex<LadderLog>,
}

impl std::fmt::Debug for MemoryGovernor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let t = self.tables.lock().map(|v| v.len()).unwrap_or(0);
        let d = self.degradables.lock().map(|v| v.len()).unwrap_or(0);
        write!(f, "MemoryGovernor(tables={t}, degradables={d})")
    }
}

impl MemoryGovernor {
    /// Register a table: its occupancy joins the health doc's `tables` (the
    /// runner appends [`table_stats`](Self::table_stats) on the tick), and —
    /// when `evict` is provided — the ladder may LRU it under pressure.
    /// A table registers **here or** via
    /// [`crate::SensorHealth::register_table_stats`], never both.
    pub fn register_table(&self, handle: TableHandle) {
        self.tables
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(handle);
    }

    /// Register optional work the Degrade step may stop (`apply(true)`) and
    /// the recovery path restores (`apply(false)`).
    pub fn register_degradable(&self, name: impl Into<String>, apply: DegradeFn) {
        self.degradables
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((name.into(), apply));
    }

    /// Current occupancy of every registered table — the health tick appends
    /// this to `self_stats.tables`.
    pub fn table_stats(&self) -> Vec<TableStats> {
        let tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        tables.iter().map(|t| (t.stats)()).collect()
    }

    /// One ladder tick, driven by the runner with the tick's fresh
    /// measurement. Returns the state to publish — `None` when no ladder is
    /// armed (no budget, or RSS unmeasured), which the health doc renders as
    /// *absent*, never as "step 0".
    pub fn step(&self, stats: &SelfStats) -> Option<LadderState> {
        let (rss, budget) = (stats.rss_bytes?, stats.budget_bytes?);
        let decision = self
            .machine
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .decide(rss, budget);

        let mut round: Vec<(String, EvictOutcome)> = Vec::new();
        if let Some(target) = decision.evict_target {
            round = self.evict_round(target);
            if round.iter().any(|(_, o)| o.bytes > 0 || o.entries > 0) {
                malloc_trim();
            }
        }
        if let Some(apply) = decision.apply_degrade {
            let degradables = self.degradables.lock().unwrap_or_else(|e| e.into_inner());
            for (name, f) in degradables.iter() {
                tracing::info!(degradable = %name, apply, "memory governor: degrade transition");
                f(apply);
            }
        }

        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        if decision.step != log.last_step {
            log.since_ms = Some(chrono::Utc::now().timestamp_millis());
            if decision.step == 0 {
                // Leaving the ladder entirely: the next incident starts a
                // fresh eviction account.
                log.evicted.clear();
            }
            log.last_step = decision.step;
        }
        for (name, o) in &round {
            let e = log.evicted.entry(name.clone()).or_default();
            e.0 += o.entries;
            e.1 += o.bytes;
        }
        if let Some(apply) = decision.apply_degrade {
            log.degraded = if apply {
                let degradables = self.degradables.lock().unwrap_or_else(|e| e.into_inner());
                degradables.iter().map(|(n, _)| n.clone()).collect()
            } else {
                Vec::new()
            };
        }

        let mut evicted: Vec<LadderEviction> = log
            .evicted
            .iter()
            .map(|(table, &(entries, bytes))| LadderEviction {
                table: table.clone(),
                entries,
                bytes: (bytes > 0).then_some(bytes),
            })
            .collect();
        evicted.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.table.cmp(&b.table)));

        Some(LadderState {
            step: decision.step,
            since_ms: log.since_ms,
            reason: (decision.step > 0)
                .then(|| ladder_reason(rss, budget, &evicted, &log.degraded)),
            degraded: log.degraded.clone(),
            evicted,
        })
    }

    /// One eviction round: walk evictable tables **largest first** (by
    /// reported bytes, falling back to entries), asking each for at most the
    /// remaining shortfall.
    fn evict_round(&self, mut remaining: u64) -> Vec<(String, EvictOutcome)> {
        let tables = self.tables.lock().unwrap_or_else(|e| e.into_inner());
        // (index, size-key) for evictable tables, largest first.
        let mut order: Vec<(usize, u64, u64)> = tables
            .iter()
            .enumerate()
            .filter(|(_, t)| t.evict.is_some())
            .map(|(i, t)| {
                let s = (t.stats)();
                (i, s.bytes.unwrap_or(0), s.entries)
            })
            .collect();
        order.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));

        let mut out = Vec::new();
        for (i, bytes, _) in order {
            if remaining == 0 {
                break;
            }
            let t = &tables[i];
            let ask = if bytes > 0 {
                remaining.min(bytes)
            } else {
                remaining
            };
            let outcome = (t.evict.as_ref().expect("filtered evictable"))(ask);
            if outcome.entries > 0 || outcome.bytes > 0 {
                tracing::warn!(
                    table = %t.name,
                    entries = outcome.entries,
                    bytes = outcome.bytes,
                    "memory governor: evicted under budget pressure"
                );
                remaining = remaining.saturating_sub(outcome.bytes);
                out.push((t.name.clone(), outcome));
            }
        }
        out
    }
}

/// The ladder's human account — the sentence an operator reads before any
/// number: pressure, and what was done about it.
fn ladder_reason(rss: u64, budget: u64, evicted: &[LadderEviction], degraded: &[String]) -> String {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let pct = (rss as f64 / budget as f64 * 100.0).round();
    let mut out = format!(
        "rss {:.0} MiB at {pct:.0}% of {:.0} MiB budget",
        mib(rss),
        mib(budget)
    );
    if let Some(top) = evicted.first() {
        match top.bytes {
            Some(b) => out.push_str(&format!(
                "; evicted {} entries ({:.1} MiB) from {}",
                top.entries,
                mib(b),
                top.table
            )),
            None => out.push_str(&format!(
                "; evicted {} entries from {}",
                top.entries, top.table
            )),
        }
    }
    if !degraded.is_empty() {
        out.push_str(&format!("; degraded: {}", degraded.join(", ")));
    }
    out
}

/// One governed health-tick measurement (#812): take the self-measuring
/// snapshot, append the governor's table occupancy, run one ladder step on
/// the same measurement, stamp the ladder state, and apply the
/// [`crate::health::ladder_status`] upgrade. This is the tick body the
/// runner publishes and the done-when integration test drives directly —
/// one function so the two cannot drift.
pub fn governed_snapshot(
    health: &crate::SensorHealth,
    governor: &MemoryGovernor,
) -> crate::HealthSnapshot {
    let mut snapshot = health.snapshot_with_self();
    if let Some(stats) = snapshot.self_stats.as_mut() {
        stats.tables.extend(governor.table_stats());
        let ladder = governor.step(stats);
        stats.ladder = ladder;
    }
    let step = snapshot
        .self_stats
        .as_ref()
        .and_then(|s| s.ladder.as_ref())
        .map_or(0, |l| l.step);
    snapshot.status = crate::health::ladder_status(snapshot.status, step);
    snapshot
}

/// Return freed heap to the kernel so the ladder can observe its own
/// progress — without this, glibc keeps evicted memory in arena free lists
/// and RSS never moves. A no-op on non-glibc targets.
fn malloc_trim() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim(0) is async-signal-unsafe but thread-safe; it only
    // releases free heap back to the OS.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const MIB: u64 = 1024 * 1024;

    fn machine() -> LadderMachine {
        LadderMachine::default()
    }

    #[test]
    fn ladder_arms_at_the_warning_line_and_aims_at_the_clear_line() {
        let mut m = machine();
        let budget = 100 * MIB;
        // 79%: nominal, nothing to do.
        let d = m.decide(79 * MIB, budget);
        assert_eq!((d.step, d.evict_target), (0, None));
        // 85%: arm, and ask for exactly rss − 75% of budget.
        let d = m.decide(85 * MIB, budget);
        assert_eq!(d.step, 1);
        assert_eq!(d.evict_target, Some(10 * MIB));
        assert_eq!(d.apply_degrade, None);
    }

    #[test]
    fn stuck_eviction_escalates_to_degrade_and_saturates_at_critical() {
        let mut m = machine();
        let budget = 100 * MIB;
        // Arm.
        assert_eq!(m.decide(85 * MIB, budget).step, 1);
        // Stuck ≥ 80% for STUCK_TICKS beyond the cool-down: escalate once.
        let mut degrade_seen = 0;
        for _ in 0..(STUCK_TICKS + COOLDOWN_TICKS) {
            let d = m.decide(85 * MIB, budget);
            if d.apply_degrade == Some(true) {
                degrade_seen += 1;
                assert_eq!(d.step, 2);
            }
        }
        assert_eq!(degrade_seen, 1, "degrade fans exactly once per transition");
        // Still Critical after a full degraded tick: saturate (step 3) —
        // and never anything past 3.
        let mut max_step = 0;
        for _ in 0..10 {
            let d = m.decide(99 * MIB, budget);
            max_step = max_step.max(d.step);
            assert!(d.evict_target.is_some(), "eviction never stops while over");
        }
        assert_eq!(max_step, 3);
    }

    #[test]
    fn recovery_walks_down_with_hysteresis_and_restores_once() {
        let mut m = machine();
        let budget = 100 * MIB;
        m.decide(85 * MIB, budget); // → 1
        for _ in 0..(STUCK_TICKS + COOLDOWN_TICKS) {
            m.decide(96 * MIB, budget); // → 2
        }
        assert_eq!(m.step, 2);
        // 76% is below entry but above clear: holds (hysteresis).
        for _ in 0..10 {
            assert_eq!(m.decide(76 * MIB, budget).step, 2);
        }
        // Under 75% for RECOVER_TICKS: step down to 1 with restore fanned once.
        let mut restores = 0;
        for _ in 0..(RECOVER_TICKS + COOLDOWN_TICKS) {
            if m.decide(70 * MIB, budget).apply_degrade == Some(false) {
                restores += 1;
            }
        }
        assert_eq!((m.step, restores), (1, 1));
        // And on down to nominal.
        for _ in 0..(RECOVER_TICKS + COOLDOWN_TICKS) {
            m.decide(70 * MIB, budget);
        }
        assert_eq!(m.step, 0);
    }

    #[test]
    fn cooldown_separates_transitions() {
        let mut m = machine();
        let budget = 100 * MIB;
        // Even at instant Critical, 0→1 happens first and 1→2 must wait out
        // the cool-down: no double-jump in one or two ticks.
        assert_eq!(m.decide(99 * MIB, budget).step, 1);
        assert_eq!(m.decide(99 * MIB, budget).step, 1);
        assert_eq!(m.decide(99 * MIB, budget).step, 1);
        assert_eq!(m.decide(99 * MIB, budget).step, 2);
    }

    #[test]
    fn governor_evicts_largest_first_and_accounts_honestly() {
        let gov = MemoryGovernor::default();
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::default();

        // Two evictable tables; "big" must be asked first.
        for (name, size, freed) in [("small", 2 * MIB, MIB), ("big", 20 * MIB, 8 * MIB)] {
            let o = order.clone();
            gov.register_table(TableHandle {
                name: name.into(),
                stats: Box::new(move || TableStats {
                    name: name.into(),
                    entries: 100,
                    bytes: Some(size),
                    ..Default::default()
                }),
                evict: Some(Box::new(move |_ask| {
                    o.lock().unwrap().push(name);
                    EvictOutcome {
                        entries: 10,
                        bytes: freed,
                    }
                })),
            });
        }

        let stats = SelfStats {
            rss_bytes: Some(90 * MIB),
            budget_bytes: Some(100 * MIB),
            ..Default::default()
        };
        let state = gov.step(&stats).expect("ladder armed");
        assert_eq!(state.step, 1);
        // 90 − 75 = 15 MiB target: big (8 MiB) is asked first, small next.
        assert_eq!(*order.lock().unwrap(), vec!["big", "small"]);
        let names: Vec<&str> = state.evicted.iter().map(|e| e.table.as_str()).collect();
        assert!(names.contains(&"big") && names.contains(&"small"));
        assert!(state.reason.as_deref().unwrap_or("").contains("90 MiB"));
    }

    #[test]
    fn no_budget_means_no_ladder_not_step_zero() {
        let gov = MemoryGovernor::default();
        let stats = SelfStats {
            rss_bytes: Some(90 * MIB),
            budget_bytes: None,
            ..Default::default()
        };
        assert_eq!(gov.step(&stats), None);
    }
}
