//! Historical passive-DNS publisher (`@catalog/state/pdns`, #310).
//!
//! The correlator already accumulates provenance-tagged names per IP in its
//! [`NameStore`](crate::store::NameStore). This task drains a channel of
//! [`PdnsRecord`]s — emitted by the [`Engine`](crate::engine::Engine) whenever a
//! name observation updates an IP — and PUTs each on its
//! `zensight/v1/@catalog/state/pdns/<ip-slug>` key with a **plain** publisher
//! (a `session.put`, not a per-IP declared publisher — the IP set is unbounded).
//!
//! These records stay off the telemetry class selector and the `*`-origin state
//! selectors because `@catalog` is a verbatim origin the `*` selectors never
//! match (D4). They are **meant to be captured** by a router-hosted storage
//! backend (filesystem snapshot or InfluxDB time series) subscribed on
//! `zensight/v1/@catalog/state/pdns/**`, giving a durable historical IP↔name
//! tier. Publishing here is cheap and off the packet hot path: it fires on
//! correlator name-store updates, not per packet.
//!
//! Records are reliable + block ([`QosClass::Entity`]): a dropped pdns PUT
//! would be a gap in the historical record, so it back-pressures rather than
//! drops. `last_writer_wins` reconciliation on the storage side keeps the newest
//! accumulated name set per IP (see `docs/storage.md`).

use std::sync::Arc;

use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};
use zenoh::Session;
use zensight_common::serialization::Format;
use zensight_common::{PdnsRecord, QosClass, encode, pdns_key};

/// Run the `@catalog/state/pdns` publisher: drain `rx`, PUT each record on its
/// key, until shutdown.
pub async fn run(
    session: Arc<Session>,
    format: Format,
    mut rx: mpsc::Receiver<PdnsRecord>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Durable historical records must arrive at the storage backend.
    let q = QosClass::Entity;
    info!("passive-DNS (@pdns) publisher ready");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            rec = rx.recv() => {
                let Some(rec) = rec else { break }; // engine gone
                let key = pdns_key(&rec.ip);
                let payload = match encode(&rec, format) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = %e, ip = %rec.ip, "encode pdns record failed");
                        continue;
                    }
                };
                match session
                    .put(&key, payload)
                    .encoding(format.encoding())
                    .congestion_control(q.congestion_control())
                    .priority(q.priority())
                    .express(q.express())
                    .reliability(q.reliability())
                    .await
                {
                    Ok(()) => debug!(key = %key, names = rec.names.len(), "pdns record published"),
                    Err(e) => warn!(error = %e, key = %key, "pdns put failed"),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use zensight_common::pdns_key;

    #[test]
    fn pdns_publish_key_mapping() {
        assert_eq!(pdns_key("10.0.0.9"), "v1/@catalog/state/pdns/10-0-0-9");
    }
}
