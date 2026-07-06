//! Host-evidence feed (#307): republish observed assets and passive-DNS name
//! observations onto `zensight/_meta/evidence/**` for the correlator (epic #312).
//!
//! These are **third-party** claims (`observer = Some("netring")`) about devices
//! seen on the wire — the correlator weighs them lower than a host's self-report
//! but uses them to attach identity (hostname/vendor, IP↔name) to entities that
//! emit no telemetry of their own. Publishing is change-driven with a periodic
//! liveness refresh and hard per-tick caps so a busy L2 segment can't flood the
//! evidence bus.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use zensight_common::{
    AssetRecord, HostEvidence, NameObservation, host_evidence_key, name_observation_key,
};
use zensight_sensor_core::AdvancedPublisherRegistry;

use crate::config::EvidenceConfig;
use crate::monitor::{AssetDirty, AssetInventory};

/// Cap on the per-source dedup bookkeeping maps — bounds memory on a network
/// with a very large L2 neighbor count.
const DEDUP_CAP: usize = 16_384;

/// Slugify a MAC for use as an evidence `source`: lowercase, `:` → `-`.
pub fn mac_slug(mac: &str) -> String {
    mac.to_ascii_lowercase().replace(':', "-")
}

/// Whether a MAC is empty or all-zero (`00:00:...`) — not an identifying claim.
pub fn is_zero_mac(mac: &str) -> bool {
    mac.chars().all(|c| matches!(c, '0' | ':' | '-'))
}

/// Pure map: an observed asset → a third-party `HostEvidence` claim.
///
/// Returns `None` for a record with an empty / all-zero MAC (nothing to key on).
pub fn asset_to_evidence(rec: &AssetRecord, now_ms: i64) -> Option<HostEvidence> {
    if rec.mac.is_empty() || is_zero_mac(&rec.mac) {
        return None;
    }
    let mut ips = rec.ipv4.clone();
    ips.extend(rec.ipv6.iter().cloned());
    Some(HostEvidence {
        sensor: "netring".to_string(),
        source: mac_slug(&rec.mac),
        observer: Some("netring".to_string()),
        host_id: None,
        boot_id: None,
        hostname: rec.hostname.clone(),
        fqdn: None,
        ips,
        macs: vec![rec.mac.clone()],
        vendor: rec.vendor.clone(),
        platform: rec.platform.clone(),
        // Observed assets: container/cloud identity is only knowable from the
        // device's own sensor self-report, never from passive wire data (#311).
        container_id: None,
        cloud: None,
        last_updated: now_ms,
    })
}

/// Content hash of the *identifying* fields of a claim (everything but
/// `last_updated`), used to detect real changes vs. pure liveness refreshes.
/// IPs/MACs are sorted so a reordering isn't mistaken for a change.
pub fn content_hash(ev: &HostEvidence) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ev.sensor.hash(&mut h);
    ev.source.hash(&mut h);
    ev.observer.hash(&mut h);
    ev.host_id.hash(&mut h);
    ev.boot_id.hash(&mut h);
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

/// Pure decision: should this claim be (re)published now?
///
/// - New source or changed content → yes.
/// - Unchanged but `refresh_ms` elapsed since the last emit → yes (liveness).
/// - Otherwise → no.
pub fn should_publish(
    now_ms: i64,
    last_hash: Option<u64>,
    new_hash: u64,
    last_emit_ms: Option<i64>,
    refresh_ms: i64,
) -> bool {
    match last_hash {
        Some(h) if h == new_hash => match last_emit_ms {
            Some(t) => now_ms.saturating_sub(t) >= refresh_ms,
            None => true,
        },
        _ => true,
    }
}

/// Insert/refresh a per-source dedup entry, evicting the least-recently-emitted
/// entry when the map is at capacity (bounds memory on a huge L2 segment).
fn record_emit(
    last_hash: &mut HashMap<String, u64>,
    last_emit: &mut HashMap<String, i64>,
    source: &str,
    hash: u64,
    now_ms: i64,
) {
    if !last_hash.contains_key(source) && last_hash.len() >= DEDUP_CAP {
        // Evict the stalest source by last-emit time.
        if let Some(victim) = last_emit
            .iter()
            .min_by_key(|&(_, &t)| t)
            .map(|(k, _)| k.clone())
        {
            last_hash.remove(&victim);
            last_emit.remove(&victim);
        }
    }
    last_hash.insert(source.to_string(), hash);
    last_emit.insert(source.to_string(), now_ms);
}

/// Asset → host-evidence feed task. Ticks every `min_interval_secs`, publishing
/// changed assets (drained from the dirty set) plus a full-inventory liveness
/// refresh every `refresh_secs`, capped at `max_per_tick` records per tick.
pub async fn run_asset_evidence(
    assets: AssetInventory,
    dirty: AssetDirty,
    registry: Arc<AdvancedPublisherRegistry>,
    cfg: EvidenceConfig,
) {
    let interval = cfg.min_interval_secs.max(1);
    let refresh_ms = (cfg.refresh_secs.max(1) as i64) * 1000;
    let mut tick = tokio::time::interval(Duration::from_secs(interval));
    let mut last_hash: HashMap<String, u64> = HashMap::new();
    let mut last_emit: HashMap<String, i64> = HashMap::new();
    let mut last_refresh_ms: i64 = 0;

    loop {
        tick.tick().await;
        let now = zensight_common::current_timestamp_millis();

        // Candidate MACs this tick: everything that changed, plus (on the refresh
        // cadence) the whole inventory so live claims never age out of TTL.
        let mut candidates: HashSet<String> = match dirty.lock() {
            Ok(mut d) => std::mem::take(&mut *d),
            Err(_) => HashSet::new(),
        };
        let refresh_due = now.saturating_sub(last_refresh_ms) >= refresh_ms;
        if refresh_due {
            last_refresh_ms = now;
            if let Ok(map) = assets.lock() {
                candidates.extend(map.keys().cloned());
            }
        }

        if candidates.is_empty() {
            continue;
        }

        // Apply the per-tick cap; carry the remainder back onto the dirty set so
        // it's picked up next tick rather than lost.
        let mut list: Vec<String> = candidates.into_iter().collect();
        if list.len() > cfg.max_per_tick {
            let overflow = list.split_off(cfg.max_per_tick);
            let dropped = overflow.len();
            if let Ok(mut d) = dirty.lock() {
                d.extend(overflow);
            }
            tracing::warn!(
                dropped,
                cap = cfg.max_per_tick,
                "netring: asset-evidence tick over cap; deferring remainder"
            );
        }

        for mac in list {
            let record = match assets.lock().ok().and_then(|m| m.get(&mac).cloned()) {
                Some(r) => r,
                None => continue,
            };
            let Some(ev) = asset_to_evidence(&record, now) else {
                continue;
            };
            let hash = content_hash(&ev);
            if !should_publish(
                now,
                last_hash.get(&ev.source).copied(),
                hash,
                last_emit.get(&ev.source).copied(),
                refresh_ms,
            ) {
                continue;
            }
            let key = host_evidence_key("netring", &ev.source);
            if let Err(e) = registry.publish_serializable(&key, &ev).await {
                tracing::warn!(error = %e, mac = %mac, "netring: asset-evidence publish failed");
                continue;
            }
            record_emit(&mut last_hash, &mut last_emit, &ev.source, hash, now);
        }
    }
}

/// Slugify an IP for use as a name-observation `source`: `.`/`:` → `-`, so the
/// correlator's per-IP GET is stable and updates replace in place.
pub fn ip_slug(ip: &str) -> String {
    ip.replace(['.', ':'], "-")
}

/// Max distinct IPs published per name-observation batch (a safety cap; the
/// upstream delta feed is already bounded by `names.max_batch`).
const NAME_MAX_PER_BATCH: usize = 500;

/// Passive-DNS name-observation publisher task (#307). Consumes batches of
/// newly-observed IP→name bindings forwarded off the name-map drain task,
/// dedupes to the newest binding per IP, and publishes each as a
/// `NameObservation` keyed by IP so updates replace in place.
pub async fn run_name_evidence(
    mut rx: mpsc::UnboundedReceiver<Vec<NameObservation>>,
    registry: Arc<AdvancedPublisherRegistry>,
) {
    while let Some(batch) = rx.recv().await {
        // Newest binding per IP wins (the key is per-IP; last write replaces).
        let mut newest: HashMap<String, NameObservation> = HashMap::new();
        for obs in batch {
            match newest.get(&obs.ip) {
                Some(existing) if existing.last_seen >= obs.last_seen => {}
                _ => {
                    newest.insert(obs.ip.clone(), obs);
                }
            }
        }
        let mut items: Vec<NameObservation> = newest.into_values().collect();
        if items.len() > NAME_MAX_PER_BATCH {
            tracing::warn!(
                dropped = items.len() - NAME_MAX_PER_BATCH,
                cap = NAME_MAX_PER_BATCH,
                "netring: name-evidence batch over cap; dropping remainder"
            );
            items.truncate(NAME_MAX_PER_BATCH);
        }
        for obs in items {
            let key = name_observation_key("netring", &ip_slug(&obs.ip));
            if let Err(e) = registry.publish_serializable(&key, &obs).await {
                tracing::warn!(error = %e, ip = %obs.ip, "netring: name-evidence publish failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(mac: &str) -> AssetRecord {
        AssetRecord {
            mac: mac.to_string(),
            ipv4: vec!["10.0.0.5".into()],
            ipv6: vec!["fe80::1".into()],
            hostname: Some("printer1".into()),
            vendor: Some("HP".into()),
            platform: None,
            capabilities: vec![],
            seen_via: vec!["arp".into()],
            last_seen: 123,
            ..Default::default()
        }
    }

    #[test]
    fn maps_asset_fields_and_slugs_mac() {
        let ev = asset_to_evidence(&asset("AA:BB:CC:DD:EE:FF"), 1_000).unwrap();
        assert_eq!(ev.sensor, "netring");
        assert_eq!(ev.observer.as_deref(), Some("netring"));
        assert_eq!(ev.source, "aa-bb-cc-dd-ee-ff");
        assert_eq!(ev.host_id, None);
        assert_eq!(ev.boot_id, None);
        assert_eq!(ev.hostname.as_deref(), Some("printer1"));
        assert_eq!(ev.fqdn, None);
        assert_eq!(ev.macs, vec!["AA:BB:CC:DD:EE:FF".to_string()]);
        assert_eq!(ev.vendor.as_deref(), Some("HP"));
        assert_eq!(ev.last_updated, 1_000);
    }

    #[test]
    fn merges_ipv4_then_ipv6() {
        let ev = asset_to_evidence(&asset("aa:bb:cc:dd:ee:ff"), 1).unwrap();
        assert_eq!(ev.ips, vec!["10.0.0.5".to_string(), "fe80::1".to_string()]);
    }

    #[test]
    fn skips_empty_and_zero_mac() {
        assert!(asset_to_evidence(&asset(""), 1).is_none());
        assert!(asset_to_evidence(&asset("00:00:00:00:00:00"), 1).is_none());
        assert!(asset_to_evidence(&asset("000000000000"), 1).is_none());
        // A real MAC is kept.
        assert!(asset_to_evidence(&asset("aa:00:00:00:00:00"), 1).is_some());
    }

    #[test]
    fn hash_ignores_last_updated_and_ip_order() {
        let a = asset_to_evidence(&asset("aa:bb:cc:dd:ee:ff"), 1).unwrap();
        let b = asset_to_evidence(&asset("aa:bb:cc:dd:ee:ff"), 999_999).unwrap();
        assert_eq!(content_hash(&a), content_hash(&b));
        // Reordered IPs hash the same.
        let mut c = a.clone();
        c.ips.reverse();
        assert_eq!(content_hash(&a), content_hash(&c));
        // A hostname change is a real change.
        let mut d = a.clone();
        d.hostname = Some("other".into());
        assert_ne!(content_hash(&a), content_hash(&d));
    }

    #[test]
    fn should_publish_decisions() {
        let refresh = 420_000;
        // New source (no prior hash) → publish.
        assert!(should_publish(1_000, None, 7, None, refresh));
        // Unchanged & within refresh window → skip.
        assert!(!should_publish(
            1_000 + 100,
            Some(7),
            7,
            Some(1_000),
            refresh
        ));
        // Unchanged but refresh elapsed → publish.
        assert!(should_publish(
            1_000 + refresh,
            Some(7),
            7,
            Some(1_000),
            refresh
        ));
        // Changed content → publish immediately.
        assert!(should_publish(1_000 + 1, Some(7), 9, Some(1_000), refresh));
    }

    #[test]
    fn ip_slug_replaces_dots_and_colons() {
        assert_eq!(ip_slug("10.0.0.9"), "10-0-0-9");
        assert_eq!(ip_slug("fe80::1"), "fe80--1");
        assert_eq!(ip_slug("2001:db8::42"), "2001-db8--42");
    }

    #[test]
    fn record_emit_bounds_and_evicts_stalest() {
        let mut lh: HashMap<String, u64> = HashMap::new();
        let mut le: HashMap<String, i64> = HashMap::new();
        // Fill to cap with increasing emit times.
        for i in 0..DEDUP_CAP {
            record_emit(&mut lh, &mut le, &format!("s{i}"), i as u64, i as i64);
        }
        assert_eq!(lh.len(), DEDUP_CAP);
        // Inserting a new source evicts the stalest (s0, emit time 0).
        record_emit(&mut lh, &mut le, "new", 1, 1_000_000);
        assert_eq!(lh.len(), DEDUP_CAP);
        assert!(!lh.contains_key("s0"));
        assert!(lh.contains_key("new"));
    }
}
