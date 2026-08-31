//! The `@rpc/hostspec/*` control surface (#821).
//!
//! - `expectations` (read) / `expectations/set` (write): the sentinel
//!   hot-swap pair, via the shared [`rpc::serve_topic`] seam. A submitted
//!   set is **validated before it replaces anything** — a bad regex, a
//!   relative path, a duplicate name refuse with `error/invalid-args` and
//!   the previous good set keeps running. That validation is the one thing
//!   the older sentinels do not do (they serde-reject only).
//! - `spec` (read): the latest evaluation — "what is this host being held
//!   to", answerable from the bus. `evaluated_at_ms == 0` means the first
//!   sweep has not completed; never a fabricated pass.

use std::sync::Arc;

use zensight_common::rpc::RpcError;
use zensight_sensor_core::rpc;

use crate::sentinel::{ExpectationsConfig, SentinelHandle};

pub const EXPECTATIONS_TOPIC: &str = "expectations";

/// Serve until the session closes. `marker` is the shared `applied/<topic>`
/// publisher (#816): an RPC apply is the second writer to the sentinel
/// handle, and stamping `source: rpc` here is what keeps the marker honest
/// about which writer won last.
pub async fn run(
    session: Arc<zenoh::Session>,
    producer: String,
    handle: SentinelHandle,
    marker: zensight_sensor_core::desired::AppliedMarker,
) {
    let ctx = zensight_sensor_core::v1::for_producer(&producer);
    let apply_handle = handle.clone();
    let status_handle = handle.clone();
    let mut tasks = Vec::new();
    match rpc::serve_topic::<ExpectationsConfig, _, _, _, _>(
        session.clone(),
        &ctx,
        EXPECTATIONS_TOPIC,
        move |cfg: ExpectationsConfig| {
            let h = apply_handle.clone();
            let m = marker.clone();
            async move {
                crate::sentinel::validate(&cfg).map_err(RpcError::invalid_args)?;
                h.replace(cfg.clone()).await;
                // The RPC writer stamps the shared marker (#816): two writers,
                // LWW by arrival, and this is what says who won last.
                m.publish(
                    zensight_common::desired::AppliedSource::Rpc,
                    &cfg,
                    None,
                    None,
                )
                .await;
                Ok(())
            }
        },
        move || {
            let h = status_handle.clone();
            async move {
                let snapshot = h.snapshot().await;
                serde_json::to_vec(&snapshot)
                    .map_err(|e| RpcError::new("error/hostspec/serialize", e.to_string()))
            }
        },
    )
    .await
    {
        Ok(t) => tasks.extend(t),
        Err(e) => tracing::error!(error = %e, "hostspec: failed to serve expectations procedures"),
    }

    let spec_handle = handle.clone();
    match rpc::serve(session, &ctx, &["spec"], move |_req| {
        let h = spec_handle.clone();
        async move {
            let eval = h.evaluation().await;
            serde_json::to_vec(&eval)
                .map_err(|e| RpcError::new("error/hostspec/serialize", e.to_string()))
        }
    })
    .await
    {
        Ok(t) => tasks.push(t),
        Err(e) => tracing::error!(error = %e, "hostspec: failed to serve spec procedure"),
    }

    for t in tasks {
        let _ = t.await;
    }
}
