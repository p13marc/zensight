//! `--demo` synthetic evidence feed.
//!
//! Publishes a fixed, deterministic set of `HostEvidence` + `NameObservation`
//! through the REAL engine/store/publisher pipeline (no Zenoh input needed), so
//! the frontend (#306) can develop against a live correlator without any
//! sensors. Data is deterministic — no `now()`/rand in the identifying content
//! (a fixed base timestamp) — so entity ids are stable run-to-run; only the
//! liveness `last_updated` refresh uses the wall clock.

use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tracing::info;
use zensight_common::{HostEvidence, NameObservation, current_timestamp_millis};

use crate::engine::EvidenceMsg;

/// Fixed base timestamp for synthetic identifying content (2023-11-14T22:13:20Z).
pub const DEMO_BASE_TS: i64 = 1_700_000_000_000;

fn host_id(tag: &str) -> String {
    // A sha256-shaped 64-hex host_id, deterministic from a short tag.
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(tag.as_bytes()))
}

/// The synthetic evidence + name set. Exercises: a two-sensor self-report merge
/// (sysinfo+netlink, same host_id → one entity, two members); weak-merge safety
/// (two hosts sharing a hostname but with different host_ids → two entities); an
/// observed asset that merges with a self-report on MAC+IP; and passive-DNS name
/// enrichment on a couple of IPs.
pub fn synthetic() -> (Vec<HostEvidence>, Vec<NameObservation>) {
    let ts = DEMO_BASE_TS;
    let mk = |sensor: &str,
              source: &str,
              observer: Option<&str>,
              hid: Option<String>,
              hostname: Option<&str>,
              fqdn: Option<&str>,
              ips: &[&str],
              macs: &[&str]| HostEvidence {
        sensor: sensor.into(),
        source: source.into(),
        observer: observer.map(String::from),
        host_id: hid,
        boot_id: None,
        hostname: hostname.map(String::from),
        fqdn: fqdn.map(String::from),
        ips: ips.iter().map(|s| s.to_string()).collect(),
        macs: macs.iter().map(|s| s.to_string()).collect(),
        vendor: None,
        platform: None,
        container_id: None,
        cloud: None,
        last_updated: ts,
    };

    let evidence = vec![
        // alpha: two self-reports of the same host → merge to one entity.
        mk(
            "sysinfo",
            "alpha",
            None,
            Some(host_id("alpha")),
            Some("alpha"),
            Some("alpha.demo.local"),
            &["10.0.0.10"],
            &["aa:bb:cc:00:00:01"],
        ),
        mk(
            "netlink",
            "alpha",
            None,
            Some(host_id("alpha")),
            Some("alpha"),
            None,
            &["10.0.0.10"],
            &["aa:bb:cc:00:00:01"],
        ),
        // beta & gamma: SAME hostname, DIFFERENT host_ids → must stay 2 entities.
        mk(
            "sysinfo",
            "beta",
            None,
            Some(host_id("beta")),
            Some("shared"),
            None,
            &["10.0.0.11"],
            &[],
        ),
        mk(
            "sysinfo",
            "gamma",
            None,
            Some(host_id("gamma")),
            Some("shared"),
            None,
            &["10.0.0.12"],
            &[],
        ),
        // delta self-report + a netring-observed asset sharing MAC+IP → merge.
        mk(
            "sysinfo",
            "delta",
            None,
            Some(host_id("delta")),
            Some("delta"),
            None,
            &["10.0.0.20"],
            &["aa:bb:cc:00:00:02"],
        ),
        mk(
            "netring",
            "aa-bb-cc-00-00-02",
            Some("netring"),
            None,
            None,
            Some("printer.demo.local"),
            &["10.0.0.20"],
            &["aa:bb:cc:00:00:02"],
        ),
    ];

    let names = vec![
        NameObservation {
            observer: "netring".into(),
            ip: "10.0.0.10".into(),
            name: "alpha.demo.local".into(),
            provenance: "dns_a".into(),
            last_seen: ts,
        },
        NameObservation {
            observer: "netring".into(),
            ip: "10.0.0.20".into(),
            name: "printer.demo.local".into(),
            provenance: "mdns".into(),
            last_seen: ts,
        },
    ];

    (evidence, names)
}

/// Feed the synthetic set into the engine's channel, then re-send it on a slow
/// cadence (bumping `last_updated` to the wall clock) so the evidence stays
/// TTL-live for a long-running demo. Identifying content never changes, so
/// entity ids are stable.
pub async fn feed(tx: mpsc::Sender<EvidenceMsg>, mut shutdown: watch::Receiver<bool>) {
    info!("demo mode: feeding synthetic evidence");
    let mut refresh = tokio::time::interval(Duration::from_secs(30));
    loop {
        let now = current_timestamp_millis();
        let (evidence, names) = synthetic();
        for mut ev in evidence {
            ev.last_updated = now;
            if tx.send(EvidenceMsg::Host(Box::new(ev))).await.is_err() {
                return;
            }
        }
        for mut obs in names {
            obs.last_seen = now;
            if tx.send(EvidenceMsg::Name(obs)).await.is_err() {
                return;
            }
        }
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
            _ = refresh.tick() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CorrelatorConfig;
    use crate::engine::{CorrelatorState, EntityOp};

    fn run(
        evidence: &[HostEvidence],
        names: &[NameObservation],
    ) -> Vec<zensight_common::HostEntity> {
        let mut state = CorrelatorState::new(CorrelatorConfig::default());
        for ev in evidence {
            state.apply(EvidenceMsg::Host(Box::new(ev.clone())));
        }
        for obs in names {
            state.apply(EvidenceMsg::Name(obs.clone()));
        }
        let _ = state.recompute(DEMO_BASE_TS + 1);
        state.current_entities()
    }

    #[test]
    fn synthetic_scenario_shapes() {
        let (evidence, names) = synthetic();
        let entities = run(&evidence, &names);
        // alpha (2 self-reports) + beta + gamma + delta/asset merge = 4 entities.
        assert_eq!(entities.len(), 4, "expected 4 demo entities");
        // The alpha entity has two members.
        assert!(
            entities.iter().any(|e| e.members.len() == 2
                && e.members.iter().any(|m| m.sensor == "sysinfo")
                && e.members.iter().any(|m| m.sensor == "netlink")),
            "alpha must merge sysinfo + netlink"
        );
        // beta and gamma share a hostname but stay separate (guard).
        let shared: Vec<_> = entities
            .iter()
            .filter(|e| e.hostname.as_deref() == Some("shared"))
            .collect();
        assert_eq!(
            shared.len(),
            2,
            "same-hostname/different-host_id → 2 entities"
        );
        // Name enrichment landed on the IPs.
        assert!(
            entities
                .iter()
                .any(|e| e.names.iter().any(|n| n.name == "printer.demo.local")),
            "printer name must enrich delta's entity"
        );
    }

    #[test]
    fn synthetic_is_deterministic() {
        let (evidence, names) = synthetic();
        let a = run(&evidence, &names);
        // Shuffle the input order.
        let mut ev2 = evidence.clone();
        ev2.reverse();
        let b = run(&ev2, &names);
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "demo output must be byte-identical regardless of input order"
        );
    }

    #[test]
    fn feed_produces_ops_through_the_real_pipeline() {
        // Sanity: the same synthetic set drives the engine's recompute to ops.
        let (evidence, names) = synthetic();
        let mut state = CorrelatorState::new(CorrelatorConfig::default());
        for ev in &evidence {
            state.apply(EvidenceMsg::Host(Box::new(ev.clone())));
        }
        for obs in &names {
            state.apply(EvidenceMsg::Name(obs.clone()));
        }
        let ops = state.recompute(DEMO_BASE_TS + 1);
        assert!(ops.iter().any(|o| matches!(o, EntityOp::Upsert(_))));
    }
}
