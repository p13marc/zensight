//! The controller's own RPC surface (#939), and the liveliness that means it
//! is answerable.
//!
//! Three procedures, the first this service origin has had. Until #938 nothing
//! published `@desired` at all, so there was nobody to ask.

use std::sync::Arc;

use tokio::sync::{Mutex, watch};
use zenoh::Session;
use zensight_common::desired::DesiredOverride;
use zensight_common::rpc::RpcError;

use crate::overrides::Overrides;

/// Everything `override/set` needs to answer.
pub struct OverrideCtx {
    pub overrides: Arc<Mutex<Overrides>>,
    pub path: std::path::PathBuf,
    /// Gate. `false` still **serves** the procedure and replies
    /// `error/gated` — an operator learns the feature exists and is switched
    /// off, rather than learning nothing from a timeout (RFC 05 §3).
    pub allowed: bool,
    /// Nudges the compile loop, so an adoption converges in seconds rather
    /// than at the next refresh. A `watch` rather than a notify: the loop may
    /// be mid-pass, and this must not be missed.
    pub wake: watch::Sender<u64>,
}

/// The keys this daemon must be answering before it says `alive`.
///
/// **Read from the registry slice, not hand-listed.** The correlator's
/// equivalent is a hand-maintained array, and its own source records that
/// `incident`, `ack` and `silence` were each missing from it at some point —
/// *"which is exactly how a GUI came to issue three seed GETs of which two
/// were answered by nothing at all"*. A list derived from the slice cannot
/// drift from it.
pub fn declared_rpc_keys() -> Vec<String> {
    let toml = zensight_common::registry::desired::REGISTRY_TOML;
    let slice = zenkey::parse_slice(toml).expect("the shipped @desired slice parses");
    let mut keys: Vec<String> = slice
        .procedures
        .iter()
        .map(|p| zensight_common::keyexpr::desired_rpc_key(&p.path))
        .collect();
    keys.sort();
    keys
}

/// Serve `override/set`, `introspect` and `describe`.
pub async fn serve(
    session: Arc<Session>,
    ctx: Arc<OverrideCtx>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let set_key = zensight_common::keyexpr::desired_rpc_key("override/set");
    let introspect_key = zensight_common::keyexpr::desired_rpc_key("introspect");
    let describe_key = zensight_common::keyexpr::desired_rpc_key("describe");

    // The write rides the audited seam (#957): recording a per-host exception
    // is exactly the operator action SYS-SUP-019 asks to be journalled, and
    // this seam has no unrecorded way to answer.
    let set_q = zensight_common::served::serve_write_queryable(&session, &set_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare override/set: {e}"))?;
    let introspect_q = zensight_common::served::serve_queryable(&session, &introspect_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare introspect: {e}"))?;
    let describe_q = zensight_common::served::serve_queryable(&session, &describe_key)
        .await
        .map_err(|e| anyhow::anyhow!("declare describe: {e}"))?;

    tracing::info!(gated = !ctx.allowed, "@desired procedures ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            q = set_q.recv_async() => {
                let Ok(q) = q else { break };
                handle_set(&ctx, &set_key, q).await;
            }
            q = introspect_q.recv_async() => {
                let Ok(q) = q else { break };
                let toml = zensight_common::registry::desired::REGISTRY_TOML;
                if let Err(e) = q.reply(introspect_key.as_str(), toml.as_bytes()).await {
                    tracing::warn!(error = %e, "introspect reply failed");
                }
            }
            q = describe_q.recv_async() => {
                let Ok(q) = q else { break };
                let json = zensight_common::schema::DESCRIBE_JSON.as_str();
                if let Err(e) = q
                    .reply(describe_key.as_str(), json.as_bytes())
                    .encoding(zenoh::bytes::Encoding::APPLICATION_JSON)
                    .await
                {
                    tracing::warn!(error = %e, "describe reply failed");
                }
            }
        }
    }
    Ok(())
}

async fn handle_set(
    ctx: &OverrideCtx,
    reply_key: &str,
    query: zensight_common::served::WriteQuery,
) {
    let req = query.request();
    // What was acted on, for the trail. An override names one host's one
    // topic, and the record is read without the payload.
    let target = req
        .json::<DesiredOverride>()
        .ok()
        .map(|o| format!("{}:{}", o.host, o.key()));

    let outcome: Result<Vec<u8>, RpcError> = (|| {
        if !ctx.allowed {
            return Err(RpcError::gated(
                "recording overrides is disabled; set `desired.allow_overrides: true` in the \
                 controller config. An override changes what a host is told to do on an \
                 operator's say-so, and is durable — it outlives the session that made it",
            )
            .with_refused_by("allow_overrides"));
        }
        let mut o: DesiredOverride = req.json()?;
        // The actor is the call's, never the body's. An author who reports
        // themselves is an author nobody can be asked about.
        o.by = req.param("actor");
        if o.at == 0 {
            o.at = zensight_common::current_timestamp_millis();
        }
        o.validate().map_err(RpcError::invalid_args)?;
        Ok(serde_json::to_vec(&o).unwrap_or_default())
    })();

    let parsed: Option<DesiredOverride> = outcome
        .as_ref()
        .ok()
        .and_then(|b| serde_json::from_slice(b).ok());

    let (body, o) = match (outcome, parsed) {
        (Ok(body), Some(o)) => (body, o),
        (Err(err), _) => {
            let _ = query.refused(&err, target.as_deref()).await;
            return;
        }
        (Ok(_), None) => {
            let err = RpcError::invalid_args("override/set: undecodable request");
            let _ = query.refused(&err, target.as_deref()).await;
            return;
        }
    };

    {
        {
            let changed = {
                let mut store = ctx.overrides.lock().await;
                let changed = store.apply(&o);
                if changed && let Err(e) = store.save(&ctx.path) {
                    // Refuse rather than accept: an override that is not on
                    // disk is one that vanishes at the next restart, and the
                    // operator would have been told it landed.
                    tracing::error!(error = %e, "could not write the overrides file");
                    let err = RpcError::new("error/desired/not-durable", e);
                    let _ = query.refused(&err, None).await;
                    return;
                }
                changed
            };
            if changed {
                // Nudge the compile loop; an adoption should converge in
                // seconds, not at the next refresh.
                ctx.wake.send_modify(|n| *n += 1);
            }
            tracing::info!(
                host = %o.host, topic = %o.key(), by = ?o.by,
                removed = o.doc.is_none(), changed,
                "override recorded"
            );
            if let Err(e) = query.executed(reply_key, body, target.as_deref()).await {
                tracing::warn!(error = %e, "override reply failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list `await_served` waits on comes from the slice, so a procedure
    /// added to the registry is waited for without anyone remembering to
    /// update a second list.
    #[test]
    fn the_callable_list_is_derived_from_the_registry() {
        let keys = declared_rpc_keys();
        assert!(
            keys.contains(&"v1/@desired/@rpc/override/set".to_string()),
            "{keys:?}"
        );
        assert!(keys.contains(&"v1/@desired/@rpc/introspect".to_string()));
        assert!(keys.contains(&"v1/@desired/@rpc/describe".to_string()));
        assert_eq!(keys.len(), 3, "one per declared procedure: {keys:?}");
    }
}
