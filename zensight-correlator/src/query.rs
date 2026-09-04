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
    let queryable = zensight_common::served::serve_state_queryable(&session, &key)
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
                // its concrete state key, stamped — a producer answering a
                // plain GET on a state selector IS a storage for the duration
                // of that reply, and a storage's samples are timestamped
                // (RFC 04 §3.2, #782).
                //
                // The stamp is taken INSIDE the lock, with the snapshot it
                // describes. Stamping per reply instead would let an entity
                // the engine updates mid-loop have its live `put` stamped
                // earlier than this loop's stale copy, and LWW would keep the
                // stale one. Same session as every entity `put`, so the two
                // are totally ordered.
                let (entities, stamp) = {
                    let guard = state.lock().unwrap();
                    (
                        guard.current_entities(),
                        zensight_common::served::seed_stamp(&session),
                    )
                };
                for entity in entities {
                    let key = zensight_common::entity_key(&entity.entity_id);
                    match serde_json::to_vec(&entity) {
                        Ok(payload) => {
                            if let Err(e) = query.reply_state(&key, payload, stamp).await {
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

/// Serve the edge seed queryable until shutdown (#917).
///
/// The edge-side twin of [`serve_entities`], and it exists for the same
/// reason: a GUI joining a running fleet must see the graph that is already
/// there, not wait for something to change. Without it a late joiner shows an
/// empty map until the next re-emit — which, with the change gate doing its
/// job, may be minutes away and is *supposed* to be.
pub async fn serve_edges(
    session: Arc<Session>,
    state: SharedState,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = zensight_common::keyexpr::edges_query_key();
    let queryable = zensight_common::served::serve_state_queryable(&session, &key)
        .await
        .map_err(|e| anyhow::anyhow!("declare edges queryable: {e}"))?;
    info!(key = %key, "edges seed queryable ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                // Storage-shaped, stamped inside the lock — see the long note
                // in `serve_entities`. The hazard is identical here: an edge
                // the engine updates mid-loop would have its live `put`
                // stamped earlier than this loop's stale copy, and LWW would
                // keep the stale one.
                let (edges, stamp) = {
                    let guard = state.lock().unwrap();
                    (
                        guard.current_edges(),
                        zensight_common::served::seed_stamp(&session),
                    )
                };
                for edge in edges {
                    let key = zensight_common::keyexpr::edge_key(&edge.edge_id);
                    match serde_json::to_vec(&edge) {
                        Ok(payload) => {
                            if let Err(e) = query.reply_state(&key, payload, stamp).await {
                                warn!(error = %e, "edges seed reply failed");
                            }
                        }
                        Err(e) => warn!(error = %e, "serialize edge failed"),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Serve the incident seed queryable until shutdown (#923).
///
/// The third twin of [`serve_entities`], for the third reason it exists: a GUI
/// or a notifier joining a running fleet must see what is *already* on fire.
/// Without it, a late joiner shows an empty triage surface until the next
/// re-emit — which, with the change gate doing its job, may be a minute away
/// and is supposed to be.
pub async fn serve_incidents(
    session: Arc<Session>,
    state: SharedState,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = zensight_common::keyexpr::all_incidents_wildcard();
    let queryable = zensight_common::served::serve_state_queryable(&session, &key)
        .await
        .map_err(|e| anyhow::anyhow!("declare incidents queryable: {e}"))?;
    info!(key = %key, "incidents seed queryable ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                // Storage-shaped, stamped inside the lock — the same hazard as
                // the other two seeds: an incident the engine updates mid-loop
                // would have its live `put` stamped earlier than this loop's
                // stale copy, and LWW would keep the stale one.
                let (incidents, stamp) = {
                    let guard = state.lock().unwrap();
                    (
                        guard.current_incidents(),
                        zensight_common::served::seed_stamp(&session),
                    )
                };
                for incident in incidents {
                    let key = zensight_common::keyexpr::incident_key(&incident.id);
                    match serde_json::to_vec(&incident) {
                        Ok(payload) => {
                            if let Err(e) = query.reply_state(&key, payload, stamp).await {
                                warn!(error = %e, "incidents seed reply failed");
                            }
                        }
                        Err(e) => warn!(error = %e, "serialize incident failed"),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Serve the acknowledgement seed until shutdown (#925).
///
/// # Why this exists
///
/// The whole point of epic #900 is that an ack outlives the process that made
/// it. That needs two things and #924 shipped only one: a live subscriber sees
/// the `put`, but a GUI that starts *afterwards* — a second operator joining a
/// running incident, or the same operator after a restart — has nothing to
/// read. [`crate::publisher::publish_ack`] uses a plain publisher that is
/// dropped at the end of the call, so there is no publisher cache for the
/// subscriber's `history()` to recover from, and in the deployment the
/// `configs/` ship there is no router storage either. The seed GET the
/// frontend already issues simply returned nothing, silently: every
/// acknowledged alert came back unacknowledged, and a second operator started
/// work someone was already doing.
///
/// So the catalog answers for its own acks, exactly as it does for assertions
/// ([`serve_assertions`]) and incidents ([`serve_incidents`]) — storage-shaped,
/// one reply per document on its concrete key.
pub async fn serve_acks(
    session: Arc<Session>,
    state: SharedState,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = zensight_common::keyexpr::all_acks_wildcard();
    let queryable = zensight_common::served::serve_state_queryable(&session, &key)
        .await
        .map_err(|e| anyhow::anyhow!("declare acks queryable: {e}"))?;
    info!(key = %key, "acks seed queryable ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                // Stamped inside the lock, the same hazard as every other
                // seed: an ack retired by the sweep mid-loop would have its
                // live tombstone stamped earlier than this loop's stale copy,
                // and LWW would resurrect it.
                let (acks, stamp) = {
                    let guard = state.lock().unwrap();
                    (
                        guard.current_acks(),
                        zensight_common::served::seed_stamp(&session),
                    )
                };
                for ack in acks {
                    let key = zensight_common::keyexpr::ack_key(&ack.alert_ref);
                    match serde_json::to_vec(&ack) {
                        Ok(payload) => {
                            if let Err(e) = query.reply_state(&key, payload, stamp).await {
                                warn!(error = %e, "acks seed reply failed");
                            }
                        }
                        Err(e) => warn!(error = %e, "serialize ack failed"),
                    }
                }
            }
        }
    }
    Ok(())
}

/// Serve the suppression seed until shutdown (#925).
///
/// The counterpart to [`serve_acks`], and broken for the same reason before
/// this existed. A silence matters *more* to a late joiner than an ack does: a
/// GUI that cannot see the window renders alerts an operator deliberately
/// quieted, which is the noise the silence was created to remove.
pub async fn serve_silences(
    session: Arc<Session>,
    state: SharedState,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = zensight_common::keyexpr::all_silences_wildcard();
    let queryable = zensight_common::served::serve_state_queryable(&session, &key)
        .await
        .map_err(|e| anyhow::anyhow!("declare silences queryable: {e}"))?;
    info!(key = %key, "silences seed queryable ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                let (silences, stamp) = {
                    let guard = state.lock().unwrap();
                    (
                        guard.current_silences(),
                        zensight_common::served::seed_stamp(&session),
                    )
                };
                for silence in silences {
                    let key = zensight_common::keyexpr::silence_key(&silence.id);
                    match serde_json::to_vec(&silence) {
                        Ok(payload) => {
                            if let Err(e) = query.reply_state(&key, payload, stamp).await {
                                warn!(error = %e, "silences seed reply failed");
                            }
                        }
                        Err(e) => warn!(error = %e, "serialize silence failed"),
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
    let queryable = zensight_common::served::serve_queryable(&session, &key)
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
    // `link` and `unlink` are the catalog's write procedures: the audited seam
    // (#957), which has no unrecorded way to answer. Merging two hosts into one
    // entity, or splitting them again, is exactly the kind of operator action
    // SYS-SUP-019 asks to be journalled.
    let link_q = zensight_common::served::serve_write_queryable(&session, &link_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare link queryable: {e}"))?;
    let unlink_q = zensight_common::served::serve_write_queryable(&session, &unlink_key)
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

/// Serve `ack`, `unack`, `silence` and `unsilence` until shutdown (#924).
///
/// The same shape as [`serve_assertions`] and behind the **same gate**: all
/// six change what the fleet believes about itself on an operator's say-so,
/// and all six ride the audited write seam (#957), which has no unrecorded way
/// to answer. "Who silenced this, and when" is precisely the question an
/// incident review asks and the one a `HashSet` in a GUI could never answer.
///
/// When gated they still reply `error/gated` rather than timing out, so an
/// operator learns the feature exists and is switched off.
pub async fn serve_ack_and_silence(
    session: Arc<Session>,
    state: SharedState,
    format: Format,
    allowed: bool,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let ack_key = catalog_rpc_key("ack");
    let unack_key = catalog_rpc_key("unack");
    let silence_key = catalog_rpc_key("silence");
    let unsilence_key = catalog_rpc_key("unsilence");
    let ack_q = zensight_common::served::serve_write_queryable(&session, &ack_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare ack queryable: {e}"))?;
    let unack_q = zensight_common::served::serve_write_queryable(&session, &unack_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare unack queryable: {e}"))?;
    let silence_q = zensight_common::served::serve_write_queryable(&session, &silence_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare silence queryable: {e}"))?;
    let unsilence_q = zensight_common::served::serve_write_queryable(&session, &unsilence_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare unsilence queryable: {e}"))?;
    info!(gated = !allowed, "ack/silence procedures ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = ack_q.recv_async() => {
                let Ok(query) = query else { break };
                handle_ack(&session, &state, format, allowed, &ack_key, query).await;
            }
            query = unack_q.recv_async() => {
                let Ok(query) = query else { break };
                handle_unack(&session, &state, allowed, &unack_key, query).await;
            }
            query = silence_q.recv_async() => {
                let Ok(query) = query else { break };
                handle_silence(&session, &state, format, allowed, &silence_key, query).await;
            }
            query = unsilence_q.recv_async() => {
                let Ok(query) = query else { break };
                handle_unsilence(&session, &state, allowed, &unsilence_key, query).await;
            }
        }
    }
    Ok(())
}

/// Tombstone acks whose occurrence has ended and silences whose window has
/// closed (#924).
///
/// Both are lifecycle the catalog owns, not the operator: an ack that outlived
/// its alert and a silence past `ends_at` are documents that say something
/// untrue, and a fleet reading them — a GUI, an exporter, a notifier — would
/// act on it. The projection rule already makes a stale ack *inert*, so this
/// is not a correctness backstop; it is the difference between "inert" and
/// "gone", which is what an operator sees when they list what is
/// acknowledged.
///
/// Runs on a timer rather than on every change, and deliberately: the input is
/// a clock (`ends_at`) as much as it is an event, and a silence must expire
/// even on a fleet where nothing else is happening.
pub async fn run_lifecycle_sweep(
    session: Arc<Session>,
    state: SharedState,
    interval: std::time::Duration,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // consume the immediate first tick
    info!(
        interval_secs = interval.as_secs(),
        "ack/silence sweep started"
    );
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            _ = tick.tick() => {
                let now = zensight_common::current_timestamp_millis();
                let (stale, expired) = {
                    let st = state.lock().unwrap();
                    (st.stale_acks(), st.expired_silences(now))
                };
                for r in stale {
                    state
                        .lock()
                        .unwrap()
                        .apply(EvidenceMsg::RemoveAck(Box::new(r.clone())));
                    if let Err(e) = crate::publisher::retire_ack(&session, &r).await {
                        warn!(error = %e, "retiring a stale ack failed");
                    }
                }
                for id in expired {
                    state
                        .lock()
                        .unwrap()
                        .apply(EvidenceMsg::RemoveSilence { id: id.clone() });
                    if let Err(e) = crate::publisher::retire_silence(&session, &id).await {
                        warn!(error = %e, "retiring an expired silence failed");
                    }
                }
            }
        }
    }
    Ok(())
}

/// The gate all four share. `None` = allowed.
fn write_gate(allowed: bool) -> Option<RpcError> {
    (!allowed).then(|| {
        RpcError::gated(
            "operator writes are disabled; set `allow_operator_assertions: true` in the \
             correlator config. Acknowledging and silencing change what the fleet believes \
             about itself, so they share the gate that guards `link`/`unlink`",
        )
        .with_refused_by("allow_operator_assertions")
    })
}

/// `?ref=` as a parsed [`AlertRef`], or the refusal the caller gets back.
fn ref_param(req: &RpcRequest) -> Result<zensight_common::alert::AlertRef, RpcError> {
    let raw = req
        .param("ref")
        .ok_or_else(|| RpcError::invalid_args("missing ?ref=<origin>.<producer>.<alert_key>"))?;
    zensight_common::alert::AlertRef::parse(&raw)
        .map_err(|e| RpcError::invalid_args(format!("`{raw}` is not an alert ref: {e}")))
}

async fn handle_ack(
    session: &Session,
    state: &SharedState,
    format: Format,
    allowed: bool,
    reply_key: &str,
    query: zensight_common::served::WriteQuery,
) {
    let req = query.request();
    let target = req.param("ref");
    let outcome = (|| {
        if let Some(gated) = write_gate(allowed) {
            return Err(gated);
        }
        let r = ref_param(&req)?;
        // The gate that matters (#900): an ack for a problem nobody is
        // reporting would sit on the key, inert by the projection rule, and
        // then quietly apply the moment that exact alert next fired within its
        // `fired_at`. Refusing here is refusing a suppression nobody asked for.
        let firing = state.lock().unwrap().firing_alert(&r).ok_or_else(|| {
            RpcError::new(
                "error/catalog/not-firing",
                format!(
                    "no firing alert for {r}. An acknowledgement names an occurrence \
                     someone looked at; there is nothing here to have looked at."
                ),
            )
        })?;
        Ok(zensight_common::ack::AlertAck {
            alert_ref: r,
            // The occurrence, not the moment of clicking: this is what makes a
            // re-fire page again.
            fired_at: firing.timestamp,
            by: req.actor().unwrap_or_else(|| "unknown".to_string()),
            note: req.param("note").unwrap_or_default(),
            at: zensight_common::current_timestamp_millis(),
        })
    })();

    match outcome {
        Ok(ack) => {
            // Apply locally *and* publish, exactly as an assertion does: the
            // local apply makes the next recompute see it without waiting for
            // our own subscriber to loop the put back.
            state
                .lock()
                .unwrap()
                .apply(EvidenceMsg::Ack(Box::new(ack.clone())));
            if let Err(e) = crate::publisher::publish_ack(session, format, &ack).await {
                warn!(error = %e, "publishing the ack failed");
                let err = RpcError::new("error/catalog/publish", e.to_string());
                let _ = query.refused(&err, target.as_deref()).await;
                return;
            }
            reply_doc(query, reply_key, &ack, target.as_deref()).await;
        }
        Err(e) => {
            if let Err(e) = query.refused(&e, target.as_deref()).await {
                warn!(error = %e, "ack reply_err failed");
            }
        }
    }
}

async fn handle_unack(
    session: &Session,
    state: &SharedState,
    allowed: bool,
    reply_key: &str,
    query: zensight_common::served::WriteQuery,
) {
    let req = query.request();
    let target = req.param("ref");
    let outcome = (|| {
        if let Some(gated) = write_gate(allowed) {
            return Err(gated);
        }
        ref_param(&req)
    })();
    match outcome {
        Ok(r) => {
            // Idempotent: unacking what is not acked is a no-op, not an error.
            // An operator clearing a stale ack should not have to know whether
            // the sweep beat them to it.
            state
                .lock()
                .unwrap()
                .apply(EvidenceMsg::RemoveAck(Box::new(r.clone())));
            if let Err(e) = crate::publisher::retire_ack(session, &r).await {
                warn!(error = %e, "retiring the ack failed");
            }
            if let Err(e) = query
                .executed(reply_key, Vec::<u8>::new(), target.as_deref())
                .await
            {
                warn!(error = %e, "unack reply failed");
            }
        }
        Err(e) => {
            if let Err(e) = query.refused(&e, target.as_deref()).await {
                warn!(error = %e, "unack reply_err failed");
            }
        }
    }
}

async fn handle_silence(
    session: &Session,
    state: &SharedState,
    format: Format,
    allowed: bool,
    reply_key: &str,
    query: zensight_common::served::WriteQuery,
) {
    let req = query.request();
    let outcome = (|| {
        if let Some(gated) = write_gate(allowed) {
            return Err(gated);
        }
        build_silence(&req)
    })();
    match outcome {
        Ok(silence) => {
            let target = silence.id.clone();
            state
                .lock()
                .unwrap()
                .apply(EvidenceMsg::Silence(Box::new(silence.clone())));
            if let Err(e) = crate::publisher::publish_silence(session, format, &silence).await {
                warn!(error = %e, "publishing the silence failed");
                let err = RpcError::new("error/catalog/publish", e.to_string());
                let _ = query.refused(&err, Some(&target)).await;
                return;
            }
            reply_doc(query, reply_key, &silence, Some(&target)).await;
        }
        Err(e) => {
            if let Err(e) = query.refused(&e, None).await {
                warn!(error = %e, "silence reply_err failed");
            }
        }
    }
}

async fn handle_unsilence(
    session: &Session,
    state: &SharedState,
    allowed: bool,
    reply_key: &str,
    query: zensight_common::served::WriteQuery,
) {
    let req = query.request();
    let target = req.param("id");
    let outcome = (|| {
        if let Some(gated) = write_gate(allowed) {
            return Err(gated);
        }
        req.param("id")
            .filter(|id| !id.is_empty())
            .ok_or_else(|| RpcError::invalid_args("missing ?id=<ulid>"))
    })();
    match outcome {
        Ok(id) => {
            state
                .lock()
                .unwrap()
                .apply(EvidenceMsg::RemoveSilence { id: id.clone() });
            if let Err(e) = crate::publisher::retire_silence(session, &id).await {
                warn!(error = %e, "retiring the silence failed");
            }
            if let Err(e) = query
                .executed(reply_key, Vec::<u8>::new(), target.as_deref())
                .await
            {
                warn!(error = %e, "unsilence reply failed");
            }
        }
        Err(e) => {
            if let Err(e) = query.refused(&e, target.as_deref()).await {
                warn!(error = %e, "unsilence reply_err failed");
            }
        }
    }
}

/// Pure: request body → silence, or the refusal the caller gets back.
///
/// Validated **before** it applies, because the failure mode is asymmetric: a
/// refused silence costs a page, and a silence that matches everything costs
/// an outage nobody hears about.
pub fn build_silence(req: &RpcRequest) -> Result<zensight_common::silence::Silence, RpcError> {
    let mut silence: zensight_common::silence::Silence = req.json()?;

    if silence.matchers.is_empty() {
        return Err(RpcError::invalid_args(
            "a silence needs at least one matcher. An empty set matches nothing here — the \
             vacuous reading, 'all zero conditions hold', is how one typo mutes a fleet",
        ));
    }
    for m in &silence.matchers {
        let known = matches!(m.name.as_str(), "origin" | "producer" | "source" | "rule")
            || m.name.starts_with("labels.");
        if !known {
            return Err(RpcError::invalid_args(format!(
                "`{}` is not a matchable field. Use origin, producer, source, rule, or \
                 labels.<name>",
                m.name
            )));
        }
        if m.op == zensight_common::silence::MatchOp::Regex
            && let Err(e) = regex::Regex::new(&m.value)
        {
            // Refused here rather than at match time, where a bad pattern
            // silently matches nothing and the operator believes they muted
            // something they did not.
            return Err(RpcError::invalid_args(format!(
                "matcher `{}` has an invalid regex: {e}",
                m.name
            )));
        }
    }
    if silence.ends_at <= silence.starts_at {
        return Err(RpcError::invalid_args(
            "ends_at must be after starts_at — a window that closes before it opens \
             suppresses nothing and reads as if it does",
        ));
    }
    // The author is required and comes from `?actor=`, never from the body: a
    // silence whose author is self-reported is a silence nobody can be asked
    // about, and "who muted this" is the first question of any review.
    let by = req.actor().filter(|a| !a.is_empty()).ok_or_else(|| {
        RpcError::invalid_args(
            "missing ?actor= — a silence records who opened it, and the caller is the \
                 only party that knows",
        )
    })?;
    silence.by = by;
    if silence.id.is_empty() {
        silence.id = ulid::Ulid::generate().to_string().to_ascii_lowercase();
    }
    Ok(silence)
}

async fn reply_doc<T: serde::Serialize>(
    query: zensight_common::served::WriteQuery,
    reply_key: &str,
    doc: &T,
    target: Option<&str>,
) {
    match serde_json::to_vec(doc) {
        Ok(payload) => {
            if let Err(e) = query.executed(reply_key, payload, target).await {
                warn!(error = %e, "reply failed");
            }
        }
        Err(e) => {
            warn!(error = %e, "serialize reply failed");
            let err = RpcError::new("error/catalog/serialize", e.to_string());
            let _ = query.refused(&err, target).await;
        }
    }
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
    query: zensight_common::served::WriteQuery,
) {
    let req = query.request();
    // What was acted on, for the trail: the operator is merging or splitting
    // *these two* origins, and the record is read without the payload.
    let target = match (req.param("old"), req.param("new")) {
        (Some(old), Some(new)) => Some(format!("{old}->{new}")),
        _ => None,
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
                let err = RpcError::new("error/catalog/publish", e.to_string());
                if let Err(e) = query.refused(&err, target.as_deref()).await {
                    warn!(error = %e, "assertion reply_err failed");
                }
                return;
            }
            match serde_json::to_vec(&assertion) {
                Ok(payload) => {
                    if let Err(e) = query.executed(reply_key, payload, target.as_deref()).await {
                        warn!(error = %e, "assertion reply failed");
                    }
                }
                Err(e) => {
                    warn!(error = %e, "serialize assertion failed");
                    let err = RpcError::new("error/catalog/serialize", e.to_string());
                    if let Err(e) = query.refused(&err, target.as_deref()).await {
                        warn!(error = %e, "assertion reply_err failed");
                    }
                }
            }
        }
        Err(e) => {
            if let Err(e) = query.refused(&e, target.as_deref()).await {
                warn!(error = %e, "assertion reply_err failed");
            }
        }
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

/// Serve `introspect` — the catalog registry slice this build was compiled
/// against (RFC 08 §6), mirroring what every sensor serves via its runner.
pub async fn serve_introspect(
    session: Arc<Session>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = catalog_rpc_key("introspect");
    let slice = zensight_common::registry::registry_toml("catalog")
        .ok_or_else(|| anyhow::anyhow!("catalog registry slice missing from the build"))?;
    let queryable = zensight_common::served::serve_queryable(&session, &key)
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

/// Serve `describe` — the RFC 08 §7 SchemaSet, next to `introspect` (the
/// catalog serves the same fleet-wide superset every sensor serves).
pub async fn serve_describe(
    session: Arc<Session>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let key = catalog_rpc_key("describe");
    let json = zensight_common::schema::DESCRIBE_JSON.as_str();
    let queryable = zensight_common::served::serve_queryable(&session, &key)
        .await
        .map_err(|e| anyhow::anyhow!("declare describe queryable: {e}"))?;
    info!(key = %key, "describe queryable ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            query = queryable.recv_async() => {
                let Ok(query) = query else { break };
                if let Err(e) = query
                    .reply(key.as_str(), json.as_bytes())
                    .encoding(zenoh::bytes::Encoding::APPLICATION_JSON)
                    .await
                {
                    warn!(error = %e, "describe reply failed");
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

#[cfg(test)]
mod ack_silence_tests {
    use super::*;
    use zensight_common::silence::{MatchOp, Matcher, Silence};
    use zensight_common::{AlertKind, AlertSeverity, Protocol};

    fn req(params: &str, body: &str) -> RpcRequest {
        RpcRequest::new(body.as_bytes().to_vec(), params.to_string())
    }

    fn alert(source: &str, rule: &str) -> zensight_common::alert::Alert {
        zensight_common::alert::Alert::new(
            source,
            Protocol::Netlink,
            AlertKind::Expectation,
            rule,
            AlertSeverity::Critical,
            "x",
        )
    }

    // ---- silence validation ---------------------------------------------

    /// **The asymmetric-harm gate.** A refused silence costs a page; a silence
    /// that matches everything costs an outage nobody hears about.
    #[test]
    fn a_silence_with_no_matchers_is_refused() {
        let e = build_silence(&req(
            "actor=marc",
            r#"{"id":"","matchers":[],"starts_at":0,"ends_at":10,"by":""}"#,
        ))
        .expect_err("an empty matcher set must be refused");
        assert_eq!(e.error, "error/invalid-args");
        assert!(e.message.contains("at least one matcher"), "{}", e.message);
    }

    /// A bad regex is refused at WRITE time, not at match time — where it
    /// silently matches nothing and the operator believes they muted something
    /// they did not.
    #[test]
    fn a_silence_with_an_invalid_regex_is_refused() {
        let e = build_silence(&req(
            "actor=marc",
            r#"{"id":"","matchers":[{"name":"source","op":"regex","value":"web("}],
               "starts_at":0,"ends_at":10,"by":""}"#,
        ))
        .expect_err("an uncompilable regex must be refused");
        assert!(e.message.contains("invalid regex"), "{}", e.message);
    }

    /// A field nobody can match on is a typo, and a typo that silently
    /// suppresses nothing is worse than an error.
    #[test]
    fn a_silence_on_an_unknown_field_is_refused() {
        let e = build_silence(&req(
            "actor=marc",
            r#"{"id":"","matchers":[{"name":"hostname","op":"eq","value":"web01"}],
               "starts_at":0,"ends_at":10,"by":""}"#,
        ))
        .expect_err("an unmatchable field must be refused");
        assert!(e.message.contains("not a matchable field"), "{}", e.message);
        // `labels.*` is the escape hatch and must still work.
        assert!(
            build_silence(&req(
                "actor=marc",
                r#"{"id":"","matchers":[{"name":"labels.unit","op":"eq","value":"sshd"}],
                   "starts_at":0,"ends_at":10,"by":""}"#,
            ))
            .is_ok()
        );
    }

    #[test]
    fn a_window_that_closes_before_it_opens_is_refused() {
        let e = build_silence(&req(
            "actor=marc",
            r#"{"id":"","matchers":[{"name":"source","op":"eq","value":"web01"}],
               "starts_at":10,"ends_at":10,"by":""}"#,
        ))
        .expect_err("ends_at must be after starts_at");
        assert!(e.message.contains("after starts_at"), "{}", e.message);
    }

    /// **The author comes from `?actor=`, never the body.** A silence whose
    /// author is self-reported is a silence nobody can be asked about, and
    /// "who muted this" is the first question of any review.
    #[test]
    fn the_author_comes_from_the_caller_not_the_body() {
        let body = r#"{"id":"","matchers":[{"name":"source","op":"eq","value":"web01"}],
                       "starts_at":0,"ends_at":10,"by":"someone-else"}"#;
        let e = build_silence(&req("", body)).expect_err("actor is required");
        assert!(e.message.contains("?actor="), "{}", e.message);

        let s = build_silence(&req("actor=marc", body)).expect("with an actor");
        assert_eq!(s.by, "marc", "the body's `by` must not win");
        assert!(!s.id.is_empty(), "a silence gets a ULID when none is given");
    }

    // ---- the matcher semantics the procedures gate --------------------

    /// A `labels.*` matcher mutes only alerts carrying that label.
    #[test]
    fn a_label_matcher_mutes_only_matching_alerts() {
        let s = Silence {
            id: "s1".into(),
            matchers: vec![Matcher {
                name: "labels.unit".into(),
                op: MatchOp::Eq,
                value: "sshd.service".into(),
            }],
            starts_at: 0,
            ends_at: 10_000,
            by: "marc".into(),
            note: String::new(),
        };
        let matching = alert("web01", "unit-failed").with_label("unit", "sshd.service");
        let other = alert("web01", "unit-failed").with_label("unit", "nginx.service");
        let unlabelled = alert("web01", "disk-full");
        assert!(s.matches(1_000, "h-1", "systemd", &matching));
        assert!(!s.matches(1_000, "h-1", "systemd", &other));
        assert!(!s.matches(1_000, "h-1", "systemd", &unlabelled));
    }

    // ---- the gate --------------------------------------------------------

    /// Gated procedures **reply** rather than time out, and the refusal names
    /// the switch that refused it (#866).
    #[test]
    fn a_gated_write_names_the_switch() {
        let e = write_gate(false).expect("gated");
        assert_eq!(e.error, "error/gated");
        assert_eq!(e.refused_by.as_deref(), Some("allow_operator_assertions"));
        assert!(write_gate(true).is_none());
    }

    // ---- ref parsing -----------------------------------------------------

    #[test]
    fn a_missing_or_malformed_ref_is_refused() {
        assert!(ref_param(&req("", "")).is_err(), "missing ?ref=");
        assert!(
            ref_param(&req("ref=not-a-ref", "")).is_err(),
            "a ref with too few components"
        );
        let r = ref_param(&req("ref=h-3fa9c2d41b7e.netlink.a1b2c3d4", "")).expect("a good ref");
        assert_eq!(r.producer, "netlink");
    }
}
