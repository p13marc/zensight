//! Active load-shedding controller (netring 0.28, issue #224).
//!
//! Wraps netring's [`LoadShedder`] — the *userspace* "action half" of overload
//! handling — with the bookkeeping the sensor needs: it maps our config policy,
//! remembers which new flows were shed (so the matching `FlowEnded` is dropped
//! from telemetry, not silently double-counted), and exposes shed counters for
//! the `capture/<source>/shed/*` telemetry family.
//!
//! Driven from two handlers (see `monitor.rs`): `observe(drop_rate)` from the
//! capture-stats tick (advances the hysteresis), and `admit(flow_hash)` from
//! `FlowStarted<Tcp>` (the per-new-flow admission decision). The shedder only
//! ever sheds while the inner detector is in `Emergency`, so with
//! `shed.enabled = false` no controller is built at all and behaviour is
//! byte-for-byte unchanged.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use netring::monitor::overload::{LoadShedder, OverloadConfig, OverloadState, ShedPolicy};

use crate::config::{OverloadConfig as SensorOverloadConfig, ShedPolicyKind};

/// Cap on remembered shed-flow hashes — a backstop so a long-running overload
/// can't grow the set without bound. Shed flows are removed on their
/// `FlowEnded`; this only bounds the leak from flows whose end we never see.
const SHED_SET_CAP: usize = 65_536;

/// Couples a netring [`LoadShedder`] with the shed-flow set + policy kind.
#[derive(Debug)]
pub struct ShedController {
    shedder: LoadShedder,
    policy: ShedPolicyKind,
    shed_flows: HashSet<u64>,
}

impl ShedController {
    /// Build a controller from the sensor's overload config. Reuses the same
    /// hysteresis thresholds as the plain detector so the Emergency↔Normal
    /// transition is identical; only the *action* (shedding) is added.
    pub fn new(cfg: &SensorOverloadConfig) -> Self {
        let policy = cfg.shed.policy;
        let shed_policy = match policy {
            ShedPolicyKind::NewFlows => ShedPolicy::ShedNewFlows,
            ShedPolicyKind::Sample => ShedPolicy::SampleFlows {
                keep: cfg.shed.sample_rate.clamp(0.0, 1.0),
            },
        };
        let detect = OverloadConfig::default()
            .enter_at(cfg.enter_drop_rate)
            .recover_at(cfg.recover_drop_rate, cfg.recover_windows);
        Self {
            shedder: LoadShedder::new(detect, shed_policy),
            policy,
            shed_flows: HashSet::new(),
        }
    }

    /// Direction-invariant hash of a flow key — netring's `FlowKey` is
    /// address-canonical, so both legs of a biflow hash identically and share
    /// one admission verdict (the sampling guarantee).
    pub fn flow_hash<K: Hash>(key: &K) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut h);
        h.finish()
    }

    /// Advance the hysteresis with one windowed drop-rate sample. Returns
    /// `Some(state)` on an Emergency↔Normal transition (same signal as the bare
    /// [`netring::monitor::overload::OverloadDetector`]).
    pub fn observe(&mut self, drop_rate: f64) -> Option<OverloadState> {
        self.shedder.observe(drop_rate)
    }

    /// Admission decision for a new flow. Returns `true` to keep, `false` to
    /// shed; shed flows are remembered so [`take_shed`](Self::take_shed) can
    /// drop their `FlowEnded`. Counters update either way (honest accounting).
    pub fn admit(&mut self, flow_hash: u64) -> bool {
        let admitted = self.shedder.admit_new_flow(flow_hash).is_admitted();
        if !admitted && self.shed_flows.len() < SHED_SET_CAP {
            self.shed_flows.insert(flow_hash);
        }
        admitted
    }

    /// Was this flow shed at start? Consumes the record (one `FlowEnded` per
    /// flow), so the set stays bounded under normal flow lifecycles.
    pub fn take_shed(&mut self, flow_hash: u64) -> bool {
        self.shed_flows.remove(&flow_hash)
    }

    /// Actively shedding right now — Emergency state with a non-`Observe` policy.
    pub fn is_shedding(&self) -> bool {
        self.shedder.is_shedding()
    }

    /// Cumulative count of new flows deliberately shed.
    pub fn shed_total(&self) -> u64 {
        self.shedder.stats().shed
    }

    /// Which policy is configured (selects the telemetry counter leaf).
    pub fn policy(&self) -> ShedPolicyKind {
        self.policy
    }

    /// Stable label for the active policy, for alert annotation.
    pub fn policy_label(&self) -> &'static str {
        match self.policy {
            ShedPolicyKind::NewFlows => "new_flows",
            ShedPolicyKind::Sample => "sample",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OverloadConfig as Cfg, ShedConfig};

    fn cfg(enabled: bool, policy: ShedPolicyKind, sample_rate: f64) -> Cfg {
        Cfg {
            enabled: true,
            // Force an aggressive entry so a single high sample trips Emergency,
            // and require many calm windows so it stays tripped for the test.
            enter_drop_rate: 0.05,
            recover_drop_rate: 0.01,
            recover_windows: 3,
            shed: ShedConfig {
                enabled,
                policy,
                sample_rate,
            },
        }
    }

    #[test]
    fn no_shed_until_emergency() {
        let mut c = ShedController::new(&cfg(true, ShedPolicyKind::NewFlows, 0.5));
        // Calm: every new flow is admitted, nothing shed.
        c.observe(0.0);
        assert!(c.admit(1));
        assert!(c.admit(2));
        assert!(!c.is_shedding());
        assert_eq!(c.shed_total(), 0);
    }

    #[test]
    fn new_flows_policy_sheds_all_while_overloaded() {
        let mut c = ShedController::new(&cfg(true, ShedPolicyKind::NewFlows, 0.5));
        c.observe(0.5); // > enter_drop_rate → Emergency
        assert!(c.is_shedding());
        // Every new flow shed; remembered for the matching FlowEnded.
        assert!(!c.admit(10));
        assert!(!c.admit(11));
        assert_eq!(c.shed_total(), 2);
        assert!(c.take_shed(10));
        assert!(!c.take_shed(10)); // consumed
        assert!(c.take_shed(11));
    }

    #[test]
    fn sample_policy_is_deterministic_in_hash() {
        let mut c = ShedController::new(&cfg(true, ShedPolicyKind::Sample, 0.5));
        c.observe(0.5);
        assert!(c.is_shedding());
        // Deterministic in the hash: the same hash always gets the same verdict.
        let first = c.admit(0xDEAD_BEEF);
        let mut c2 = ShedController::new(&cfg(true, ShedPolicyKind::Sample, 0.5));
        c2.observe(0.5);
        assert_eq!(c2.admit(0xDEAD_BEEF), first);
        // keep=0.0 sheds everything; keep=1.0 keeps everything.
        let mut none = ShedController::new(&cfg(true, ShedPolicyKind::Sample, 0.0));
        none.observe(0.5);
        assert!(!none.admit(123));
        let mut all = ShedController::new(&cfg(true, ShedPolicyKind::Sample, 1.0));
        all.observe(0.5);
        assert!(all.admit(123));
    }
}
