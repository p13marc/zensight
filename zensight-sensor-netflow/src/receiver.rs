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
use std::net::{IpAddr, SocketAddr};
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

    // Per-exporter parsers to avoid mutex contention between different exporters.
    // NetFlow v9/IPFIX parsers maintain template state per exporter.
    let parsers: Arc<Mutex<HashMap<IpAddr, Arc<Mutex<NetflowParser>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, addr)) => {
                let data = buf[..len].to_vec();
                let tx = tx.clone();
                let names = exporter_names.clone();

                // Get or create a parser for this exporter
                let parser = {
                    let mut map = parsers.lock().await;
                    map.entry(addr.ip())
                        .or_insert_with(|| Arc::new(Mutex::new(NetflowParser::default())))
                        .clone()
                };

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

    #[test]
    fn test_flow_field_value_all_variants() {
        assert!(matches!(
            FlowFieldValue::Float(1.5).to_telemetry_value(),
            TelemetryValue::Gauge(_)
        ));
        assert!(matches!(
            FlowFieldValue::MacAddr("aa:bb".into()).to_telemetry_value(),
            TelemetryValue::Text(_)
        ));
        assert!(matches!(
            FlowFieldValue::String("x".into()).to_telemetry_value(),
            TelemetryValue::Text(_)
        ));
        assert!(matches!(
            FlowFieldValue::Bytes(vec![1, 2]).to_telemetry_value(),
            TelemetryValue::Binary(_)
        ));
    }

    /// A FlowRecord uses `bytes` as the primary metric value, falls back to
    /// `packets`, and projects fields onto labels + protocol = Netflow.
    #[test]
    fn test_to_telemetry_point() {
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
        fields.insert("packets".to_string(), FlowFieldValue::Uint(10));
        fields.insert("bytes".to_string(), FlowFieldValue::Uint(1500));

        let record = FlowRecord {
            exporter_ip: "172.16.0.1".to_string(),
            exporter_name: "router01".to_string(),
            version: 5,
            fields,
            timestamp: 42,
        };

        let tp = to_telemetry_point(&record);
        assert_eq!(tp.protocol, Protocol::Netflow);
        assert_eq!(tp.source, "router01");
        assert_eq!(tp.metric, "192.168.1.1/10.0.0.1/tcp");
        assert_eq!(tp.timestamp, 42);
        // `bytes` is preferred over `packets` as the series value.
        assert!(matches!(tp.value, TelemetryValue::Counter(1500)));
        assert_eq!(tp.labels.get("version").map(String::as_str), Some("v5"));
        assert_eq!(
            tp.labels.get("exporter_ip").map(String::as_str),
            Some("172.16.0.1")
        );
        assert_eq!(tp.labels.get("bytes").map(String::as_str), Some("1500"));
    }

    /// `packets` is used as the value when no `bytes` field is present.
    #[test]
    fn test_to_telemetry_point_packets_fallback() {
        let mut fields = HashMap::new();
        fields.insert("packets".to_string(), FlowFieldValue::Uint(7));
        let record = FlowRecord {
            exporter_ip: "e".to_string(),
            exporter_name: "e".to_string(),
            version: 9,
            fields,
            timestamp: 0,
        };
        assert!(matches!(
            to_telemetry_point(&record).value,
            TelemetryValue::Counter(7)
        ));
    }

    /// Build a minimal NetFlow v5 packet (24-byte header + one 48-byte record)
    /// and run it through the real parser + `parse_v5_flow`, asserting the wire
    /// fields decode and map onto the FlowRecord correctly.
    #[test]
    fn test_parse_v5_packet_roundtrip() {
        let mut pkt: Vec<u8> = Vec::new();
        // ── Header (24 bytes) ──
        pkt.extend_from_slice(&5u16.to_be_bytes()); // version
        pkt.extend_from_slice(&1u16.to_be_bytes()); // count
        pkt.extend_from_slice(&1000u32.to_be_bytes()); // sys_uptime
        pkt.extend_from_slice(&1_700_000_000u32.to_be_bytes()); // unix_secs
        pkt.extend_from_slice(&0u32.to_be_bytes()); // unix_nsecs
        pkt.extend_from_slice(&0u32.to_be_bytes()); // flow_sequence
        pkt.push(0); // engine_type
        pkt.push(0); // engine_id
        pkt.extend_from_slice(&0u16.to_be_bytes()); // sampling_interval
        // ── Record (48 bytes) ──
        pkt.extend_from_slice(&0xC0A8_0101u32.to_be_bytes()); // src 192.168.1.1
        pkt.extend_from_slice(&0x0A00_0001u32.to_be_bytes()); // dst 10.0.0.1
        pkt.extend_from_slice(&0u32.to_be_bytes()); // next_hop
        pkt.extend_from_slice(&1u16.to_be_bytes()); // input
        pkt.extend_from_slice(&2u16.to_be_bytes()); // output
        pkt.extend_from_slice(&10u32.to_be_bytes()); // d_pkts
        pkt.extend_from_slice(&1500u32.to_be_bytes()); // d_octets
        pkt.extend_from_slice(&100u32.to_be_bytes()); // first
        pkt.extend_from_slice(&200u32.to_be_bytes()); // last
        pkt.extend_from_slice(&12345u16.to_be_bytes()); // src_port
        pkt.extend_from_slice(&80u16.to_be_bytes()); // dst_port
        pkt.push(0); // pad1
        pkt.push(0x10); // tcp_flags
        pkt.push(6); // protocol (TCP)
        pkt.push(0); // tos
        pkt.extend_from_slice(&0u16.to_be_bytes()); // src_as
        pkt.extend_from_slice(&0u16.to_be_bytes()); // dst_as
        pkt.push(24); // src_mask
        pkt.push(16); // dst_mask
        pkt.extend_from_slice(&0u16.to_be_bytes()); // pad2
        assert_eq!(pkt.len(), 72);

        let mut parser = NetflowParser::default();
        let packets = parser.parse_bytes(&pkt);

        let uint = |f: &FlowFieldValue| match f {
            FlowFieldValue::Uint(v) => *v,
            other => panic!("expected Uint, got {other:?}"),
        };
        let ip = |f: &FlowFieldValue| match f {
            FlowFieldValue::IpAddr(s) => s.clone(),
            other => panic!("expected IpAddr, got {other:?}"),
        };

        let mut saw_flow = false;
        for packet in packets {
            if let NetflowPacket::V5(v5) = packet {
                for flow in &v5.flowsets {
                    let r = parse_v5_flow("1.2.3.4", "exp", flow, 7);
                    assert_eq!(r.version, 5);
                    assert_eq!(r.timestamp, 7);
                    assert_eq!(ip(&r.fields["src_addr"]), "192.168.1.1");
                    assert_eq!(ip(&r.fields["dst_addr"]), "10.0.0.1");
                    assert_eq!(uint(&r.fields["packets"]), 10);
                    assert_eq!(uint(&r.fields["bytes"]), 1500);
                    assert_eq!(uint(&r.fields["src_port"]), 12345);
                    assert_eq!(uint(&r.fields["dst_port"]), 80);
                    assert_eq!(uint(&r.fields["protocol"]), 6);
                    assert_eq!(uint(&r.fields["tcp_flags"]), 16);
                    saw_flow = true;
                }
            }
        }
        assert!(saw_flow, "parser did not yield a V5 flow record");
    }

    /// Garbage / truncated input must not panic the parser path.
    #[test]
    fn test_parse_garbage_does_not_panic() {
        let mut parser = NetflowParser::default();
        let _ = parser.parse_bytes(&[0xff, 0x00, 0x01]);
        let _ = parser.parse_bytes(&[]);
    }
}
