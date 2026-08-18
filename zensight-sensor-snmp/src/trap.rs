//! SNMP trap/inform pipeline (#535), on async-snmp's `NotificationReceiver`.
//!
//! Receives v1 traps, v2c traps/informs, and v3 traps/informs (USM), with
//! per-listener community/user filtering; **inform acknowledgements are sent
//! automatically** by the receiver, so senders stop retransmitting and we
//! can offer reliable delivery. Each notification becomes:
//!
//! - a durable [`EventRecord`] on the `events` class (#534):
//!   `v1/<origin>/events/snmp/<device>/trap/<ulid>` — translated trap OID
//!   and varbinds (MIB tables + loaded SMI modules), reliable QoS, one key
//!   per record;
//! - a lightweight cumulative telemetry counter per (device, trap type) at
//!   `telemetry/snmp/<device>/trap/<trap_id>` for dashboards;
//! - optionally an [`Alert`] transition: configured (or built-in
//!   linkDown/linkUp) `fire`/`resolve` trap OIDs raise and clear alerts
//!   keyed by device + interface through the shared reporter.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_snmp::notification::{Notification, NotificationReceiver};
use zenoh::Session as ZenohSession;

use zensight_common::{
    Alert, AlertKind, AlertSeverity, EventRecord, Protocol, TelemetryPoint, TelemetryValue, encode,
};
use zensight_sensor_core::{AlertReporter, EventPublisher, Publisher};

use crate::config::{
    AuthProtocol, PrivProtocol, SnmpV3Security, TrapAlertRule, TrapListenerConfig,
};
use crate::mib::MibResolver;
use crate::smi::{SmiResolver, snake_case};

/// The trap receiver, configured but not yet bound.
pub struct TrapReceiver {
    config: TrapListenerConfig,
    /// Declared-publisher registry for the telemetry counters (drop QoS) —
    /// never a one-shot `session.put`.
    registry: Arc<zensight_common::PublisherRegistry>,
    events: EventPublisher,
    mib_resolver: Arc<MibResolver>,
    smi: Option<Arc<SmiResolver>>,
    format: zensight_common::Format,
    alerts: Option<Arc<AlertReporter>>,
    rules: Vec<TrapAlertRule>,
    /// Cumulative per-(device, trap_id) counts for the telemetry counter.
    counts: std::sync::Mutex<HashMap<(String, String), u64>>,
}

impl TrapReceiver {
    /// Create a new trap receiver.
    pub fn new(
        config: TrapListenerConfig,
        zenoh: Arc<ZenohSession>,
        mib_resolver: Arc<MibResolver>,
        format: zensight_common::Format,
    ) -> Self {
        let rules = config.effective_rules();
        Self {
            config,
            registry: Arc::new(zensight_common::PublisherRegistry::new(zenoh.clone())),
            events: EventPublisher::new(Publisher::new(zenoh, "snmp", format)),
            mib_resolver,
            smi: None,
            format,
            alerts: None,
            rules,
            counts: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Attach loaded SMI MIBs for trap/varbind translation (#532).
    pub fn with_smi(&mut self, smi: Arc<SmiResolver>) {
        self.smi = Some(smi);
    }

    /// Attach the shared alert reporter for trap → alert mappings.
    pub fn with_alerts(&mut self, reporter: Arc<AlertReporter>) {
        self.alerts = Some(reporter);
    }

    /// Bind the listener socket. Split from [`BoundTrapReceiver::run`] so
    /// callers (and tests) can learn the bound address.
    pub async fn bind(self) -> Result<BoundTrapReceiver> {
        let mut builder = NotificationReceiver::builder().bind(&self.config.bind);
        if !self.config.communities.is_empty() {
            builder = builder.communities(&self.config.communities);
        }
        let mut engine_state = None;
        if !self.config.users.is_empty() {
            // async-snmp 0.17 requires local authoritative engine state for any
            // v3 *receiving* role. It is durable now (#650): informs are
            // authenticated against this engine's (boots, time), so a fresh
            // identity every start does not merely cost a re-handshake — the
            // receiver drops a live sender's informs outright until it
            // rediscovers.
            let path = resolve_engine_state_path(self.config.engine_state_path.as_deref());
            match authoritative_engine(path.as_deref())? {
                Some(engine) => {
                    engine_state = Some((hex_encode(engine.engine_id()), engine.engine_boots()));
                    builder = builder.authoritative_engine(engine);
                    for user in &self.config.users {
                        builder = usm_user(builder, user);
                    }
                }
                None => {
                    // The USM users are dropped with the engine on purpose:
                    // `build()` refuses a v3 user without authoritative state,
                    // which would take v1/v2c listening down with it.
                    tracing::error!(
                        path = ?path,
                        "v3 trap/inform receiving DISABLED: the engine state file is not \
                         writable. v1/v2c listening continues. Fix the path or its \
                         permissions (systemd: StateDirectory=), or unset \
                         trap_listener.engine_state_path to fall back to an ephemeral \
                         identity."
                    );
                }
            }
        }
        let receiver = builder
            .build()
            .await
            .with_context(|| format!("Failed to bind trap listener to {}", self.config.bind))?;

        tracing::info!(
            bind = %self.config.bind,
            local = %receiver.local_addr(),
            communities = self.config.communities.len(),
            v3_users = self.config.users.len(),
            rules = self.rules.len(),
            engine_id = engine_state.as_ref().map(|(id, _)| id.as_str()).unwrap_or("-"),
            engine_boots = engine_state.as_ref().map(|(_, b)| *b).unwrap_or(0),
            "SNMP trap listener bound"
        );
        Ok(BoundTrapReceiver {
            receiver,
            inner: self,
        })
    }
}

/// Map a config v3 user onto the receiver builder.
fn usm_user(
    builder: async_snmp::notification::NotificationReceiverBuilder,
    user: &SnmpV3Security,
) -> async_snmp::notification::NotificationReceiverBuilder {
    let auth_password = user.auth_password.clone().unwrap_or_default();
    let priv_password = user.priv_password.clone().unwrap_or_default();
    let auth_protocol = user.auth_protocol;
    let priv_protocol = user.priv_protocol;
    builder.usm_user(user.username.clone(), move |mut u| {
        let auth = match auth_protocol {
            AuthProtocol::None => None,
            AuthProtocol::Md5 => Some(async_snmp::AuthProtocol::Md5),
            AuthProtocol::Sha1 => Some(async_snmp::AuthProtocol::Sha1),
            AuthProtocol::Sha224 => Some(async_snmp::AuthProtocol::Sha224),
            AuthProtocol::Sha256 => Some(async_snmp::AuthProtocol::Sha256),
            AuthProtocol::Sha384 => Some(async_snmp::AuthProtocol::Sha384),
            AuthProtocol::Sha512 => Some(async_snmp::AuthProtocol::Sha512),
        };
        if let Some(proto) = auth {
            let privacy = match priv_protocol {
                PrivProtocol::None => None,
                PrivProtocol::Des => Some(async_snmp::PrivProtocol::Des),
                PrivProtocol::Aes128 => Some(async_snmp::PrivProtocol::Aes128),
                PrivProtocol::Aes192 => Some(async_snmp::PrivProtocol::Aes192),
                PrivProtocol::Aes256 => Some(async_snmp::PrivProtocol::Aes256),
            };
            // 0.17: authPriv is one constructor — `.privacy()` is gone.
            u = match privacy {
                Some(cipher) => u.auth_priv(
                    proto,
                    auth_password.as_bytes(),
                    cipher,
                    priv_password.as_bytes(),
                ),
                None => u.auth(proto, auth_password.as_bytes()),
            };
        }
        u
    })
}

/// A bound trap listener, ready to run.
pub struct BoundTrapReceiver {
    receiver: NotificationReceiver,
    inner: TrapReceiver,
}

impl BoundTrapReceiver {
    /// The actual bound address (tests bind `:0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.receiver.local_addr()
    }

    /// This receiver's local authoritative `snmpEngineID` (#650). Stable across
    /// restarts once the identity is persisted; empty on a v1/v2c-only listener.
    pub fn engine_id(&self) -> Vec<u8> {
        self.receiver.engine_id().to_vec()
    }

    /// `snmpEngineBoots` — increments on each restart of a persisted identity.
    pub fn engine_boots(&self) -> u32 {
        self.receiver.engine_boots()
    }

    /// `usmStatsUnknownEngineIDs`: bumped by the RFC 3414 engine-ID discovery
    /// probe. Zero after a restart is direct evidence that senders did not have
    /// to rediscover this engine.
    pub fn usm_unknown_engine_ids(&self) -> u32 {
        self.receiver.usm_unknown_engine_ids()
    }

    /// `usmStatsNotInTimeWindows`: bumped when an authenticated message falls
    /// outside the boots/time window. Exactly one of these per sender is
    /// *expected* after a restart — boots incremented, so the sender's cached
    /// tuple is stale and RFC 3414 requires a time-sync Report.
    pub fn usm_not_in_time_windows(&self) -> u32 {
        self.receiver.usm_not_in_time_windows()
    }

    /// Receive-and-publish loop. Informs are acknowledged by the receiver
    /// before this returns each notification.
    pub async fn run(self) -> Result<()> {
        loop {
            match self.receiver.recv().await {
                Ok((notification, source)) => {
                    self.inner.handle(notification, source).await;
                }
                Err(e) => {
                    // Malformed/unauthenticated datagrams are logged, not fatal.
                    tracing::debug!(error = %e, "trap receiver: dropped datagram");
                }
            }
        }
    }
}

impl TrapReceiver {
    async fn handle(&self, notification: Notification, source: SocketAddr) {
        // Device identity = slugged sender IP (the pre-#535 convention).
        let device = source.ip().to_string().replace(['.', ':'], "-");

        let trap_oid = match notification.trap_oid() {
            Ok(oid) => oid.to_string(),
            Err(e) => {
                tracing::warn!(error = %e, source = %source, "trap without a resolvable trap OID");
                return;
            }
        };
        let trap_id = self.trap_name(&trap_oid);

        // Structured fields: translated varbinds + notification metadata.
        let mut event = EventRecord::new(
            &device,
            Protocol::Snmp,
            format!("trap/{trap_id}"),
            self.rule_for_fire(&trap_oid)
                .map(|r| severity(&r.severity))
                .unwrap_or(AlertSeverity::Info),
            format!("{device}: {trap_id}"),
        )
        .with_field("trap_oid", &trap_oid)
        .with_field("snmp_version", format!("{:?}", notification.version()))
        .with_field(
            "uptime_secs",
            format!("{:.2}", f64::from(notification.uptime()) / 100.0),
        )
        .with_field("confirmed", notification.is_confirmed().to_string());

        let mut if_index: Option<String> = None;
        for varbind in notification.varbinds() {
            let oid_str = varbind.oid.to_string();
            let name = self.varbind_name(&oid_str);
            let value = self.varbind_value(&oid_str, &varbind.value);
            if oid_str.starts_with("1.3.6.1.2.1.2.2.1.1.") {
                if_index = Some(value.clone());
            }
            event.fields.insert(name, value);
        }

        tracing::info!(
            device = %device,
            trap = %trap_id,
            confirmed = notification.is_confirmed(),
            "SNMP notification received"
        );

        // 1. Alert mapping FIRST (#651): the record must be able to name the
        //    alert it caused, and the bus ordering that falls out is the one we
        //    want — a consumer never sees a record pointing at an alert it has
        //    not ingested yet.
        event.alert_key = self
            .apply_alert_rules(&device, &trap_oid, if_index.as_deref())
            .await;

        // 2. Durable event record.
        if let Err(e) = self.events.publish(&[&device, "trap"], &event).await {
            tracing::warn!(error = %e, device = %device, "trap event publish failed");
        }

        // 3. Lightweight per-type telemetry counter.
        self.publish_counter(&device, &trap_id).await;
    }

    /// Trap OID → key-safe name: explicit tables → SMI notifications →
    /// dotted OID. Mixed-case MIB names are snake_cased (key chunks).
    fn trap_name(&self, trap_oid: &str) -> String {
        let resolved = self.mib_resolver.resolve(trap_oid);
        if resolved != trap_oid {
            return snake_case(&resolved);
        }
        if let Some(name) = self
            .smi
            .as_ref()
            .and_then(|s| s.notification_name(trap_oid))
        {
            return name;
        }
        trap_oid.to_string()
    }

    fn varbind_name(&self, oid_str: &str) -> String {
        let resolved = self.mib_resolver.resolve(oid_str);
        if resolved != oid_str {
            return resolved;
        }
        self.smi
            .as_ref()
            .and_then(|s| s.metric_name(oid_str))
            .unwrap_or_else(|| oid_str.to_string())
    }

    /// Render a varbind value, preferring MIB enum labels.
    fn varbind_value(&self, oid_str: &str, value: &async_snmp::Value) -> String {
        if let async_snmp::Value::Integer(n) = value
            && let Some(label) = self
                .smi
                .as_ref()
                .and_then(|s| s.enum_label(oid_str, i64::from(*n)))
        {
            return label;
        }
        value.to_string()
    }

    async fn publish_counter(&self, device: &str, trap_id: &str) {
        let count = {
            let mut counts = self.counts.lock().unwrap();
            let entry = counts
                .entry((device.to_string(), trap_id.to_string()))
                .or_insert(0);
            *entry += 1;
            *entry
        };
        let metric = format!("trap/{trap_id}");
        let point = TelemetryPoint::new(
            device,
            Protocol::Snmp,
            &metric,
            TelemetryValue::Counter(count),
        );
        // #559: through the generated builder — slugs device and trap-id
        // chunks so a resolved trap name can never trip the metric guard.
        let key = zensight_common::registry::snmp::key(
            &zensight_common::PROFILE.local_origin(),
            &zensight_common::registry::snmp::Subject::device_metric(device, metric.split('/')),
        );
        let key = key.as_str();
        match encode(&point, self.format) {
            Ok(payload) => {
                if let Err(e) = self
                    .registry
                    .put(key, payload, zensight_common::QosClass::Telemetry)
                    .await
                {
                    tracing::warn!(key = %key, error = %e, "trap counter publish failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "trap counter encode failed"),
        }
    }

    fn rule_for_fire(&self, trap_oid: &str) -> Option<&TrapAlertRule> {
        self.rules.iter().find(|r| r.fire == trap_oid)
    }

    /// Returns the `alert_key` of the alert this trap raised or cleared, for
    /// the event record to carry (#651). `None` when no rule matched, no
    /// reporter is attached, or a clear matched no firing alert.
    async fn apply_alert_rules(
        &self,
        device: &str,
        trap_oid: &str,
        if_index: Option<&str>,
    ) -> Option<String> {
        let reporter = self.alerts.as_ref()?;

        if let Some(rule) = self.rule_for_fire(trap_oid) {
            let mut alert = Alert::new(
                device,
                Protocol::Snmp,
                AlertKind::Expectation,
                &rule.rule,
                severity(&rule.severity),
                format!("{device}: {} fired", rule.rule),
            )
            .with_label("device", device);
            if let Some(idx) = if_index {
                alert = alert.with_label("if_index", idx);
            }
            // Computed before `observe` takes the alert. Safe because `observe`
            // stamps only `host.id`, and `alert_key()` excludes every `host.`
            // label — so this is byte-identical to the key it publishes under.
            // Pinned by `fire_key_matches_the_reporters_key`.
            let key = alert.alert_key();
            // `Some(ZERO)`, not the reporter's default debounce: a trap is a
            // single observation, and `observe` only publishes once a *second*
            // one arrives after the window. With `alerts.for_secs > 0` the
            // alert was entered and never published — so the link this record
            // now carries would point at an alert nobody could see.
            if let Err(e) = reporter.observe(alert, Some(Duration::ZERO)).await {
                tracing::warn!(error = %e, rule = %rule.rule, "trap alert publish failed");
            }
            return Some(key);
        }

        if let Some(rule) = self
            .rules
            .iter()
            .find(|r| r.resolve.as_deref() == Some(trap_oid))
        {
            let mut labels: Vec<(&str, &str)> = vec![("device", device)];
            if let Some(idx) = if_index {
                labels.push(("if_index", idx));
            }
            match reporter.resolve_matching(&rule.rule, &labels).await {
                // Exactly one is the normal case (device + if_index identify
                // one alert). Zero means the clear arrived with nothing firing;
                // several would be ambiguous, so neither gets a link.
                Ok(keys) if keys.len() == 1 => return keys.into_iter().next(),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, rule = %rule.rule, "trap alert resolve failed");
                }
            }
        }
        None
    }
}

fn severity(s: &str) -> AlertSeverity {
    match s {
        "info" => AlertSeverity::Info,
        "critical" => AlertSeverity::Critical,
        _ => AlertSeverity::Warning,
    }
}

/// RFC 3414 §2.2's ceiling. At this value boots latches and authenticated
/// inbound is rejected until the engine is reconfigured with a new ID, so a
/// state file that has reached it is worse than no state file.
const MAX_ENGINE_BOOTS: u32 = 2_147_483_647;

/// The persisted local authoritative engine identity (#650, RFC 3414 §2.2).
#[derive(serde::Serialize, serde::Deserialize)]
struct EngineStateFile {
    /// Lowercase hex of the `snmpEngineID` octets (RFC 3411 §5).
    engine_id: String,
    /// `snmpEngineBoots` at the last (re)initialization.
    engine_boots: u32,
}

/// Resolve where the engine identity lives: explicit config, else systemd
/// `STATE_DIRECTORY` / XDG state / `~/.local/state`. Mirrors the logs sensor's
/// cursor and offsets resolvers so a sensor's durable files sit together.
///
/// `None` means "this host offers no durable location", which is a different
/// situation from "the location is unwritable" — see [`authoritative_engine`].
fn resolve_engine_state_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Ok(state) = std::env::var("STATE_DIRECTORY") {
        let first = state.split(':').next().unwrap_or(state.as_str());
        if !first.is_empty() {
            return Some(Path::new(first).join("snmp-trap-engine.json"));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        // Split the join so no source literal spells the deployment base
        // (CI guard #466) — this is a filesystem path, not a Zenoh key.
        return Some(
            Path::new(&xdg)
                .join("zensight")
                .join("snmp-trap-engine.json"),
        );
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(
            Path::new(&home)
                .join(".local/state/zensight")
                .join("snmp-trap-engine.json"),
        );
    }
    None
}

/// Load a previous identity. `None` for missing, unreadable, malformed, or
/// latched-at-maximum state — in every one of those cases the old value is
/// unusable and a fresh engine is the honest answer.
fn load_engine_state(path: &Path) -> Option<async_snmp::PersistedAuthoritativeEngine> {
    let bytes = std::fs::read(path).ok()?;
    let file: EngineStateFile = match serde_json::from_slice(&bytes) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "engine state file is unreadable — minting a fresh identity");
            return None;
        }
    };
    if file.engine_boots >= MAX_ENGINE_BOOTS {
        tracing::error!(path = %path.display(),
            "engine boots has latched at the RFC 3414 maximum — minting a NEW engine id, \
             because restarting into a latched engine rejects all authenticated inbound");
        return None;
    }
    let id = crate::poller::parse_hex(&file.engine_id)?;
    async_snmp::PersistedAuthoritativeEngine::new(id, file.engine_boots).ok()
}

/// Build the receiver's authoritative engine.
///
/// * `Ok(Some(engine))` — usable. Persisted when `path` is `Some`, ephemeral
///   (per-start identity) when this host resolves no durable location at all.
/// * `Ok(None)` — refuse v3. A location resolved and could not be written.
///
/// The asymmetry is the point. An operator with no state directory never asked
/// for durability, and refusing would turn an upgrade into an outage for every
/// working v3 deployment; they get a warning instead. An operator who *did*
/// configure a location and whose writes fail would otherwise get a receiver
/// that silently breaks every inform sender on each restart, forever, with only
/// a log line — so that case fails closed on v3 while v1/v2c keeps working.
fn authoritative_engine(path: Option<&Path>) -> Result<Option<async_snmp::AuthoritativeEngine>> {
    let Some(path) = path else {
        tracing::warn!(
            "no durable location for the SNMPv3 engine identity (no \
             trap_listener.engine_state_path, STATE_DIRECTORY, XDG_STATE_HOME or HOME) — \
             using a per-start identity. Every inform sender will re-handshake after a \
             restart of this sensor."
        );
        let engine =
            async_snmp::AuthoritativeEngine::install(async_snmp::generate_engine_id(), |_| {
                Ok::<(), std::convert::Infallible>(())
            })
            .map_err(|e| anyhow::anyhow!("v3 receiver engine setup failed: {e}"))?;
        return Ok(Some(engine));
    };

    let owned = path.to_path_buf();
    let persist = move |state: &async_snmp::PersistedAuthoritativeEngine| -> std::io::Result<()> {
        let body = serde_json::to_vec(&EngineStateFile {
            engine_id: hex_encode(state.engine_id()),
            engine_boots: state.engine_boots(),
        })
        .map_err(std::io::Error::other)?;
        write_atomic(&owned, &body)
    };

    // `install`/`restart` persist *before* returning, so an unwritable location
    // is discovered synchronously here — before the socket exists — rather than
    // by a sender months later.
    let built = match load_engine_state(path) {
        Some(previous) => async_snmp::AuthoritativeEngine::restart(previous, persist),
        None => async_snmp::AuthoritativeEngine::install(async_snmp::generate_engine_id(), persist),
    };
    match built {
        Ok(engine) => Ok(Some(engine)),
        Err(e) => {
            tracing::error!(path = %path.display(), error = %e,
                "could not persist the SNMPv3 engine identity");
            Ok(None)
        }
    }
}

/// Write `bytes` to `path` atomically: temp file, fsync, rename. A crash after
/// the rename leaves boots at or above the value actually used — never below,
/// which is the direction that would re-open a replay window.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #651's load-bearing invariant: the key computed *before* `observe` is
    /// the key the reporter publishes under.
    ///
    /// It holds because `observe` stamps only `host.id`, and `alert_key()`
    /// excludes every `host.`-prefixed label from the hash. If either side of
    /// that ever changes, the link a trap record carries silently points at an
    /// alert that does not exist — a failure mode nothing else would catch, so
    /// it is pinned here rather than assumed.
    #[test]
    fn fire_key_matches_the_reporters_key() {
        let build = || {
            Alert::new(
                "router01",
                Protocol::Snmp,
                AlertKind::Expectation,
                "link-down",
                AlertSeverity::Warning,
                "router01: link-down fired",
            )
            .with_label("device", "router01")
            .with_label("if_index", "3")
        };

        let before = build().alert_key();

        // What `AlertReporter::observe` does to the alert before keying it.
        let mut stamped = build();
        stamped
            .labels
            .insert("host.id".to_string(), "h-3fa9c2d41b7e".to_string());

        assert_eq!(
            before,
            stamped.alert_key(),
            "the identity stamp must not move the alert key — the record's link \
             would point at an alert nobody published"
        );
    }

    /// #650: the identity survives a restart and boots increments.
    ///
    /// This is the whole point. A stable `snmpEngineID` with a *non*-increasing
    /// boots would be worse than the ephemeral identity it replaces — it is the
    /// exact replay condition RFC 3414 §2.2 has senders reject.
    #[test]
    fn engine_state_persists_and_restarts_with_higher_boots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.json");

        let first = authoritative_engine(Some(&path))
            .expect("first start")
            .expect("usable engine");
        let id1 = first.engine_id().to_vec();
        assert_eq!(first.engine_boots(), 1, "a first install starts at boots=1");
        assert!(path.exists(), "install persists before returning");

        let second = authoritative_engine(Some(&path))
            .expect("second start")
            .expect("usable engine");
        assert_eq!(second.engine_id(), &id1[..], "the identity must be stable");
        assert_eq!(
            second.engine_boots(),
            2,
            "boots must increment across starts"
        );

        let on_disk: EngineStateFile =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).expect("decode state");
        assert_eq!(on_disk.engine_boots, 2);
        assert_eq!(on_disk.engine_id, hex_encode(&id1));
    }

    /// Unreadable state is not a reason to refuse: the old value is unusable,
    /// so a fresh identity is the honest answer.
    #[test]
    fn corrupt_engine_state_installs_a_fresh_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.json");
        std::fs::write(&path, b"{not json at all").unwrap();

        let engine = authoritative_engine(Some(&path))
            .expect("start")
            .expect("usable engine");
        assert_eq!(engine.engine_boots(), 1);
        let on_disk: EngineStateFile =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).expect("rewritten");
        assert_eq!(on_disk.engine_boots, 1);
    }

    /// A latched boots counter means all authenticated inbound is rejected, so
    /// restarting into it would be a silently dead receiver. Mint a new id.
    #[test]
    fn latched_boots_mints_a_new_engine_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.json");
        let first = authoritative_engine(Some(&path)).unwrap().unwrap();
        let old_id = first.engine_id().to_vec();

        std::fs::write(
            &path,
            serde_json::to_vec(&EngineStateFile {
                engine_id: hex_encode(&old_id),
                engine_boots: MAX_ENGINE_BOOTS,
            })
            .unwrap(),
        )
        .unwrap();

        let next = authoritative_engine(Some(&path)).unwrap().unwrap();
        assert_ne!(
            next.engine_id(),
            &old_id[..],
            "a latched engine must be replaced"
        );
        assert_eq!(next.engine_boots(), 1);
    }

    /// A configured-but-unwritable location refuses v3 rather than silently
    /// downgrading: the operator asked for durability and must hear about it.
    #[test]
    fn unwritable_engine_state_refuses_v3() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A regular file where a directory must be, so `create_dir_all` fails.
        let blocker = dir.path().join("afile");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("engine.json");

        assert!(
            authoritative_engine(Some(&path))
                .expect("no hard error")
                .is_none(),
            "an unwritable state path must refuse v3, not fall back silently"
        );
    }

    /// No durable location at all is the pre-#650 behaviour, kept on purpose:
    /// this operator never asked for durability, and refusing would turn an
    /// upgrade into an outage for every working v3 deployment.
    #[test]
    fn no_state_path_keeps_an_ephemeral_identity() {
        let engine = authoritative_engine(None)
            .expect("start")
            .expect("an ephemeral engine is still usable");
        assert_eq!(engine.engine_boots(), 1);
    }

    #[test]
    fn builtin_rules_include_link_pair() {
        let cfg = TrapListenerConfig::default();
        let rules = cfg.effective_rules();
        let link = rules.iter().find(|r| r.rule == "trap_link_down").unwrap();
        assert_eq!(link.fire, "1.3.6.1.6.3.1.1.5.3");
        assert_eq!(link.resolve.as_deref(), Some("1.3.6.1.6.3.1.1.5.4"));

        let cfg = TrapListenerConfig {
            builtin_rules: false,
            ..Default::default()
        };
        assert!(cfg.effective_rules().is_empty());
    }

    #[test]
    fn severity_parses_with_warning_default() {
        assert_eq!(severity("info"), AlertSeverity::Info);
        assert_eq!(severity("critical"), AlertSeverity::Critical);
        assert_eq!(severity("nonsense"), AlertSeverity::Warning);
    }
}
