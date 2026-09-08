//! NetFlow/IPFIX packet receiver and parser.

use crate::config::{ListenerConfig, NetFlowConfig};
use anyhow::{Context, Result};
use netflow_parser::static_versions::v5::FlowSet as V5FlowSet;
use netflow_parser::static_versions::v7::FlowSet as V7FlowSet;
use netflow_parser::variable_versions::field_value::FieldValue;
use netflow_parser::variable_versions::ipfix::{FlowSetBody as IpFixFlowSetBody, IPFixFieldPair};
use netflow_parser::variable_versions::v9::{FlowSetBody as V9FlowSetBody, V9FieldPair};
use netflow_parser::{NetflowPacket, NetflowParser};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

// The wire types live in `zensight-common` (#469): `@rpc/netflow/flows` is the
// bounded ring that keyspace-v2 put in place of per-flow-pair telemetry keys,
// and a reply type only the producer can name is a reply nobody can read — which
// is why that procedure had no consumer. Aliased locally so the parser below and
// its ~40 call sites are unchanged.
pub use zensight_common::{NetflowFieldValue as FlowFieldValue, NetflowRecord as FlowRecord};

/// Start all configured listeners and return a channel for receiving flow records.
pub async fn start_listeners(
    config: &NetFlowConfig,
    sampling: crate::fields::SharedSampling,
) -> Result<mpsc::Receiver<FlowRecord>> {
    let (tx, rx) = mpsc::channel(10000);
    let exporter_names = Arc::new(config.exporter_names.clone());

    for listener_config in &config.listeners {
        let tx = tx.clone();
        let names = exporter_names.clone();
        let config = listener_config.clone();
        let sampling = sampling.clone();

        tokio::spawn(async move {
            if let Err(e) = run_listener(&config, tx, names, sampling).await {
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
    sampling: crate::fields::SharedSampling,
) -> Result<()> {
    let socket = UdpSocket::bind(&config.bind)
        .await
        .with_context(|| format!("Failed to bind UDP socket to {}", config.bind))?;

    tracing::info!("NetFlow listener started on {}", config.bind);

    let mut buf = vec![0u8; config.max_packet_size];

    // Per-exporter parsers: NetFlow v9/IPFIX parsers keep template state per
    // exporter, so one parser per source address. BOUNDED. NetFlow is UDP
    // with no handshake, the source address is whatever the datagram says,
    // and this map used to grow by one parser — with its own template cache —
    // per distinct address ever seen, forever: a /16 sweep was 65k parsers,
    // a spoofing sender was unbounded. Past the cap the exporter seen least
    // recently is evicted; a real exporter re-sends its templates and is
    // back within its refresh interval, which is the protocol's own recovery.
    let mut parsers: HashMap<IpAddr, (NetflowParser, Instant)> = HashMap::new();
    let mut consecutive_errors: u32 = 0;

    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, addr)) => {
                consecutive_errors = 0;
                let data = &buf[..len];
                if !parsers.contains_key(&addr.ip())
                    && parsers.len() >= MAX_EXPORTERS
                    && let Some((&stale, _)) = parsers.iter().min_by_key(|(_, (_, seen))| *seen)
                {
                    tracing::warn!(
                        evicted = %stale, arriving = %addr.ip(), cap = MAX_EXPORTERS,
                        "NetFlow: exporter cap reached; evicting the least recently seen"
                    );
                    parsers.remove(&stale);
                }
                let (parser, seen) = parsers
                    .entry(addr.ip())
                    .or_insert_with(|| (NetflowParser::default(), Instant::now()));
                *seen = Instant::now();

                // Inline, on the receive loop. A task per datagram, each
                // holding a copy of the payload and blocking on a bounded
                // channel, piled up without limit under a burst; here the
                // socket's own buffer is the backpressure, and a datagram
                // that does not fit is dropped by the kernel — counted, and
                // the honest outcome for a receiver that is behind.
                if let Err(e) =
                    process_packet(data, addr, &tx, &exporter_names, parser, &sampling).await
                {
                    tracing::debug!("Failed to process NetFlow packet from {}: {}", addr, e);
                }
            }
            Err(e) => {
                // A socket in a persistent error state re-looped at full
                // speed: a log flood and a hot CPU. Back off, and give up on
                // this listener after a bound — the supervisor restarts the
                // process, which is the recovery that actually works.
                consecutive_errors += 1;
                tracing::error!(consecutive = consecutive_errors, "UDP receive error: {}", e);
                if consecutive_errors >= MAX_CONSECUTIVE_RECV_ERRORS {
                    anyhow::bail!(
                        "NetFlow listener on {}: {consecutive_errors} consecutive receive errors, giving up",
                        config.bind
                    );
                }
                tokio::time::sleep(std::time::Duration::from_millis(
                    (100u64 << consecutive_errors.min(6)).min(5_000),
                ))
                .await;
            }
        }
    }
}

/// Upper bound on distinct exporter addresses with live parser state.
const MAX_EXPORTERS: usize = 256;
/// Receive errors in a row before a listener gives up and lets the supervisor
/// restart the process.
const MAX_CONSECUTIVE_RECV_ERRORS: u32 = 50;

/// Process a single NetFlow/IPFIX packet.
async fn process_packet(
    data: &[u8],
    addr: SocketAddr,
    tx: &mpsc::Sender<FlowRecord>,
    exporter_names: &HashMap<String, String>,
    parser: &mut NetflowParser,
    sampling: &crate::fields::SharedSampling,
) -> Result<()> {
    let exporter_ip = addr.ip().to_string();
    let exporter_name = exporter_names
        .get(&exporter_ip)
        .cloned()
        .unwrap_or_else(|| exporter_ip.clone());

    let timestamp = zensight_common::current_timestamp_millis();

    // Parse the packet
    // 1.0: `parse_bytes` returns a `ParseResult` — the packets parsed before
    // any error, plus the error that stopped parsing (the pre-1.0
    // `NetflowPacket::Error` variant is gone).
    let result = parser.parse_bytes(data);
    if let Some(e) = &result.error {
        tracing::debug!("NetFlow parse error: {:?}", e);
    }

    for packet in result.packets {
        match packet {
            NetflowPacket::V5(v5) => {
                // v5 carries the sampling mode and interval in the HEADER's
                // low 14 bits, one field for the whole datagram (#1075). It was
                // never read: `process_packet` iterated `flowsets` only.
                if let Some(n) = sampling_from_v5_header(v5.header.sampling_interval)
                    && let Ok(mut s) = sampling.lock()
                {
                    s.observe(&exporter_name, n);
                }
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
                    match &flowset.body {
                        V9FlowSetBody::Data(data) => {
                            for flow_record in &data.fields {
                                let record = parse_v9_flow(
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
                        // Options data is where the sampling interval rides
                        // (#1075). Every non-Data body used to be dropped here
                        // silently, so a router exporting `1 in 1000` said so
                        // once per template refresh and was never heard.
                        V9FlowSetBody::OptionsData(opts) => {
                            for rec in &opts.fields {
                                if let Some(n) = sampling_from_v9(&rec.options_fields)
                                    && let Ok(mut s) = sampling.lock()
                                {
                                    s.observe(&exporter_name, n);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            NetflowPacket::IPFix(ipfix) => {
                for flowset in &ipfix.flowsets {
                    match &flowset.body {
                        IpFixFlowSetBody::Data(data) => {
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
                        IpFixFlowSetBody::OptionsData(opts) => {
                            for rec in &opts.fields {
                                if let Some(n) = sampling_from_ipfix(rec)
                                    && let Ok(mut s) = sampling.lock()
                                {
                                    s.observe(&exporter_name, n);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            // `NetflowPacket` is #[non_exhaustive] since 1.0.
            other => {
                tracing::debug!("unhandled NetFlow packet variant: {:?}", other);
            }
        }
    }

    Ok(())
}

/// The "1 in N" a v5 header declares, or `None` when it declares none.
fn sampling_from_v5_header(raw: u16) -> Option<u32> {
    crate::fields::v5_sampling(raw)
}

/// The "1 in N" a v9 options-data record declares.
///
/// Options data is scoped (per system, per interface, …) and the scope is not
/// read here: an exporter that samples differently per interface is a shape
/// this sensor's per-exporter rollup cannot express anyway, and taking the
/// most recent declaration is what pmacct does.
fn sampling_from_v9(
    options_fields: &[netflow_parser::variable_versions::v9::V9FieldPair],
) -> Option<u32> {
    for (field_type, value) in options_fields {
        if crate::fields::v9_semantic(field_type) == Some(crate::fields::SAMPLING_INTERVAL)
            && let Some(n) = field_value_as_u32(value)
            && n > 1
        {
            return Some(n);
        }
    }
    None
}

/// The "1 in N" an IPFIX options-data record declares.
fn sampling_from_ipfix(
    record: &[netflow_parser::variable_versions::ipfix::IPFixFieldPair],
) -> Option<u32> {
    for (field_type, value) in record {
        if crate::fields::ipfix_semantic(field_type) == Some(crate::fields::SAMPLING_INTERVAL)
            && let Some(n) = field_value_as_u32(value)
            && n > 1
        {
            return Some(n);
        }
    }
    None
}

/// A sampling interval is a plain unsigned count in every spelling; anything
/// else in that field is not one, and is better ignored than coerced.
fn field_value_as_u32(v: &FieldValue) -> Option<u32> {
    match parse_field_value(v) {
        FlowFieldValue::Uint(n) => u32::try_from(n).ok(),
        _ => None,
    }
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
        let value = parse_field_value(field_value);
        // The canonical name first, so a later duplicate of the same semantic
        // (a template carrying both In* and Out*) does not overwrite it with a
        // second direction — the first is the one the flow is about.
        if let Some(sem) = crate::fields::v9_semantic(field_type) {
            fields
                .entry(sem.to_string())
                .or_insert_with(|| value.clone());
        }
        // …and the raw name, which is what `@rpc/netflow/flows` serves as the
        // record's own detail.
        let field_name = format!("{:?}", field_type).to_lowercase();
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
        let value = parse_field_value(field_value);
        if let Some(sem) = crate::fields::ipfix_semantic(field_type) {
            fields
                .entry(sem.to_string())
                .or_insert_with(|| value.clone());
        }
        let field_name = format!("{:?}", field_type).to_lowercase();
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
        // 1.0: the wire bytes, formatted the way the crate itself does.
        FieldValue::MacAddr(mac) => FlowFieldValue::MacAddr(format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        )),
        // 1.0: `StringValue` carries the cleaned display string + raw bytes.
        FieldValue::String(s) => FlowFieldValue::String(s.value.clone()),
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
        FieldValue::Duration(dur) => FlowFieldValue::Uint(dur.as_duration().as_millis() as u64),
        FieldValue::ProtocolType(proto) => FlowFieldValue::Uint(u8::from(*proto) as u64),
        FieldValue::Float64(f) => FlowFieldValue::Float(*f),
        FieldValue::DataNumber(dn) => {
            // DataNumber can be various integer types
            use netflow_parser::variable_versions::field_value::DataNumber;
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
                // 1.0: an over-8-byte numeric field lands as raw bytes.
                DataNumber::Vec(bytes) => FlowFieldValue::Bytes(bytes.clone()),
            }
        }
        FieldValue::ApplicationId(app_id) => FlowFieldValue::String(format!(
            "{}:{:?}",
            app_id.classification_engine_id, app_id.selector_id
        )),
        // 1.0 replaced `Unknown(bytes)` with a family of typed variants
        // (TcpControlBits, FlowEndReason, NatEvent, …). None of them map to a
        // numeric FlowFieldValue; keep them as their debug rendering so the
        // information survives into labels instead of being dropped.
        other => FlowFieldValue::String(format!("{other:?}")),
    }
}

/// Convert protocol number to name.
pub(crate) fn protocol_number_to_name(proto: u8) -> String {
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

    /// FlowRecords ride the `flows` procedure as JSON — the enum must
    /// round-trip.
    #[test]
    fn test_flow_field_value_serde_roundtrip() {
        for v in [
            FlowFieldValue::Uint(100),
            FlowFieldValue::Int(-50),
            FlowFieldValue::Float(1.5),
            FlowFieldValue::IpAddr("192.168.1.1".to_string()),
            FlowFieldValue::MacAddr("aa:bb:cc:00:00:01".to_string()),
            FlowFieldValue::String("x".to_string()),
            FlowFieldValue::Bytes(vec![1, 2, 3]),
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: FlowFieldValue = serde_json::from_str(&json).unwrap();
            assert_eq!(back, v);
        }
    }

    #[test]
    fn test_protocol_number_to_name() {
        assert_eq!(protocol_number_to_name(6), "tcp");
        assert_eq!(protocol_number_to_name(17), "udp");
        assert_eq!(protocol_number_to_name(1), "icmp");
        assert_eq!(protocol_number_to_name(200), "proto_200");
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
        let result = parser.parse_bytes(&pkt);
        assert!(result.is_ok(), "parse error: {:?}", result.error);

        let uint = |f: &FlowFieldValue| match f {
            FlowFieldValue::Uint(v) => *v,
            other => panic!("expected Uint, got {other:?}"),
        };
        let ip = |f: &FlowFieldValue| match f {
            FlowFieldValue::IpAddr(s) => s.clone(),
            other => panic!("expected IpAddr, got {other:?}"),
        };

        let mut saw_flow = false;
        for packet in result.packets {
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

    /// v9 and IPFIX must reach `bytes`, `packets` and `protocol` — the three
    /// keys the rollup looks up — and before #1072 neither did.
    ///
    /// The parsers minted keys as `format!("{:?}", field_type).to_lowercase()`,
    /// which for v9 gives `inbytes`/`inpkts` and for IPFIX gives
    /// `iana(octetdeltacount)`, parentheses and all. So on the only two
    /// versions anyone deploys today `{exporter}/bytes_total` and
    /// `packets_total` stayed at **zero forever** while `flows_total` counted
    /// correctly — the shape that makes an exporter look healthy. IPFIX's
    /// protocol breakdown was entirely `unknown`.
    ///
    /// Templates first, then data: v9 and IPFIX are stateful, and the parser
    /// cannot decode a data record it has no template for. That statefulness is
    /// why hand-built packets are the only fixture available here — the crate
    /// ships no pcap corpus.
    #[test]
    fn v9_and_ipfix_reach_the_keys_the_rollup_reads() {
        let uint = |f: &FlowFieldValue| match f {
            FlowFieldValue::Uint(v) => *v,
            other => panic!("expected Uint, got {other:?}"),
        };

        // ── NetFlow v9 ────────────────────────────────────────────────────
        // Template 256: IN_BYTES(1,4), IN_PKTS(2,4), PROTOCOL(4,1).
        let mut tpl: Vec<u8> = Vec::new();
        tpl.extend_from_slice(&9u16.to_be_bytes()); // version
        tpl.extend_from_slice(&1u16.to_be_bytes()); // count (flowsets)
        tpl.extend_from_slice(&1000u32.to_be_bytes()); // sys_uptime
        tpl.extend_from_slice(&1_700_000_000u32.to_be_bytes()); // unix_secs
        tpl.extend_from_slice(&0u32.to_be_bytes()); // sequence
        tpl.extend_from_slice(&0u32.to_be_bytes()); // source_id
        tpl.extend_from_slice(&0u16.to_be_bytes()); // flowset_id 0 = template
        tpl.extend_from_slice(&(4u16 + 4 + 3 * 4).to_be_bytes()); // length
        tpl.extend_from_slice(&256u16.to_be_bytes()); // template_id
        tpl.extend_from_slice(&3u16.to_be_bytes()); // field_count
        for (id, len) in [(1u16, 4u16), (2, 4), (4, 1)] {
            tpl.extend_from_slice(&id.to_be_bytes());
            tpl.extend_from_slice(&len.to_be_bytes());
        }

        // Data for template 256: 1500 bytes, 10 packets, protocol 6.
        let mut dat: Vec<u8> = Vec::new();
        dat.extend_from_slice(&9u16.to_be_bytes());
        dat.extend_from_slice(&1u16.to_be_bytes());
        dat.extend_from_slice(&1000u32.to_be_bytes());
        dat.extend_from_slice(&1_700_000_000u32.to_be_bytes());
        dat.extend_from_slice(&1u32.to_be_bytes());
        dat.extend_from_slice(&0u32.to_be_bytes());
        dat.extend_from_slice(&256u16.to_be_bytes()); // flowset_id = template id
        dat.extend_from_slice(&(4u16 + 9 + 3).to_be_bytes()); // length (+3 pad)
        dat.extend_from_slice(&1500u32.to_be_bytes());
        dat.extend_from_slice(&10u32.to_be_bytes());
        dat.push(6);
        dat.extend_from_slice(&[0, 0, 0]); // pad to 4-byte boundary

        let mut parser = NetflowParser::default();
        let tpl_result = parser.parse_bytes(&tpl);
        assert!(tpl_result.is_ok(), "v9 template: {:?}", tpl_result.error);
        let result = parser.parse_bytes(&dat);
        assert!(result.is_ok(), "v9 data: {:?}", result.error);

        let mut saw = false;
        for packet in result.packets {
            if let NetflowPacket::V9(v9) = packet {
                for flowset in &v9.flowsets {
                    if let V9FlowSetBody::Data(data) = &flowset.body {
                        for fr in &data.fields {
                            let r = parse_v9_flow("1.2.3.4", "edge01", fr, 7);
                            assert_eq!(r.version, 9);
                            assert_eq!(uint(&r.fields[crate::fields::BYTES]), 1500);
                            assert_eq!(uint(&r.fields[crate::fields::PACKETS]), 10);
                            assert_eq!(uint(&r.fields[crate::fields::PROTOCOL]), 6);
                            // The raw name survives beside it: it is what
                            // `@rpc/netflow/flows` serves as the record's detail.
                            assert!(r.fields.contains_key("inbytes"));
                            saw = true;
                        }
                    }
                }
            }
        }
        assert!(saw, "parser yielded no v9 data record");

        // ── IPFIX ─────────────────────────────────────────────────────────
        // Template 256: octetDeltaCount(1,4), packetDeltaCount(2,4),
        // protocolIdentifier(4,1).
        let mut tpl: Vec<u8> = Vec::new();
        let tpl_set_len = 4u16 + 4 + 3 * 4;
        tpl.extend_from_slice(&10u16.to_be_bytes()); // version
        tpl.extend_from_slice(&(16u16 + tpl_set_len).to_be_bytes()); // total length
        tpl.extend_from_slice(&1_700_000_000u32.to_be_bytes()); // export time
        tpl.extend_from_slice(&0u32.to_be_bytes()); // sequence
        tpl.extend_from_slice(&0u32.to_be_bytes()); // domain id
        tpl.extend_from_slice(&2u16.to_be_bytes()); // set id 2 = template
        tpl.extend_from_slice(&tpl_set_len.to_be_bytes());
        tpl.extend_from_slice(&256u16.to_be_bytes());
        tpl.extend_from_slice(&3u16.to_be_bytes());
        for (id, len) in [(1u16, 4u16), (2, 4), (4, 1)] {
            tpl.extend_from_slice(&id.to_be_bytes());
            tpl.extend_from_slice(&len.to_be_bytes());
        }

        let mut dat: Vec<u8> = Vec::new();
        let dat_set_len = 4u16 + 9;
        dat.extend_from_slice(&10u16.to_be_bytes());
        dat.extend_from_slice(&(16u16 + dat_set_len).to_be_bytes());
        dat.extend_from_slice(&1_700_000_000u32.to_be_bytes());
        dat.extend_from_slice(&1u32.to_be_bytes());
        dat.extend_from_slice(&0u32.to_be_bytes());
        dat.extend_from_slice(&256u16.to_be_bytes()); // set id = template id
        dat.extend_from_slice(&dat_set_len.to_be_bytes());
        dat.extend_from_slice(&9000u32.to_be_bytes());
        dat.extend_from_slice(&12u32.to_be_bytes());
        dat.push(17); // UDP

        let mut parser = NetflowParser::default();
        let tpl_result = parser.parse_bytes(&tpl);
        assert!(tpl_result.is_ok(), "ipfix template: {:?}", tpl_result.error);
        let result = parser.parse_bytes(&dat);
        assert!(result.is_ok(), "ipfix data: {:?}", result.error);

        let mut saw = false;
        for packet in result.packets {
            if let NetflowPacket::IPFix(ipfix) = packet {
                for flowset in &ipfix.flowsets {
                    if let IpFixFlowSetBody::Data(data) = &flowset.body {
                        for fr in &data.fields {
                            let r = parse_ipfix_flow("1.2.3.4", "edge02", fr, 7);
                            assert_eq!(r.version, 10);
                            assert_eq!(uint(&r.fields[crate::fields::BYTES]), 9000);
                            assert_eq!(uint(&r.fields[crate::fields::PACKETS]), 12);
                            assert_eq!(uint(&r.fields[crate::fields::PROTOCOL]), 17);
                            saw = true;
                        }
                    }
                }
            }
        }
        assert!(saw, "parser yielded no IPFIX data record");
    }

    /// A v5 header that declares `1 in 1000` is heard, and one that declares no
    /// mode is not (#1075). The field was never read at all: `process_packet`
    /// iterated `flowsets` and the header's interval went nowhere.
    #[test]
    fn a_v5_header_sampling_declaration_is_read() {
        assert_eq!(sampling_from_v5_header((1 << 14) | 1000), Some(1000));
        assert_eq!(sampling_from_v5_header(1000), None);
    }

    /// Garbage / truncated input must not panic the parser path.
    #[test]
    fn test_parse_garbage_does_not_panic() {
        let mut parser = NetflowParser::default();
        let _ = parser.parse_bytes(&[0xff, 0x00, 0x01]);
        let _ = parser.parse_bytes(&[]);
    }
}
