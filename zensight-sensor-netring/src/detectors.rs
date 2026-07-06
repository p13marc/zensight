//! Runtime-tunable NDR detector registry (#369).
//!
//! netring 0.29 / flowscope 0.22 ship a `DetectorRegistry<FlowKey>` of stock
//! detectors (beacon / RITA / port-scan / connection-flood / data-exfil / DGA /
//! DNS-tunnel / newly-observed-domain). The Monitor drives them from its own
//! tracked flow + DNS stream and publishes each emitted `OwnedAnomaly` through
//! the sink chain — so ZenSight's existing `ChannelSink → to_view →
//! map::anomaly_alert` drain handles every detector uniformly.
//!
//! The catch: stock detectors bake their thresholds in at construction and can't
//! be muted or retuned. ZenSight's detection-tuning GUI (#121 / #328) hot-swaps
//! thresholds, mutes detectors and edits allowlists at runtime through a
//! [`LiveConfig`] (`Arc<ArcSwap<AnomalyConfig>>`). To keep that contract we wrap
//! every stock detector in [`Tuned`], which
//!
//! 1. delegates all [`Detector`] hooks to the inner stock detector;
//! 2. post-filters every emitted anomaly against the current `LiveConfig`
//!    snapshot — a muted detector drops its anomaly, a runtime threshold above
//!    the anomaly's score drops it, an allowlisted dst/src/domain drops it;
//! 3. re-keys flow-driven anomalies with the triggering flow's full 5-tuple so
//!    `src:port` / `proto` / Community-ID survive on the alert (the `HostPair` /
//!    `SrcHost` the detector state is keyed on has no source port).
//!
//! **Stock gates are constructed at the LOWEST sensible floor** (see the
//! `*_STOCK_*` constants), *below* the runtime defaults, so a runtime command
//! that *lowers* a threshold still lets sub-default-score events through — the
//! stock detector must emit them for `Tuned` to have anything to keep. The floor
//! is not literally zero: detectors with a per-source cooldown (exfil, flood,
//! dns-tunnel) keep a sane floor so benign noise can't trip the cooldown and mask
//! a real event before `Tuned` ever sees it.

use arc_swap::ArcSwap;
use std::sync::Arc;

use flowscope::detect::patterns::{
    BeaconDetector, ConnectionFloodDetector, DataExfilDetector, DnsTunnelDetector,
    NewlyObservedDomainDetector, PortScanDetector, RitaBeaconDetector,
};
use flowscope::detect::{DetectorRegistry, DgaDetector, HostPair, SrcHost};
use flowscope::event::FlowStats;
use flowscope::{Detector, DetectorKind, HistoryString, L4Proto, OwnedAnomaly, Timestamp};
use netring::prelude::FlowKey;

use crate::config::{AnomalyConfig, NetringConfig};

/// Lock-free live view of the tunable [`AnomalyConfig`] (#121).
pub type LiveConfig = Arc<ArcSwap<AnomalyConfig>>;

// ─── Stock-detector construction floors (see module docs) ───────────────────
/// CV/RITA beacon score floor — well below the 0.8 / 0.9 runtime defaults so a
/// runtime-lowered threshold still sees candidates. Beacons key per `HostPair`,
/// so a benign pair's low score can't mask a C2 pair (independent cooldowns).
const BEACON_STOCK_THRESHOLD: f64 = 0.3;
/// Connection-flood count floor (runtime default 100). Per-source 60 s cooldown,
/// so the floor stays sane rather than 1.
const FLOOD_STOCK_THRESHOLD: u64 = 20;
/// Data-exfil sigma floor (runtime default 4.0) and byte floor (runtime default
/// 10 MiB). Per-source 300 s cooldown → keep sane floors so a modest above-mean
/// upload can't trip the cooldown and mask a later real exfil.
const EXFIL_STOCK_SIGMA: f64 = 2.0;
const EXFIL_STOCK_MIN_BYTES: u64 = 1024 * 1024;
/// DNS-tunnel floors (runtime defaults distinct 50 / qname-len 100). Per-source
/// cooldown → keep sane floors.
const DNS_TUNNEL_STOCK_DISTINCT: usize = 5;
const DNS_TUNNEL_STOCK_QNAME_LEN: usize = 30;
/// DGA log-likelihood floor (runtime default -8.0; fires when score < threshold).
/// A higher (less-negative) stock threshold emits MORE candidates for `Tuned` to
/// filter. Per-domain LRU suppression → no cross-masking. -2.0 still excludes
/// clearly english-like names we'd never alert on.
const DGA_STOCK_THRESHOLD: f32 = -2.0;

/// A stock [`Detector`] wrapped with ZenSight runtime tunability (#369).
///
/// See the module docs. Generic over the inner detector `D`; the key type is
/// fixed to netring's [`FlowKey`] (`FiveTupleKey`).
pub struct Tuned<D> {
    inner: D,
    live: LiveConfig,
    /// Reused emission buffer — the inner detector writes here, then we filter
    /// into the registry's `out`.
    scratch: Vec<OwnedAnomaly>,
}

impl<D> Tuned<D> {
    pub fn new(inner: D, live: LiveConfig) -> Self {
        Self {
            inner,
            live,
            scratch: Vec::new(),
        }
    }

    /// Drain the scratch buffer through the runtime filter into `out`. When
    /// `flow_key` is `Some`, kept anomalies are re-keyed with the full 5-tuple
    /// (flow-driven hooks); DNS-driven hooks pass `None` to preserve the stock
    /// source-only key.
    fn drain_filtered(&mut self, flow_key: Option<&FlowKey>, out: &mut Vec<OwnedAnomaly>) {
        if self.scratch.is_empty() {
            return;
        }
        let cfg = self.live.load();
        for anomaly in self.scratch.drain(..) {
            if keep(anomaly.kind, &anomaly, &cfg) {
                match flow_key {
                    Some(k) => out.push(anomaly.with_key(k)),
                    None => out.push(anomaly),
                }
            }
        }
    }
}

impl<D: Detector<FlowKey>> Detector<FlowKey> for Tuned<D> {
    fn kind(&self) -> DetectorKind {
        self.inner.kind()
    }

    fn on_flow_start(&mut self, key: &FlowKey, ts: Timestamp, out: &mut Vec<OwnedAnomaly>) {
        self.inner.on_flow_start(key, ts, &mut self.scratch);
        self.drain_filtered(Some(key), out);
    }

    fn on_flow_established(&mut self, key: &FlowKey, ts: Timestamp, out: &mut Vec<OwnedAnomaly>) {
        self.inner.on_flow_established(key, ts, &mut self.scratch);
        self.drain_filtered(Some(key), out);
    }

    fn on_flow_end(
        &mut self,
        key: &FlowKey,
        stats: &FlowStats,
        history: &HistoryString,
        l4: Option<L4Proto>,
        ts: Timestamp,
        out: &mut Vec<OwnedAnomaly>,
    ) {
        self.inner
            .on_flow_end(key, stats, history, l4, ts, &mut self.scratch);
        self.drain_filtered(Some(key), out);
    }

    fn on_flow_tick(
        &mut self,
        key: &FlowKey,
        stats: &FlowStats,
        ts: Timestamp,
        out: &mut Vec<OwnedAnomaly>,
    ) {
        self.inner.on_flow_tick(key, stats, ts, &mut self.scratch);
        self.drain_filtered(Some(key), out);
    }

    fn on_dns_query(
        &mut self,
        key: &FlowKey,
        qname: &str,
        ts: Timestamp,
        out: &mut Vec<OwnedAnomaly>,
    ) {
        self.inner.on_dns_query(key, qname, ts, &mut self.scratch);
        // DNS-driven anomalies keep their stock source-only key so alert
        // bucketing stays `(rule, src)` (no resolver dst fragmenting the key).
        self.drain_filtered(None, out);
    }

    fn tracked(&self) -> usize {
        self.inner.tracked()
    }

    fn evict_expired(&mut self, now: Timestamp) {
        self.inner.evict_expired(now);
    }
}

/// Read a metric off an anomaly by label.
fn metric(a: &OwnedAnomaly, label: &str) -> Option<f64> {
    a.metrics
        .iter()
        .find_map(|(k, v)| (*k == label).then_some(*v))
}

/// Read an observation off an anomaly by label.
fn observation<'a>(a: &'a OwnedAnomaly, label: &str) -> Option<&'a str> {
    a.observations
        .iter()
        .find_map(|(k, v)| (*k == label).then_some(v.as_ref()))
}

/// `true` if `host` matches any allowlist entry (case-insensitive substring).
fn allowlisted(host: &str, allowlist: &[String]) -> bool {
    let h = host.to_ascii_lowercase();
    allowlist
        .iter()
        .any(|a| !a.is_empty() && h.contains(&a.to_ascii_lowercase()))
}

fn dst_allowlisted(a: &OwnedAnomaly, allowlist: &[String]) -> bool {
    a.dest_ip
        .is_some_and(|ip| allowlisted(&ip.to_string(), allowlist))
}

fn src_allowlisted(a: &OwnedAnomaly, allowlist: &[String]) -> bool {
    a.src_ip
        .is_some_and(|ip| allowlisted(&ip.to_string(), allowlist))
}

fn domain_allowlisted(a: &OwnedAnomaly, allowlist: &[String]) -> bool {
    observation(a, "domain").is_some_and(|d| allowlisted(d, allowlist))
}

/// Exfil z-score reconstructed from the anomaly's `bytes`/`mean`/`sigma` metrics
/// (a flat baseline is floored at 1 byte, mirroring the stock detector).
fn exfil_zscore(a: &OwnedAnomaly) -> Option<f64> {
    let bytes = metric(a, "bytes")?;
    let mean = metric(a, "mean")?;
    let sigma = metric(a, "sigma")?;
    Some((bytes - mean) / sigma.max(1.0))
}

/// Decide whether a stock anomaly survives the current runtime config: mute →
/// drop; runtime threshold above the anomaly's score → drop; allowlisted target
/// → drop. Unknown kinds are always kept (threat-intel / lateral etc. don't flow
/// through this registry, but be permissive).
fn keep(kind: DetectorKind, a: &OwnedAnomaly, c: &AnomalyConfig) -> bool {
    use DetectorKind::*;
    match kind {
        BeaconCv => {
            c.beaconing
                && metric(a, "score").is_some_and(|s| s >= c.beacon_threshold)
                && !dst_allowlisted(a, &c.allowlist)
        }
        BeaconRita => {
            c.rita_beacon
                && metric(a, "score").is_some_and(|s| s >= c.rita_beacon_threshold)
                && !dst_allowlisted(a, &c.allowlist)
        }
        PortScanTrw => c.port_scan,
        ConnectionFlood => {
            c.connection_flood && metric(a, "count").is_some_and(|n| n >= c.flood_threshold as f64)
        }
        DataExfil => {
            c.data_exfil
                && exfil_zscore(a).is_some_and(|z| z >= c.exfil_sigma)
                && metric(a, "bytes").is_some_and(|b| b >= c.exfil_min_bytes as f64)
                && !src_allowlisted(a, &c.allowlist)
        }
        Dga => {
            c.dga
                && metric(a, "log_likelihood").is_some_and(|ll| ll < c.dga_threshold)
                && !domain_allowlisted(a, &c.allowlist)
        }
        DnsTunnel => {
            c.dns_tunnel
                && (metric(a, "distinct_subdomains")
                    .is_some_and(|d| d >= c.dns_tunnel_distinct as f64)
                    || metric(a, "qname_len").is_some_and(|l| l >= c.dns_tunnel_qname_len as f64))
                && !domain_allowlisted(a, &c.allowlist)
        }
        NewlyObservedDomain => c.nod && !domain_allowlisted(a, &c.allowlist),
        _ => true,
    }
}

/// Build the flow + DNS driven detector registry from config, wrapping every
/// stock detector in [`Tuned`]. Only detectors enabled at build time are
/// registered (a runtime *mute* can still disable a registered detector, and
/// re-enable it within the session — matching the pre-#369 behaviour). DNS-driven
/// detectors additionally require `collect.dns` (no qname without DNS parsing).
///
/// Returns `None` when nothing is enabled, so the caller can skip
/// `MonitorBuilder::detectors` entirely.
pub fn build_registry(cfg: &NetringConfig, live: &LiveConfig) -> Option<DetectorRegistry<FlowKey>> {
    let a = &cfg.anomalies;
    let dns = cfg.collect.dns;
    let mut reg = DetectorRegistry::new();
    let mut any = false;

    if a.port_scan {
        // TRW: state keyed on the scanner IP (SrcHost), verdict-gated — no score
        // floor to lower, so stock defaults stand; `Tuned` applies mute.
        reg.register(Tuned::new(PortScanDetector::<SrcHost>::new(), live.clone()));
        any = true;
    }
    if a.beaconing {
        let d = BeaconDetector::<HostPair>::new().with_anomaly_threshold(BEACON_STOCK_THRESHOLD);
        reg.register(Tuned::new(d, live.clone()));
        any = true;
    }
    if a.rita_beacon {
        let d =
            RitaBeaconDetector::<HostPair>::new().with_anomaly_threshold(BEACON_STOCK_THRESHOLD);
        reg.register(Tuned::new(d, live.clone()));
        any = true;
    }
    if a.connection_flood {
        let d = ConnectionFloodDetector::with_params(
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(1),
            FLOOD_STOCK_THRESHOLD,
        );
        reg.register(Tuned::new(d, live.clone()));
        any = true;
    }
    if a.data_exfil {
        let d = DataExfilDetector::new()
            .with_n_sigma(EXFIL_STOCK_SIGMA)
            .with_min_bytes(EXFIL_STOCK_MIN_BYTES);
        reg.register(Tuned::new(d, live.clone()));
        any = true;
    }
    if a.dga && dns {
        let d = DgaDetector::new().with_threshold(DGA_STOCK_THRESHOLD);
        reg.register(Tuned::new(d, live.clone()));
        any = true;
    }
    if a.dns_tunnel && dns {
        let d = DnsTunnelDetector::new()
            .with_subdomain_threshold(DNS_TUNNEL_STOCK_DISTINCT)
            .with_min_qname_len(DNS_TUNNEL_STOCK_QNAME_LEN);
        reg.register(Tuned::new(d, live.clone()));
        any = true;
    }
    if a.nod && dns {
        reg.register(Tuned::new(NewlyObservedDomainDetector::new(), live.clone()));
        any = true;
    }

    any.then_some(reg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowscope::event::Severity;
    use std::net::{IpAddr, Ipv4Addr};

    fn live(cfg: AnomalyConfig) -> LiveConfig {
        Arc::new(ArcSwap::from_pointee(cfg))
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    fn pair() -> HostPair {
        HostPair {
            src: ip(1),
            dst: ip(2),
            dst_port: Some(443),
        }
    }

    /// A stock detector stub that emits one fixed anomaly on every flow end, so
    /// we can exercise `Tuned`'s post-filter deterministically.
    struct Emitter(OwnedAnomaly);
    impl Detector<FlowKey> for Emitter {
        fn kind(&self) -> DetectorKind {
            self.0.kind
        }
        fn on_flow_end(
            &mut self,
            _key: &FlowKey,
            _stats: &FlowStats,
            _history: &HistoryString,
            _l4: Option<L4Proto>,
            _ts: Timestamp,
            out: &mut Vec<OwnedAnomaly>,
        ) {
            out.push(self.0.clone());
        }
    }

    fn beacon_anomaly(score: f64) -> OwnedAnomaly {
        OwnedAnomaly::new(
            DetectorKind::BeaconCv,
            Severity::Warning,
            Timestamp::new(0, 0),
        )
        .with_key(&pair())
        .with_metric("score", score)
    }

    fn flowkey() -> FlowKey {
        use std::net::SocketAddr;
        FlowKey::new(
            L4Proto::Tcp,
            SocketAddr::new(ip(1), 33333),
            SocketAddr::new(ip(2), 443),
        )
    }

    fn drive(det: &mut dyn Detector<FlowKey>) -> Vec<OwnedAnomaly> {
        let key = flowkey();
        let stats = FlowStats::default();
        let hist = HistoryString::default();
        let mut out = Vec::new();
        det.on_flow_end(
            &key,
            &stats,
            &hist,
            Some(L4Proto::Tcp),
            Timestamp::new(0, 0),
            &mut out,
        );
        out
    }

    #[test]
    fn muted_detector_drops_anomaly() {
        let cfg = AnomalyConfig {
            beaconing: false, // muted at runtime
            beacon_threshold: 0.8,
            ..Default::default()
        };
        let mut t = Tuned::new(Emitter(beacon_anomaly(0.95)), live(cfg));
        assert!(
            drive(&mut t).is_empty(),
            "a muted detector must drop its anomaly"
        );
    }

    #[test]
    fn lowered_runtime_threshold_passes_sub_default_score() {
        // Stock floor is 0.3; runtime default would be 0.8. A 0.5-score beacon is
        // below the stock default but a runtime threshold lowered to 0.4 must let
        // it through — proving runtime-lowering works.
        let cfg = AnomalyConfig {
            beaconing: true,
            beacon_threshold: 0.4,
            ..Default::default()
        };
        let mut t = Tuned::new(Emitter(beacon_anomaly(0.5)), live(cfg));
        let out = drive(&mut t);
        assert_eq!(out.len(), 1, "sub-default score must pass a lowered gate");
        // Re-keyed with the full 5-tuple (source port preserved).
        assert_eq!(out[0].src_port, Some(33333));
        assert_eq!(out[0].dest_port, Some(443));
    }

    #[test]
    fn runtime_threshold_above_score_drops() {
        let cfg = AnomalyConfig {
            beaconing: true,
            beacon_threshold: 0.9, // stricter than the 0.5 score
            ..Default::default()
        };
        let mut t = Tuned::new(Emitter(beacon_anomaly(0.5)), live(cfg));
        assert!(drive(&mut t).is_empty());
    }

    #[test]
    fn allowlisted_dst_drops() {
        let cfg = AnomalyConfig {
            beaconing: true,
            beacon_threshold: 0.4,
            allowlist: vec!["10.0.0.2".into()],
            ..Default::default()
        };
        let mut t = Tuned::new(Emitter(beacon_anomaly(0.95)), live(cfg));
        assert!(drive(&mut t).is_empty(), "allowlisted dst must drop");
    }
}
