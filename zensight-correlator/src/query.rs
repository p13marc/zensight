//! Late-joiner queryables served by the correlator.
//!
//! - `entities_query_key()` (the entity state selector) → storage-shaped
//!   seed: one reply per entity on its concrete key,
//!   the seed a late-joining frontend GETs on connect (mirrors the sensors'
//!   alert-state seed: a queryable on `state/<producer>/alert/*`, RFC 05 §4).
//! - `names_query_key()` with selector `?ip=<ip>` → that IP's accumulated
//!   `Vec<NameVal>` from the [`NameStore`], so arbitrary/external IPs are
//!   resolved on demand instead of flooding the bus. A missing/blank `ip`
//!   replies with an empty set (error-free).
//!
//! Replies are JSON (consistent with the existing alert-state seed).

use std::sync::Arc;

use tokio::sync::watch;
use tracing::{info, warn};
use zenoh::Session;
use zensight_common::{catalog_rpc_key, entities_query_key, names_query_key};

use crate::engine::SharedState;

/// Cap on names returned for one IP query.
const NAMES_TOP_N: usize = 32;

/// Serve the entities seed queryable until shutdown.
pub async fn serve_entities(
    session: Arc<Session>,
    state: SharedState,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = entities_query_key();
    let queryable = session
        .declare_queryable(&key)
        .await
        .map_err(|e| anyhow::anyhow!("declare entities queryable: {e}"))?;
    info!(key = %key, "entities seed queryable ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                // Storage-shaped seed (RFC 05 §4): one reply per entity on
                // its concrete state key.
                let entities = state.lock().unwrap().current_entities();
                for entity in entities {
                    let key = zensight_common::entity_key(&entity.entity_id);
                    match serde_json::to_vec(&entity) {
                        Ok(payload) => {
                            if let Err(e) = query.reply(key, payload).await {
                                warn!(error = %e, "entities seed reply failed");
                            }
                        }
                        Err(e) => warn!(error = %e, "serialize entity failed"),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Serve the on-demand names queryable until shutdown.
pub async fn serve_names(
    session: Arc<Session>,
    state: SharedState,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = names_query_key();
    let queryable = session
        .declare_queryable(&key)
        .await
        .map_err(|e| anyhow::anyhow!("declare names queryable: {e}"))?;
    info!(key = %key, "names queryable ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                let ip = query
                    .parameters()
                    .get("ip")
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let names = match ip {
                    Some(ip) => state.lock().unwrap().names_for_ip(ip, NAMES_TOP_N),
                    None => Vec::new(),
                };
                match serde_json::to_vec(&names) {
                    Ok(payload) => {
                        // Concrete reply key (RFC 05 §2.1).
                        if let Err(e) = query.reply(key.as_str(), payload).await {
                            warn!(error = %e, "names query reply failed");
                        }
                    }
                    Err(e) => warn!(error = %e, "serialize names failed"),
                }
            }
        }
    }
    Ok(())
}

/// Serve `introspect` — the catalog registry slice this build was compiled
/// against (RFC 08 §6), mirroring what every sensor serves via its runner.
pub async fn serve_introspect(
    session: Arc<Session>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = catalog_rpc_key("introspect");
    let slice = zensight_keyspace::registry::registry_toml("catalog")
        .ok_or_else(|| anyhow::anyhow!("catalog registry slice missing from the build"))?;
    let queryable = session
        .declare_queryable(&key)
        .await
        .map_err(|e| anyhow::anyhow!("declare introspect queryable: {e}"))?;
    info!(key = %key, "introspect queryable ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                if let Err(e) = query.reply(key.as_str(), slice.as_bytes()).await {
                    warn!(error = %e, "introspect reply failed");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use zensight_common::{HostEntity, MemberClaim, NameVal};

    #[test]
    fn entities_reply_roundtrips() {
        let ents = vec![HostEntity {
            entity_id: "h_0123456789ab".into(),
            aliases: vec![],
            host_id: None,
            boot_id: None,
            ips: vec!["10.0.0.5".into()],
            macs: vec![],
            container_ids: vec![],
            hostname: Some("host1".into()),
            fqdn: None,
            names: vec![],
            vendor: None,
            platform: None,
            members: vec![MemberClaim {
                sensor: "sysinfo".into(),
                source: "host1".into(),
                rule: "self".into(),
                confidence: 1.0,
                last_seen: 1,
            }],
            status: None,
            last_updated: 1,
        }];
        let payload = serde_json::to_vec(&ents).unwrap();
        let back: Vec<HostEntity> = serde_json::from_slice(&payload).unwrap();
        assert_eq!(back, ents);
    }

    #[test]
    fn names_reply_roundtrips() {
        let names = vec![NameVal {
            name: "printer.example.com".into(),
            provenance: "dns_ptr".into(),
            last_seen: 123,
        }];
        let payload = serde_json::to_vec(&names).unwrap();
        let back: Vec<NameVal> = serde_json::from_slice(&payload).unwrap();
        assert_eq!(back, names);
    }
}
