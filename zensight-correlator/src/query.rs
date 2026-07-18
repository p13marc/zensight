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
use zensight_common::serialization::Format;
use zensight_common::{
    AssertionKind, OperatorAssertion, RpcError, RpcRequest, catalog_rpc_key, entities_query_key,
    names_query_key,
};

use crate::engine::{EvidenceMsg, SharedState};

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

/// Serve `link` and `unlink` — the operator identity assertions (#473, RFC 06
/// §5.4).
///
/// `GET …/@catalog/@rpc/link?old=<origin>;new=<origin>` says *these two origins
/// are the same machine* — the reinstall case, where the host minted a new
/// origin, the old one's evidence is still live, and the correlator's
/// conflicting-strong-ids guard correctly refuses to merge them because it
/// cannot tell a reinstall from two machines. Only an operator can.
///
/// `unlink` says the opposite, and retires the `link` it replaces.
///
/// Both are **write** procedures, and both are **gated** (`allow_operator_
/// assertions`). Both are idempotent: the assertion id is derived from the pair,
/// so re-asserting overwrites rather than accumulating.
pub async fn serve_assertions(
    session: Arc<Session>,
    state: SharedState,
    format: Format,
    allowed: bool,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let link_key = catalog_rpc_key("link");
    let unlink_key = catalog_rpc_key("unlink");
    let link_q = session
        .declare_queryable(&link_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare link queryable: {e}"))?;
    let unlink_q = session
        .declare_queryable(&unlink_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare unlink queryable: {e}"))?;
    info!(
        link = %link_key, unlink = %unlink_key, gated = !allowed,
        "operator assertion procedures ready"
    );

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = link_q.recv_async() => {
                let Ok(query) = query else { break };
                handle_assertion(
                    &session, &state, format, allowed, AssertionKind::Link, &link_key, query,
                ).await;
            }
            query = unlink_q.recv_async() => {
                let Ok(query) = query else { break };
                handle_assertion(
                    &session, &state, format, allowed, AssertionKind::Unlink, &unlink_key, query,
                ).await;
            }
        }
    }
    Ok(())
}

/// Validate, record, publish, reply. Failures ride `reply_err` with a
/// machine-readable name (RFC 05 §3) — a value reply always means it worked.
async fn handle_assertion(
    session: &Session,
    state: &SharedState,
    format: Format,
    allowed: bool,
    kind: AssertionKind,
    reply_key: &str,
    query: zenoh::query::Query,
) {
    let req = RpcRequest {
        payload: query
            .payload()
            .map(|p| p.to_bytes().to_vec())
            .unwrap_or_default(),
        parameters: query.parameters().to_string(),
    };
    match build_assertion(&req, allowed, kind) {
        Ok(assertion) => {
            // Record it locally *and* publish it. The local apply makes the next
            // recompute see it without waiting for our own subscriber to loop the
            // put back; the publish is what makes it survive us.
            state
                .lock()
                .unwrap()
                .apply(EvidenceMsg::Assert(assertion.clone()));

            // An `unlink` retires the `link` it contradicts: without this the
            // link document would sit in the storage, and a correlator restarting
            // from that storage would re-seed a link the operator has revoked.
            if kind == AssertionKind::Unlink {
                let link_id =
                    OperatorAssertion::id(AssertionKind::Link, &assertion.old, &assertion.new);
                state.lock().unwrap().apply(EvidenceMsg::RemoveAssertion {
                    id: link_id.clone(),
                });
                if let Err(e) = crate::publisher::retire_assertion(session, &link_id).await {
                    warn!(error = %e, "retiring the superseded link failed");
                }
            }

            if let Err(e) = crate::publisher::publish_assertion(session, format, &assertion).await {
                warn!(error = %e, "publishing the assertion failed");
                reply_err(
                    &query,
                    RpcError::new("error/catalog/publish", e.to_string()),
                )
                .await;
                return;
            }
            match serde_json::to_vec(&assertion) {
                Ok(payload) => {
                    if let Err(e) = query.reply(reply_key, payload).await {
                        warn!(error = %e, "assertion reply failed");
                    }
                }
                Err(e) => warn!(error = %e, "serialize assertion failed"),
            }
        }
        Err(e) => reply_err(&query, e).await,
    }
}

/// Pure: request → assertion, or the error the caller gets back.
fn build_assertion(
    req: &RpcRequest,
    allowed: bool,
    kind: AssertionKind,
) -> Result<OperatorAssertion, RpcError> {
    if !allowed {
        return Err(RpcError::gated(
            "operator assertions are disabled; set `allow_operator_assertions: true` \
             in the correlator config (RFC 06 §5.4 — a link overrides the guard that \
             keeps two machines from fusing into one host)",
        ));
    }
    let old = req
        .param("old")
        .ok_or_else(|| RpcError::invalid_args("missing ?old=<origin>"))?;
    let new = req
        .param("new")
        .ok_or_else(|| RpcError::invalid_args("missing ?new=<origin>"))?;

    // Origin ids only — never the weaker evidence-derived entity ids. An entity
    // id computed from a hostname or a MAC *changes when the set it names
    // changes*, so an assertion keyed on one would dangle the instant it took
    // effect. An origin id is minted by the host and never moves.
    for id in [&old, &new] {
        if !zenkey::grammar::is_valid_host_origin(id) {
            return Err(RpcError::invalid_args(format!(
                "`{id}` is not a host origin id (expected `h-<12hex>`). Operator \
                 assertions name origins, not the evidence-derived entity ids that \
                 change shape when a merge does."
            )));
        }
    }
    if old == new {
        return Err(RpcError::invalid_args(
            "`old` and `new` are the same origin — nothing to assert",
        ));
    }

    Ok(OperatorAssertion {
        id: OperatorAssertion::id(kind, &old, &new),
        kind,
        old,
        new,
        asserted_at: zensight_common::current_timestamp_millis(),
        note: req.param("note"),
    })
}

async fn reply_err(query: &zenoh::query::Query, err: RpcError) {
    let payload = serde_json::to_vec(&err).unwrap_or_default();
    if let Err(e) = query.reply_err(payload).await {
        warn!(error = %e, "reply_err failed");
    }
}

/// Serve `introspect` — the catalog registry slice this build was compiled
/// against (RFC 08 §6), mirroring what every sensor serves via its runner.
pub async fn serve_introspect(
    session: Arc<Session>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = catalog_rpc_key("introspect");
    let slice = zenkey::registry::registry_toml("catalog")
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
