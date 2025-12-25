//! NetFlow/IPFIX packet receiver and parser.

use crate::config::{ListenerConfig, NetFlowConfig};
use anyhow::{Context, Result};
use netflow_parser::static_versions::v5::FlowSet as V5FlowSet;
use netflow_parser::static_versions::v7::FlowSet as V7FlowSet;
use netflow_parser::variable_versions::data_number::FieldValue;
use netflow_parser::variable_versions::ipfix::{FlowSetBody as IpFixFlowSetBody, IPFixFieldPair};
use netflow_parser::variable_versions::v9::{FlowSetBody as V9FlowSetBody, V9FieldPair};
use netflow_parser::{NetflowPacket, NetflowParser};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

/// A parsed flow record ready for publishing.
#[derive(Debug, Clone)]
pub struct FlowRecord {
    /// Exporter IP address.
    pub exporter_ip: String,
    /// Resolved exporter name.
    pub exporter_name: String,
    /// NetFlow version (5, 7, 9, or 10 for IPFIX).
    pub version: u16,
    /// Flow fields as key-value pairs.
    pub fields: HashMap<String, FlowFieldValue>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A flow field value.
#[derive(Debug, Clone)]
pub enum FlowFieldValue {
    /// Unsigned integer value.
    Uint(u64),
    /// Signed integer value.
    Int(i64),
    /// Float value.
    Float(f64),
    /// IP address (v4 or v6).
    IpAddr(String),
    /// MAC address.
    MacAddr(String),
    /// String value.
    String(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
}

impl FlowFieldValue {
    /// Convert to TelemetryValue.
    pub fn to_telemetry_value(&self) -> TelemetryValue {
        match self {
            FlowFieldValue::Uint(v) => TelemetryValue::Counter(*v),
            FlowFieldValue::Int(v) => TelemetryValue::Gauge(*v as f64),
            FlowFieldValue::Float(v) => TelemetryValue::Gauge(*v),
            FlowFieldValue::IpAddr(s) => TelemetryValue::Text(s.clone()),
            FlowFieldValue::MacAddr(s) => TelemetryValue::Text(s.clone()),
            FlowFieldValue::String(s) => TelemetryValue::Text(s.clone()),
            FlowFieldValue::Bytes(b) => TelemetryValue::Binary(b.clone()),
        }
    }
}

/// Start all configured listeners and return a channel for receiving flow records.
pub async fn start_listeners(config: &NetFlowConfig) -> Result<mpsc::Receiver<FlowRecord>> {
    let (tx, rx) = mpsc::channel(10000);
    let exporter_names = Arc::new(config.exporter_names.clone());

    for listener_config in &config.listeners {
        let tx = tx.clone();
        let names = exporter_names.clone();
        let config = listener_config.clone();

        tokio::spawn(async move {
            if let Err(e) = run_listener(&config, tx, names).await {
                tracing::error!("NetFlow listener error: {}", e);
            }
        });
    }

    Ok(rx)
}

/// Run a single UDP listener.
async fn run_listener(
    config: &ListenerConfig,
    tx: mpsc::Sender<FlowRecord>,
    exporter_names: Arc<HashMap<String, String>>,
) -> Result<()> {
    let socket = UdpSocket::bind(&config.bind)
        .await
        .with_context(|| format!("Failed to bind UDP socket to {}", config.bind))?;

    tracing::info!("NetFlow listener started on {}", config.bind);

    let mut buf = vec![0u8; config.max_packet_size];

    // Parser maintains template state for NetFlow v9/IPFIX
    let parser = Arc::new(Mutex::new(NetflowParser::default()));

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, addr)) => {
                let data = buf[..len].to_vec();
                let tx = tx.clone();
                let names = exporter_names.clone();
                let parser = parser.clone();

                // Process in a separate task to not block the receiver
                tokio::spawn(async move {
                    if let Err(e) = process_packet(&data, addr, tx, names, parser).await {
                        tracing::debug!("Failed to process NetFlow packet from {}: {}", addr, e);
                    }
                });
            }
            Err(e) => {
                tracing::error!("UDP receive error: {}", e);
            }
        }
    }
}

/// Process a single NetFlow/IPFIX packet.
async fn process_packet(
    data: &[u8],
    addr: SocketAddr,
    tx: mpsc::Sender<FlowRecord>,
    exporter_names: Arc<HashMap<String, String>>,
    parser: Arc<Mutex<NetflowParser>>,
) -> Result<()> {
    let exporter_ip = addr.ip().to_string();
    let exporter_name = exporter_names
        .get(&exporter_ip)
        .cloned()
        .unwrap_or_else(|| exporter_ip.clone());

    let timestamp = zensight_common::current_timestamp_millis();

    // Parse the packet
    let mut parser_guard = parser.lock().await;
    let packets = parser_guard.parse_bytes(data);

    for packet in packets {
        match packet {
            NetflowPacket::V5(v5) => {
                for flow in &v5.flowsets {
                    let record = parse_v5_flow(&exporter_ip, &exporter_name, flow, timestamp);
                    if tx.send(record).await.is_err() {
                        return Ok(());
                    }
                }
            }
            NetflowPacket::V7(v7) => {
                for flow in &v7.flowsets {
                    let record = parse_v7_flow(&exporter_ip, &exporter_name, flow, timestamp);
                    if tx.send(record).await.is_err() {
                        return Ok(());
                    }
                }
            }
            NetflowPacket::V9(v9) => {
                for flowset in &v9.flowsets {
                    if let V9FlowSetBody::Data(data) = &flowset.body {
                        for flow_record in &data.fields {
                            let record =
                                parse_v9_flow(&exporter_ip, &exporter_name, flow_record, timestamp);
                            if tx.send(record).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            NetflowPacket::IPFix(ipfix) => {
                for flowset in &ipfix.flowsets {
                    if let IpFixFlowSetBody::Data(data) = &flowset.body {
                        for flow_record in &data.fields {
                            let record = parse_ipfix_flow(
                                &exporter_ip,
                                &exporter_name,
                                flow_record,
                                timestamp,
                            );
                            if tx.send(record).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
            }
            NetflowPacket::Error(e) => {
                tracing::debug!("NetFlow parse error: {:?}", e);
            }
        }
    }

    Ok(())
}

/// Parse a NetFlow v5 flow record.
fn parse_v5_flow(
    exporter_ip: &str,
    exporter_name: &str,
    flow: &V5FlowSet,
    timestamp: i64,
) -> FlowRecord {
    let mut fields = HashMap::new();

    fields.insert(
        "src_addr".to_string(),
        FlowFieldValue::IpAddr(flow.src_addr.to_string()),
    );
    fields.insert(
        "dst_addr".to_string(),
        FlowFieldValue::IpAddr(flow.dst_addr.to_string()),
    );
    fields.insert(
        "next_hop".to_string(),
        FlowFieldValue::IpAddr(flow.next_hop.to_string()),
    );
    fields.insert(
        "input_iface".to_string(),
        FlowFieldValue::Uint(flow.input.into()),
    );
    fields.insert(
        "output_iface".to_string(),
        FlowFieldValue::Uint(flow.output.into()),
    );
    fields.insert(
        "packets".to_string(),
        FlowFieldValue::Uint(flow.d_pkts.into()),
    );
    fields.insert(
        "bytes".to_string(),
        FlowFieldValue::Uint(flow.d_octets.into()),
    );
    fields.insert("first".to_string(), FlowFieldValue::Uint(flow.first.into()));
    fields.insert("last".to_string(), FlowFieldValue::Uint(flow.last.into()));
    fields.insert(
        "src_port".to_string(),
        FlowFieldValue::Uint(flow.src_port.into()),
    );
    fields.insert(
        "dst_port".to_string(),
        FlowFieldValue::Uint(flow.dst_port.into()),
    );
    fields.insert(
        "tcp_flags".to_string(),
        FlowFieldValue::Uint(flow.tcp_flags.into()),
    );
    fields.insert(
        "protocol".to_string(),
        FlowFieldValue::Uint(flow.protocol_number.into()),
    );
    fields.insert("tos".to_string(), FlowFieldValue::Uint(flow.tos.into()));
    fields.insert(
        "src_as".to_string(),
        FlowFieldValue::Uint(flow.src_as.into()),
    );
    fields.insert(
        "dst_as".to_string(),
        FlowFieldValue::Uint(flow.dst_as.into()),
    );
    fields.insert(
        "src_mask".to_string(),
        FlowFieldValue::Uint(flow.src_mask.into()),
    );
    fields.insert(
        "dst_mask".to_string(),
        FlowFieldValue::Uint(flow.dst_mask.into()),
    );

    FlowRecord {
        exporter_ip: exporter_ip.to_string(),
        exporter_name: exporter_name.to_string(),
        version: 5,
        fields,
        timestamp,
    }
}

/// Parse a NetFlow v7 flow record.
fn parse_v7_flow(
    exporter_ip: &str,
    exporter_name: &str,
    flow: &V7FlowSet,
    timestamp: i64,
) -> FlowRecord {
    let mut fields = HashMap::new();

    fields.insert(
        "src_addr".to_string(),
        FlowFieldValue::IpAddr(flow.src_addr.to_string()),
    );
    fields.insert(
        "dst_addr".to_string(),
        FlowFieldValue::IpAddr(flow.dst_addr.to_string()),
    );
    fields.insert(
        "next_hop".to_string(),
        FlowFieldValue::IpAddr(flow.next_hop.to_string()),
    );
    fields.insert(
        "input_iface".to_string(),
        FlowFieldValue::Uint(flow.input.into()),
    );
    fields.insert(
        "output_iface".to_string(),
        FlowFieldValue::Uint(flow.output.into()),
    );
    fields.insert(
        "packets".to_string(),
        FlowFieldValue::Uint(flow.d_pkts.into()),
    );
    fields.insert(
        "bytes".to_string(),
        FlowFieldValue::Uint(flow.d_octets.into()),
    );
    fields.insert("first".to_string(), FlowFieldValue::Uint(flow.first.into()));
    fields.insert("last".to_string(), FlowFieldValue::Uint(flow.last.into()));
    fields.insert(
        "src_port".to_string(),
        FlowFieldValue::Uint(flow.src_port.into()),
    );
    fields.insert(
        "dst_port".to_string(),
        FlowFieldValue::Uint(flow.dst_port.into()),
    );
    fields.insert(
        "tcp_flags".to_string(),
        FlowFieldValue::Uint(flow.tcp_flags.into()),
    );
    fields.insert(
        "protocol".to_string(),
        FlowFieldValue::Uint(flow.protocol_number.into()),
    );
    fields.insert("tos".to_string(), FlowFieldValue::Uint(flow.tos.into()));
    fields.insert(
        "src_as".to_string(),
        FlowFieldValue::Uint(flow.src_as.into()),
    );
    fields.insert(
        "dst_as".to_string(),
        FlowFieldValue::Uint(flow.dst_as.into()),
    );
    fields.insert(
        "src_mask".to_string(),
        FlowFieldValue::Uint(flow.src_mask.into()),
    );
    fields.insert(
        "dst_mask".to_string(),
        FlowFieldValue::Uint(flow.dst_mask.into()),
    );
    fields.insert(
        "router_src".to_string(),
        FlowFieldValue::IpAddr(flow.router_src.to_string()),
    );

    FlowRecord {
        exporter_ip: exporter_ip.to_string(),
        exporter_name: exporter_name.to_string(),
        version: 7,
        fields,
        timestamp,
    }
}

/// Parse a NetFlow v9 flow record.
fn parse_v9_flow(
    exporter_ip: &str,
    exporter_name: &str,
    data: &[V9FieldPair],
    timestamp: i64,
) -> FlowRecord {
    let mut fields = HashMap::new();

    for (field_type, field_value) in data {
        let field_name = format!("{:?}", field_type).to_lowercase();
        let value = parse_field_value(field_value);
        fields.insert(field_name, value);
    }

    FlowRecord {
        exporter_ip: exporter_ip.to_string(),
        exporter_name: exporter_name.to_string(),
        version: 9,
        fields,
        timestamp,
    }
}

/// Parse an IPFIX flow record.
fn parse_ipfix_flow(
    exporter_ip: &str,
    exporter_name: &str,
    data: &[IPFixFieldPair],
    timestamp: i64,
) -> FlowRecord {
    let mut fields = HashMap::new();

    for (field_type, field_value) in data {
        let field_name = format!("{:?}", field_type).to_lowercase();
        let value = parse_field_value(field_value);
        fields.insert(field_name, value);
    }

    FlowRecord {
        exporter_ip: exporter_ip.to_string(),
        exporter_name: exporter_name.to_string(),
        version: 10,
        fields,
        timestamp,
    }
}

/// Parse a FieldValue to FlowFieldValue.
fn parse_field_value(field_value: &FieldValue) -> FlowFieldValue {
    match field_value {
        FieldValue::Ip4Addr(addr) => FlowFieldValue::IpAddr(addr.to_string()),
        FieldValue::Ip6Addr(addr) => FlowFieldValue::IpAddr(addr.to_string()),
        FieldValue::MacAddr(mac) => FlowFieldValue::MacAddr(mac.clone()),
        FieldValue::String(s) => FlowFieldValue::String(s.clone()),
        FieldValue::Vec(bytes) => {
            if bytes.len() <= 8 {
                let mut value: u64 = 0;
                for b in bytes {
                    value = (value << 8) | (*b as u64);
                }
                FlowFieldValue::Uint(value)
            } else {
                FlowFieldValue::Bytes(bytes.clone())
            }
        }
        FieldValue::Duration(dur) => FlowFieldValue::Uint(dur.as_millis() as u64),
        FieldValue::ProtocolType(proto) => FlowFieldValue::Uint(*proto as u64),
        FieldValue::Float64(f) => FlowFieldValue::Float(*f),
        FieldValue::DataNumber(dn) => {
            // DataNumber can be various integer types
            use netflow_parser::variable_versions::data_number::DataNumber;
            match dn {
                DataNumber::U8(v) => FlowFieldValue::Uint(*v as u64),
                DataNumber::I8(v) => FlowFieldValue::Int(*v as i64),
                DataNumber::U16(v) => FlowFieldValue::Uint(*v as u64),
                DataNumber::I16(v) => FlowFieldValue::Int(*v as i64),
                DataNumber::U24(v) => FlowFieldValue::Uint(*v as u64),
                DataNumber::I24(v) => FlowFieldValue::Int(*v as i64),
                DataNumber::U32(v) => FlowFieldValue::Uint(*v as u64),
                DataNumber::I32(v) => FlowFieldValue::Int(*v as i64),
                DataNumber::U64(v) => FlowFieldValue::Uint(*v),
                DataNumber::I64(v) => FlowFieldValue::Int(*v),
                DataNumber::U128(v) => FlowFieldValue::Uint(*v as u64),
                DataNumber::I128(v) => FlowFieldValue::Int(*v as i64),
            }
        }
        FieldValue::ApplicationId(app_id) => FlowFieldValue::String(format!(
            "{}:{:?}",
            app_id.classification_engine_id, app_id.selector_id
        )),
        FieldValue::Unknown(bytes) => FlowFieldValue::Bytes(bytes.clone()),
    }
}

/// Convert a FlowRecord to a TelemetryPoint.
pub fn to_telemetry_point(record: &FlowRecord) -> TelemetryPoint {
    let mut labels = HashMap::new();

    labels.insert("version".to_string(), format!("v{}", record.version));
    labels.insert("exporter_ip".to_string(), record.exporter_ip.clone());

    // Add common flow fields as labels
    for (key, value) in &record.fields {
        match value {
            FlowFieldValue::IpAddr(s) | FlowFieldValue::MacAddr(s) | FlowFieldValue::String(s) => {
                labels.insert(key.clone(), s.clone());
            }
            FlowFieldValue::Uint(v) => {
                labels.insert(key.clone(), v.to_string());
            }
            FlowFieldValue::Int(v) => {
                labels.insert(key.clone(), v.to_string());
            }
            FlowFieldValue::Float(v) => {
                labels.insert(key.clone(), v.to_string());
            }
            FlowFieldValue::Bytes(_) => {
                // Skip binary data in labels
            }
        }
    }

    // Use bytes as the primary metric value if available
    let value = record
        .fields
        .get("bytes")
        .map(|v| v.to_telemetry_value())
        .or_else(|| record.fields.get("packets").map(|v| v.to_telemetry_value()))
        .unwrap_or(TelemetryValue::Counter(1));

    // Build metric name from flow key fields
    let metric = build_flow_metric(record);

    TelemetryPoint {
        timestamp: record.timestamp,
        source: record.exporter_name.clone(),
        protocol: Protocol::Netflow,
        metric,
        value,
        labels,
    }
}

/// Build a metric name from flow fields.
fn build_flow_metric(record: &FlowRecord) -> String {
    let src = record
        .fields
        .get("src_addr")
        .map(|v| match v {
            FlowFieldValue::IpAddr(s) => s.clone(),
            _ => "unknown".to_string(),
        })
        .unwrap_or_else(|| "unknown".to_string());

    let dst = record
        .fields
        .get("dst_addr")
        .map(|v| match v {
            FlowFieldValue::IpAddr(s) => s.clone(),
            _ => "unknown".to_string(),
        })
        .unwrap_or_else(|| "unknown".to_string());

    let proto = record
        .fields
        .get("protocol")
        .map(|v| match v {
            FlowFieldValue::Uint(p) => protocol_number_to_name(*p as u8),
            _ => "unknown".to_string(),
        })
        .unwrap_or_else(|| "unknown".to_string());

    format!("{}/{}/{}", src, dst, proto)
}

/// Build the key expression for a flow record.
pub fn build_key_expr(prefix: &str, record: &FlowRecord) -> String {
    let src = record
        .fields
        .get("src_addr")
        .map(|v| match v {
            FlowFieldValue::IpAddr(s) => s.replace(['.', ':'], "_"),
            _ => "unknown".to_string(),
        })
        .unwrap_or_else(|| "unknown".to_string());

    let dst = record
        .fields
        .get("dst_addr")
        .map(|v| match v {
            FlowFieldValue::IpAddr(s) => s.replace(['.', ':'], "_"),
            _ => "unknown".to_string(),
        })
        .unwrap_or_else(|| "unknown".to_string());

    format!(
        "{}/{}/{}/{}",
        prefix,
        record.exporter_name.replace('.', "_"),
        src,
        dst
    )
}

/// Convert protocol number to name.
fn protocol_number_to_name(proto: u8) -> String {
    match proto {
        1 => "icmp".to_string(),
        6 => "tcp".to_string(),
        17 => "udp".to_string(),
        47 => "gre".to_string(),
        50 => "esp".to_string(),
        51 => "ah".to_string(),
        58 => "icmpv6".to_string(),
        89 => "ospf".to_string(),
        132 => "sctp".to_string(),
        _ => format!("proto_{}", proto),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flow_field_value_to_telemetry() {
        assert!(matches!(
            FlowFieldValue::Uint(100).to_telemetry_value(),
            TelemetryValue::Counter(100)
        ));
        assert!(matches!(
            FlowFieldValue::Int(-50).to_telemetry_value(),
            TelemetryValue::Gauge(_)
        ));
        assert!(matches!(
            FlowFieldValue::IpAddr("192.168.1.1".to_string()).to_telemetry_value(),
            TelemetryValue::Text(_)
        ));
    }

    #[test]
    fn test_protocol_number_to_name() {
        assert_eq!(protocol_number_to_name(6), "tcp");
        assert_eq!(protocol_number_to_name(17), "udp");
        assert_eq!(protocol_number_to_name(1), "icmp");
        assert_eq!(protocol_number_to_name(200), "proto_200");
    }

    #[test]
    fn test_build_key_expr() {
        let mut fields = HashMap::new();
        fields.insert(
            "src_addr".to_string(),
            FlowFieldValue::IpAddr("192.168.1.1".to_string()),
        );
        fields.insert(
            "dst_addr".to_string(),
            FlowFieldValue::IpAddr("10.0.0.1".to_string()),
        );

        let record = FlowRecord {
            exporter_ip: "172.16.0.1".to_string(),
            exporter_name: "router01".to_string(),
            version: 5,
            fields,
            timestamp: 0,
        };

        let key = build_key_expr("zensight/netflow", &record);
        assert_eq!(key, "zensight/netflow/router01/192_168_1_1/10_0_0_1");
    }

    #[test]
    fn test_build_flow_metric() {
        let mut fields = HashMap::new();
        fields.insert(
            "src_addr".to_string(),
            FlowFieldValue::IpAddr("192.168.1.1".to_string()),
        );
        fields.insert(
            "dst_addr".to_string(),
            FlowFieldValue::IpAddr("10.0.0.1".to_string()),
        );
        fields.insert("protocol".to_string(), FlowFieldValue::Uint(6));

        let record = FlowRecord {
            exporter_ip: "172.16.0.1".to_string(),
            exporter_name: "router01".to_string(),
            version: 5,
            fields,
            timestamp: 0,
        };

        let metric = build_flow_metric(&record);
        assert_eq!(metric, "192.168.1.1/10.0.0.1/tcp");
    }
}
