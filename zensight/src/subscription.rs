use iced::Subscription;
use iced::keyboard::{self, Key, key};

use zenoh::sample::SampleKind;
use zenoh_ext::{AdvancedSubscriberBuilderExt, HistoryConfig, RecoveryConfig};

use zenkey::{Class, ClassOrPlane, CommonState, Origin};
use zensight_common::ZensightState;
use zensight_common::keyexpr::{parse_key, refine_key};
use zensight_common::{
    Alert, ErrorReport, HealthSnapshot, HostEntity, LinkProfile, SensorInfo, TelemetryPoint,
    ZenohConfig, all_entity_wildcard, all_telemetry_wildcard, decode_auto, entities_query_key,
};

use crate::message::{Message, Reading};

// The fleet liveliness selectors used to be two hand-spelled consts here — a
// second spelling of `all_liveliness_wildcard()` / `all_device_liveliness_wildcard()`,
// which is exactly the drift the keyspace crate exists to prevent. #466 made
// the duplication fatal (a full key on a namespaced session matches nothing),
// so they are gone: `liveliness_exprs` calls the builders like everything else.

/// Everything the telemetry subscription is keyed on (#364).
///
/// `Subscription::run_with` restarts the stream whenever this value changes
/// (`Hash`/`Eq`), so a scope or profile edit tears the session down and
/// reconnects exactly like a connection-settings change does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LinkConfig {
    /// How to join the Zenoh mesh.
    pub zenoh: ZenohConfig,
    /// Telemetry subscription key expressions. Empty ⇒ the full
    /// [`all_telemetry_wildcard`] class selector
    /// (`zensight/v1/*/telemetry/**`).
    pub scope: Vec<String>,
    /// Standard (history/recovery) vs. constrained (plain, store back-fill).
    pub profile: LinkProfile,
    /// **Focus mode** (#476): when set, subscribe to this one origin
    /// (`h-<12hex>`) instead of the fleet — telemetry, state, alerts and
    /// liveliness all narrow to it. Runtime-only; not persisted.
    ///
    /// This is the clearest bandwidth win the v1 keyspace unlocked: the origin
    /// is a single chunk at a fixed position, so "one host's whole data plane
    /// and nothing else" is expressible at all (RFC 09 §1). Overrides `scope`.
    pub focus: Option<String>,
}

/// The telemetry key expressions to subscribe to.
///
/// Focus mode wins over the configured scope: the point of focusing is that
/// *nothing* but the chosen host crosses the link, and a scope entry naming
/// another origin would defeat that. Otherwise: the configured scope, or the
/// full telemetry class selector. The state/entity subscribers are unaffected
/// by `scope` — classes are disjoint (D3) — but they *are* narrowed by focus.
fn effective_scopes(config: &LinkConfig) -> Vec<String> {
    match &config.focus {
        Some(origin) => vec![zensight_common::keyexpr::origin_telemetry_wildcard(origin)],
        None if config.scope.is_empty() => vec![all_telemetry_wildcard()],
        None => config.scope.clone(),
    }
}

/// The state-plane selector: one origin under focus, the fleet otherwise.
fn state_selector(config: &LinkConfig) -> String {
    match &config.focus {
        Some(origin) => zensight_common::keyexpr::origin_state_wildcard(origin),
        None => zensight_common::keyexpr::all_state_wildcard(),
    }
}

/// The events-plane selector (#536): one origin under focus, else fleet.
fn events_selector(config: &LinkConfig) -> String {
    match &config.focus {
        Some(origin) => zensight_common::keyexpr::origin_events_wildcard(origin),
        None => zensight_common::keyexpr::all_events_wildcard(),
    }
}

/// The alert seed selector: one origin under focus, the fleet otherwise.
fn alerts_selector(config: &LinkConfig) -> String {
    match &config.focus {
        Some(origin) => zensight_common::keyexpr::origin_alerts_wildcard(origin),
        None => zensight_common::all_alerts_wildcard(),
    }
}

/// The sensor/device liveliness selectors: one origin under focus, else fleet.
fn liveliness_exprs(config: &LinkConfig) -> (String, String) {
    match &config.focus {
        Some(origin) => (
            zensight_common::keyexpr::origin_liveliness_expr(origin),
            zensight_common::keyexpr::origin_device_liveliness_expr(origin),
        ),
        None => (
            zensight_common::all_liveliness_wildcard(),
            zensight_common::all_device_liveliness_wildcard(),
        ),
    }
}

/// Create a subscription that connects to Zenoh and receives telemetry.
pub fn zenoh_subscription(config: LinkConfig) -> Subscription<Message> {
    Subscription::run_with(config, move |config| {
        let config = config.clone();
        async_stream::stream! {
            // Signal that we're attempting to connect
            yield Message::Connecting;

            // Connect to Zenoh
            let session = match connect_zenoh(&config.zenoh).await {
                Ok(session) => {
                    yield Message::Connected(Some(std::sync::Arc::new(session.clone())));
                    session
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to connect to Zenoh");
                    yield Message::Disconnected(e.to_string());
                    // Wait before the stream ends (subscription will restart)
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    return;
                }
            };

            // ORDERING MATTERS HERE. The telemetry AdvancedSubscriber is
            // deliberately declared LAST, after every one-shot seed query below.
            // Its channel is bounded, and on declare the sensors' advanced
            // publishers deliver their whole history caches at once; if it is
            // declared first, the channel fills before the drain loop starts,
            // the session's RX task blocks on the full channel, and the seed
            // queries below can never receive their replies — the GUI then sits
            // frozen with no telemetry until the query timeout, and wakes up to
            // a multi-second backlog (observed live: alerts-seed query 0 timing
            // out at exactly 10s with every reply arriving as "unknown Query").

            // v1 (epic #453): everything stateful rides ONE class selector —
            // health, errors, sensor registrations, device liveness docs,
            // alerts, evidence, stream docs are all
            // `zensight/v1/<origin>/state/<producer>/<subject…>` (RFC 04).
            // The telemetry selector cannot double-deliver it (class chunks
            // differ, design property D3), and the verbatim planes
            // (@rpc/@media/@blob) are structurally invisible (D2).
            // Focus mode narrows this to one origin (#476); the catalog entity
            // subscriber below deliberately stays fleet-wide, because it is what
            // lets an operator un-focus and focus onto a *different* host.
            let control = session
                .declare_subscriber(state_selector(&config))
                .with(flume::unbounded())
                .await
                .ok();
            if control.is_none() {
                tracing::warn!("Failed to create state-plane subscriber (health/alerts)");
            }

            // Events plane (#536): append-only records (SNMP traps today).
            // A startup GET on the same selector backfills history when a
            // Zenoh storage is aligned on the events tree; live records then
            // stream through the subscriber.
            let events_sub = session
                .declare_subscriber(&events_selector(&config))
                .with(flume::unbounded())
                .await
                .ok();
            if events_sub.is_none() {
                tracing::warn!("Failed to create events subscriber");
            }
            let event_backfill: Vec<Message> = match session
                .get(&events_selector(&config))
                .timeout(std::time::Duration::from_secs(3))
                .await
            {
                Ok(replies) => {
                    let mut msgs = Vec::new();
                    while let Ok(reply) = replies.recv_async().await {
                        if let Ok(sample) = reply.result() {
                            let payload = sample.payload().to_bytes();
                            if let Some(msg) =
                                decode_sample(sample.key_expr().as_str(), &payload)
                            {
                                msgs.push(msg);
                            }
                        }
                    }
                    msgs
                }
                Err(_) => Vec::new(),
            };
            for msg in event_backfill {
                yield msg;
            }

            // Entity subscriber (#306): catalog host-entity docs live under
            // `zensight/v1/@catalog/state/entity/*` — the `*`-origin
            // telemetry/state selectors never match the verbatim `@catalog`
            // chunk (D4), so the catalog is named explicitly. Plain
            // subscriber, unbounded — puts are HostEntity docs, deletes are
            // tombstones.
            let entity_sub = session
                .declare_subscriber(&all_entity_wildcard())
                .with(flume::unbounded())
                .await
                .ok();
            if entity_sub.is_none() {
                tracing::warn!("Failed to create entity subscriber (host entities)");
            }

            let (sensor_liveliness_expr, device_liveliness_expr) = liveliness_exprs(&config);

            // Subscribe to sensor liveliness tokens. `history(true)` delivers
            // the currently-alive tokens through this same subscriber (#520):
            // the initial state and live transitions share one ordered path,
            // so the old seed-GET's blocking timeout and its token-appears-
            // between-get-and-subscribe ordering hazard are both gone.
            let sensor_liveliness = match session
                .liveliness()
                .declare_subscriber(sensor_liveliness_expr.as_str())
                .history(true)
                .await
            {
                Ok(sub) => Some(sub),
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to create sensor liveliness subscriber");
                    None
                }
            };

            // Subscribe to device liveliness tokens (same history(true) seed).
            let device_liveliness = match session
                .liveliness()
                .declare_subscriber(device_liveliness_expr.as_str())
                .history(true)
                .await
            {
                Ok(sub) => Some(sub),
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to create device liveliness subscriber");
                    None
                }
            };

            // The remaining one-shot seeds are bounded to a short timeout:
            // they run before telemetry drains, so a dead/slow responder must
            // not hold up the whole GUI for zenoh's default 10s.
            let seed_timeout = std::time::Duration::from_secs(3);

            // Late-joiner alert seed (v1, RFC 05 §4): a plain GET on the alert
            // state selector, answered storage-shaped — one firing alert per
            // reply on its concrete key — by producers and/or a router
            // storage. (The state subscriber above is already declared, so an
            // alert firing during this get is not lost.) This GET stays a GET
            // (#520): alerts/entities are durable, storage/queryable-backed
            // state (RFC 04 §3.5/H7) — the queryable is authoritative for
            // them, unlike liveliness, which zenoh itself can replay.
            if let Ok(replies) = session
                .get(alerts_selector(&config))
                .target(zenoh::query::QueryTarget::All)
                .timeout(seed_timeout)
                .await
            {
                let mut seeded = Vec::new();
                while let Ok(reply) = replies.recv_async().await {
                    if let Ok(sample) = reply.result()
                        && let Ok(alert) =
                            zensight_common::decode_auto::<Alert>(&sample.payload().to_bytes())
                    {
                        seeded.push(alert);
                    }
                }
                if !seeded.is_empty() {
                    yield Message::AlertsSeed(seeded);
                }
            }

            // Late-joiner entity seed (#306): fetch the correlator's current
            // host-entity set so a GUI opened mid-session groups by host
            // immediately. Absent correlator ⇒ no replies ⇒ empty store ⇒
            // degraded per-source path, by construction.
            if let Ok(replies) = session
                .get(entities_query_key())
                .target(zenoh::query::QueryTarget::All)
                .timeout(seed_timeout)
                .await
            {
                let mut seeded = Vec::new();
                while let Ok(reply) = replies.recv_async().await {
                    if let Ok(sample) = reply.result()
                        && let Ok(entity) = zensight_common::decode_auto::<HostEntity>(
                            &sample.payload().to_bytes(),
                        )
                    {
                        seeded.push(entity);
                    }
                }
                if !seeded.is_empty() {
                    yield Message::EntitySeed(seeded);
                }
            }

            // Only now — with every blocking one-shot done and the drain loop
            // next — subscribe to telemetry. In the Standard profile each scope
            // gets an AdvancedSubscriber:
            // - history(): cached samples from publishers on subscription
            // - detect_late_publishers(): history from publishers appearing later
            // - recovery(): automatic recovery of missed samples
            // In the Constrained profile (#364) each scope gets a PLAIN
            // subscriber instead — no history burst, no recovery traffic; the
            // app back-fills late-join state from the local redb store.
            //
            // All telemetry subscribers feed ONE shared unbounded flume channel
            // via callbacks (their handles are kept alive in `_telemetry_subs`).
            // The channel MUST be unbounded. With the default bounded (256)
            // channel, the history fetch deadlocks the whole session at declare
            // time: cached samples pour into the channel before anything drains
            // it, the channel fills, the session RX task blocks on it, and the
            // in-flight history queries can never finalize — `.await` below
            // never returns, no telemetry is ever delivered, and every other
            // query on the session times out (observed live with 5 sensors'
            // caches; "Advanced subscriber created" never logged). Memory stays
            // bounded in practice: publisher caches are finite and the drain
            // loop batches the backlog away as soon as the declare completes.
            let (telemetry_tx, telemetry_rx) = flume::unbounded::<zenoh::sample::Sample>();
            let scopes = effective_scopes(&config);
            let mut advanced_subs = Vec::new();
            let mut plain_subs = Vec::new();
            for key_expr in &scopes {
                match config.profile {
                    LinkProfile::Standard => {
                        let tx = telemetry_tx.clone();
                        match session
                            .declare_subscriber(key_expr)
                            .callback(move |sample| {
                                let _ = tx.send(sample);
                            })
                            .history(HistoryConfig::default().detect_late_publishers())
                            .recovery(RecoveryConfig::default())
                            .subscriber_detection()
                            .await
                        {
                            Ok(sub) => advanced_subs.push(sub),
                            Err(e) => {
                                tracing::error!(
                                    error = %e, key = %key_expr,
                                    "Failed to create advanced subscriber"
                                );
                                yield Message::Disconnected(e.to_string());
                                return;
                            }
                        }
                    }
                    LinkProfile::Constrained => {
                        let tx = telemetry_tx.clone();
                        match session
                            .declare_subscriber(key_expr)
                            .callback(move |sample| {
                                let _ = tx.send(sample);
                            })
                            .await
                        {
                            Ok(sub) => plain_subs.push(sub),
                            Err(e) => {
                                tracing::error!(
                                    error = %e, key = %key_expr,
                                    "Failed to create plain telemetry subscriber"
                                );
                                yield Message::Disconnected(e.to_string());
                                return;
                            }
                        }
                    }
                }
            }
            // Drop the original sender so the channel closes if every
            // subscriber is somehow gone (the clones live in the callbacks).
            drop(telemetry_tx);

            tracing::info!(
                profile = %config.profile,
                scopes = ?scopes,
                "Telemetry subscribers created"
            );

            // Process incoming samples from all subscriptions
            loop {
                tokio::select! {
                    // Telemetry subscriptions (all scopes share one channel).
                    // One awaited sample, then an opportunistic drain of
                    // whatever else is already queued: a startup history burst
                    // (or any streaming spike) becomes ONE batched message per
                    // iced update instead of thousands of per-sample updates
                    // starving the UI thread.
                    result = telemetry_rx.recv_async() => {
                        match result {
                            Ok(sample) => {
                                let mut telemetry: Vec<Reading> = Vec::new();
                                let mut others: Vec<Message> = Vec::new();
                                if let Some(msg) = sample_to_message(&sample) {
                                    push_sorted(msg, &mut telemetry, &mut others);
                                }
                                while telemetry.len() < TELEMETRY_BATCH_MAX {
                                    match telemetry_rx.try_recv() {
                                        Ok(s) => {
                                            if let Some(msg) = sample_to_message(&s) {
                                                push_sorted(msg, &mut telemetry, &mut others);
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                                match telemetry.len() {
                                    0 => {}
                                    1 => yield Message::TelemetryReceived(
                                        telemetry.pop().expect("len checked")),
                                    _ => yield Message::TelemetryBatch(telemetry),
                                }
                                for msg in others {
                                    yield msg;
                                }
                            }
                            Err(e) => {
                                // Only closes if every telemetry subscriber
                                // (and its callback sender) has been dropped.
                                tracing::error!(error = %e, "Telemetry channel closed");
                                yield Message::Disconnected(e.to_string());
                                return;
                            }
                        }
                    }

                    // Sensor liveliness subscription
                    result = async {
                        match &sensor_liveliness {
                            Some(sub) => sub.recv_async().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Ok(sample) = result {
                            let is_alive = sample.kind() == SampleKind::Put;
                            if let Some(msg) = parse_sensor_liveliness(sample.key_expr().as_str(), is_alive) {
                                yield msg;
                            }
                        }
                    }

                    // Device liveliness subscription
                    result = async {
                        match &device_liveliness {
                            Some(sub) => sub.recv_async().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Ok(sample) = result {
                            let is_alive = sample.kind() == SampleKind::Put;
                            if let Some(msg) = parse_device_liveliness(sample.key_expr().as_str(), is_alive) {
                                yield msg;
                            }
                        }
                    }

                    // State plane (v1): health / errors / registrations /
                    // alerts / device liveness / stream docs — plain puts and
                    // tombstone deletes on one class selector.
                    result = async {
                        match &control {
                            Some(sub) => sub.recv_async().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Ok(sample) = result {
                            let key = sample.key_expr().as_str();
                            if sample.kind() == SampleKind::Delete {
                                if let Some(msg) = parse_tombstone(key) {
                                    yield msg;
                                }
                            } else {
                                let payload = sample.payload().to_bytes();
                                if let Some(msg) = decode_sample(key, &payload) {
                                    yield msg;
                                }
                            }
                        }
                    }

                    // Events plane (#536): append-only records, puts only.
                    result = async {
                        match &events_sub {
                            Some(sub) => sub.recv_async().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Ok(sample) = result
                            && sample.kind() != SampleKind::Delete
                        {
                            let payload = sample.payload().to_bytes();
                            if let Some(msg) = decode_sample(sample.key_expr().as_str(), &payload) {
                                yield msg;
                            }
                        }
                    }

                    // Entity plane (#306): host-entity docs. Delete = tombstone
                    // → EntityRemoved; Put = HostEntity doc → EntityReceived.
                    result = async {
                        match &entity_sub {
                            Some(sub) => sub.recv_async().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Ok(sample) = result {
                            let key = sample.key_expr().as_str();
                            if sample.kind() == SampleKind::Delete {
                                if let Some(msg) = parse_tombstone(key) {
                                    yield msg;
                                }
                            } else {
                                let payload = sample.payload().to_bytes();
                                if let Some(msg) = decode_sample(key, &payload) {
                                    yield msg;
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

/// Cap on how many already-queued telemetry samples are folded into one
/// [`Message::TelemetryBatch`] per stream iteration. Bounds per-update latency
/// while still collapsing a startup history burst into a handful of updates.
const TELEMETRY_BATCH_MAX: usize = 512;

/// Decode one subscriber sample into a message (Delete = alert tombstone).
fn sample_to_message(sample: &zenoh::sample::Sample) -> Option<Message> {
    let key = sample.key_expr().as_str();
    if sample.kind() == SampleKind::Delete {
        parse_tombstone(key)
    } else {
        decode_sample(key, &sample.payload().to_bytes())
    }
}

/// Route a decoded message into the telemetry batch or the pass-through list.
fn push_sorted(msg: Message, telemetry: &mut Vec<Reading>, others: &mut Vec<Message>) {
    match msg {
        Message::TelemetryReceived(reading) => telemetry.push(reading),
        other => others.push(other),
    }
}

/// Parse a v1 sensor liveliness key:
/// `zensight/v1/<origin>/state/<producer>/alive` (RFC 04 §5). The origin is
/// carried as the instance discriminator (the legacy `source` slot).
///
/// `alive` is a **liveliness token**, not a data subject — the registry rejects
/// it as a subject chunk on purpose (RFC 03 §3) — so this stays structural.
fn parse_sensor_liveliness(key: &str, is_alive: bool) -> Option<Message> {
    let parsed = parse_key(key)?;
    // `@catalog/state/alive` is a service token, not a sensor.
    let Origin::Host(_) = parsed.origin else {
        return None;
    };
    if parsed.subject != ["alive"] {
        return None;
    }
    let protocol = parsed.producer?.name().to_string();
    let source = Some(parsed.origin.chunk().to_string());
    if is_alive {
        tracing::info!(protocol = %protocol, source = ?source, "Sensor came online");
        Some(Message::SensorOnline(protocol, source))
    } else {
        tracing::warn!(protocol = %protocol, source = ?source, "Sensor went offline");
        Some(Message::SensorOffline(protocol, source))
    }
}

/// Parse a v1 device liveliness key:
/// `zensight/v1/<origin>/state/<producer>/device/<device>/alive`. Also a
/// liveliness token, so also structural.
fn parse_device_liveliness(key: &str, is_alive: bool) -> Option<Message> {
    let parsed = parse_key(key)?;
    let Origin::Host(_) = parsed.origin else {
        return None;
    };
    let [device_head, device_id, alive] = parsed.subject.as_slice() else {
        return None;
    };
    if *device_head != "device" || *alive != "alive" {
        return None;
    }
    let protocol = parsed.producer?.name().to_string();
    let origin = parsed.origin.chunk().to_string();
    let device = device_id.to_string();
    if is_alive {
        tracing::debug!(protocol = %protocol, origin = %origin, device = %device, "Device came online");
        Some(Message::DeviceOnline {
            protocol,
            origin,
            device,
        })
    } else {
        tracing::debug!(protocol = %protocol, origin = %origin, device = %device, "Device went offline");
        Some(Message::DeviceOffline {
            protocol,
            origin,
            device,
        })
    }
}

/// Route a Delete tombstone. Alerts and entities are the two keyed, lifecycle-
/// managed state families the GUI tracks, and the registry tells them apart.
fn parse_tombstone(key: &str) -> Option<Message> {
    let (parsed, protocol, subject) = refine_key(key)?;
    if !matches!(parsed.class, ClassOrPlane::Class(Class::State)) {
        return None;
    }
    match subject.common_state()? {
        CommonState::Alert { alert_key } => Some(Message::AlertCleared {
            protocol,
            alert_key: alert_key.to_string(),
        }),
        CommonState::CatalogEntity { entity_id } => {
            Some(Message::EntityRemoved(entity_id.to_string()))
        }
        // An alias tombstone: an operator un-merged (`@catalog/@rpc/unlink`).
        // Dropping it matters as much as adding it — a stale alias would keep
        // re-pointing consumers at an entity that no longer claims them.
        CommonState::CatalogAlias { old_id } => Some(Message::AliasRemoved(old_id.to_string())),
        _ => None,
    }
}

/// Decode a sample from its v1 key (RFC 03) via the registry's **parse**
/// direction (RFC 08 §1, issue #475).
///
/// This used to re-implement the grammar by hand — `strip_prefix("v1/")`,
/// `split_once('/')` four times, then a string match on the subject head, with
/// the producer's instance suffix stripped by a private copy of
/// `Producer::parse_chunk`. All of that is what the registry generates, and
/// generating it is the whole reason the registry exists: a subject that is not
/// registered does not parse, and the variables come out named and typed
/// instead of as `parts[3]`.
///
/// The decoded messages keep their legacy shapes (protocol = producer base
/// name, source = payload/origin) so the view layer is untouched.
fn decode_sample(key: &str, payload: &[u8]) -> Option<Message> {
    let parsed = parse_key(key)?;

    // The origin is chunk 3 of every key, so it is known here for EVERY class.
    // It used to be read only after the telemetry early-return below — i.e.
    // thrown away for the one class that carries 99% of the samples — which is
    // why the GUI had to rebuild it from a `source -> origin` side map fed by
    // health docs, and why that map's hostname collisions could misroute
    // drill-downs to the wrong host (#474).
    let origin = parsed.origin.chunk().to_string();

    // Telemetry: the point carries its own metric name; a subscriber does not
    // need to know *which* subject it is (the views that do refine it
    // themselves, against their own producer's Subject enum).
    if matches!(parsed.class, ClassOrPlane::Class(Class::Telemetry)) {
        return match decode_auto::<TelemetryPoint>(payload) {
            Ok(point) => Some(Message::TelemetryReceived(Reading::new(point, origin))),
            Err(e) => {
                tracing::warn!(error = %e, key = %key, "Failed to decode TelemetryPoint");
                None
            }
        };
    }

    let (_, protocol, subject) = refine_key(key)?;

    // Events class (#536): append-only records — SNMP trap records today.
    if matches!(parsed.class, ClassOrPlane::Class(Class::Events)) {
        return match subject {
            zensight_common::registry::AnySubject::Snmp(
                zensight_common::registry::snmp::Subject::Trap { .. },
            ) => match decode_auto::<zensight_common::EventRecord>(payload) {
                Ok(record) => Some(Message::SnmpEventReceived(record)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode EventRecord");
                    None
                }
            },
            _ => None,
        };
    }

    // One typed match over the framework state set, instead of a string match
    // on the subject head. A registry move is now a compile error here.
    macro_rules! decode {
        ($ty:ty, $ok:expr) => {
            match decode_auto::<$ty>(payload) {
                Ok(v) => Some($ok(v)),
                Err(e) => {
                    tracing::warn!(error = %e, key = %key, "Failed to decode {}", stringify!($ty));
                    None
                }
            }
        };
    }

    match ZensightState::of(&subject)? {
        ZensightState::Common(CommonState::Health) => {
            decode!(HealthSnapshot, Message::HealthSnapshotReceived)
        }
        ZensightState::Common(CommonState::Errors) => {
            decode!(ErrorReport, |report| Message::ErrorReportReceived(
                protocol,
                Some(origin),
                report
            ))
        }
        ZensightState::Common(CommonState::Sensor) => {
            decode!(SensorInfo, Message::SensorInfoReceived)
        }
        ZensightState::Common(CommonState::Alert { .. }) => decode!(Alert, Message::AlertReceived),
        ZensightState::Stream { .. } => {
            decode!(zensight_common::stream::StreamStatus, |status| {
                Message::ParallaxStreamStatus {
                    source: origin,
                    status,
                }
            })
        }
        ZensightState::SnmpInterfaces { device } => {
            let device = device.to_string();
            decode!(zensight_common::InterfaceTable, |table| {
                Message::SnmpInterfaceTable { device, table }
            })
        }
        ZensightState::SysinfoSystemInfo => {
            decode!(zensight_common::SystemInfo, |info| {
                Message::SystemInfoReceived { origin, info }
            })
        }
        ZensightState::Common(CommonState::CatalogEntity { .. }) => {
            decode!(HostEntity, Message::EntityReceived)
        }
        // An alias re-points a retired entity id at its successor. The GUI must
        // consume it (RFC 06 §5.1 step 1, #486): the retired id's entity doc is
        // gone, so without the alias an operator's merge simply makes a host
        // vanish for anyone holding the old id.
        ZensightState::Common(CommonState::CatalogAlias { .. }) => {
            decode!(zensight_common::AliasRecord, Message::AliasReceived)
        }
        // Evidence is the catalog's input, not the GUI's; artifact progress is
        // polled over @rpc; pdns is resolved on demand over @rpc/names; an
        // assertion is the *operator's* input to the catalog — the GUI reads its
        // consequences (entities + aliases), not the instruction.
        ZensightState::Common(
            CommonState::EvidenceSelf
            | CommonState::EvidenceDevice { .. }
            | CommonState::EvidenceNames { .. }
            | CommonState::CatalogPdns { .. },
        )
        | ZensightState::Artifact { .. }
        | ZensightState::CatalogAssertion { .. } => None,
    }
}

/// Connect to Zenoh using the provided configuration.
///
/// Delegates to the shared builder — which is the point of #466: the session
/// `namespace` (= the deployment base) is set in exactly one place, and a
/// second hand-rolled `zenoh::Config` here would be a GUI that talks to a
/// different keyspace than every sensor. It also means the GUI now gets the
/// `timestamping` and scouting settings it had been quietly missing.
async fn connect_zenoh(config: &ZenohConfig) -> anyhow::Result<zenoh::Session> {
    zensight_common::session::connect(config)
        .await
        .map_err(|e| anyhow::anyhow!("Zenoh open failed: {}", e))
}

/// Create a tick subscription for periodic UI updates.
pub fn tick_subscription() -> Subscription<Message> {
    iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::Tick)
}

/// Create a keyboard subscription for global shortcuts.
///
/// Handles:
/// - Ctrl+F: Focus search input
/// - Escape: Close dialogs, clear selections
pub fn keyboard_subscription() -> Subscription<Message> {
    keyboard::listen()
        .map(|event| {
            if let keyboard::Event::KeyPressed { key, modifiers, .. } = event {
                match key.as_ref() {
                    // Ctrl+F: Focus search
                    Key::Character("f") if modifiers.control() => Some(Message::FocusSearch),

                    // Ctrl+K: Open the global cross-device metric search (#27).
                    Key::Character("k") if modifiers.control() => Some(Message::OpenGlobalSearch),

                    // Ctrl+P: Open the command palette (#28).
                    Key::Character("p") if modifiers.control() => Some(Message::OpenCommandPalette),

                    // "?": Toggle the keyboard-shortcuts help overlay (#28).
                    // No modifier check — "?" is itself Shift+/ on most layouts.
                    Key::Character("?") if !modifiers.control() => Some(Message::ToggleHelp),

                    // Escape: Close/back
                    Key::Named(key::Named::Escape) => Some(Message::EscapePressed),

                    _ => None,
                }
            } else {
                None
            }
        })
        .filter_map(|msg| msg)
}

/// Create a demo subscription that generates mock telemetry data.
///
/// This subscription uses the [`DemoSimulator`](crate::demo::DemoSimulator) to generate
/// realistic, time-varying telemetry with random anomalies and events that trigger alerts.
/// It also generates sensor health snapshots and device liveness updates to showcase
/// the health monitoring features.
pub fn demo_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        async_stream::stream! {
            use crate::demo::DemoSimulator;

            // Signal connected state (demo mode has no real session)
            yield Message::Connected(None);

            // Create the demo simulator
            let mut simulator = DemoSimulator::new();
            let mut tick_count = 0u64;

            loop {
                // Update interval (500-800ms for responsive UI)
                tokio::time::sleep(std::time::Duration::from_millis(600)).await;

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);

                // Generate a tick of telemetry
                let points = simulator.tick(now);

                // Track metrics per sensor
                let mut sysinfo_count = 0u64;
                let mut snmp_count = 0u64;
                let mut modbus_count = 0u64;
                let mut syslog_count = 0u64;
                let mut netlink_count = 0u64;
                let mut netring_count = 0u64;
                let mut netflow_count = 0u64;
                let mut gnmi_count = 0u64;

                // Yield all telemetry points
                for point in points {
                    match point.protocol {
                        zensight_common::Protocol::Sysinfo => sysinfo_count += 1,
                        zensight_common::Protocol::Snmp => snmp_count += 1,
                        zensight_common::Protocol::Modbus => modbus_count += 1,
                        zensight_common::Protocol::Logs => syslog_count += 1,
                        zensight_common::Protocol::Netlink => netlink_count += 1,
                        zensight_common::Protocol::Netring => netring_count += 1,
                        zensight_common::Protocol::Netflow => netflow_count += 1,
                        zensight_common::Protocol::Gnmi => gnmi_count += 1,
                        _ => {}
                    }
                    let origin = crate::demo::demo_origin(&point);
                    yield Message::TelemetryReceived(Reading::new(point, origin));
                }

                // Update metrics counts
                simulator.record_metrics("sysinfo", sysinfo_count);
                simulator.record_metrics("snmp", snmp_count);
                simulator.record_metrics("modbus", modbus_count);
                simulator.record_metrics("logs", syslog_count);
                simulator.record_metrics("netlink", netlink_count);
                simulator.record_metrics("netring", netring_count);
                simulator.record_metrics("netflow", netflow_count);
                simulator.record_metrics("gnmi", gnmi_count);

                // Emit sensor-decided alerts (netring anomalies, netlink
                // expectation violations) with firing/resolved transitions.
                for alert in simulator.generate_alerts() {
                    yield Message::AlertReceived(alert);
                }

                // Every 5 ticks (~3 seconds), generate health snapshots
                if tick_count.is_multiple_of(5) {
                    for snapshot in simulator.generate_health_snapshots() {
                        yield Message::HealthSnapshotReceived(snapshot);
                    }
                }

                // Every 3 ticks (~1.8 seconds), generate liveness updates
                if tick_count.is_multiple_of(3) {
                    for (protocol, mut liveness) in simulator.generate_liveness_updates() {
                        liveness.last_seen = now;
                        yield Message::DeviceLivenessReceived(protocol, liveness);
                    }
                }

                // Re-emit correlator host entities on the first tick and every
                // ~30 ticks thereafter (~18 s) to exercise the freshness path (#306).
                if tick_count.is_multiple_of(30) {
                    for entity in simulator.generate_entities(now) {
                        yield Message::EntityReceived(entity);
                    }
                    // Static host facts for the sysinfo hosts (LWW per origin),
                    // so the sysinfo drill-down shows the System information
                    // card in demo mode too.
                    for host in ["server01", "server02", "server03", "database01"] {
                        yield Message::SystemInfoReceived {
                            origin: crate::mock::entity_id_for(host),
                            info: crate::mock::system_info_for(host, now),
                        };
                    }
                }

                tick_count += 1;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sensor_liveliness() {
        let key = "v1/h-3fa9c2d41b7e/state/sysinfo/alive";
        let msg = parse_sensor_liveliness(key, true);
        assert!(matches!(
            msg,
            Some(Message::SensorOnline(ref p, Some(ref s))) if p == "sysinfo" && s == "h-3fa9c2d41b7e"
        ));
        let msg = parse_sensor_liveliness(key, false);
        assert!(matches!(
            msg,
            Some(Message::SensorOffline(ref p, Some(ref s))) if p == "sysinfo" && s == "h-3fa9c2d41b7e"
        ));
        // Instance suffix folds back to the base producer name.
        let msg = parse_sensor_liveliness("v1/h-3fa9c2d41b7e/state/snmp-2/alive", true);
        assert!(matches!(
            msg,
            Some(Message::SensorOnline(ref p, _)) if p == "snmp"
        ));
    }

    #[test]
    fn test_parse_sensor_liveliness_ignores_catalog_token() {
        // The catalog's alive/claim tokens are service machinery, not sensors
        // (they also never match the `*`-origin selector — D4 — but the
        // parser guards regardless).
        assert!(parse_sensor_liveliness("v1/@catalog/state/alive", true).is_none());
    }

    #[test]
    fn test_parse_sensor_liveliness_invalid() {
        // Legacy shapes no longer parse.
        assert!(parse_sensor_liveliness("zensight/sysinfo/@/alive", true).is_none());
        assert!(parse_sensor_liveliness("zensight/sysinfo/hostA/@/alive", true).is_none());
        // Missing alive leaf.
        assert!(parse_sensor_liveliness("v1/h-3fa9c2d41b7e/state/snmp/health", true).is_none());
        // Wrong prefix.
        assert!(
            parse_sensor_liveliness("other/v1/h-3fa9c2d41b7e/state/snmp/alive", true).is_none()
        );
    }

    #[test]
    fn test_parse_device_liveliness() {
        let key = "v1/h-3fa9c2d41b7e/state/snmp/device/router01/alive";
        let msg = parse_device_liveliness(key, true);
        assert!(matches!(
            msg,
            Some(Message::DeviceOnline { ref protocol, ref origin, ref device })
                if protocol == "snmp" && origin == "h-3fa9c2d41b7e" && device == "router01"
        ));
        let msg = parse_device_liveliness(key, false);
        assert!(matches!(
            msg,
            Some(Message::DeviceOffline { ref protocol, ref origin, ref device })
                if protocol == "snmp" && origin == "h-3fa9c2d41b7e" && device == "router01"
        ));
    }

    #[test]
    fn test_parse_device_liveliness_invalid() {
        // Sensor token, not a device token.
        assert!(parse_device_liveliness("v1/h-3fa9c2d41b7e/state/snmp/alive", true).is_none());
        // Wrong leaf.
        assert!(
            parse_device_liveliness("v1/h-3fa9c2d41b7e/state/snmp/device/router01/status", true)
                .is_none()
        );
        // Legacy shape.
        assert!(parse_device_liveliness("zensight/snmp/@/devices/router01/alive", true).is_none());
    }

    #[test]
    fn test_effective_scopes_empty_is_firehose() {
        let config = LinkConfig {
            zenoh: ZenohConfig::default(),
            scope: vec![],
            profile: LinkProfile::Standard,
            focus: None,
        };
        assert_eq!(
            effective_scopes(&config),
            vec!["v1/*/telemetry/**".to_string()]
        );
    }

    #[test]
    fn test_focus_overrides_scope_with_one_origin() {
        // Focus mode (#476): one origin's telemetry, whatever the configured scope was.
        let config = LinkConfig {
            zenoh: ZenohConfig::default(),
            scope: vec!["v1/*/telemetry/netring/**".into()],
            profile: LinkProfile::Standard,
            focus: Some("h-3fa9c2d41b7e".into()),
        };
        assert_eq!(
            effective_scopes(&config),
            vec!["v1/h-3fa9c2d41b7e/telemetry/**".to_string()]
        );
        assert_eq!(state_selector(&config), "v1/h-3fa9c2d41b7e/state/**");
        assert_eq!(
            alerts_selector(&config),
            "v1/h-3fa9c2d41b7e/state/*/alert/*"
        );
        assert_eq!(
            liveliness_exprs(&config),
            (
                "v1/h-3fa9c2d41b7e/state/*/alive".to_string(),
                "v1/h-3fa9c2d41b7e/state/*/device/*/alive".to_string()
            )
        );
    }

    #[test]
    fn test_effective_scopes_passthrough_preserves_order() {
        let config = LinkConfig {
            zenoh: ZenohConfig::default(),
            scope: vec![
                "v1/*/telemetry/netring/**".into(),
                "v1/*/telemetry/sysinfo/**".into(),
            ],
            profile: LinkProfile::Constrained,
            focus: None,
        };
        assert_eq!(
            effective_scopes(&config),
            vec![
                "v1/*/telemetry/netring/**".to_string(),
                "v1/*/telemetry/sysinfo/**".to_string()
            ]
        );
    }

    #[test]
    fn test_liveliness_key_expressions() {
        // v1 presence selectors (RFC 04 §5), base-relative (#466): the session
        // namespace supplies `zensight/`.
        let cfg = LinkConfig {
            zenoh: ZenohConfig::default(),
            scope: vec![],
            profile: LinkProfile::Standard,
            focus: None,
        };
        assert_eq!(
            liveliness_exprs(&cfg),
            (
                "v1/*/state/*/alive".to_string(),
                "v1/*/state/*/device/*/alive".to_string()
            )
        );
    }

    /// v1 health keys route to HealthSnapshotReceived; the origin scopes the
    /// key, the payload carries the human source label.
    #[test]
    fn test_decode_sample_health() {
        let snapshot = HealthSnapshot {
            sensor: "sysinfo".into(),
            status: zensight_common::HealthStatus::Healthy,
            uptime_secs: 1,
            devices_total: 0,
            devices_responding: 0,
            devices_failed: 0,
            last_poll_duration_ms: 0,
            errors_last_hour: 0,
            metrics_published: 0,
            host_id: None,
            source: Some("hosta".into()),
        };
        let payload = zensight_common::encode(&snapshot, zensight_common::Format::Json).unwrap();
        match decode_sample("v1/h-3fa9c2d41b7e/state/sysinfo/health", &payload) {
            Some(Message::HealthSnapshotReceived(s)) => {
                assert_eq!(s.source.as_deref(), Some("hosta"));
            }
            other => panic!("expected HealthSnapshotReceived, got {other:?}"),
        }
    }

    /// v1 error keys carry the origin as the instance discriminator.
    #[test]
    fn test_decode_sample_errors() {
        let report = ErrorReport {
            timestamp: 0,
            device: None,
            error_type: zensight_common::ErrorType::Other,
            message: "boom".into(),
            retryable: true,
        };
        let payload = zensight_common::encode(&report, zensight_common::Format::Json).unwrap();
        match decode_sample("v1/h-3fa9c2d41b7e/state/netlink/errors", &payload) {
            Some(Message::ErrorReportReceived(protocol, source, _)) => {
                assert_eq!(protocol, "netlink");
                assert_eq!(source.as_deref(), Some("h-3fa9c2d41b7e"));
            }
            other => panic!("expected ErrorReportReceived, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_tombstone() {
        let msg =
            parse_tombstone("v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1").unwrap();
        match msg {
            Message::AlertCleared {
                protocol,
                alert_key,
            } => {
                assert_eq!(protocol, "netlink");
                assert_eq!(alert_key, "9f2c81ab04d7e3f1");
            }
            _ => panic!("expected AlertCleared"),
        }
        // The catalog's entity tombstone rides the same parser.
        assert!(matches!(
            parse_tombstone("v1/@catalog/state/entity/e-123"),
            Some(Message::EntityRemoved(id)) if id == "e-123"
        ));
        // A state subject that is not a keyed, lifecycle-managed one.
        assert!(parse_tombstone("v1/h-3fa9c2d41b7e/state/netlink/health").is_none());
        // `alert/{alert_key}` is one chunk: a nested key is not that subject,
        // and the registry — not a hand-rolled `contains('/')` check — is what
        // rejects it now.
        assert!(parse_tombstone("v1/h-3fa9c2d41b7e/state/netlink/alert/a/b").is_none());
        // An instance-suffixed producer still resolves to its base name
        // (RFC 03 §1.5) — that used to need a private copy of
        // `Producer::parse_chunk` in this file.
        assert!(matches!(
            parse_tombstone("v1/h-3fa9c2d41b7e/state/netlink-2/alert/deadbeef00000000"),
            Some(Message::AlertCleared { protocol, .. }) if protocol == "netlink"
        ));
    }

    /// An unregistered subject does not decode. This is the property the
    /// registry buys (RFC 08 §1) and it is why #468 had to delete the
    /// `{metric...}` catch-all first.
    #[test]
    fn unregistered_subjects_do_not_decode() {
        assert!(decode_sample("v1/h-3fa9c2d41b7e/state/netlink/bogus", b"{}").is_none());
        // `device/<device>/liveness` documents: the GUI used to have a decode
        // arm for these, but no producer has ever published one (device liveness
        // rides the liveliness *token* plane, `state/<producer>/device/<d>/alive`,
        // which `parse_device_liveliness` handles). It is not registered, so it
        // no longer parses — the dead arm is gone.
        assert!(
            decode_sample(
                "v1/h-3fa9c2d41b7e/state/netlink/device/dev1/liveness",
                b"{}"
            )
            .is_none()
        );
    }

    #[test]
    fn test_decode_sample_alert() {
        let alert = zensight_common::Alert::new(
            "host1",
            zensight_common::Protocol::Netring,
            zensight_common::AlertKind::Anomaly,
            "port_scan",
            zensight_common::AlertSeverity::Warning,
            "scan",
        );
        let key = format!(
            "v1/h-3fa9c2d41b7e/state/netring/alert/{}",
            alert.alert_key()
        );
        let payload = zensight_common::encode(&alert, zensight_common::Format::Json).unwrap();
        match decode_sample(&key, &payload) {
            Some(Message::AlertReceived(got)) => assert_eq!(got.rule, "port_scan"),
            other => panic!("expected AlertReceived, got {other:?}"),
        }
    }
}
