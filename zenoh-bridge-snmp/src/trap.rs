use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use zenoh::Session as ZenohSession;

use zensight_common::{encode, Format, KeyExprBuilder, Protocol, TelemetryPoint, TelemetryValue};

use crate::oid::OidNameMapper;

/// SNMP trap receiver.
pub struct TrapReceiver {
    bind_addr: String,
    zenoh: Arc<ZenohSession>,
    key_builder: KeyExprBuilder,
    #[allow(dead_code)]
    oid_mapper: OidNameMapper,
    format: Format,
}

impl TrapReceiver {
    /// Create a new trap receiver.
    pub fn new(
        bind_addr: &str,
        zenoh: Arc<ZenohSession>,
        key_prefix: &str,
        oid_names: &HashMap<String, String>,
        format: Format,
    ) -> Self {
        Self {
            bind_addr: bind_addr.to_string(),
            zenoh,
            key_builder: KeyExprBuilder::with_prefix(key_prefix, Protocol::Snmp),
            oid_mapper: OidNameMapper::new(oid_names),
            format,
        }
    }

    /// Bind and run the trap receiver.
    pub async fn run(self) -> Result<()> {
        let socket = UdpSocket::bind(&self.bind_addr)
            .await
            .with_context(|| format!("Failed to bind trap listener to {}", self.bind_addr))?;

        tracing::info!(bind = %self.bind_addr, "SNMP trap receiver started");

        let mut buf = vec![0u8; 65535];

        loop {
            match socket.recv_from(&mut buf).await {
                Ok((len, src_addr)) => {
                    let data = &buf[..len];
                    tracing::debug!(
                        src = %src_addr,
                        len = len,
                        "Received SNMP trap"
                    );

                    // Parse and publish the trap
                    if let Err(e) = self.handle_trap(data, &src_addr.ip().to_string()).await {
                        tracing::warn!(
                            src = %src_addr,
                            error = %e,
                            "Failed to process trap"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Error receiving trap");
                }
            }
        }
    }

    /// Handle an incoming SNMP trap.
    async fn handle_trap(&self, _data: &[u8], source_ip: &str) -> Result<()> {
        // Note: Full trap parsing requires decoding the SNMP PDU.
        // The snmp2 crate focuses on client operations, not trap reception.
        // For now, we'll publish a basic trap notification.
        // A full implementation would use rasn-snmp or similar for decoding.

        // Publish a trap received notification
        let point = TelemetryPoint::new(
            source_ip,
            Protocol::Snmp,
            "trap/received",
            TelemetryValue::Counter(1),
        )
        .with_label("type", "trap");

        let key = self.key_builder.build(source_ip, "trap/received");

        let payload = encode(&point, self.format).context("Failed to encode trap")?;

        self.zenoh
            .put(&key, payload)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to publish trap: {}", e))?;

        tracing::debug!(key = %key, "Published trap notification");

        Ok(())
    }
}
