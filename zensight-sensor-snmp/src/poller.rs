use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_snmp::{Auth, Client, EngineCache, Retry, UdpHandle, Value, v3::EngineState};
use bytes::Bytes;
use tokio::time::interval;
use zenoh::Session as ZenohSession;

use zensight_common::{Format, Protocol, TelemetryPoint, TelemetryValue, encode};

use crate::config::{AuthProtocol, DeviceConfig, OidGroup, PrivProtocol, SnmpVersion};
use crate::mib::MibResolver;
use crate::oid::{oid_to_string, parse_oid};
use crate::rate::RateTracker;

/// sysUpTime.0 — auto-polled every cycle for reset detection (#527).
const SYS_UPTIME_OID: &str = "1.3.6.1.2.1.1.3.0";

/// SNMP poller for a single device.
pub struct SnmpPoller {
    device: DeviceConfig,
    /// Declared-publisher registry for the telemetry path (declare-on-first-use +
    /// cache per key, drop QoS) — never a one-shot `session.put`.
    registry: Arc<zensight_common::PublisherRegistry>,
    telemetry_prefix: String,
    mib_resolver: Arc<MibResolver>,
    format: Format,
    oids: Vec<String>,
    walks: Vec<String>,
    /// Persistent client (one UDP socket per device, every SNMP version).
    /// Timeout, retry, and GETBULK sizing are configured at build time;
    /// v3 engine discovery/resync and tooBig recovery are handled inside.
    ///
    /// Behind a lock because the poller rebuilds it from `&self` when a v3
    /// engine identity changes (see [`poll_once`](Self::poll_once)).
    client: tokio::sync::RwLock<Option<Client<UdpHandle>>>,
    /// Counter→rate state (#527). Std mutex: locked only for synchronous
    /// bookkeeping, never across an await.
    rate: std::sync::Mutex<RateTracker>,
}

impl SnmpPoller {
    /// Create a new poller for a device.
    pub fn new(
        device: DeviceConfig,
        zenoh: Arc<ZenohSession>,
        mib_resolver: Arc<MibResolver>,
        oid_groups: &HashMap<String, OidGroup>,
        format: Format,
    ) -> Self {
        let telemetry_prefix =
            zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "snmp")
                .telemetry_prefix();

        let oids = device.all_oids(oid_groups);
        let walks = device.all_walks(oid_groups);

        Self {
            device,
            registry: Arc::new(zensight_common::PublisherRegistry::new(zenoh)),
            telemetry_prefix: telemetry_prefix.into(),
            mib_resolver,
            format,
            oids,
            walks,
            client: tokio::sync::RwLock::new(None),
            rate: std::sync::Mutex::new(RateTracker::new()),
        }
    }

    /// Build the persistent SNMP client for this device.
    pub async fn init(&mut self) -> Result<()> {
        let client = self.build_client(true).await?;
        *self.client.get_mut() = Some(client);

        tracing::info!(
            device = %self.device.name,
            version = ?self.device.version,
            timeout_secs = self.device.timeout_secs,
            retries = self.device.retries,
            "SNMP client initialized"
        );
        Ok(())
    }

    /// `seed_engine`: honor a configured v3 `engine_id`. Off on rebuilds —
    /// a rebuild means the device's engine identity looks changed, so a
    /// stale configured id must not short-circuit rediscovery.
    async fn build_client(&self, seed_engine: bool) -> Result<Client<UdpHandle>> {
        let auth = build_auth(&self.device)?;

        let mut builder = Client::builder(self.device.address.as_str(), auth)
            .timeout(Duration::from_secs(self.device.timeout_secs))
            // Each attempt already waits out the full request timeout, so
            // retransmit immediately (classic SNMP retry behavior).
            .retry(Retry::fixed(self.device.retries, Duration::ZERO))
            .max_repetitions(self.device.max_repetitions);

        if seed_engine && let Some(cache) = seeded_engine_cache(&self.device) {
            builder = builder.engine_cache(cache);
        }

        builder
            .connect()
            .await
            .with_context(|| format!("Failed to create SNMP client for {}", self.device.address))
    }

    /// Cheap handle to the current client (`Client` is internally shared).
    async fn client(&self) -> Result<Client<UdpHandle>> {
        self.client
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("SNMP client not initialized"))
    }

    /// Replace the client, dropping all cached v3 engine state — the recovery
    /// path when a polled device comes back with a new engine identity (agent
    /// replaced/reset), which the client itself cannot resynchronize from.
    async fn rebuild_client(&self) {
        match self.build_client(false).await {
            Ok(client) => {
                *self.client.write().await = Some(client);
                tracing::info!(device = %self.device.name, "SNMP client rebuilt");
            }
            Err(e) => {
                tracing::warn!(device = %self.device.name, error = %e, "Failed to rebuild SNMP client");
            }
        }
    }

    /// Run the polling loop.
    pub async fn run(self) {
        let poll_interval = Duration::from_secs(self.device.poll_interval_secs);
        let mut ticker = interval(poll_interval);

        tracing::info!(
            device = %self.device.name,
            address = %self.device.address,
            interval_secs = self.device.poll_interval_secs,
            oids = self.oids.len(),
            walks = self.walks.len(),
            "Starting SNMP poller"
        );

        loop {
            ticker.tick().await;

            if let Err(e) = self.poll_once().await {
                tracing::warn!(
                    device = %self.device.name,
                    error = %e,
                    "SNMP poll failed"
                );
            }
        }
    }

    /// Perform a single poll cycle.
    ///
    /// Public so integration tests can drive individual cycles against an
    /// in-process agent without the endless [`run`](Self::run) loop.
    pub async fn poll_once(&self) -> Result<()> {
        let mut requests = 0usize;
        let mut failures = 0usize;
        let mut auth_failures = 0usize;

        // Read sysUpTime up front: it anchors reset detection for every
        // counter this cycle (a reboot must suppress the whole interval).
        let uptime_ticks = self.fetch_uptime_ticks().await;
        let device_reset = self.rate.lock().unwrap().begin_cycle(uptime_ticks);
        if device_reset {
            tracing::info!(
                device = %self.device.name,
                "sysUpTime went backwards — device rebooted; suppressing rates this cycle"
            );
        }
        let mut seen_counters = std::collections::HashSet::new();

        // Poll individual OIDs with GET
        for oid_str in &self.oids {
            requests += 1;
            match self.snmp_get(oid_str).await {
                Ok(Some((oid, value))) => {
                    self.publish(&oid, &value, &mut seen_counters).await;
                }
                Ok(None) => {
                    tracing::debug!(device = %self.device.name, oid = %oid_str, "No value returned");
                }
                Err(e) => {
                    failures += 1;
                    auth_failures += usize::from(is_auth_error(&e));
                    tracing::warn!(device = %self.device.name, oid = %oid_str, error = %e, "GET failed");
                }
            }
        }

        // Walk OID subtrees (GETBULK on v2c/v3, GETNEXT on v1)
        for subtree in &self.walks {
            requests += 1;
            match self.snmp_walk(subtree).await {
                Ok(entries) => {
                    for (oid, value) in entries {
                        self.publish(&oid, &value, &mut seen_counters).await;
                    }
                }
                Err(e) => {
                    failures += 1;
                    auth_failures += usize::from(is_auth_error(&e));
                    tracing::warn!(device = %self.device.name, subtree = %subtree, error = %e, "WALK failed");
                }
            }
        }

        // Drop rate baselines for counters that vanished (removed table rows)
        // — but only after a fully successful cycle, so a transient failure
        // doesn't wipe baselines that just happened to go unobserved.
        if requests > 0 && failures == 0 {
            self.rate.lock().unwrap().retain(&seen_counters);
        }

        // A whole v3 cycle failing authentication usually means the device's
        // engine identity changed (agent replaced/reset) — the client cannot
        // resynchronize that itself, so rebuild it to force rediscovery.
        if self.device.version == SnmpVersion::V3 && requests > 0 && auth_failures == requests {
            tracing::warn!(
                device = %self.device.name,
                "all requests failed authentication — rebuilding client to rediscover engine"
            );
            self.rebuild_client().await;
        }

        Ok(())
    }

    /// Read sysUpTime for reset detection; best-effort (None on any failure).
    async fn fetch_uptime_ticks(&self) -> Option<u32> {
        let oid = parse_oid(SYS_UPTIME_OID).ok()?;
        match self.client().await.ok()?.get(&oid).await {
            Ok(varbind) => match varbind.value {
                Value::TimeTicks(ticks) => Some(ticks),
                _ => None,
            },
            Err(e) => {
                tracing::debug!(device = %self.device.name, error = %e, "sysUpTime probe failed");
                None
            }
        }
    }

    /// Perform an SNMP GET operation, returning the wire value.
    async fn snmp_get(&self, oid_str: &str) -> Result<Option<(String, Value)>> {
        let oid = parse_oid(oid_str)?;
        let varbind = self
            .client()
            .await?
            .get(&oid)
            .await
            .context("SNMP GET error")?;

        if matches!(
            varbind.value,
            Value::Null | Value::NoSuchObject | Value::NoSuchInstance | Value::EndOfMibView
        ) {
            return Ok(None);
        }
        let oid_string = oid_to_string(&varbind.oid);
        Ok(Some((oid_string, varbind.value)))
    }

    /// Walk an OID subtree.
    ///
    /// The client picks GETBULK for v2c/v3 and GETNEXT for v1, stops at the
    /// subtree boundary / EndOfMibView, and bisects on tooBig.
    async fn snmp_walk(&self, subtree_str: &str) -> Result<Vec<(String, Value)>> {
        let subtree = parse_oid(subtree_str)?;
        let mut stream = self
            .client()
            .await?
            .walk(subtree)
            .context("SNMP WALK error")?;

        let mut results = Vec::new();
        while let Some(varbind) = stream.next().await {
            let varbind = varbind.context("SNMP WALK error")?;
            let oid_string = oid_to_string(&varbind.oid);
            results.push((oid_string, varbind.value));
        }
        Ok(results)
    }

    /// Publish the raw point for a polled value and, for counters, a derived
    /// `<metric>.rate` sibling (per-second Gauge) when the tracker has a
    /// plausible previous sample.
    async fn publish(
        &self,
        oid_str: &str,
        value: &Value,
        seen_counters: &mut std::collections::HashSet<String>,
    ) {
        let metric_name = self.mib_resolver.resolve(oid_str);
        let syntax = self.mib_resolver.syntax(oid_str);

        let Some((telemetry_value, unit)) = snmp_value_to_telemetry(value) else {
            return;
        };
        self.publish_point(oid_str, &metric_name, telemetry_value, unit)
            .await;

        // Counter → rate. The wire tag decides; MIB SYNTAX backs it up for
        // agents that mis-tag counters as Gauge32/Unsigned32.
        let counter = match value {
            Value::Counter32(n) => Some((u64::from(*n), true)),
            Value::Counter64(n) => Some((*n, false)),
            Value::Gauge32(n) | Value::UInteger32(n)
                if matches!(syntax, Some("Counter32" | "Counter64")) =>
            {
                Some((u64::from(*n), syntax == Some("Counter32")))
            }
            _ => None,
        };
        let Some((counter_value, is_32bit)) = counter else {
            return;
        };

        seen_counters.insert(oid_str.to_string());
        let rate = self.rate.lock().unwrap().observe(
            oid_str,
            counter_value,
            is_32bit,
            std::time::Instant::now(),
        );
        if let Some(rate) = rate {
            let rate_metric = format!("{metric_name}.rate");
            let unit = rate_unit_for(&metric_name);
            self.publish_point(
                oid_str,
                &rate_metric,
                TelemetryValue::Gauge(rate),
                Some(unit),
            )
            .await;
        }
    }

    async fn publish_point(
        &self,
        oid_str: &str,
        metric_name: &str,
        value: TelemetryValue,
        unit: Option<&str>,
    ) {
        let mut point = TelemetryPoint::new(&self.device.name, Protocol::Snmp, metric_name, value)
            .with_label("oid", oid_str);
        if let Some(unit) = unit {
            point = point.with_unit(unit);
        }

        let key = format!(
            "{}/{}/{}",
            self.telemetry_prefix, self.device.name, metric_name
        );

        match encode(&point, self.format) {
            Ok(payload) => {
                if let Err(e) = self
                    .registry
                    .put(&key, payload, zensight_common::QosClass::Telemetry)
                    .await
                {
                    tracing::error!(key = %key, error = %e, "Failed to publish to Zenoh");
                } else {
                    tracing::trace!(key = %key, "Published telemetry");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to encode telemetry");
            }
        }
    }
}

/// Unit for a derived rate metric: octet counters are bytes/second,
/// everything else (packets, errors, discards) counts per second.
fn rate_unit_for(metric_name: &str) -> &'static str {
    let last = metric_name.rsplit('/').next().unwrap_or(metric_name);
    if last.to_ascii_lowercase().contains("octets") {
        "By/s"
    } else {
        "1/s"
    }
}

/// Whether an error from the SNMP client is an authentication failure
/// (wrong credentials, engine identity mismatch, time-window rejection).
fn is_auth_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Box<async_snmp::Error>>()
        .is_some_and(|e| matches!(e.as_ref(), async_snmp::Error::Auth { .. }))
}

/// Convert an SNMP wire value to a `TelemetryValue` + optional unit (#527):
/// Gauge32/Unsigned32 are gauges (the pre-#527 code published them as
/// `Counter`), and TimeTicks converts from centiseconds to a `Gauge` in
/// seconds so consumers render durations without special-casing.
fn snmp_value_to_telemetry(value: &Value) -> Option<(TelemetryValue, Option<&'static str>)> {
    match value {
        Value::Integer(n) => Some((TelemetryValue::Gauge(f64::from(*n)), None)),
        Value::OctetString(s) => {
            // Try to interpret as UTF-8 string, fall back to binary
            match String::from_utf8(s.to_vec()) {
                Ok(text)
                    if text
                        .chars()
                        .all(|c| !c.is_control() || c == '\n' || c == '\t') =>
                {
                    Some((TelemetryValue::Text(text), None))
                }
                _ => Some((TelemetryValue::Binary(s.to_vec()), None)),
            }
        }
        Value::ObjectIdentifier(oid) => Some((TelemetryValue::Text(oid_to_string(oid)), None)),
        Value::IpAddress(ip) => Some((
            TelemetryValue::Text(format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])),
            None,
        )),
        Value::Counter32(n) => Some((TelemetryValue::Counter(u64::from(*n)), None)),
        Value::Gauge32(n) | Value::UInteger32(n) => {
            Some((TelemetryValue::Gauge(f64::from(*n)), None))
        }
        Value::TimeTicks(n) => Some((TelemetryValue::Gauge(f64::from(*n) / 100.0), Some("s"))),
        Value::Counter64(n) => Some((TelemetryValue::Counter(*n), None)),
        _ => None,
    }
}

/// Build client authentication from device configuration, preserving the
/// pre-migration validation errors.
fn build_auth(device: &DeviceConfig) -> Result<Auth> {
    match device.version {
        SnmpVersion::V1 => Ok(Auth::v1(device.community.clone())),
        SnmpVersion::V2c => Ok(Auth::v2c(device.community.clone())),
        SnmpVersion::V3 => {
            let config = device
                .security
                .as_ref()
                .ok_or_else(|| anyhow!("SNMPv3 requires security configuration"))?;

            let auth_protocol = match config.auth_protocol {
                AuthProtocol::None => None,
                AuthProtocol::Md5 => Some(async_snmp::AuthProtocol::Md5),
                AuthProtocol::Sha1 => Some(async_snmp::AuthProtocol::Sha1),
                AuthProtocol::Sha224 => Some(async_snmp::AuthProtocol::Sha224),
                AuthProtocol::Sha256 => Some(async_snmp::AuthProtocol::Sha256),
                AuthProtocol::Sha384 => Some(async_snmp::AuthProtocol::Sha384),
                AuthProtocol::Sha512 => Some(async_snmp::AuthProtocol::Sha512),
            };

            let mut usm = Auth::usm(config.username.clone());
            match (auth_protocol, config.priv_protocol) {
                // noAuthNoPriv
                (None, PrivProtocol::None) => {}
                // noAuthPriv is not valid in SNMPv3
                (None, _) => {
                    return Err(anyhow!("Privacy requires authentication in SNMPv3"));
                }
                (Some(auth_proto), priv_proto) => {
                    let auth_password = config.auth_password.as_ref().ok_or_else(|| {
                        anyhow!("Authentication password required for auth protocol")
                    })?;
                    usm = usm.auth(auth_proto, auth_password.clone());

                    if priv_proto != PrivProtocol::None {
                        let priv_password = config.priv_password.as_ref().ok_or_else(|| {
                            anyhow!("Privacy password required for privacy protocol")
                        })?;
                        let cipher = match priv_proto {
                            PrivProtocol::None => unreachable!("guarded above"),
                            PrivProtocol::Des => async_snmp::PrivProtocol::Des,
                            PrivProtocol::Aes128 => async_snmp::PrivProtocol::Aes128,
                            PrivProtocol::Aes192 => async_snmp::PrivProtocol::Aes192,
                            PrivProtocol::Aes256 => async_snmp::PrivProtocol::Aes256,
                        };
                        usm = usm.privacy(cipher, priv_password.clone());
                    }
                }
            }
            Ok(usm.into())
        }
    }
}

/// Pre-seed an engine cache with a configured engine ID (hex), skipping the
/// discovery round-trip. Boots/time start at zero — the first authenticated
/// exchange time-syncs through the standard report flow.
///
/// The cache is keyed by socket address, so this only works when `address`
/// is a literal `ip:port`; hostnames fall back to auto-discovery.
fn seeded_engine_cache(device: &DeviceConfig) -> Option<Arc<EngineCache>> {
    let hex = device.security.as_ref()?.engine_id.as_ref()?;

    let Some(engine_id) = parse_hex(hex) else {
        tracing::warn!(
            device = %device.name,
            engine_id = %hex,
            "Configured engine_id is not valid hex — falling back to discovery"
        );
        return None;
    };
    let Ok(target) = device.address.parse::<std::net::SocketAddr>() else {
        tracing::warn!(
            device = %device.name,
            address = %device.address,
            "Configured engine_id needs a literal ip:port address — falling back to discovery"
        );
        return None;
    };

    let cache = Arc::new(EngineCache::new());
    cache.insert(target, EngineState::new(Bytes::from(engine_id), 0, 0));
    Some(cache)
}

/// Decode a hex string, tolerating an `0x` prefix and `:` separators.
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s).replace(':', "");
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("80000001"), Some(vec![0x80, 0x00, 0x00, 0x01]));
        assert_eq!(parse_hex("0x8000"), Some(vec![0x80, 0x00]));
        assert_eq!(parse_hex("80:00:00:01"), Some(vec![0x80, 0x00, 0x00, 0x01]));
        assert_eq!(parse_hex("8"), None);
        assert_eq!(parse_hex("zz"), None);
        assert_eq!(parse_hex(""), None);
    }
}
