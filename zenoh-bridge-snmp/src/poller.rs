use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use snmp2::{AsyncSession, Value};
use tokio::time::{interval, timeout};
use zenoh::Session as ZenohSession;

use zensight_common::{encode, Format, KeyExprBuilder, Protocol, TelemetryPoint, TelemetryValue};

use crate::config::{DeviceConfig, OidGroup, SnmpVersion};
use crate::oid::{oid_starts_with, oid_to_string, parse_oid, OidNameMapper};

/// SNMP poller for a single device.
pub struct SnmpPoller {
    device: DeviceConfig,
    zenoh: Arc<ZenohSession>,
    key_builder: KeyExprBuilder,
    oid_mapper: OidNameMapper,
    format: Format,
    oids: Vec<String>,
    walks: Vec<String>,
    request_timeout: Duration,
}

impl SnmpPoller {
    /// Create a new poller for a device.
    pub fn new(
        device: DeviceConfig,
        zenoh: Arc<ZenohSession>,
        key_prefix: &str,
        oid_names: &HashMap<String, String>,
        oid_groups: &HashMap<String, OidGroup>,
        format: Format,
    ) -> Self {
        let key_builder = KeyExprBuilder::with_prefix(key_prefix, Protocol::Snmp);
        let oid_mapper = OidNameMapper::new(oid_names);

        let oids = device.all_oids(oid_groups);
        let walks = device.all_walks(oid_groups);

        Self {
            device,
            zenoh,
            key_builder,
            oid_mapper,
            format,
            oids,
            walks,
            request_timeout: Duration::from_secs(5),
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

    /// Create an SNMP session for this device.
    async fn create_session(&self) -> Result<AsyncSession> {
        let community = self.device.community.as_bytes();

        let session = match self.device.version {
            SnmpVersion::V1 => AsyncSession::new_v1(&self.device.address, community, 0)
                .await
                .context("Failed to create SNMPv1 session")?,
            SnmpVersion::V2c => AsyncSession::new_v2c(&self.device.address, community, 0)
                .await
                .context("Failed to create SNMPv2c session")?,
        };

        Ok(session)
    }

    /// Perform an SNMP GET operation.
    async fn snmp_get(&self, oid_str: &str) -> Result<Option<(String, TelemetryValue)>> {
        let oid = parse_oid(oid_str)?;
        let mut session = self.create_session().await?;

        let response = timeout(self.request_timeout, session.get(&oid))
            .await
            .map_err(|_| anyhow!("SNMP GET timeout"))?
            .context("SNMP GET error")?;

        if let Some((resp_oid, value)) = response.varbinds.into_iter().next() {
            let oid_string = oid_to_string(&resp_oid);
            if let Some(tv) = snmp_value_to_telemetry(&value) {
                return Ok(Some((oid_string, tv)));
            }
        }

        Ok(None)
    }

    /// Perform an SNMP WALK operation (using GETNEXT).
    async fn snmp_walk(&self, subtree_str: &str) -> Result<Vec<(String, TelemetryValue)>> {
        let subtree = parse_oid(subtree_str)?;
        let mut results = Vec::new();
        let mut current_oid = subtree.clone();
        let mut session = self.create_session().await?;

        loop {
            let response = timeout(self.request_timeout, session.getnext(&current_oid))
                .await
                .map_err(|_| anyhow!("SNMP GETNEXT timeout"))?
                .context("SNMP GETNEXT error")?;

            let Some((resp_oid, value)) = response.varbinds.into_iter().next() else {
                break;
            };

            // Check if we're still within the subtree
            if !oid_starts_with(&resp_oid, &subtree) {
                break;
            }

            // Check for end of MIB
            if matches!(value, Value::EndOfMibView) {
                break;
            }

            let oid_string = oid_to_string(&resp_oid);
            if let Some(tv) = snmp_value_to_telemetry(&value) {
                results.push((oid_string, tv));
            }

            current_oid = resp_oid.to_owned();
        }

        Ok(results)
    }

    /// Publish a telemetry point to Zenoh.
    async fn publish(&self, oid_str: &str, value: TelemetryValue) {
        let metric_name = self.oid_mapper.get_name(oid_str);

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
