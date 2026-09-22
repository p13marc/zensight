//! Identity evidence for the cameras this sensor polls or discovers (#413).
//!
//! A camera is a host on the network. Without a claim about it the correlator
//! has nothing to file its streams under, so they hang off the sensor's own
//! host card as if the sensor were the camera. This module publishes
//! **third-party** claims (`observer: Some("parallax")`) on
//! `state/parallax/evidence/device/<stream>` for two kinds of camera:
//!
//! - a **configured RTSP target** — the claim carries what the URL says: an IP
//!   or a name, never the credentials. Its key chunk is the catalogue stream
//!   name, the operator's own stable identifier for it;
//! - a **discovered responder** (mDNS `_rtsp._tcp`, WS-Discovery) when
//!   `include_discovered` is on — the address, the advertised name, and the
//!   ONVIF `hardware` scope as a display-only `vendor`. Its key chunk is the
//!   same slug the discovery proposal suggests as a stream name, so a camera
//!   the operator later adopts under that name keeps its key.
//!
//! **What these claims can and cannot merge on.** Neither probe yields a MAC
//! or a serial: WS-Discovery scopes carry a model and a name, mDNS a name and
//! an address. So a claim here sits on the `ip`, `fqdn` and `hostname` rungs
//! of `zensight-common/docs/identity-evidence.md` and no higher — it will
//! attach to a host that netring or netlink saw with that IP, and it adds no
//! new correlator rule. `host_id` is always `None`: a synthetic id would
//! masquerade as the hashed-machine-id contract (the snmp/netlink precedent).
//! Local V4L2 devices get no claim at all — they *are* this host, which
//! `evidence/self` already covers.
//!
//! Publishing is change-driven with a periodic liveness refresh (the netring
//! shape): a claim goes out when it is new or its identifying fields changed,
//! and again every `refresh_secs` so the consumer's TTL does not expire it.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use zensight_common::registry::parallax::Subject;
use zensight_common::v1::V1ContextExt;
use zensight_common::{DiscoveredStream, HostEvidence, QosClass};
use zensight_sensor_core::AdvancedPublisherRegistry;
use zensight_sensor_core::v1::V1Context;

use crate::catalog::{Catalog, SourceKind};

/// The `parallax.evidence` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceConfig {
    /// Publish observed-camera claims at all (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Liveness refresh: an unchanged claim is republished this often so the
    /// consumer's TTL (900 s) never expires a camera that is still configured.
    #[serde(default = "default_refresh_secs")]
    pub refresh_secs: u64,
    /// Also claim discovered responders (default: true). Only meaningful with
    /// a `discovery` block; a configured target is always claimed.
    #[serde(default = "default_true")]
    pub include_discovered: bool,
}

fn default_true() -> bool {
    true
}

fn default_refresh_secs() -> u64 {
    300
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_secs: default_refresh_secs(),
            include_discovered: true,
        }
    }
}

impl EvidenceConfig {
    /// A zero refresh would let every claim expire on the consumer between
    /// publishes and read as a camera that came and went.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.enabled && self.refresh_secs == 0 {
            anyhow::bail!("evidence.refresh_secs must be > 0");
        }
        Ok(())
    }
}

/// The host part of an `authority` (`host`, `host:port`, `[v6]:port`, with
/// any `user:pass@` in front). Credentials never leave this function.
fn host_of(authority: &str) -> Option<String> {
    let after_creds = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(rest) = after_creds.strip_prefix('[') {
        rest.split_once(']').map(|(v6, _)| v6)?
    } else {
        after_creds
            .rsplit_once(':')
            .map_or(after_creds, |(h, port)| {
                // `a:b:c` without brackets is a bare v6 literal, not host:port.
                if port.chars().all(|c| c.is_ascii_digit()) {
                    h
                } else {
                    after_creds
                }
            })
    };
    let host = host.trim().trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_string())
}

/// Loopback and the unspecified address identify nothing (snmp's `push_ip`).
fn is_identifying_ip(ip: &IpAddr) -> bool {
    !(ip.is_loopback() || ip.is_unspecified())
}

/// Which rung a host string lands on: an IP, a bare or `.local` name, or a
/// fully-qualified name.
fn place_host(host: &str, ev: &mut HostEvidence) -> bool {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_identifying_ip(&ip) {
            return false;
        }
        ev.ips.push(ip.to_string());
        return true;
    }
    if host.contains('.') && !host.ends_with(".local") {
        ev.fqdn = Some(host.to_string());
    } else {
        ev.hostname = Some(host.to_string());
    }
    true
}

fn blank(source: &str, now_ms: i64) -> HostEvidence {
    HostEvidence {
        sensor: "parallax".to_string(),
        source: source.to_string(),
        observer: Some("parallax".to_string()),
        host_id: None,
        boot_id: None,
        hostname: None,
        fqdn: None,
        ips: Vec::new(),
        macs: Vec::new(),
        vendor: None,
        platform: None,
        container_id: None,
        cloud: None,
        last_updated: now_ms,
    }
}

/// The claim for a configured RTSP target: what its URL says about the host,
/// keyed by the catalogue stream name. `None` when the URL names nothing a
/// claim could carry (no authority, or only loopback).
pub fn rtsp_evidence(stream: &str, url: &str, now_ms: i64) -> Option<HostEvidence> {
    let authority = crate::discovery::authority_of(url)?;
    let host = host_of(&authority)?;
    let mut ev = blank(stream, now_ms);
    if !place_host(&host, &mut ev) {
        return None;
    }
    ev.platform = Some("rtsp".to_string());
    Some(ev)
}

/// The claim for a discovered responder, keyed by the slug the proposal
/// suggests as its stream name. `None` when the address names nothing.
pub fn discovered_evidence(d: &DiscoveredStream, now_ms: i64) -> Option<HostEvidence> {
    let host = host_of(&d.address)?;
    let name = d.name.as_deref().unwrap_or("");
    let slug = crate::discovery::stream_name(name, &d.address);
    let mut ev = blank(&slug, now_ms);
    if !place_host(&host, &mut ev) {
        return None;
    }
    if !name.is_empty() && ev.hostname.is_none() {
        ev.hostname = Some(name.to_string());
    }
    // ONVIF's `hardware` scope is a model string — descriptive only. It is
    // deliberately `vendor`, which no merge rule reads.
    ev.vendor = d.attributes.get("hardware").cloned();
    ev.platform = Some(d.via.clone());
    Some(ev)
}

/// Content hash of the identifying fields (everything but `last_updated`),
/// with the address lists sorted so a reordering is not a change.
pub fn content_hash(ev: &HostEvidence) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ev.sensor.hash(&mut h);
    ev.source.hash(&mut h);
    ev.observer.hash(&mut h);
    ev.hostname.hash(&mut h);
    ev.fqdn.hash(&mut h);
    let mut ips = ev.ips.clone();
    ips.sort();
    ips.hash(&mut h);
    let mut macs = ev.macs.clone();
    macs.sort();
    macs.hash(&mut h);
    ev.vendor.hash(&mut h);
    ev.platform.hash(&mut h);
    h.finish()
}

/// Per-source publish bookkeeping: what was last said about each camera and
/// when, so the loop republishes on change or refresh and never per tick.
#[derive(Debug, Default)]
pub struct Ledger {
    last: HashMap<String, (u64, i64)>,
}

impl Ledger {
    /// Whether `ev` should go out now; records it if so.
    pub fn should_publish(&mut self, ev: &HostEvidence, now_ms: i64, refresh_ms: i64) -> bool {
        let hash = content_hash(ev);
        let publish = match self.last.get(&ev.source) {
            Some((h, at)) if *h == hash => now_ms.saturating_sub(*at) >= refresh_ms,
            _ => true,
        };
        if publish {
            self.last.insert(ev.source.clone(), (hash, now_ms));
        }
        publish
    }

    /// Cameras that were claimed once and are no longer configured or seen
    /// are simply not refreshed: the consumer's TTL retires them. Keeping the
    /// entry bounded to what is currently claimable is what keeps this map
    /// from growing with every responder a hostile network ever answered as.
    pub fn retain(&mut self, live: &BTreeMap<String, HostEvidence>) {
        self.last.retain(|k, _| live.contains_key(k));
    }
}

/// The claims this round: every configured RTSP target, plus every
/// discovered responder when that is enabled. Keyed by source; a discovered
/// responder whose slug collides with a configured stream loses — the
/// operator's entry is the authoritative one.
pub fn claims(
    catalog: &Catalog,
    discovered: &[DiscoveredStream],
    include_discovered: bool,
    now_ms: i64,
) -> BTreeMap<String, HostEvidence> {
    let mut out = BTreeMap::new();
    if include_discovered {
        for d in discovered {
            if let Some(ev) = discovered_evidence(d, now_ms) {
                out.insert(ev.source.clone(), ev);
            }
        }
    }
    for entry in catalog.entries() {
        if let SourceKind::Rtsp { url, .. } = &entry.kind
            && let Some(ev) = rtsp_evidence(&entry.name, url, now_ms)
        {
            out.insert(ev.source.clone(), ev);
        }
    }
    out
}

/// The evidence loop: wakes on a discovery round or every half refresh,
/// publishes what is new, changed or due, and never anything else.
pub async fn run(
    registry: AdvancedPublisherRegistry,
    v1: V1Context,
    catalog: Arc<Catalog>,
    mut discovered: watch::Receiver<Vec<DiscoveredStream>>,
    cfg: EvidenceConfig,
) {
    let refresh_ms = (cfg.refresh_secs as i64).saturating_mul(1000);
    let tick = Duration::from_secs((cfg.refresh_secs / 2).max(5));
    let mut ledger = Ledger::default();
    loop {
        let now_ms = zensight_common::current_timestamp_millis();
        let found = discovered.borrow().clone();
        let live = claims(&catalog, &found, cfg.include_discovered, now_ms);
        ledger.retain(&live);
        for ev in live.values() {
            if !ledger.should_publish(ev, now_ms, refresh_ms) {
                continue;
            }
            let key: String = v1.subject_key(&Subject::evidence_device(&ev.source)).into();
            if let Err(e) = registry.publish_serializable(&key, ev).await {
                tracing::warn!(camera = %ev.source, error = %e, "camera evidence publish failed");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(tick) => {}
            changed = discovered.changed() => {
                if changed.is_err() {
                    // The discovery loop is gone; configured targets still
                    // need their refresh, so keep ticking on the timer alone.
                    tokio::time::sleep(tick).await;
                }
            }
        }
    }
}

/// The registry the loop publishes through: cache-only advanced publishers
/// with the evidence QoS, exactly as snmp builds its own.
pub fn registry(
    session: Arc<zenoh::Session>,
    counters: Arc<zensight_common::PublishCounters>,
) -> AdvancedPublisherRegistry {
    AdvancedPublisherRegistry::new(
        session,
        zensight_sensor_core::v1::for_producer("parallax").telemetry_prefix(),
        zensight_common::Format::Json,
        zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
        counters,
    )
    .with_qos(QosClass::Evidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn discovered(via: &str, address: &str, name: Option<&str>) -> DiscoveredStream {
        DiscoveredStream {
            via: via.to_string(),
            address: address.to_string(),
            name: name.map(str::to_string),
            url: None,
            attributes: BTreeMap::new(),
            suggested: String::new(),
        }
    }

    #[test]
    fn rtsp_ip_url_yields_an_ip_claim_and_leaks_no_credentials() {
        let ev = rtsp_evidence("door", "rtsp://viewer:s3cret@10.0.0.7:554/stream1", NOW).unwrap();
        assert_eq!(ev.source, "door");
        assert_eq!(ev.ips, vec!["10.0.0.7".to_string()]);
        assert_eq!(ev.observer.as_deref(), Some("parallax"));
        assert_eq!(ev.sensor, "parallax");
        assert!(ev.host_id.is_none());
        assert!(ev.macs.is_empty());
        assert_eq!(ev.platform.as_deref(), Some("rtsp"));
        let wire = serde_json::to_string(&ev).unwrap();
        assert!(
            !wire.contains("s3cret") && !wire.contains("viewer"),
            "{wire}"
        );
    }

    #[test]
    fn rtsp_name_url_yields_hostname_or_fqdn() {
        let local = rtsp_evidence("a", "rtsp://cam.local:554/s", NOW).unwrap();
        assert_eq!(local.hostname.as_deref(), Some("cam.local"));
        assert!(local.fqdn.is_none());
        let bare = rtsp_evidence("b", "rtsp://cam2/s", NOW).unwrap();
        assert_eq!(bare.hostname.as_deref(), Some("cam2"));
        let fq = rtsp_evidence("c", "rtsp://cam.example.net:8554/s", NOW).unwrap();
        assert_eq!(fq.fqdn.as_deref(), Some("cam.example.net"));
        assert!(fq.hostname.is_none());
        let v6 = rtsp_evidence("d", "rtsp://[fe80::1]:554/s", NOW).unwrap();
        assert_eq!(v6.ips, vec!["fe80::1".to_string()]);
    }

    #[test]
    fn loopback_and_junk_targets_yield_nothing() {
        assert!(rtsp_evidence("x", "rtsp://127.0.0.1:554/s", NOW).is_none());
        assert!(rtsp_evidence("x", "rtsp://[::1]/s", NOW).is_none());
        assert!(rtsp_evidence("x", "rtsp://0.0.0.0/s", NOW).is_none());
        assert!(rtsp_evidence("x", "not a url", NOW).is_none());
        assert!(rtsp_evidence("x", "rtsp:///s", NOW).is_none());
    }

    #[test]
    fn discovered_claim_key_is_the_suggested_slug_and_stable_across_rounds() {
        let d = discovered("mdns", "10.0.0.9:554", Some("Front Door Cam"));
        let a = discovered_evidence(&d, NOW).unwrap();
        let b = discovered_evidence(&d, NOW + 60_000).unwrap();
        assert_eq!(a.source, "front-door-cam");
        assert_eq!(a.source, b.source);
        assert_eq!(content_hash(&a), content_hash(&b));
        assert_eq!(a.ips, vec!["10.0.0.9".to_string()]);
        assert_eq!(a.hostname.as_deref(), Some("Front Door Cam"));
        assert_eq!(a.platform.as_deref(), Some("mdns"));
        // No usable name: the address slug, as the proposal would suggest.
        let anon = discovered("ws-discovery", "10.0.0.10:8000", None);
        assert_eq!(
            discovered_evidence(&anon, NOW).unwrap().source,
            "10-0-0-10-8000"
        );
    }

    #[test]
    fn onvif_hardware_lands_in_vendor_only() {
        let mut d = discovered("ws-discovery", "10.0.0.11:8000", Some("cam"));
        d.attributes
            .insert("hardware".to_string(), "AXIS P3245".to_string());
        d.attributes.insert(
            "scopes".to_string(),
            "onvif://www.onvif.org/name/cam".to_string(),
        );
        let ev = discovered_evidence(&d, NOW).unwrap();
        assert_eq!(ev.vendor.as_deref(), Some("AXIS P3245"));
        assert!(ev.macs.is_empty() && ev.host_id.is_none() && ev.fqdn.is_none());
    }

    #[test]
    fn ledger_republishes_on_change_or_refresh_only() {
        let mut ledger = Ledger::default();
        let ev = rtsp_evidence("door", "rtsp://10.0.0.7/s", NOW).unwrap();
        let refresh = 300_000;
        assert!(ledger.should_publish(&ev, NOW, refresh), "new");
        assert!(
            !ledger.should_publish(&ev, NOW + 1_000, refresh),
            "unchanged, not due"
        );
        assert!(ledger.should_publish(&ev, NOW + refresh, refresh), "due");
        let moved = rtsp_evidence("door", "rtsp://10.0.0.8/s", NOW + refresh + 1).unwrap();
        assert!(
            ledger.should_publish(&moved, NOW + refresh + 1, refresh),
            "changed"
        );
        let mut live = BTreeMap::new();
        live.insert("other".to_string(), ev.clone());
        ledger.retain(&live);
        assert!(
            ledger.should_publish(&moved, NOW + refresh + 2, refresh),
            "forgotten"
        );
    }

    #[test]
    fn claims_prefer_the_configured_entry_and_skip_local_devices() {
        let cfg: crate::config::ParallaxConfig = json5::from_str(
            r#"{ enumerate_v4l2: false,
                 rtsp: [{ name: "door", url: "rtsp://10.0.0.7/s" }],
                 test_sources: [{ name: "test0" }] }"#,
        )
        .unwrap();
        let catalog = Catalog::build(&cfg);
        let d = discovered("mdns", "10.0.0.99:554", Some("door"));
        let live = claims(&catalog, std::slice::from_ref(&d), true, NOW);
        assert_eq!(live.len(), 1, "{live:?}");
        assert_eq!(
            live["door"].ips,
            vec!["10.0.0.7".to_string()],
            "configured wins"
        );
        let without = claims(&catalog, &[d], false, NOW);
        assert_eq!(without.len(), 1);
        assert!(!live.contains_key("test0"));
    }
}
