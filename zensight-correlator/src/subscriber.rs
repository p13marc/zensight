//! Zenoh subscribers feeding the engine.
//!
//! Two AdvancedSubscribers on the evidence keyspace (host claims + name
//! observations). Each decodes its samples and forwards an [`EvidenceMsg`]
//! into the engine's mpsc.
//!
//! The evidence subscribers use `history()` (+ `detect_late_publishers`) so a
//! freshly-started correlator immediately receives the sensors' cached
//! self-reports — this is what makes the entity view stateless-recomputable
//! across restarts. Like the frontend's telemetry subscriber, they use an
//! **unbounded** channel so the startup history burst can't deadlock the session.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::{info, trace, warn};
use zenoh::Session;
use zenoh::sample::{Sample, SampleKind};
use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};
use zensight_common::{
    HostEvidence, NameObservation, OperatorAssertion, all_assertion_wildcard,
    all_evidence_wildcard, all_name_evidence_wildcard,
};

use crate::engine::EvidenceMsg;

/// Decode a payload as JSON first, then CBOR.
fn decode<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Option<T> {
    serde_json::from_slice(payload)
        .ok()
        .or_else(|| ciborium::from_reader(payload).ok())
}

/// Declare the evidence subscribers and run the forwarding loop until shutdown.
///
pub async fn run(
    session: Arc<Session>,
    tx: mpsc::Sender<EvidenceMsg>,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Host evidence. `all_evidence_wildcard()` also matches the names subtree, so
    // this subscriber deliberately skips `/names/` keys (handled by the dedicated
    // names subscriber below) to avoid double-processing.
    let host_key = all_evidence_wildcard();
    info!(key = %host_key, "subscribing to host evidence");
    let host_sub = session
        .declare_subscriber(&host_key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare host-evidence subscriber: {e}"))?;

    let names_key = all_name_evidence_wildcard();
    info!(key = %names_key, "subscribing to name observations");
    let names_sub = session
        .declare_subscriber(&names_key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare name-evidence subscriber: {e}"))?;

    // The catalog's own operator assertions (#473). Subscribing to what we
    // publish is not a loop: it is what makes a *restarted* correlator (or a
    // second one, or one recovering from a router storage) re-learn the
    // operator's decisions through the same path as everything else, keeping the
    // catalog a pure function of bus state (RFC 06 §5). `history()` is what makes
    // the re-seed happen at all.
    let assertion_key = all_assertion_wildcard();
    info!(key = %assertion_key, "subscribing to operator assertions");
    let assertion_sub = session
        .declare_subscriber(&assertion_key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare assertion subscriber: {e}"))?;

    // Alerts (#923). Every producer's alert family across the fleet — LWW, a
    // handful of documents per host, which is what made a subscription inside
    // the catalog the right call rather than a separate service.
    let alert_key = zensight_common::keyexpr::all_alerts_wildcard();
    info!(key = %alert_key, "subscribing to sensor alerts");
    let alert_sub = session
        .declare_subscriber(&alert_key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare alert subscriber: {e}"))?;

    // The catalog's own acks and silences (#922), for the same reason as
    // assertions above: a restarted correlator re-learns the operator's
    // decisions through the same path a live one takes.
    let ack_key = zensight_common::keyexpr::all_acks_wildcard();
    info!(key = %ack_key, "subscribing to acknowledgements");
    let ack_sub = session
        .declare_subscriber(&ack_key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare ack subscriber: {e}"))?;

    let silence_key = zensight_common::keyexpr::all_silences_wildcard();
    info!(key = %silence_key, "subscribing to silences");
    let silence_sub = session
        .declare_subscriber(&silence_key)
        .with(flume::unbounded())
        .history(HistoryConfig::default().detect_late_publishers())
        .recovery(RecoveryConfig::default())
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare silence subscriber: {e}"))?;

    // Liveliness (#923). The input to `down`, and the only evidence that a
    // machine which stopped answering is the *cause* of what its guests are
    // reporting — a dead host publishes no alert of its own. `history(true)`
    // delivers the currently-alive tokens through this same subscriber, so the
    // initial state and live transitions share one ordered path.
    let liveliness_key = zensight_common::keyexpr::all_liveliness_wildcard();
    info!(key = %liveliness_key, "subscribing to sensor liveliness");
    let liveliness_sub = session
        .liveliness()
        .declare_subscriber(liveliness_key.as_str())
        .history(true)
        .await
        .map_err(|e| anyhow::anyhow!("failed to declare liveliness subscriber: {e}"))?;

    info!("evidence subscribers ready");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("shutdown signal received, stopping subscribers");
                    break;
                }
            }
            sample = host_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_evidence(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "host-evidence recv error"),
                }
            }
            sample = names_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_name(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "name-evidence recv error"),
                }
            }
            sample = assertion_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_assertion(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "assertion recv error"),
                }
            }
            sample = alert_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_alert(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "alert recv error"),
                }
            }
            sample = ack_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_ack(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "ack recv error"),
                }
            }
            sample = silence_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_silence(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "silence recv error"),
                }
            }
            sample = liveliness_sub.recv_async() => {
                match sample {
                    Ok(sample) => handle_liveliness(&sample, &tx).await,
                    Err(e) => warn!(error = %e, "liveliness recv error"),
                }
            }
        }
    }

    Ok(())
}

/// Whether a sample is a live (non-tombstone) `PUT`.
fn is_put(sample: &Sample) -> bool {
    sample.kind() == SampleKind::Put
}

/// Extract `(sensor, device)` from a v1 device-evidence key (used to resolve
/// a tombstone into the claim it withdraws).
///
/// `v1/<origin>/state/<sensor>/evidence/device/<device>` — base-relative,
/// because the session namespace already stripped the base on ingress (#466).
///
/// Refined through the registry's parse direction rather than by positional
/// chunk matching: `refine_key` resolves the producer and the registered
/// subject, and `common_state()` names the RFC 06 evidence family — so the
/// chunk positions come from the registry, not from counting (RFC 08 §1),
/// and an unregistered subject "does not exist" here either.
///
/// A `…/evidence/self` tombstone carries only the origin — the store keys
/// claims by the payload's source, so it just ages out by TTL instead.
fn parse_host_evidence_key(key: &str) -> Option<(String, String)> {
    let (_, sensor, subject) = zensight_common::keyexpr::refine_key(key)?;
    match subject.common_state()? {
        zenkey::CommonState::EvidenceDevice { device } => Some((sensor, device.to_string())),
        _ => None,
    }
}

/// Whether a key under `evidence/**` is a *host identity* claim — the only
/// thing [`handle_host`] may decode.
///
/// Dispatches on the refined subject, not on a substring of the key. That
/// distinction is load-bearing. The selector this subscriber uses is
/// `all_evidence_wildcard()` = `v1/*/state/*/evidence/**` (a hand-spelled
/// union of three families), so **every** evidence subject reaches here,
/// including ones that are not host identity at all. The previous filter
/// excluded exactly one subtree by substring and fed everything else to
/// `decode::<HostEvidence>`.
///
/// `HostEvidence` carries no `deny_unknown_fields` and requires only `sensor`
/// and `source`, both of which a relationship claim naturally has (#915). So
/// the moment relation evidence started publishing, every one of those
/// documents would have decoded cleanly as a host-identity claim and been
/// inserted into the `EvidenceStore` — where it becomes input to the
/// union-find that decides which machines are the same machine. No error, no
/// warning: entities silently fusing or splitting, in the one component whose
/// entire job is being deterministic.
///
/// An allow-list of subjects makes the next family added under `evidence/**`
/// inert here by default, which is the safe direction to fail.
fn is_host_identity_subject(key: &str) -> bool {
    let Some((_, _, subject)) = zensight_common::keyexpr::refine_key(key) else {
        return false;
    };
    matches!(
        subject.common_state(),
        Some(zenkey::CommonState::EvidenceSelf) | Some(zenkey::CommonState::EvidenceDevice { .. })
    )
}

/// Route one sample from the `evidence/**` subscription to the handler for
/// its subject.
///
/// One subscription, several families: `all_evidence_wildcard()` is a
/// hand-spelled union (`v1/*/state/*/evidence/**`), so identity claims, name
/// observations and relationship claims all arrive here. Routing on the
/// refined subject — never on a key substring — is what keeps a family added
/// later inert rather than silently mis-decoded (#915).
async fn handle_evidence(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    if is_relation_subject(key) {
        handle_relation(sample, tx).await;
        return;
    }
    handle_host(sample, tx).await;
}

/// Whether a key is a relationship claim (`evidence/relation/{relation_id}`).
///
/// Framework vocabulary since RFC 06 v1.30, so this is one `common_state()`
/// call over the refined subject — it used to refine app-side through
/// `ZensightState`, because `zenkey::CommonState` could not yet say it.
fn is_relation_subject(key: &str) -> bool {
    let Some((_, _, subject)) = zensight_common::keyexpr::refine_key(key) else {
        return false;
    };
    matches!(
        subject.common_state(),
        Some(zenkey::CommonState::EvidenceRelation { .. })
    )
}

/// Extract `(origin, sensor, relation_id)` from a relationship-claim key.
fn parse_relation_key(key: &str) -> Option<(String, String, String)> {
    let (parsed, sensor, subject) = zensight_common::keyexpr::refine_key(key)?;
    match subject.common_state()? {
        zenkey::CommonState::EvidenceRelation { relation_id } => {
            Some((parsed.origin.to_string(), sensor, relation_id.to_string()))
        }
        _ => None,
    }
}

/// A relationship claim, or its tombstone.
///
/// The **origin is carried through** from the key, not read from the payload.
/// A claim says which sensor made it; only the key says which host that sensor
/// was running on, and the catalog needs both to decide whether an edge still
/// has an observer when one host goes quiet.
async fn handle_relation(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    let Some((origin, sensor, relation_id)) = parse_relation_key(key) else {
        trace!(key = %key, "ignoring malformed relation-evidence key");
        return;
    };
    if !is_put(sample) {
        let _ = tx
            .send(EvidenceMsg::RemoveRelation {
                sensor,
                origin,
                relation_id,
            })
            .await;
        return;
    }
    match decode::<zensight_common::relation::RelationshipEvidence>(&sample.payload().to_bytes()) {
        Some(ev) => {
            let _ = tx
                .send(EvidenceMsg::Relation {
                    origin,
                    ev: Box::new(ev),
                })
                .await;
        }
        None => warn!(key = %key, "failed to decode RelationshipEvidence"),
    }
}

async fn handle_host(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    // Only `evidence/self` and `evidence/device/{device}` are host identity.
    // `evidence/names/**` has its own subscriber; `evidence/relation/**` went
    // to `handle_relation` above and must never reach the identity store.
    if !is_host_identity_subject(key) {
        return;
    }
    if !is_put(sample) {
        // Evidence tombstone (a `Delete`): drop that claim from the store now
        // instead of waiting for it to age out by TTL.
        if let Some((sensor, source)) = parse_host_evidence_key(key) {
            let _ = tx.send(EvidenceMsg::RemoveHost { sensor, source }).await;
        } else {
            trace!(key = %key, "ignoring malformed host-evidence tombstone");
        }
        return;
    }
    // The origin comes from the key, not the payload: only the key says which
    // host the publishing sensor ran on, and the topology graph needs it to
    // attribute an observed-device claim to a segment (#917).
    let origin = zensight_common::keyexpr::refine_key(key)
        .map(|(parsed, _, _)| parsed.origin.to_string())
        .unwrap_or_default();
    match decode::<HostEvidence>(&sample.payload().to_bytes()) {
        Some(ev) => {
            let _ = tx
                .send(EvidenceMsg::Host {
                    origin,
                    ev: Box::new(ev),
                })
                .await;
        }
        None => warn!(key = %key, "failed to decode HostEvidence"),
    }
}

/// An assertion put, or its tombstone (an `unlink` retiring the `link` it
/// supersedes — the id comes from the key, since a Delete carries no payload).
async fn handle_assertion(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    if !is_put(sample) {
        if let Some(id) = key.rsplit('/').next().filter(|id| !id.is_empty()) {
            let _ = tx
                .send(EvidenceMsg::RemoveAssertion { id: id.to_string() })
                .await;
        }
        return;
    }
    match decode::<OperatorAssertion>(&sample.payload().to_bytes()) {
        Some(a) => {
            let _ = tx.send(EvidenceMsg::Assert(a)).await;
        }
        None => warn!(key = %key, "failed to decode OperatorAssertion"),
    }
}

/// A sensor alert (#923).
///
/// The ref is built from the **key**, never the payload: origin and producer
/// are key chunks, and an `Alert`'s `source` is the polled device for a proxy
/// sensor (#883) — so the document alone cannot say which host published it,
/// which is exactly the join an incident needs.
async fn handle_alert(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    let Some(r) = alert_ref_from_key(key) else {
        warn!(key = %key, "alert key did not parse into an alert ref");
        return;
    };
    // A tombstone and a `Resolved` document mean the same thing to an
    // incident, and both arrive: the sensor publishes the resolution, then
    // deletes the key. Either removes the member.
    let alert = is_put(sample)
        .then(|| decode::<zensight_common::alert::Alert>(&sample.payload().to_bytes()))
        .flatten();
    if is_put(sample) && alert.is_none() {
        warn!(key = %key, "failed to decode Alert");
        return;
    }
    let _ = tx
        .send(EvidenceMsg::Alert {
            r: Box::new(r),
            alert: alert.map(Box::new),
        })
        .await;
}

/// Build an [`AlertRef`] from a base-relative alert key.
///
/// Structural, not positional: the registry knows where an origin and a
/// producer live in a key, and re-deriving that with `split('/')` is exactly
/// what RFC 08 §1 exists to delete.
fn alert_ref_from_key(key: &str) -> Option<zensight_common::alert::AlertRef> {
    let parsed = zensight_common::keyexpr::parse_key(key)?;
    let origin = match &parsed.origin {
        zenkey::grammar::Origin::Host(h) => h.as_str().to_string(),
        // A service origin publishes no per-sensor alerts; if one ever did, an
        // incident could not attribute it to a host anyway.
        zenkey::grammar::Origin::Service(_) => return None,
    };
    let producer = parsed.producer()?.name().to_string();
    let alert_key = parsed.subject.last()?.to_string();
    if alert_key.is_empty() {
        return None;
    }
    zensight_common::alert::AlertRef::parse(&format!("{origin}.{producer}.{alert_key}")).ok()
}

async fn handle_ack(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    if !is_put(sample) {
        if let Some(r) = key
            .rsplit('/')
            .next()
            .and_then(|c| zensight_common::alert::AlertRef::parse(c).ok())
        {
            let _ = tx.send(EvidenceMsg::RemoveAck(Box::new(r))).await;
        }
        return;
    }
    match decode::<zensight_common::ack::AlertAck>(&sample.payload().to_bytes()) {
        Some(a) => {
            let _ = tx.send(EvidenceMsg::Ack(Box::new(a))).await;
        }
        None => warn!(key = %key, "failed to decode AlertAck"),
    }
}

async fn handle_silence(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    if !is_put(sample) {
        if let Some(id) = key.rsplit('/').next().filter(|id| !id.is_empty()) {
            let _ = tx
                .send(EvidenceMsg::RemoveSilence { id: id.to_string() })
                .await;
        }
        return;
    }
    match decode::<zensight_common::silence::Silence>(&sample.payload().to_bytes()) {
        Some(s) => {
            let _ = tx.send(EvidenceMsg::Silence(Box::new(s))).await;
        }
        None => warn!(key = %key, "failed to decode Silence"),
    }
}

/// A liveliness token appearing or vanishing (#923).
///
/// One token per *sensor*, and a host runs several — so an origin is down only
/// when the last of its tokens goes, which the engine's set handles by keying
/// on the origin: any surviving sensor's Put removes it from `dead_origins`.
/// A host that has genuinely stopped loses every token, which is the case that
/// matters.
async fn handle_liveliness(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    let Some(parsed) = zensight_common::keyexpr::parse_key(key) else {
        return;
    };
    let zenkey::grammar::Origin::Host(h) = &parsed.origin else {
        return;
    };
    let _ = tx
        .send(EvidenceMsg::Liveliness {
            origin: h.as_str().to_string(),
            alive: is_put(sample),
        })
        .await;
}

async fn handle_name(sample: &Sample, tx: &mpsc::Sender<EvidenceMsg>) {
    let key = sample.key_expr().as_str();
    if !is_put(sample) {
        return;
    }
    match decode::<NameObservation>(&sample.payload().to_bytes()) {
        Some(obs) => {
            let _ = tx.send(EvidenceMsg::Name(obs)).await;
        }
        None => warn!(key = %key, "failed to decode NameObservation"),
    }
}

#[cfg(test)]
mod tests {
    use super::{is_host_identity_subject, parse_host_evidence_key};

    /// The identity handler accepts exactly the two host-identity subjects and
    /// nothing else under `evidence/**`.
    ///
    /// This is the guard on a silent-corruption path, so it is worth stating
    /// what it prevents. The subscriber's selector is
    /// `v1/*/state/*/evidence/**`, so every evidence subject arrives here.
    /// `HostEvidence` has no `deny_unknown_fields` and requires only `sensor`
    /// and `source` — which a `RelationshipEvidence` document also has. Fed to
    /// `decode::<HostEvidence>`, a relation claim would deserialize cleanly and
    /// be inserted into the store that feeds the identity union-find: entities
    /// fusing or splitting with no error and no log line. The old filter
    /// excluded one subtree by substring, so this held only for as long as
    /// `evidence/**` had exactly three families in it.
    #[test]
    fn only_self_and_device_evidence_reach_the_identity_store() {
        for k in [
            "v1/h-3fa9c2d41b7e/state/netlink/evidence/self",
            "v1/h-3fa9c2d41b7e/state/netlink/evidence/device/host1",
            "v1/h-3fa9c2d41b7e/state/netring/evidence/device/aa-bb-cc-00-00-02",
        ] {
            assert!(is_host_identity_subject(k), "must be accepted: {k}");
        }
        for k in [
            // Its own subscriber owns this one.
            "v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-5",
            // #915: graph input, never identity input. The whole point.
            "v1/h-3fa9c2d41b7e/state/pve/evidence/relation/r-0123456789abcdef",
            "v1/h-3fa9c2d41b7e/state/container/evidence/relation/r-0123456789abcdef",
            "v1/h-3fa9c2d41b7e/state/probe/evidence/relation/r-0123456789abcdef",
            "v1/h-3fa9c2d41b7e/state/netlink/evidence/relation/r-0123456789abcdef",
            // Not a subject at all.
            "v1/h-3fa9c2d41b7e/state/netlink/evidence/device/host1/extra",
            "not/a/key",
        ] {
            assert!(!is_host_identity_subject(k), "must be rejected: {k}");
        }
    }

    /// The trap this guards, demonstrated rather than asserted in the abstract:
    /// a real relation document really does decode as `HostEvidence`.
    ///
    /// If this ever fails because `RelationshipEvidence` stopped carrying
    /// `sensor`/`source`, the guard above is still correct and this test should
    /// be deleted, not "fixed" — but until then it is the reason the guard is
    /// structural instead of a field check.
    #[test]
    fn a_relation_document_would_have_decoded_as_host_evidence() {
        use zensight_common::relation::{EndpointClaim, RelationKind, RelationshipEvidence};
        let ev = RelationshipEvidence {
            sensor: "pve".into(),
            source: "node1".into(),
            kind: RelationKind::Hosts,
            from: EndpointClaim::host("h-0123456789ab"),
            to: EndpointClaim::device("101"),
            attrs: Default::default(),
            last_updated: 1,
        };
        let bytes = serde_json::to_vec(&ev).unwrap();
        let as_host: Result<zensight_common::HostEvidence, _> = serde_json::from_slice(&bytes);
        assert!(
            as_host.is_ok(),
            "the premise of the guard: a relation claim IS a structurally valid \
             HostEvidence, so only the subject can tell them apart"
        );
        assert_eq!(as_host.unwrap().sensor, "pve");
    }

    #[test]
    fn parses_host_evidence_key() {
        assert_eq!(
            parse_host_evidence_key("v1/h-3fa9c2d41b7e/state/netlink/evidence/device/host1"),
            Some(("netlink".to_string(), "host1".to_string()))
        );
        // A MAC-slug device (third-party evidence) stays a single chunk.
        assert_eq!(
            parse_host_evidence_key(
                "v1/h-3fa9c2d41b7e/state/netring/evidence/device/aa-bb-cc-00-00-02"
            ),
            Some(("netring".to_string(), "aa-bb-cc-00-00-02".to_string()))
        );
        // The names subtree is not a device key.
        assert_eq!(
            parse_host_evidence_key("v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-5"),
            None
        );
        // A self-evidence tombstone carries only the origin — ages out by TTL.
        assert_eq!(
            parse_host_evidence_key("v1/h-3fa9c2d41b7e/state/netlink/evidence/self"),
            None
        );
        // Trailing chunks make it malformed.
        assert_eq!(
            parse_host_evidence_key("v1/h-3fa9c2d41b7e/state/netlink/evidence/device/host1/extra"),
            None
        );
    }
}
