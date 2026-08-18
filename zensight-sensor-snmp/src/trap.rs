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
        if !self.config.users.is_empty() {
            // async-snmp 0.17 requires local authoritative engine state for
            // v3 receiving. Deliberately stateless: a fresh engine identity
            // per process start makes inform senders re-handshake (unknown
            // engine → rediscovery), whereas a persisted identity with a
            // non-incrementing boots counter would trip their replay window.
            let engine =
                async_snmp::AuthoritativeEngine::install(async_snmp::generate_engine_id(), |_| {
                    Ok::<(), std::convert::Infallible>(())
                })
                .map_err(|e| anyhow::anyhow!("v3 receiver engine setup failed: {e}"))?;
            builder = builder.authoritative_engine(engine);
        }
        for user in &self.config.users {
            builder = usm_user(builder, user);
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
