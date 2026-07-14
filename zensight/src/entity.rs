//! Frontend host-entity view model (#306).
//!
//! The **correlator** (#305) publishes [`HostEntity`] docs on
//! `zensight/v1/@catalog/state/entity/<entity_id>` — one per real host, carrying the
//! union of its identifying fields plus the reversible list of
//! [`MemberClaim`]s (`(sensor, source)` evidence) that were merged into it.
//! [`EntityStore`] is the GUI-side materialized index over those docs: it maps
//! per-protocol [`DeviceId`]s and identifying IPs back to the entity that owns
//! them, so the dashboard can render **one host card per physical host** and the
//! topology can bridge wire-level flows to hosts.
//!
//! The store is a *pure view model* — when it is empty (no correlator on the
//! bus) every consumer falls back to the pre-correlation per-source behavior
//! (the **degraded path**). Correlation can never become a hard dependency of
//! rendering.

use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;

use zensight_common::{HostEntity, MemberClaim, Protocol};

use crate::message::DeviceId;

/// Staleness threshold for an entity doc: 3× the correlator's ~60 s re-emit
/// period. Past this age with no refresh the entity is rendered with a
/// freshness indicator (the correlator may be gone).
pub const ENTITY_STALE_MS: i64 = 180_000;

/// The human identity of a device: `(protocol, source)`.
///
/// The entity index is keyed on this rather than on [`DeviceId`] because a
/// [`MemberClaim`] names a sensor and a source and carries no origin — the
/// correlator merges *human* identity, which is the whole point of it. A
/// `DeviceId` projects down to a `MemberKey` (dropping its origin), so lookups
/// still start from the device handle the GUI holds.
pub type MemberKey = (Protocol, String);

/// The [`MemberKey`] a claim was merged from. `None` when the claim's `sensor`
/// string is not a known [`Protocol`] (forward-compat: a newer correlator
/// merging an unknown sensor).
pub fn member_key(m: &MemberClaim) -> Option<MemberKey> {
    Protocol::from_str(&m.sensor)
        .ok()
        .map(|p| (p, m.source.clone()))
}

/// Project a device handle onto its human identity.
pub fn device_member_key(id: &DeviceId) -> MemberKey {
    (id.protocol, id.source.clone())
}

/// GUI-side index over the correlator's [`HostEntity`] docs.
///
/// Indexes are rebuilt per-upsert (remove the old doc's entries, insert the
/// new) rather than fully — the entity count is small (fleet hosts).
#[derive(Debug, Default, Clone)]
pub struct EntityStore {
    /// Entities keyed by `entity_id`.
    pub hosts: HashMap<String, HostEntity>,
    /// `(protocol, source)` → owning `entity_id` (from each entity's
    /// `members[]`). See [`MemberKey`] for why this is not keyed on `DeviceId`.
    pub by_device: HashMap<MemberKey, String>,
    /// Identifying `IpAddr` → owning `entity_id` (from each entity's `ips[]`).
    pub by_ip: HashMap<IpAddr, String>,
    /// Normalized (lowercase) MAC → owning `entity_id` (from `macs[]`, #314) —
    /// the inventory's MAC-keyed asset table joins through this.
    pub by_mac: HashMap<String, String>,
    /// Prior/aliased `entity_id` → current `entity_id` (from `aliases[]`).
    pub aliases: HashMap<String, String>,
}

/// Normalize a MAC for index lookup: lowercase, `:`-separated (accepts `-`).
pub fn normalize_mac(mac: &str) -> String {
    mac.trim().to_ascii_lowercase().replace('-', ":")
}

impl EntityStore {
    /// Drop every index entry that points at `entity_id` (used before an
    /// upsert re-inserts the fresh entries, and on remove).
    fn drop_indexes(&mut self, entity_id: &str) {
        self.by_device.retain(|_, v| v != entity_id);
        self.by_ip.retain(|_, v| v != entity_id);
        self.by_mac.retain(|_, v| v != entity_id);
        self.aliases.retain(|_, v| v != entity_id);
    }

    /// Insert (or replace) an entity and rebuild its index entries.
    pub fn upsert(&mut self, e: HostEntity) {
        let id = e.entity_id.clone();
        // Remove-then-insert so a shrunk members/ips/aliases list never leaves
        // stale index entries behind.
        self.drop_indexes(&id);
        for m in &e.members {
            if let Some(key) = member_key(m) {
                self.by_device.insert(key, id.clone());
            }
        }
        for ip in &e.ips {
            if let Ok(addr) = ip.parse::<IpAddr>() {
                self.by_ip.insert(addr, id.clone());
            }
        }
        for mac in &e.macs {
            self.by_mac.insert(normalize_mac(mac), id.clone());
        }
        for alias in &e.aliases {
            self.aliases.insert(alias.clone(), id.clone());
        }
        self.hosts.insert(id, e);
    }

    /// Remove an entity (tombstone) and its index entries.
    pub fn remove(&mut self, entity_id: &str) {
        self.hosts.remove(entity_id);
        self.drop_indexes(entity_id);
    }

    /// Replace the whole store from a seed snapshot (connect-time GET).
    pub fn seed(&mut self, v: Vec<HostEntity>) {
        self.hosts.clear();
        self.by_device.clear();
        self.by_ip.clear();
        self.by_mac.clear();
        self.aliases.clear();
        for e in v {
            self.upsert(e);
        }
    }

    /// The entity a given per-protocol device belongs to, if any.
    pub fn entity_for_device(&self, id: &DeviceId) -> Option<&HostEntity> {
        self.entity_id_for_device(id)
            .and_then(|eid| self.hosts.get(eid))
    }

    /// The `entity_id` a given per-protocol device belongs to, if any.
    pub fn entity_id_for_device(&self, id: &DeviceId) -> Option<&String> {
        self.by_device.get(&device_member_key(id))
    }

    /// The entity that claims a given identifying IP, if any.
    pub fn entity_for_ip(&self, ip: &IpAddr) -> Option<&HostEntity> {
        self.by_ip.get(ip).and_then(|eid| self.hosts.get(eid))
    }

    /// The entity that claims a given MAC (#314), if any. Normalizes the input
    /// so `AA-BB-…` inventory spellings match.
    pub fn entity_for_mac(&self, mac: &str) -> Option<&HostEntity> {
        self.by_mac
            .get(&normalize_mac(mac))
            .and_then(|eid| self.hosts.get(eid))
    }

    /// Resolve an aliased/old id to its current entity id (follows the alias
    /// chain, bounded against cycles). Returns the input unchanged when it is
    /// not an alias.
    pub fn resolve_alias<'a>(&'a self, id: &'a str) -> &'a str {
        let mut cur = id;
        // Fleet alias chains are 1–2 deep; bound iterations to be safe.
        for _ in 0..16 {
            match self.aliases.get(cur) {
                Some(next) if next != cur => cur = next.as_str(),
                _ => break,
            }
        }
        cur
    }

    /// Whether an entity doc is stale (older than [`ENTITY_STALE_MS`]) at
    /// `now_ms` — i.e. the correlator has missed several re-emit periods.
    /// Associated (no `&self`) so views can call it with just the entity.
    pub fn is_stale(e: &HostEntity, now_ms: i64) -> bool {
        now_ms - e.last_updated > ENTITY_STALE_MS
    }

    /// Degraded-path predicate: no correlator ⇒ empty store ⇒ per-source view.
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::MemberClaim;

    fn member(sensor: &str, source: &str) -> MemberClaim {
        MemberClaim {
            sensor: sensor.into(),
            source: source.into(),
            rule: "host_id".into(),
            confidence: 1.0,
            last_seen: 1,
        }
    }

    fn entity(id: &str, members: Vec<MemberClaim>, ips: Vec<&str>) -> HostEntity {
        HostEntity {
            entity_id: id.into(),
            aliases: vec![],
            host_id: None,
            boot_id: None,
            ips: ips.into_iter().map(String::from).collect(),
            macs: vec![],
            container_ids: vec![],
            hostname: None,
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            members,
            status: None,
            last_updated: 1_000,
        }
    }

    #[test]
    fn member_device_id_round_trips_every_protocol() {
        for p in [
            Protocol::Snmp,
            Protocol::Logs,
            Protocol::Gnmi,
            Protocol::Netflow,
            Protocol::Opcua,
            Protocol::Modbus,
            Protocol::Sysinfo,
            Protocol::Netlink,
            Protocol::Netring,
            Protocol::Systemd,
        ] {
            let m = member(p.as_str(), "host1");
            let key = member_key(&m).expect("known protocol maps");
            assert_eq!(key, (p, "host1".to_string()));
        }
        // Unknown sensor → no member key (forward-compat).
        assert!(member_key(&member("mystery", "host1")).is_none());
    }

    #[test]
    fn mac_index_normalizes_and_follows_upsert() {
        let mut store = EntityStore::default();
        let mut e = entity("h_aaa", vec![member("netring", "sensor01")], vec![]);
        e.macs = vec!["AA:BB:CC:DD:EE:FF".into()];
        store.upsert(e);
        // Case- and separator-insensitive lookup (#314).
        assert_eq!(
            store.entity_for_mac("aa:bb:cc:dd:ee:ff").unwrap().entity_id,
            "h_aaa"
        );
        assert_eq!(
            store.entity_for_mac("AA-BB-CC-DD-EE-FF").unwrap().entity_id,
            "h_aaa"
        );
        assert!(store.entity_for_mac("00:11:22:33:44:55").is_none());
        // Re-upsert with the MAC dropped → stale index entry cleared.
        store.upsert(entity("h_aaa", vec![member("netring", "sensor01")], vec![]));
        assert!(store.entity_for_mac("aa:bb:cc:dd:ee:ff").is_none());
        // Remove clears the MAC index too.
        let mut e = entity("h_bbb", vec![], vec![]);
        e.macs = vec!["11:22:33:44:55:66".into()];
        store.upsert(e);
        store.remove("h_bbb");
        assert!(store.entity_for_mac("11:22:33:44:55:66").is_none());
    }

    #[test]
    fn upsert_builds_device_and_ip_indexes() {
        let mut store = EntityStore::default();
        store.upsert(entity(
            "h_aaa",
            vec![member("sysinfo", "srv1"), member("netlink", "srv1")],
            vec!["10.0.0.1"],
        ));

        assert!(
            store
                .entity_for_device(&DeviceId::fixture(Protocol::Sysinfo, "srv1"))
                .is_some()
        );
        assert!(
            store
                .entity_for_device(&DeviceId::fixture(Protocol::Netlink, "srv1"))
                .is_some()
        );
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(store.entity_for_ip(&ip).unwrap().entity_id, "h_aaa");
    }

    #[test]
    fn upsert_replaces_stale_index_entries() {
        let mut store = EntityStore::default();
        store.upsert(entity(
            "h_aaa",
            vec![member("sysinfo", "srv1"), member("netlink", "srv1")],
            vec!["10.0.0.1"],
        ));
        // Re-upsert with the netlink member and the old IP dropped.
        store.upsert(entity(
            "h_aaa",
            vec![member("sysinfo", "srv1")],
            vec!["10.0.0.2"],
        ));

        assert!(
            store
                .entity_for_device(&DeviceId::fixture(Protocol::Sysinfo, "srv1"))
                .is_some()
        );
        // Dropped member/ip no longer resolve.
        assert!(
            store
                .entity_for_device(&DeviceId::fixture(Protocol::Netlink, "srv1"))
                .is_none()
        );
        let old_ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(store.entity_for_ip(&old_ip).is_none());
        let new_ip: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(store.entity_for_ip(&new_ip).is_some());
    }

    #[test]
    fn remove_clears_all_indexes() {
        let mut store = EntityStore::default();
        store.upsert(entity(
            "h_aaa",
            vec![member("sysinfo", "srv1")],
            vec!["10.0.0.1"],
        ));
        store.remove("h_aaa");
        assert!(store.is_empty());
        assert!(
            store
                .entity_for_device(&DeviceId::fixture(Protocol::Sysinfo, "srv1"))
                .is_none()
        );
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(store.entity_for_ip(&ip).is_none());
    }

    #[test]
    fn resolve_alias_follows_chain() {
        let mut store = EntityStore::default();
        let mut e = entity("h_new", vec![member("sysinfo", "srv1")], vec![]);
        e.aliases = vec!["h_old".into()];
        store.upsert(e);
        // Also register a second hop old2 -> old (as if a prior merge).
        let mut e2 = entity("h_old", vec![], vec![]);
        e2.aliases = vec!["h_old2".into()];
        store.upsert(e2);

        assert_eq!(store.resolve_alias("h_old"), "h_new");
        // Two-hop chain h_old2 -> h_old -> h_new resolves all the way.
        assert_eq!(store.resolve_alias("h_old2"), "h_new");
        // Non-alias returns unchanged.
        assert_eq!(store.resolve_alias("h_new"), "h_new");
        assert_eq!(store.resolve_alias("nope"), "nope");
    }

    #[test]
    fn staleness_uses_threshold() {
        let e = entity("h_aaa", vec![], vec![]);
        assert!(!EntityStore::is_stale(&e, e.last_updated + ENTITY_STALE_MS));
        assert!(EntityStore::is_stale(
            &e,
            e.last_updated + ENTITY_STALE_MS + 1
        ));
    }

    #[test]
    fn seed_replaces_previous_contents() {
        let mut store = EntityStore::default();
        store.upsert(entity("h_aaa", vec![member("sysinfo", "srv1")], vec![]));
        store.seed(vec![entity(
            "h_bbb",
            vec![member("netlink", "srv2")],
            vec![],
        )]);
        assert!(store.hosts.contains_key("h_bbb"));
        assert!(!store.hosts.contains_key("h_aaa"));
        assert!(
            store
                .entity_for_device(&DeviceId::fixture(Protocol::Sysinfo, "srv1"))
                .is_none()
        );
    }
}
