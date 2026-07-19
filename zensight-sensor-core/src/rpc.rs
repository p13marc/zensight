//! The `@rpc` control plane (RFC 05, epic #453).
//!
//! Producers serve *procedures* as Zenoh queryables at
//! `<base>/v1/<origin>/@rpc/<producer>/<procedure...>`. All interaction is
//! request/reply — pub/sub commands are gone. The disciplines this module
//! bakes in (so sensors cannot get them wrong):
//!
//! - **Replies ride the concrete key**, never the query's (possibly
//!   wildcard) selector — fleet fan-in consolidation keeps one reply per
//!   reply key (RFC 05 §2.1).
//! - **A value reply means success; failures ride `reply_err`** with a
//!   machine-readable, namespaced error name (RFC 05 §3).
//! - Queryables MUST be declared **before** the producer's `alive`
//!   liveliness token ("alive ⇒ callable", RFC 04 §5) — serve procedures
//!   before `SensorRunner::run()` and the ordering holds.

use std::future::Future;
use std::sync::Arc;

use zenoh::Session;

use crate::error::{Result, SensorError};
use crate::v1::V1Context;

// The call contract itself (RpcError/RpcRequest/RpcResult + the reserved error
// names) lives in `zensight-common`: a *caller* is not a sensor, and neither is
// the correlator, which serves `@catalog/@rpc/link` without depending on this
// crate. Re-exported so a sensor's `use zensight_sensor_core::rpc::*` is
// unchanged.
pub use zensight_common::rpc::{
    ERR_BUSY, ERR_GATED, ERR_INVALID_ARGS, ERR_NOT_FOUND, ERR_UNAUTHORIZED, ERR_UNSUPPORTED,
    RpcError, RpcRequest, RpcResult,
};

/// Serve one procedure. The returned task runs until the session closes;
/// track it with `SensorRunner::spawn`-style lifetime (or just hold it).
pub async fn serve<H, Fut>(
    session: Arc<Session>,
    ctx: &V1Context,
    procedure: &[&str],
    handler: H,
) -> Result<tokio::task::JoinHandle<()>>
where
    H: Fn(RpcRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = RpcResult> + Send + 'static,
{
    let key = ctx.rpc_key(procedure);
    let queryable =
        session
            .declare_queryable(key.as_str())
            .await
            .map_err(|e| SensorError::Publish {
                key: key.clone().into(),
                message: format!("failed to declare procedure queryable: {e}"),
            })?;
    tracing::info!(key = %key, "procedure ready");
    let handle = tokio::spawn(async move {
        while let Ok(query) = queryable.recv_async().await {
            let request = RpcRequest {
                payload: query
                    .payload()
                    .map(|p| p.to_bytes().to_vec())
                    .unwrap_or_default(),
                parameters: query.parameters().as_str().to_string(),
            };
            match handler(request).await {
                Ok(bytes) => {
                    // Concrete reply key (RFC 05 §2.1) — never echo the selector.
                    if let Err(e) = query.reply(key.as_str(), bytes).await {
                        tracing::warn!(key = %key, error = %e, "failed to reply");
                    }
                }
                Err(err) => {
                    let payload = serde_json::to_vec(&err).unwrap_or_default();
                    if let Err(e) = query.reply_err(payload).await {
                        tracing::warn!(key = %key, error = %e, "failed to reply_err");
                    }
                }
            }
        }
        tracing::debug!(key = %key, "procedure queryable closed");
    });
    Ok(handle)
}

/// Serve the `<topic>` read + `<topic>/set` write pair that replaces the
/// legacy `@/commands/<topic>` + `@/status/<topic>` channels (RFC 05 §5).
///
/// `apply` handles a decoded command (the write body); `status` produces the
/// current-configuration reply for the read.
pub async fn serve_topic<Cmd, A, AF, S, SF>(
    session: Arc<Session>,
    ctx: &V1Context,
    topic: &str,
    apply: A,
    status: S,
) -> Result<Vec<tokio::task::JoinHandle<()>>>
where
    Cmd: serde::de::DeserializeOwned + Send + 'static,
    A: Fn(Cmd) -> AF + Send + Sync + 'static,
    AF: Future<Output = std::result::Result<(), RpcError>> + Send + 'static,
    S: Fn() -> SF + Send + Sync + 'static,
    SF: Future<Output = RpcResult> + Send + 'static,
{
    let read = serve(session.clone(), ctx, &[topic], move |_req| status()).await?;
    let write = serve(session, ctx, &[topic, "set"], move |req: RpcRequest| {
        let cmd = req.json::<Cmd>();
        let fut = cmd.map(&apply);
        async move {
            match fut {
                Ok(f) => f.await.map(|()| Vec::new()),
                Err(e) => Err(e),
            }
        }
    })
    .await?;
    Ok(vec![read, write])
}

/// Serve `introspect` — the registry slice this build was compiled against
/// (RFC 08 §6). Pass the producer's generated `REGISTRY_TOML`.
pub async fn serve_introspect(
    session: Arc<Session>,
    ctx: &V1Context,
    registry_toml: &'static str,
) -> Result<tokio::task::JoinHandle<()>> {
    serve(session, ctx, &["introspect"], move |_req| async move {
        Ok(registry_toml.as_bytes().to_vec())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_names_are_namespaced() {
        assert_eq!(RpcError::gated("no").error, "error/gated");
        assert_eq!(
            RpcError::producer("netring", "capture-busy", "x").error,
            "error/netring/capture-busy"
        );
    }

    #[test]
    fn request_params() {
        let req = RpcRequest {
            payload: vec![],
            parameters: "since=17;max=500;source=web01".into(),
        };
        assert_eq!(req.param("max").as_deref(), Some("500"));
        assert_eq!(req.param("source").as_deref(), Some("web01"));
        assert_eq!(req.param("nope"), None);
    }
}
