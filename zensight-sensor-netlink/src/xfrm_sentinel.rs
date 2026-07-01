//! XFRM/IPsec lifecycle sentinel (#267, nlink roadmap R7).
//!
//! The periodic XFRM snapshot + the control-plane timeline (`events/ipsec/*`)
//! record *that* IPsec state changed, but nothing alerts on the two dangerous
//! lifecycle patterns:
//!
//! - **hard SA expiry without rekey** — a healthy tunnel soft-expires and rekeys
//!   (a fresh `NewSa` for the same src/dst/proto with a new SPI) *before* the old
//!   SA hard-expires. A hard expiry with no recent rekey means the tunnel just
//!   went dark.
//! - **repeated `Acquire`** — the kernel asks userspace (IKE) to establish an SA
//!   again and again for the same selector: the tunnel is failing to come up.
//!
//! The detection core ([`XfrmDetector`]) is pure over a small string-keyed
//! [`XfrmSignal`] (the collector maps raw `nlink` `XfrmEvent`s to it), so the
//! rules are unit-testable without a live kernel or a Zenoh session.
//! [`XfrmSentinel`] wraps it to publish `@/alerts/<alert_key>` through the same
//! [`AlertReporter`] the expectation sentinel uses.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};
use zensight_sensor_core::AlertReporter;

/// A lifecycle signal distilled from an `nlink` `XfrmEvent` by the collector.
/// Keeping the sentinel over strings decouples it from the `nlink` types and
/// makes the rules trivially testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XfrmSignal {
    /// A new SA was installed for `tunnel` (src→dst proto) — evidence of a rekey.
    NewSa { tunnel: String },
    /// An SA for `tunnel` hard-expired (the SA is dead, not merely due-for-rekey).
    HardExpire { tunnel: String },
    /// The kernel requested SA establishment for `selector` (IKE negotiation).
    Acquire { selector: String },
    /// Anything else on the XFRM stream — no sentinel interest.
    Ignore,
}

/// Thresholds for the XFRM lifecycle rules (#267).
#[derive(Debug, Clone)]
pub struct XfrmSentinelConfig {
    /// A hard expiry is "with rekey" if a `NewSa` for the same tunnel arrived
    /// within this window before it. Longer than a typical rekey margin.
    pub rekey_grace: Duration,
    /// Sliding window over which repeated `Acquire`s are counted.
    pub acquire_window: Duration,
    /// Number of `Acquire`s for one selector within the window that trips an alert.
    pub acquire_threshold: usize,
    /// `for`-duration handed to the `AlertReporter` (debounce / auto-resolve).
    pub alert_for: Duration,
}

impl Default for XfrmSentinelConfig {
    fn default() -> Self {
        Self {
            rekey_grace: Duration::from_secs(120),
            acquire_window: Duration::from_secs(60),
            acquire_threshold: 3,
            alert_for: Duration::from_secs(300),
        }
    }
}

/// A detected lifecycle anomaly (pure output of [`XfrmDetector::classify`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XfrmFinding {
    pub rule: String,
    pub severity: AlertSeverity,
    pub summary: String,
    pub labels: Vec<(String, String)>,
}

/// Pure, reporter-free detection state for the XFRM lifecycle rules.
#[derive(Debug, Clone)]
pub struct XfrmDetector {
    cfg: XfrmSentinelConfig,
    /// Last `NewSa` time per tunnel (src→dst proto) — the rekey evidence.
    last_new_sa: HashMap<String, Instant>,
    /// Recent `Acquire` times per selector (pruned to `acquire_window`).
    acquires: HashMap<String, Vec<Instant>>,
}

impl XfrmDetector {
    pub fn new(cfg: XfrmSentinelConfig) -> Self {
        Self {
            cfg,
            last_new_sa: HashMap::new(),
            acquires: HashMap::new(),
        }
    }

    /// Update state from a signal and return any findings to alert on. Pure
    /// (no I/O), so the rules can be unit-tested with a synthetic clock.
    pub fn classify(&mut self, signal: &XfrmSignal, now: Instant) -> Vec<XfrmFinding> {
        match signal {
            XfrmSignal::NewSa { tunnel } => {
                self.last_new_sa.insert(tunnel.clone(), now);
                Vec::new()
            }
            XfrmSignal::HardExpire { tunnel } => {
                let rekeyed = self
                    .last_new_sa
                    .get(tunnel)
                    .is_some_and(|t| now.duration_since(*t) <= self.cfg.rekey_grace);
                if rekeyed {
                    Vec::new()
                } else {
                    vec![XfrmFinding {
                        rule: format!("xfrm-hard-expire-no-rekey:{tunnel}"),
                        severity: AlertSeverity::Critical,
                        summary: format!("IPsec SA hard-expired without rekey: {tunnel}"),
                        labels: vec![
                            ("kind".into(), "hard-expire-no-rekey".into()),
                            ("tunnel".into(), tunnel.clone()),
                        ],
                    }]
                }
            }
            XfrmSignal::Acquire { selector } => {
                let times = self.acquires.entry(selector.clone()).or_default();
                times.retain(|t| now.duration_since(*t) <= self.cfg.acquire_window);
                times.push(now);
                let n = times.len();
                if n >= self.cfg.acquire_threshold {
                    vec![XfrmFinding {
                        rule: format!("xfrm-repeated-acquire:{selector}"),
                        severity: AlertSeverity::Warning,
                        summary: format!(
                            "Repeated IPsec ACQUIRE (tunnel not establishing): {selector} ×{n}"
                        ),
                        labels: vec![
                            ("kind".into(), "repeated-acquire".into()),
                            ("selector".into(), selector.clone()),
                            ("count".into(), n.to_string()),
                        ],
                    }]
                } else {
                    Vec::new()
                }
            }
            XfrmSignal::Ignore => Vec::new(),
        }
    }
}

/// The XFRM sentinel: a [`XfrmDetector`] plus the [`AlertReporter`] used to
/// publish its findings on `@/alerts`.
pub struct XfrmSentinel {
    host: String,
    reporter: Arc<AlertReporter>,
    detector: XfrmDetector,
    alert_for: Duration,
}

impl XfrmSentinel {
    pub fn new(
        host: impl Into<String>,
        reporter: Arc<AlertReporter>,
        cfg: XfrmSentinelConfig,
    ) -> Self {
        let alert_for = cfg.alert_for;
        Self {
            host: host.into(),
            reporter,
            detector: XfrmDetector::new(cfg),
            alert_for,
        }
    }

    /// Observe a signal and publish any resulting alerts through the reporter.
    pub async fn observe(&mut self, signal: &XfrmSignal, now: Instant) {
        for f in self.detector.classify(signal, now) {
            let mut alert = Alert::new(
                &self.host,
                Protocol::Netlink,
                AlertKind::Anomaly,
                f.rule,
                f.severity,
                f.summary,
            );
            for (k, v) in f.labels {
                alert = alert.with_label(k, v);
            }
            if let Err(e) = self.reporter.observe(alert, Some(self.alert_for)).await {
                tracing::warn!(error = %e, "xfrm sentinel alert publish failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_expire_without_rekey_alerts() {
        let mut d = XfrmDetector::new(XfrmSentinelConfig::default());
        let t0 = Instant::now();
        let f = d.classify(
            &XfrmSignal::HardExpire {
                tunnel: "10.0.0.1→10.0.0.2 esp".into(),
            },
            t0,
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, AlertSeverity::Critical);
        assert!(f[0].rule.starts_with("xfrm-hard-expire-no-rekey:"));
    }

    #[test]
    fn hard_expire_after_recent_rekey_is_silent() {
        let mut d = XfrmDetector::new(XfrmSentinelConfig::default());
        let t0 = Instant::now();
        let tunnel = "10.0.0.1→10.0.0.2 esp".to_string();
        // A fresh SA (rekey) just arrived, then the old one hard-expires.
        assert!(
            d.classify(
                &XfrmSignal::NewSa {
                    tunnel: tunnel.clone()
                },
                t0
            )
            .is_empty()
        );
        let f = d.classify(
            &XfrmSignal::HardExpire { tunnel },
            t0 + Duration::from_secs(5),
        );
        assert!(f.is_empty(), "recent rekey should suppress the alert");
    }

    #[test]
    fn stale_rekey_does_not_suppress() {
        let mut d = XfrmDetector::new(XfrmSentinelConfig::default());
        let t0 = Instant::now();
        let tunnel = "10.0.0.1→10.0.0.2 esp".to_string();
        d.classify(
            &XfrmSignal::NewSa {
                tunnel: tunnel.clone(),
            },
            t0,
        );
        // Hard expire far beyond the rekey grace → not a rekey, alert fires.
        let f = d.classify(
            &XfrmSignal::HardExpire { tunnel },
            t0 + Duration::from_secs(600),
        );
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn repeated_acquire_trips_at_threshold() {
        let cfg = XfrmSentinelConfig {
            acquire_threshold: 3,
            acquire_window: Duration::from_secs(60),
            ..Default::default()
        };
        let mut d = XfrmDetector::new(cfg);
        let t0 = Instant::now();
        let sel = XfrmSignal::Acquire {
            selector: "10.0.0.0/24→10.0.1.0/24".into(),
        };
        assert!(d.classify(&sel, t0).is_empty());
        assert!(d.classify(&sel, t0 + Duration::from_secs(1)).is_empty());
        let f = d.classify(&sel, t0 + Duration::from_secs(2));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, AlertSeverity::Warning);
    }

    #[test]
    fn acquires_outside_window_do_not_accumulate() {
        let cfg = XfrmSentinelConfig {
            acquire_threshold: 3,
            acquire_window: Duration::from_secs(60),
            ..Default::default()
        };
        let mut d = XfrmDetector::new(cfg);
        let t0 = Instant::now();
        let sel = XfrmSignal::Acquire {
            selector: "s".into(),
        };
        d.classify(&sel, t0);
        d.classify(&sel, t0 + Duration::from_secs(120)); // first drops out of window
        let f = d.classify(&sel, t0 + Duration::from_secs(121));
        assert!(f.is_empty(), "only 2 acquires inside the window");
    }
}
