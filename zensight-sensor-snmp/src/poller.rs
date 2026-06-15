use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use snmp2::{AsyncSession, Value, v3};
use tokio::sync::Mutex;
use tokio::time::{interval, timeout};
use zenoh::Session as ZenohSession;

use zensight_common::{Format, KeyExprBuilder, Protocol, TelemetryPoint, TelemetryValue, encode};

use crate::config::{
    AuthProtocol, DeviceConfig, OidGroup, PrivProtocol, SnmpV3Security, SnmpVersion,
};
use crate::mib::MibResolver;
use crate::oid::{oid_starts_with, oid_to_string, parse_oid};

/// SNMP poller for a single device.
pub struct SnmpPoller {
    device: DeviceConfig,
    zenoh: Arc<ZenohSession>,
    key_builder: KeyExprBuilder,
    mib_resolver: Arc<MibResolver>,
    format: Format,
    oids: Vec<String>,
    walks: Vec<String>,
    request_timeout: Duration,
    /// Persistent session for SNMPv3 (to maintain engine ID and time sync).
    v3_session: Option<Mutex<AsyncSession>>,
}

impl SnmpPoller {
    /// Create a new poller for a device.
    pub fn new(
        device: DeviceConfig,
        zenoh: Arc<ZenohSession>,
        key_prefix: &str,
        mib_resolver: Arc<MibResolver>,
        oid_groups: &HashMap<String, OidGroup>,
        format: Format,
    ) -> Self {
        let key_builder = KeyExprBuilder::with_prefix(key_prefix, Protocol::Snmp);

        let oids = device.all_oids(oid_groups);
        let walks = device.all_walks(oid_groups);

        Self {
            device,
            zenoh,
            key_builder,
            mib_resolver,
            format,
            oids,
            walks,
            request_timeout: Duration::from_secs(5),
            v3_session: None,
        }
    }

    /// Initialize the poller (required for SNMPv3 to discover engine ID).
    pub async fn init(&mut self) -> Result<()> {
        if self.device.version == SnmpVersion::V3 {
            let session = self.create_v3_session().await?;
            self.v3_session = Some(Mutex::new(session));
            tracing::info!(
                device = %self.device.name,
                "SNMPv3 session initialized"
            );
        }
        Ok(())
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
    async fn poll_once(&self) -> Result<()> {
        // Poll individual OIDs with GET
        for oid_str in &self.oids {
            match self.snmp_get(oid_str).await {
                Ok(Some((oid, value))) => {
                    self.publish(&oid, value).await;
                }
                Ok(None) => {
                    tracing::debug!(device = %self.device.name, oid = %oid_str, "No value returned");
                }
                Err(e) => {
                    tracing::warn!(device = %self.device.name, oid = %oid_str, error = %e, "GET failed");
                }
            }
        }

        // Walk OID subtrees with GETNEXT
        for subtree in &self.walks {
            match self.snmp_walk(subtree).await {
                Ok(entries) => {
                    for (oid, value) in entries {
                        self.publish(&oid, value).await;
                    }
                }
                Err(e) => {
                    tracing::warn!(device = %self.device.name, subtree = %subtree, error = %e, "WALK failed");
                }
            }
        }

        Ok(())
    }

    /// Create an SNMP v1/v2c session for this device.
    async fn create_session(&self) -> Result<AsyncSession> {
        let community = self.device.community.as_bytes();

        let session = match self.device.version {
            SnmpVersion::V1 => AsyncSession::new_v1(&self.device.address, community, 0)
                .await
                .context("Failed to create SNMPv1 session")?,
            SnmpVersion::V2c => AsyncSession::new_v2c(&self.device.address, community, 0)
                .await
                .context("Failed to create SNMPv2c session")?,
            SnmpVersion::V3 => {
                return Err(anyhow!("Use create_v3_session for SNMPv3"));
            }
        };

        Ok(session)
    }

    /// Create an SNMPv3 session with USM authentication.
    async fn create_v3_session(&self) -> Result<AsyncSession> {
        let security_config = self
            .device
            .security
            .as_ref()
            .ok_or_else(|| anyhow!("SNMPv3 requires security configuration"))?;

        let security = build_v3_security(security_config)?;

        let mut session = AsyncSession::new_v3(&self.device.address, 0, security)
            .await
            .context("Failed to create SNMPv3 session")?;

        // Initialize the session to discover engine ID
        session
            .init()
            .await
            .context("Failed to initialize SNMPv3 session")?;

        Ok(session)
    }

    /// Perform an SNMP GET operation.
    async fn snmp_get(&self, oid_str: &str) -> Result<Option<(String, TelemetryValue)>> {
        let oid = parse_oid(oid_str)?;

        if self.device.version == SnmpVersion::V3 {
            // Use persistent v3 session
            let session_mutex = self
                .v3_session
                .as_ref()
                .ok_or_else(|| anyhow!("SNMPv3 session not initialized"))?;
            let mut session = session_mutex.lock().await;
            let mut response = timeout(self.request_timeout, session.get(&oid))
                .await
                .map_err(|_| anyhow!("SNMP GET timeout"))?
                .context("SNMP GET error")?;

            if let Some((resp_oid, value)) = response.varbinds.next() {
                let oid_string = oid_to_string(&resp_oid);
                if let Some(tv) = snmp_value_to_telemetry(&value) {
                    return Ok(Some((oid_string, tv)));
                }
            }
        } else {
            let mut session = self.create_session().await?;
            let mut response = timeout(self.request_timeout, session.get(&oid))
                .await
                .map_err(|_| anyhow!("SNMP GET timeout"))?
                .context("SNMP GET error")?;

            if let Some((resp_oid, value)) = response.varbinds.next() {
                let oid_string = oid_to_string(&resp_oid);
                if let Some(tv) = snmp_value_to_telemetry(&value) {
                    return Ok(Some((oid_string, tv)));
                }
            }
        }

        Ok(None)
    }

    /// Perform an SNMP WALK operation (using GETNEXT).
    async fn snmp_walk(&self, subtree_str: &str) -> Result<Vec<(String, TelemetryValue)>> {
        let subtree = parse_oid(subtree_str)?;
        let mut results = Vec::new();
        let mut current_oid = subtree.clone();

        if self.device.version == SnmpVersion::V3 {
            // Use persistent v3 session
            let session_mutex = self
                .v3_session
                .as_ref()
                .ok_or_else(|| anyhow!("SNMPv3 session not initialized"))?;
            let mut session = session_mutex.lock().await;

            loop {
                let mut response = timeout(self.request_timeout, session.getnext(&current_oid))
                    .await
                    .map_err(|_| anyhow!("SNMP GETNEXT timeout"))?
                    .context("SNMP GETNEXT error")?;

                let Some((resp_oid, value)) = response.varbinds.next() else {
                    break;
                };

                if !oid_starts_with(&resp_oid, &subtree) {
                    break;
                }

                if matches!(value, Value::EndOfMibView) {
                    break;
                }

                let oid_string = oid_to_string(&resp_oid);
                if let Some(tv) = snmp_value_to_telemetry(&value) {
                    results.push((oid_string, tv));
                }

                current_oid = resp_oid.to_owned();
            }
        } else {
            let mut session = self.create_session().await?;

            loop {
                let mut response = timeout(self.request_timeout, session.getnext(&current_oid))
                    .await
                    .map_err(|_| anyhow!("SNMP GETNEXT timeout"))?
                    .context("SNMP GETNEXT error")?;

                let Some((resp_oid, value)) = response.varbinds.next() else {
                    break;
                };

                if !oid_starts_with(&resp_oid, &subtree) {
                    break;
                }

                if matches!(value, Value::EndOfMibView) {
                    break;
                }

                let oid_string = oid_to_string(&resp_oid);
                if let Some(tv) = snmp_value_to_telemetry(&value) {
                    results.push((oid_string, tv));
                }

                current_oid = resp_oid.to_owned();
            }
        }

        Ok(results)
    }

    /// Publish a telemetry point to Zenoh.
    async fn publish(&self, oid_str: &str, value: TelemetryValue) {
        let metric_name = self.mib_resolver.resolve(oid_str);

        let point = TelemetryPoint::new(&self.device.name, Protocol::Snmp, &metric_name, value)
            .with_label("oid", oid_str);

        let key = self.key_builder.build(&self.device.name, &metric_name);

        match encode(&point, self.format) {
            Ok(payload) => {
                if let Err(e) = self.zenoh.put(&key, payload).await {
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

/// Convert an SNMP Value to a TelemetryValue.
fn snmp_value_to_telemetry(value: &Value) -> Option<TelemetryValue> {
    match value {
        Value::Integer(n) => Some(TelemetryValue::Gauge(*n as f64)),
        Value::OctetString(s) => {
            // Try to interpret as UTF-8 string, fall back to binary
            match String::from_utf8(s.to_vec()) {
                Ok(text)
                    if text
                        .chars()
                        .all(|c| !c.is_control() || c == '\n' || c == '\t') =>
                {
                    Some(TelemetryValue::Text(text))
                }
                _ => Some(TelemetryValue::Binary(s.to_vec())),
            }
        }
        Value::ObjectIdentifier(oid) => Some(TelemetryValue::Text(oid_to_string(oid))),
        Value::IpAddress(ip) => Some(TelemetryValue::Text(format!(
            "{}.{}.{}.{}",
            ip[0], ip[1], ip[2], ip[3]
        ))),
        Value::Counter32(n) => Some(TelemetryValue::Counter(*n as u64)),
        Value::Unsigned32(n) => Some(TelemetryValue::Counter(*n as u64)),
        Value::Timeticks(n) => Some(TelemetryValue::Counter(*n as u64)),
        Value::Counter64(n) => Some(TelemetryValue::Counter(*n)),
        Value::Null | Value::NoSuchObject | Value::NoSuchInstance | Value::EndOfMibView => None,
        _ => None,
    }
}

/// Build SNMPv3 security parameters from configuration.
fn build_v3_security(config: &SnmpV3Security) -> Result<v3::Security> {
    let username = config.username.as_bytes();

    // Determine authentication protocol
    let auth_protocol = match config.auth_protocol {
        AuthProtocol::None => None,
        AuthProtocol::Md5 => Some(v3::AuthProtocol::Md5),
        AuthProtocol::Sha1 => Some(v3::AuthProtocol::Sha1),
        AuthProtocol::Sha224 => Some(v3::AuthProtocol::Sha224),
        AuthProtocol::Sha256 => Some(v3::AuthProtocol::Sha256),
        AuthProtocol::Sha384 => Some(v3::AuthProtocol::Sha384),
        AuthProtocol::Sha512 => Some(v3::AuthProtocol::Sha512),
    };

    // Build security based on auth/priv levels
    let security = match (auth_protocol, config.priv_protocol) {
        // noAuthNoPriv
        (None, PrivProtocol::None) => v3::Security::new(username, b""),
        // authNoPriv
        (Some(auth_proto), PrivProtocol::None) => {
            let auth_password = config
                .auth_password
                .as_ref()
                .ok_or_else(|| anyhow!("Authentication password required for auth protocol"))?;
            v3::Security::new(username, auth_password.as_bytes()).with_auth_protocol(auth_proto)
        }
        // authPriv
        (Some(auth_proto), priv_proto) => {
            let auth_password = config
                .auth_password
                .as_ref()
                .ok_or_else(|| anyhow!("Authentication password required for auth protocol"))?;
            let priv_password = config
                .priv_password
                .as_ref()
                .ok_or_else(|| anyhow!("Privacy password required for privacy protocol"))?;

            let cipher = match priv_proto {
                PrivProtocol::None => unreachable!(),
                PrivProtocol::Des => v3::Cipher::Des,
                PrivProtocol::Aes128 => v3::Cipher::Aes128,
                PrivProtocol::Aes192 => v3::Cipher::Aes192,
                PrivProtocol::Aes256 => v3::Cipher::Aes256,
            };

            v3::Security::new(username, auth_password.as_bytes())
                .with_auth_protocol(auth_proto)
                .with_auth(v3::Auth::AuthPriv {
                    cipher,
                    privacy_password: priv_password.as_bytes().to_vec(),
                })
        }
        // noAuthPriv is not valid in SNMPv3
        (None, _) => {
            return Err(anyhow!("Privacy requires authentication in SNMPv3"));
        }
    };

    Ok(security)
}
