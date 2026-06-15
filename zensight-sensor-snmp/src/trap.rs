use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use rasn_smi::v1 as smi_v1;
use rasn_smi::v2 as smi_v2;
use rasn_snmp::v1;
use rasn_snmp::v2;
use rasn_snmp::v2c;
use tokio::net::UdpSocket;
use zenoh::Session as ZenohSession;

use zensight_common::{Format, KeyExprBuilder, Protocol, TelemetryPoint, TelemetryValue, encode};

use crate::mib::MibResolver;

/// Generic trap types as defined in RFC 1157.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericTrap {
    ColdStart = 0,
    WarmStart = 1,
    LinkDown = 2,
    LinkUp = 3,
    AuthenticationFailure = 4,
    EgpNeighborLoss = 5,
    EnterpriseSpecific = 6,
}

impl GenericTrap {
    fn from_integer(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::ColdStart),
            1 => Some(Self::WarmStart),
            2 => Some(Self::LinkDown),
            3 => Some(Self::LinkUp),
            4 => Some(Self::AuthenticationFailure),
            5 => Some(Self::EgpNeighborLoss),
            6 => Some(Self::EnterpriseSpecific),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::ColdStart => "coldStart",
            Self::WarmStart => "warmStart",
            Self::LinkDown => "linkDown",
            Self::LinkUp => "linkUp",
            Self::AuthenticationFailure => "authenticationFailure",
            Self::EgpNeighborLoss => "egpNeighborLoss",
            Self::EnterpriseSpecific => "enterpriseSpecific",
        }
    }
}

/// Parsed SNMP trap data.
#[derive(Debug, Clone)]
pub struct ParsedTrap {
    /// Source IP address of the trap sender.
    pub source_ip: String,
    /// SNMP version (1 or 2).
    pub version: u8,
    /// Community string (for v1/v2c).
    pub community: Option<String>,
    /// Enterprise OID (v1 only).
    pub enterprise_oid: Option<String>,
    /// Generic trap type (v1 only).
    pub generic_trap: Option<GenericTrap>,
    /// Specific trap code (v1 only, used with enterprise-specific traps).
    pub specific_trap: Option<i64>,
    /// Trap OID (v2 only, from snmpTrapOID.0 varbind).
    pub trap_oid: Option<String>,
    /// Variable bindings from the trap.
    pub varbinds: Vec<VarBind>,
}

/// A single variable binding from a trap.
#[derive(Debug, Clone)]
pub struct VarBind {
    /// OID of the variable.
    pub oid: String,
    /// Value of the variable.
    pub value: TelemetryValue,
}

/// SNMP trap receiver.
pub struct TrapReceiver {
    bind_addr: String,
    zenoh: Arc<ZenohSession>,
    key_builder: KeyExprBuilder,
    mib_resolver: Arc<MibResolver>,
    format: Format,
}

impl TrapReceiver {
    /// Create a new trap receiver.
    pub fn new(
        bind_addr: &str,
        zenoh: Arc<ZenohSession>,
        key_prefix: &str,
        mib_resolver: Arc<MibResolver>,
        format: Format,
    ) -> Self {
        Self {
            bind_addr: bind_addr.to_string(),
            zenoh,
            key_builder: KeyExprBuilder::with_prefix(key_prefix, Protocol::Snmp),
            mib_resolver,
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
                    if let Err(e) = self.handle_trap(data, src_addr.ip()).await {
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
    async fn handle_trap(&self, data: &[u8], source_ip: IpAddr) -> Result<()> {
        let source_ip_str = source_ip.to_string();

        // Try to parse the trap
        let parsed = parse_trap(data, &source_ip_str)?;

        tracing::debug!(
            version = parsed.version,
            community = ?parsed.community,
            enterprise = ?parsed.enterprise_oid,
            trap_oid = ?parsed.trap_oid,
            varbinds = parsed.varbinds.len(),
            "Parsed SNMP trap"
        );

        // Publish the trap as telemetry
        self.publish_trap(&parsed).await?;

        Ok(())
    }

    /// Publish a parsed trap as telemetry points.
    async fn publish_trap(&self, trap: &ParsedTrap) -> Result<()> {
        // Determine trap identifier for the key expression
        let trap_id = if let Some(ref trap_oid) = trap.trap_oid {
            // v2: use the trap OID
            self.mib_resolver.resolve(trap_oid)
        } else if let Some(generic) = trap.generic_trap {
            // v1: use generic trap name
            if generic == GenericTrap::EnterpriseSpecific {
                format!(
                    "enterprise/{}/{}",
                    trap.enterprise_oid.as_deref().unwrap_or("unknown"),
                    trap.specific_trap.unwrap_or(0)
                )
            } else {
                format!("trap/{}", generic.as_str())
            }
        } else {
            "trap/unknown".to_string()
        };

        // Build labels
        let mut labels = HashMap::new();
        labels.insert("type".to_string(), "trap".to_string());
        labels.insert("version".to_string(), format!("v{}", trap.version));

        if let Some(ref community) = trap.community {
            labels.insert("community".to_string(), community.clone());
        }
        if let Some(ref enterprise) = trap.enterprise_oid {
            labels.insert("enterprise".to_string(), enterprise.clone());
        }
        if let Some(generic) = trap.generic_trap {
            labels.insert("generic_trap".to_string(), generic.as_str().to_string());
        }
        if let Some(specific) = trap.specific_trap {
            labels.insert("specific_trap".to_string(), specific.to_string());
        }
        if let Some(ref trap_oid) = trap.trap_oid {
            labels.insert("trap_oid".to_string(), trap_oid.clone());
        }

        // Publish main trap notification
        let metric_name = format!("trap/{}", trap_id);
        let key = self.key_builder.build(&trap.source_ip, &metric_name);

        let point = TelemetryPoint {
            timestamp: zensight_common::current_timestamp_millis(),
            source: trap.source_ip.clone(),
            protocol: Protocol::Snmp,
            metric: metric_name.clone(),
            value: TelemetryValue::Counter(1),
            labels: labels.clone(),
        };

        let payload = encode(&point, self.format).context("Failed to encode trap")?;
        self.zenoh
            .put(&key, payload)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to publish trap: {}", e))?;

        tracing::debug!(key = %key, "Published trap notification");

        // Publish each varbind as a separate telemetry point
        for varbind in &trap.varbinds {
            let varbind_name = self.mib_resolver.resolve(&varbind.oid);
            let varbind_metric = format!("trap/{}/{}", trap_id, varbind_name);
            let varbind_key = self.key_builder.build(&trap.source_ip, &varbind_metric);

            let mut varbind_labels = labels.clone();
            varbind_labels.insert("oid".to_string(), varbind.oid.clone());

            let varbind_point = TelemetryPoint {
                timestamp: zensight_common::current_timestamp_millis(),
                source: trap.source_ip.clone(),
                protocol: Protocol::Snmp,
                metric: varbind_metric,
                value: varbind.value.clone(),
                labels: varbind_labels,
            };

            let varbind_payload =
                encode(&varbind_point, self.format).context("Failed to encode varbind")?;
            self.zenoh
                .put(&varbind_key, varbind_payload)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to publish varbind: {}", e))?;

            tracing::trace!(key = %varbind_key, oid = %varbind.oid, "Published trap varbind");
        }

        Ok(())
    }
}

/// Parse an SNMP trap from raw bytes.
pub fn parse_trap(data: &[u8], source_ip: &str) -> Result<ParsedTrap> {
    // Try SNMPv1 first
    if let Ok(msg) = rasn::ber::decode::<v1::Message<v1::Pdus>>(data) {
        return parse_v1_trap(msg, source_ip);
    }

    // Try SNMPv2c
    if let Ok(msg) = rasn::ber::decode::<v2c::Message<v2::Pdus>>(data) {
        return parse_v2c_trap(msg, source_ip);
    }

    anyhow::bail!("Failed to parse SNMP trap: unsupported format or corrupt data")
}

/// Parse an SNMPv1 trap message.
fn parse_v1_trap(msg: v1::Message<v1::Pdus>, source_ip: &str) -> Result<ParsedTrap> {
    let community = String::from_utf8_lossy(&msg.community).to_string();

    match msg.data {
        v1::Pdus::Trap(trap) => {
            let enterprise_oid = oid_to_string(&trap.enterprise);

            // Convert Integer to i64
            let generic_trap_value = integer_to_i64(&trap.generic_trap);
            let generic_trap = GenericTrap::from_integer(generic_trap_value);
            let specific_trap = integer_to_i64(&trap.specific_trap);

            let varbinds = parse_v1_varbinds(&trap.variable_bindings);

            Ok(ParsedTrap {
                source_ip: source_ip.to_string(),
                version: 1,
                community: Some(community),
                enterprise_oid: Some(enterprise_oid),
                generic_trap,
                specific_trap: Some(specific_trap),
                trap_oid: None,
                varbinds,
            })
        }
        _ => anyhow::bail!("Expected SNMPv1 trap PDU, got different PDU type"),
    }
}

/// Parse an SNMPv2c trap message.
fn parse_v2c_trap(msg: v2c::Message<v2::Pdus>, source_ip: &str) -> Result<ParsedTrap> {
    let community = String::from_utf8_lossy(&msg.community).to_string();

    match msg.data {
        v2::Pdus::Trap(trap) => {
            let varbinds = parse_v2_varbinds(&trap.0.variable_bindings);

            // Extract snmpTrapOID.0 (1.3.6.1.6.3.1.1.4.1.0)
            let mut trap_oid = None;

            for vb in &varbinds {
                if vb.oid == "1.3.6.1.6.3.1.1.4.1.0" {
                    // snmpTrapOID.0
                    if let TelemetryValue::Text(ref s) = vb.value {
                        trap_oid = Some(s.clone());
                    }
                }
            }

            Ok(ParsedTrap {
                source_ip: source_ip.to_string(),
                version: 2,
                community: Some(community),
                enterprise_oid: None,
                generic_trap: None,
                specific_trap: None,
                trap_oid,
                varbinds,
            })
        }
        v2::Pdus::InformRequest(inform) => {
            // InformRequest is similar to Trap but expects a response
            let varbinds = parse_v2_varbinds(&inform.0.variable_bindings);

            let mut trap_oid = None;

            for vb in &varbinds {
                if vb.oid == "1.3.6.1.6.3.1.1.4.1.0"
                    && let TelemetryValue::Text(ref s) = vb.value
                {
                    trap_oid = Some(s.clone());
                }
            }

            Ok(ParsedTrap {
                source_ip: source_ip.to_string(),
                version: 2,
                community: Some(community),
                enterprise_oid: None,
                generic_trap: None,
                specific_trap: None,
                trap_oid,
                varbinds,
            })
        }
        _ => anyhow::bail!("Expected SNMPv2c trap PDU, got different PDU type"),
    }
}

/// Parse SNMPv1 variable bindings.
fn parse_v1_varbinds(varbinds: &v1::VarBindList) -> Vec<VarBind> {
    varbinds
        .iter()
        .map(|vb| VarBind {
            oid: oid_to_string(&vb.name),
            value: v1_object_syntax_to_value(&vb.value),
        })
        .collect()
}

/// Parse SNMPv2 variable bindings.
fn parse_v2_varbinds(varbinds: &v2::VarBindList) -> Vec<VarBind> {
    varbinds
        .iter()
        .map(|vb| VarBind {
            oid: oid_to_string(&vb.name),
            value: v2_varbind_value_to_telemetry(&vb.value),
        })
        .collect()
}

/// Convert an ObjectIdentifier to a dotted string.
fn oid_to_string(oid: &rasn::types::ObjectIdentifier) -> String {
    oid.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// Convert a rasn Integer to i64.
fn integer_to_i64(int: &rasn::types::Integer) -> i64 {
    // Try to convert to i64; if it fails (too large), return 0
    i64::try_from(int).unwrap_or(0)
}

/// Convert a NetworkAddress to a string.
fn network_addr_to_string(addr: &smi_v1::NetworkAddress) -> String {
    match addr {
        smi_v1::NetworkAddress::Internet(ip_addr) => {
            // IpAddress is a newtype around [u8; 4] or similar
            format!(
                "{}.{}.{}.{}",
                ip_addr.0[0], ip_addr.0[1], ip_addr.0[2], ip_addr.0[3]
            )
        }
    }
}

/// Convert SNMPv1 ObjectSyntax to TelemetryValue.
fn v1_object_syntax_to_value(syntax: &smi_v1::ObjectSyntax) -> TelemetryValue {
    match syntax {
        smi_v1::ObjectSyntax::Simple(simple) => v1_simple_syntax_to_value(simple),
        smi_v1::ObjectSyntax::ApplicationWide(app) => v1_application_syntax_to_value(app),
    }
}

/// Convert v1 SimpleSyntax to TelemetryValue.
fn v1_simple_syntax_to_value(simple: &smi_v1::SimpleSyntax) -> TelemetryValue {
    match simple {
        smi_v1::SimpleSyntax::Number(n) => TelemetryValue::Gauge(integer_to_i64(n) as f64),
        smi_v1::SimpleSyntax::String(s) => {
            TelemetryValue::Text(String::from_utf8_lossy(s).to_string())
        }
        smi_v1::SimpleSyntax::Object(oid) => TelemetryValue::Text(oid_to_string(oid)),
        smi_v1::SimpleSyntax::Empty => TelemetryValue::Text("".to_string()),
    }
}

/// Convert v1 ApplicationSyntax to TelemetryValue.
fn v1_application_syntax_to_value(app: &smi_v1::ApplicationSyntax) -> TelemetryValue {
    match app {
        smi_v1::ApplicationSyntax::Counter(c) => TelemetryValue::Counter(c.0.into()),
        smi_v1::ApplicationSyntax::Gauge(g) => TelemetryValue::Gauge(g.0 as f64),
        smi_v1::ApplicationSyntax::Ticks(t) => TelemetryValue::Counter(t.0.into()),
        smi_v1::ApplicationSyntax::Arbitrary(bytes) => {
            TelemetryValue::Binary(bytes.as_ref().to_vec())
        }
        smi_v1::ApplicationSyntax::Address(addr) => {
            TelemetryValue::Text(network_addr_to_string(addr))
        }
    }
}

/// Convert SNMPv2 VarBindValue to TelemetryValue.
fn v2_varbind_value_to_telemetry(value: &v2::VarBindValue) -> TelemetryValue {
    match value {
        v2::VarBindValue::Value(syntax) => v2_object_syntax_to_value(syntax),
        v2::VarBindValue::Unspecified => TelemetryValue::Text("unspecified".to_string()),
        v2::VarBindValue::NoSuchObject => TelemetryValue::Text("noSuchObject".to_string()),
        v2::VarBindValue::NoSuchInstance => TelemetryValue::Text("noSuchInstance".to_string()),
        v2::VarBindValue::EndOfMibView => TelemetryValue::Text("endOfMibView".to_string()),
    }
}

/// Convert SNMPv2 ObjectSyntax to TelemetryValue.
fn v2_object_syntax_to_value(syntax: &smi_v2::ObjectSyntax) -> TelemetryValue {
    match syntax {
        smi_v2::ObjectSyntax::Simple(simple) => v2_simple_syntax_to_value(simple),
        smi_v2::ObjectSyntax::ApplicationWide(app) => v2_application_syntax_to_value(app),
    }
}

/// Convert SNMPv2 SimpleSyntax to TelemetryValue.
fn v2_simple_syntax_to_value(simple: &smi_v2::SimpleSyntax) -> TelemetryValue {
    match simple {
        smi_v2::SimpleSyntax::Integer(n) => TelemetryValue::Gauge(integer_to_i64(n) as f64),
        smi_v2::SimpleSyntax::String(s) => {
            TelemetryValue::Text(String::from_utf8_lossy(s).to_string())
        }
        smi_v2::SimpleSyntax::ObjectId(oid) => TelemetryValue::Text(oid_to_string(oid)),
    }
}

/// Convert SNMPv2 ApplicationSyntax to TelemetryValue.
fn v2_application_syntax_to_value(app: &smi_v2::ApplicationSyntax) -> TelemetryValue {
    match app {
        smi_v2::ApplicationSyntax::Counter(c) => TelemetryValue::Counter(c.0.into()),
        smi_v2::ApplicationSyntax::Unsigned(u) => TelemetryValue::Gauge(u.0 as f64),
        smi_v2::ApplicationSyntax::Ticks(t) => TelemetryValue::Counter(t.0.into()),
        smi_v2::ApplicationSyntax::Arbitrary(bytes) => {
            TelemetryValue::Binary(bytes.as_ref().to_vec())
        }
        smi_v2::ApplicationSyntax::Address(addr) => TelemetryValue::Text(format!(
            "{}.{}.{}.{}",
            addr.0[0], addr.0[1], addr.0[2], addr.0[3]
        )),
        smi_v2::ApplicationSyntax::BigCounter(c) => TelemetryValue::Counter(c.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generic_trap_from_integer() {
        assert_eq!(GenericTrap::from_integer(0), Some(GenericTrap::ColdStart));
        assert_eq!(GenericTrap::from_integer(1), Some(GenericTrap::WarmStart));
        assert_eq!(GenericTrap::from_integer(2), Some(GenericTrap::LinkDown));
        assert_eq!(GenericTrap::from_integer(3), Some(GenericTrap::LinkUp));
        assert_eq!(
            GenericTrap::from_integer(4),
            Some(GenericTrap::AuthenticationFailure)
        );
        assert_eq!(
            GenericTrap::from_integer(5),
            Some(GenericTrap::EgpNeighborLoss)
        );
        assert_eq!(
            GenericTrap::from_integer(6),
            Some(GenericTrap::EnterpriseSpecific)
        );
        assert_eq!(GenericTrap::from_integer(7), None);
        assert_eq!(GenericTrap::from_integer(-1), None);
    }

    #[test]
    fn test_generic_trap_as_str() {
        assert_eq!(GenericTrap::ColdStart.as_str(), "coldStart");
        assert_eq!(GenericTrap::WarmStart.as_str(), "warmStart");
        assert_eq!(GenericTrap::LinkDown.as_str(), "linkDown");
        assert_eq!(GenericTrap::LinkUp.as_str(), "linkUp");
        assert_eq!(
            GenericTrap::AuthenticationFailure.as_str(),
            "authenticationFailure"
        );
        assert_eq!(GenericTrap::EgpNeighborLoss.as_str(), "egpNeighborLoss");
        assert_eq!(
            GenericTrap::EnterpriseSpecific.as_str(),
            "enterpriseSpecific"
        );
    }

    /// Test parsing a programmatically generated SNMPv1 cold start trap.
    #[test]
    fn test_parse_v1_cold_start_trap() {
        use rasn::types::ObjectIdentifier;

        // Build a valid SNMPv1 trap message using rasn types
        let enterprise_oid =
            ObjectIdentifier::new_unchecked(vec![1, 3, 6, 1, 4, 1, 9, 1, 1].into());
        let agent_addr = smi_v1::NetworkAddress::Internet(smi_v1::IpAddress(
            rasn::types::FixedOctetString::new([192, 168, 1, 1]),
        ));
        let trap = v1::Trap {
            enterprise: enterprise_oid,
            agent_addr,
            generic_trap: 0.into(), // coldStart
            specific_trap: 0.into(),
            time_stamp: smi_v1::TimeTicks(100),
            variable_bindings: v1::VarBindList::new(),
        };

        let msg = v1::Message {
            version: 0.into(),
            community: b"public".to_vec().into(),
            data: v1::Pdus::Trap(trap),
        };

        // Encode to BER
        let trap_bytes = rasn::ber::encode(&msg).expect("Failed to encode trap");

        let result = parse_trap(&trap_bytes, "10.0.0.1");
        assert!(
            result.is_ok(),
            "Failed to parse v1 trap: {:?}",
            result.err()
        );

        let parsed = result.unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.community, Some("public".to_string()));
        assert_eq!(parsed.generic_trap, Some(GenericTrap::ColdStart));
        assert_eq!(parsed.specific_trap, Some(0));
        assert!(parsed.enterprise_oid.is_some());
        assert_eq!(parsed.enterprise_oid.unwrap(), "1.3.6.1.4.1.9.1.1");
    }

    #[test]
    fn test_parse_v2c_trap() {
        use rasn::types::ObjectIdentifier;

        // Build a valid SNMPv2c trap message
        // sysUpTime.0 OID
        let sys_uptime_oid =
            ObjectIdentifier::new_unchecked(vec![1, 3, 6, 1, 2, 1, 1, 3, 0].into());
        // snmpTrapOID.0 OID
        let trap_oid_oid =
            ObjectIdentifier::new_unchecked(vec![1, 3, 6, 1, 6, 3, 1, 1, 4, 1, 0].into());
        // linkDown trap OID value
        let link_down_oid =
            ObjectIdentifier::new_unchecked(vec![1, 3, 6, 1, 6, 3, 1, 1, 5, 3].into());

        let varbinds = vec![
            v2::VarBind {
                name: sys_uptime_oid,
                value: v2::VarBindValue::Value(smi_v2::ObjectSyntax::ApplicationWide(
                    smi_v2::ApplicationSyntax::Ticks(smi_v1::TimeTicks(256)),
                )),
            },
            v2::VarBind {
                name: trap_oid_oid,
                value: v2::VarBindValue::Value(smi_v2::ObjectSyntax::Simple(
                    smi_v2::SimpleSyntax::ObjectId(link_down_oid),
                )),
            },
        ];

        let pdu = v2::Pdu {
            request_id: 1.into(),
            error_status: 0u32.into(),
            error_index: 0u32.into(),
            variable_bindings: varbinds.into(),
        };

        let msg = v2c::Message {
            version: 1.into(),
            community: b"public".to_vec().into(),
            data: v2::Pdus::Trap(v2::Trap(pdu)),
        };

        // Encode to BER
        let trap_bytes = rasn::ber::encode(&msg).expect("Failed to encode trap");

        let result = parse_trap(&trap_bytes, "10.0.0.2");
        assert!(
            result.is_ok(),
            "Failed to parse v2c trap: {:?}",
            result.err()
        );

        let parsed = result.unwrap();
        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.community, Some("public".to_string()));
        assert!(parsed.trap_oid.is_some());
        assert_eq!(parsed.trap_oid.unwrap(), "1.3.6.1.6.3.1.1.5.3");
        // v2 traps don't have enterprise/generic/specific fields
        assert!(parsed.enterprise_oid.is_none());
        assert!(parsed.generic_trap.is_none());
    }

    #[test]
    fn test_parse_invalid_data() {
        let garbage = &[0x01, 0x02, 0x03, 0x04];
        let result = parse_trap(garbage, "10.0.0.1");
        assert!(result.is_err());
    }

    #[test]
    fn test_varbind_creation() {
        let vb = VarBind {
            oid: "1.3.6.1.2.1.1.3.0".to_string(),
            value: TelemetryValue::Counter(12345),
        };
        assert_eq!(vb.oid, "1.3.6.1.2.1.1.3.0");
        match vb.value {
            TelemetryValue::Counter(v) => assert_eq!(v, 12345),
            _ => panic!("Expected Counter value"),
        }
    }
}
