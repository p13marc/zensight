//! Builds the netring `Monitor` from config and wires its callbacks to ZenSight
//! publishing via channels (handlers stay cheap; publishing happens off the
//! capture path).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use zensight_common::{Alert, ElephantRecord, FlowRecord, TelemetryPoint, TlsRecord};

/// Bounded ring of recent ended-flow records served via `@/query/flows`.
pub type FlowRing = Arc<Mutex<VecDeque<FlowRecord>>>;

/// Max recent flows retained for the on-demand `@/query/flows` channel.
const FLOW_RING_CAP: usize = 512;

/// Max recent elephant flows retained for `@/query/elephant_flows`.
const ELEPHANT_RING_CAP: usize = 128;

/// Cardinality guards for the on-demand inventories (talkers / DNS / HTTP).
const TALKER_CAP: usize = 8192;
const DNS_INV_CAP: usize = 8192;
const HTTP_INV_CAP: usize = 4096;

use flowscope::EndReason;
use flowscope::detect::patterns::{
    BeaconDetector, BeaconScore, DgaScore, DgaScorer, PortScanDetector, ScanScore, ScanVerdict,
};
use flowscope::extract::FiveTupleKey;
use netring::anomaly::shipped_sinks::ChannelSink;
use netring::monitor::fingerprint::TlsFingerprint;
use netring::prelude::*;
use netring::protocol::event_typed::{FlowEnded, FlowPacket};

use crate::config::{IocConfig, NetringConfig};
use crate::map::{self, AnomalyView, DnsRcodeClass};

/// Per-destination talker histogram: `dst -> (bytes, packets, flows)`.
pub type TalkerHist = Arc<Mutex<HashMap<String, (u64, u64, u64)>>>;
/// Bounded ring of recent elephant (large) flows.
pub type ElephantRing = Arc<Mutex<VecDeque<ElephantRecord>>>;
/// Passive TLS fingerprint inventory: (sni, ja4) → record with a hit count.
pub type TlsInventory = Arc<Mutex<HashMap<(String, String), TlsRecord>>>;
/// DNS SLD inventory: `sld -> (queries, nxdomain)` for `@/query/dns`.
pub type DnsInventory = Arc<Mutex<HashMap<String, (u64, u64)>>>;
/// HTTP host inventory: `host -> (requests, errors)` for `@/query/http`.
pub type HttpInventory = Arc<Mutex<HashMap<String, (u64, u64)>>>;
/// Per-flow in-flight HTTP request state: `flow -> (request_start_ms, host)`,
/// used to derive request→response latency and attribute response status.
type HttpPending = Arc<Mutex<HashMap<FiveTupleKey, (u64, Option<String>)>>>;

/// Max distinct TLS fingerprints retained (cardinality guard).
const TLS_INVENTORY_CAP: usize = 4096;

/// DNS RED accumulators shared across the capture path and the drain.
#[derive(Default)]
pub struct DnsState {
    pub queries: AtomicU64,
    pub unanswered: AtomicU64,
    pub noerror: AtomicU64,
    pub nxdomain: AtomicU64,
    pub servfail: AtomicU64,
    pub refused: AtomicU64,
    pub rcode_other: AtomicU64,
    /// Windowed query-RTT samples (ms), drained each tick for percentiles.
    pub rtt_ms: Mutex<Vec<u64>>,
    /// Per-SLD inventory for the on-demand top-domains channel.
    pub inventory: DnsInventory,
}

/// HTTP RED accumulators shared across the capture path and the drain.
#[derive(Default)]
pub struct HttpState {
    pub requests: AtomicU64,
    pub status_2xx: AtomicU64,
    pub status_3xx: AtomicU64,
    pub status_4xx: AtomicU64,
    pub status_5xx: AtomicU64,
    /// Per-method counts (small closed set), `method -> count`.
    pub methods: Mutex<HashMap<String, u64>>,
    /// Windowed request→response latency samples (ms).
    pub latency_ms: Mutex<Vec<u64>>,
    /// Per-host inventory for the on-demand top-hosts channel.
    pub inventory: HttpInventory,
}

/// Per-L4 + connection-state breakdown accumulators (issue #16).
#[derive(Default)]
pub struct L4State {
    pub tcp_bytes: AtomicU64,
    pub tcp_flows: AtomicU64,
    pub udp_bytes: AtomicU64,
    pub udp_flows: AtomicU64,
    pub icmp_bytes: AtomicU64,
    pub icmp_flows: AtomicU64,
    pub closed_fin: AtomicU64,
    pub closed_rst: AtomicU64,
    pub closed_idle: AtomicU64,
}

/// ICMP error accumulators (issue #15).
#[derive(Default)]
pub struct IcmpState {
    pub unreachable: AtomicU64,
    pub time_exceeded: AtomicU64,
    pub mtu_signal: AtomicU64,
    /// Per-kind slug counts (≈8 classes — bounded).
    pub by_kind: Mutex<HashMap<String, u64>>,
}

/// Channels the monitor emits on, drained by [`crate::publish`] tasks.
pub struct MonitorChannels {
    pub telemetry: mpsc::UnboundedReceiver<TelemetryPoint>,
    pub anomalies: mpsc::UnboundedReceiver<flowscope::OwnedAnomaly>,
    /// Typed sensor alerts produced directly on the capture path (ICMP
    /// flow-killed). Never lossy — kept on its own channel, off the telemetry bus.
    pub alerts: mpsc::UnboundedReceiver<Alert>,
    /// Shared flow counters (started, ended) for the periodic aggregate task.
    pub flow_started: Arc<AtomicU64>,
    pub flow_ended: Arc<AtomicU64>,
    /// Flow volume RED counters, accumulated from ended-flow stats: total bytes,
    /// packets and retransmits across all completed flows.
    pub flow_bytes: Arc<AtomicU64>,
    pub flow_packets: Arc<AtomicU64>,
    pub flow_retransmits: Arc<AtomicU64>,
    /// Per-flow durations (ms) of flows ended since the last aggregate tick.
    pub flow_durations_ms: Arc<Mutex<Vec<u64>>>,
    /// Bounded ring of recent ended-flow detail records for `@/query/flows`.
    pub flow_records: FlowRing,
    /// TCP RST counters: total resets and the subset that are connection refusals.
    pub tcp_resets: Arc<AtomicU64>,
    pub tcp_refused: Arc<AtomicU64>,
    /// Total TLS handshakes seen (ClientHello fingerprinted).
    pub tls_handshakes: Arc<AtomicU64>,
    /// Passive TLS asset inventory keyed by (sni, ja4): the served `@/query/tls`.
    pub tls_inventory: TlsInventory,
    /// Per-L4 + connection-state breakdown (issue #16).
    pub l4: Arc<L4State>,
    /// ICMP error accumulators (issue #15).
    pub icmp: Arc<IcmpState>,
    /// DNS RED accumulators (issue #19).
    pub dns: Arc<DnsState>,
    /// HTTP RED accumulators (issue #20).
    pub http: Arc<HttpState>,
    /// Per-destination talker histogram (issue #21).
    pub talkers: TalkerHist,
    /// Recent elephant (large) flows ring (issue #21).
    pub elephants: ElephantRing,
}

/// Detector wrapper bridging `feed`→`verdict` for the TRW port-scan detector.
struct PortScan {
    detector: PortScanDetector<FiveTupleKey>,
    last_score: Option<ScanScore<FiveTupleKey>>,
}

/// Detector wrapper for the RITA-style beaconing detector (issue #17).
struct Beacon {
    detector: BeaconDetector<FiveTupleKey>,
    threshold: f64,
    allowlist: Vec<String>,
    last_score: Option<BeaconScore<FiveTupleKey>>,
}

/// Detector wrapper for the DGA scorer over DNS query SLDs (issue #18).
struct Dga {
    scorer: DgaScorer,
    threshold: f64,
    allowlist: Vec<String>,
    last_score: Option<DgaScore>,
}

/// Connection-flood detector (issue #18): counts TCP flow-starts per (dst,port)
/// in a sliding window via a `TimeBucketedCounter`, flagging once a single
/// (dst,port) crosses the threshold. Distinct from a port scan (many ports).
struct Flood {
    counter: TimeBucketedCounter<String>,
    threshold: u64,
    last_hit: Option<(String, u64)>,
}

/// Map a flowscope severity onto a ZenSight alert severity.
pub fn map_severity(s: flowscope::event::Severity) -> zensight_common::AlertSeverity {
    use flowscope::event::Severity as S;
    use zensight_common::AlertSeverity;
    match s {
        S::Info => AlertSeverity::Info,
        S::Warning => AlertSeverity::Warning,
        _ => AlertSeverity::Critical,
    }
}

/// Decompose a `flowscope::OwnedAnomaly` into the pure [`AnomalyView`].
pub fn to_view(a: &flowscope::OwnedAnomaly) -> AnomalyView {
    let fmt = |ip: Option<std::net::IpAddr>, port: Option<u16>| {
        ip.map(|ip| match port {
            Some(p) => format!("{ip}:{p}"),
            None => ip.to_string(),
        })
    };
    AnomalyView {
        kind: a.kind.to_string(),
        severity: map_severity(a.severity),
        src: fmt(a.src_ip, a.src_port),
        dst: fmt(a.dest_ip, a.dest_port),
        proto: a.proto.map(|p| p.to_string()),
        observations: a
            .observations
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        metrics: a.metrics.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
    }
}

/// `true` if `host` matches any allowlist entry (case-insensitive substring) —
/// kills the dominant false positives for the noisy detectors (telemetry agents,
/// benign-but-random CDN SLDs).
fn allowlisted(host: &str, allowlist: &[String]) -> bool {
    let h = host.to_ascii_lowercase();
    allowlist
        .iter()
        .any(|a| !a.is_empty() && h.contains(&a.to_ascii_lowercase()))
}

/// Compile the IOC config (inline lists + indicator files) into an `IocSet`.
/// Each file line is one indicator; a value that parses as an IP becomes a host
/// indicator, otherwise it's treated as a domain. Blank lines and `#` comments
/// are skipped; unreadable files are warned and skipped (not fatal).
fn build_ioc_set(cfg: &IocConfig) -> IocSet {
    let mut set = IocSet::new();
    for ip in &cfg.ips {
        match ip.parse::<std::net::IpAddr>() {
            Ok(addr) => set = set.ip(addr),
            Err(_) => tracing::warn!(value = %ip, "ioc: ignoring unparseable IP"),
        }
    }
    set = set.domains(cfg.domains.iter());
    for fp in &cfg.ja4 {
        set = set.ja4(fp.clone());
    }
    for fp in &cfg.ja3 {
        set = set.ja3(fp.clone());
    }
    for path in &cfg.files {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                for line in content.lines() {
                    let l = line.trim();
                    if l.is_empty() || l.starts_with('#') {
                        continue;
                    }
                    match l.parse::<std::net::IpAddr>() {
                        Ok(addr) => set = set.ip(addr),
                        Err(_) => set = set.domain(l),
                    }
                }
            }
            Err(e) => tracing::warn!(path, error = %e, "ioc: failed to read indicator file"),
        }
    }
    set
}

/// Build a netring `Monitor` from config plus the channels it emits on.
pub fn build(
    cfg: &NetringConfig,
) -> Result<
    (
        netring::monitor::Monitor,
        MonitorChannels,
        mpsc::UnboundedSender<TelemetryPoint>,
    ),
    Box<dyn std::error::Error>,
> {
    let (tel_tx, tel_rx) = mpsc::unbounded_channel::<TelemetryPoint>();
    let (anom_tx, anom_rx) = mpsc::unbounded_channel::<flowscope::OwnedAnomaly>();
    let (alert_tx, alert_rx) = mpsc::unbounded_channel::<Alert>();
    let flow_started = Arc::new(AtomicU64::new(0));
    let flow_ended = Arc::new(AtomicU64::new(0));
    let flow_bytes = Arc::new(AtomicU64::new(0));
    let flow_packets = Arc::new(AtomicU64::new(0));
    let flow_retransmits = Arc::new(AtomicU64::new(0));
    let flow_durations_ms = Arc::new(Mutex::new(Vec::<u64>::new()));
    let flow_records: FlowRing = Arc::new(Mutex::new(VecDeque::with_capacity(FLOW_RING_CAP)));
    let tcp_resets = Arc::new(AtomicU64::new(0));
    let tcp_refused = Arc::new(AtomicU64::new(0));
    let tls_handshakes = Arc::new(AtomicU64::new(0));
    let tls_inventory: TlsInventory = Arc::new(Mutex::new(HashMap::new()));
    let l4 = Arc::new(L4State::default());
    let icmp = Arc::new(IcmpState::default());
    let dns = Arc::new(DnsState::default());
    let http = Arc::new(HttpState::default());
    let talkers: TalkerHist = Arc::new(Mutex::new(HashMap::new()));
    let elephants: ElephantRing = Arc::new(Mutex::new(VecDeque::with_capacity(ELEPHANT_RING_CAP)));

    let mut b = Monitor::builder();
    b = b.name(cfg.sensor_id.clone());

    // Source: pcap replay (privilege-free) or live interfaces.
    if let Some(pcap) = &cfg.pcap {
        b = b.pcap_source(pcap);
    } else {
        for iface in &cfg.interfaces {
            b = b.interface(iface);
        }
    }

    b = b.protocol::<Tcp>();

    // Flow lifecycle counters + per-L4/connection-state breakdown + top-talkers.
    if cfg.collect.flows {
        use netring::protocol::event_typed::FlowStarted;
        let started = flow_started.clone();
        b = b.on_ctx::<FlowStarted<Tcp>>(move |_e: &FlowStarted<Tcp>, _ctx: &mut Ctx<'_>| {
            started.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        let ended = flow_ended.clone();
        let bytes = flow_bytes.clone();
        let packets = flow_packets.clone();
        let retransmits = flow_retransmits.clone();
        let durations = flow_durations_ms.clone();
        let records = flow_records.clone();
        let l4_h = l4.clone();
        let talkers_h = talkers.clone();
        let elephants_h = elephants.clone();
        let collect_talkers = cfg.collect.talkers;
        b = b.on_ctx::<FlowEnded<Tcp>>(move |e: &FlowEnded<Tcp>, _ctx: &mut Ctx<'_>| {
            on_flow_ended(
                e,
                &ended,
                &bytes,
                &packets,
                &retransmits,
                &durations,
                &records,
                &l4_h,
                &talkers_h,
                &elephants_h,
                collect_talkers,
            );
            Ok(())
        });

        // UDP + ICMP flow ends feed only the per-L4 composition (and talkers).
        let l4_udp = l4.clone();
        let talkers_udp = talkers.clone();
        let collect_talkers_udp = cfg.collect.talkers;
        b = b.protocol::<Udp>();
        b = b.on_ctx::<FlowEnded<Udp>>(move |e: &FlowEnded<Udp>, _ctx: &mut Ctx<'_>| {
            l4_udp
                .udp_bytes
                .fetch_add(e.stats.total_bytes(), Ordering::Relaxed);
            l4_udp.udp_flows.fetch_add(1, Ordering::Relaxed);
            if collect_talkers_udp {
                record_talker(&talkers_udp, &e.key.b.to_string(), &e.stats);
            }
            Ok(())
        });
    }

    // TCP resets (connection refused vs mid-transfer abort).
    if cfg.collect.tcp_resets {
        use netring::protocol::event_typed::TcpRst;
        let resets = tcp_resets.clone();
        let refused = tcp_refused.clone();
        b = b.on_tcp_reset(move |rst: &TcpRst, _ctx: &mut Ctx<'_>| {
            resets.fetch_add(1, Ordering::Relaxed);
            if rst.zero_payload {
                refused.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        });
    }

    // ICMP error telemetry (issue #15) — live-gated; correlates a flow-killing
    // ICMP error and counts the error classes.
    if cfg.collect.icmp {
        use netring::prelude::{DestUnreachableKind, IcmpErrorKind};
        let icmp_h = icmp.clone();
        let alerts_h = alert_tx.clone();
        let sensor_id = cfg.sensor_id.clone();
        b = b.on_icmp_error(move |err: &IcmpError, _ctx: &mut Ctx<'_>| {
            // Classify into the headline counters + per-kind breakdown.
            let mut is_unreachable = false;
            let mut is_time_exceeded = false;
            let mut is_mtu = false;
            match &err.kind {
                IcmpErrorKind::DestUnreachable(_) => {
                    icmp_h.unreachable.fetch_add(1, Ordering::Relaxed);
                    is_unreachable = true;
                }
                IcmpErrorKind::TimeExceeded => {
                    icmp_h.time_exceeded.fetch_add(1, Ordering::Relaxed);
                    is_time_exceeded = true;
                }
                IcmpErrorKind::MtuSignal(_) => {
                    icmp_h.mtu_signal.fetch_add(1, Ordering::Relaxed);
                    is_mtu = true;
                }
                _ => {}
            }
            let kind = err.kind.as_str();
            if let Ok(mut m) = icmp_h.by_kind.lock() {
                *m.entry(kind.to_string()).or_insert(0) += 1;
            }
            // A flow-killing error (admin-prohibited / host/net unreachable / TTL)
            // with a correlated flow is a high-signal path failure → alert.
            let killer = matches!(
                err.kind,
                IcmpErrorKind::DestUnreachable(
                    DestUnreachableKind::Host
                        | DestUnreachableKind::Network
                        | DestUnreachableKind::AdministrativelyProhibited
                ) | IcmpErrorKind::TimeExceeded
            );
            if killer && err.correlated_flow.is_some() {
                let flow = err
                    .correlated_flow
                    .map(|k| (k.a.to_string(), k.b.to_string()));
                let view = map::IcmpErrorView {
                    kind: kind.to_string(),
                    is_unreachable,
                    is_time_exceeded,
                    is_mtu_signal: is_mtu,
                    correlated_flow: flow,
                };
                let _ = alerts_h.send(map::icmp_flow_alert(&sensor_id, &view));
            }
            Ok(())
        });
    }

    // Passive TLS fingerprinting (ClientHello → SNI + JA3/JA4 asset inventory).
    if cfg.collect.tls {
        let count = tls_handshakes.clone();
        let inventory = tls_inventory.clone();
        b = b.on_fingerprint(move |fp: &TlsFingerprint, _ctx: &mut Ctx<'_>| {
            count.fetch_add(1, Ordering::Relaxed);
            let key = (
                fp.sni.clone().unwrap_or_default(),
                fp.ja4.clone().unwrap_or_default(),
            );
            if let Ok(mut inv) = inventory.lock() {
                if let Some(rec) = inv.get_mut(&key) {
                    rec.count += 1;
                } else if inv.len() < TLS_INVENTORY_CAP {
                    inv.insert(
                        key,
                        TlsRecord {
                            sni: fp.sni.clone(),
                            alpn: fp.alpn.clone(),
                            ja3: fp.ja3.clone(),
                            ja4: fp.ja4.clone(),
                            count: 1,
                        },
                    );
                }
            }
            Ok(())
        });
    }

    // L7 DNS RED (issue #19) — Monitor-builder MessageProtocol; correlation
    // installed by `.protocol::<Dns>()` surfaces RTT (elapsed) + Unanswered.
    if cfg.collect.dns {
        use flowscope::dns::{DnsMessage, DnsRcode};
        b = b.protocol::<Dns>();
        let dns_h = dns.clone();
        b = b.on_ctx::<Dns>(move |msg: &DnsMessage, _ctx: &mut Ctx<'_>| {
            match msg {
                DnsMessage::Query(q) => {
                    dns_h.queries.fetch_add(1, Ordering::Relaxed);
                    if let Some(question) = q.questions.first()
                        && let Some(sld) = map::dns_sld(&question.name)
                        && let Ok(mut inv) = dns_h.inventory.lock()
                    {
                        if let Some(e) = inv.get_mut(&sld) {
                            e.0 += 1;
                        } else if inv.len() < DNS_INV_CAP {
                            inv.insert(sld, (1, 0));
                        }
                    }
                }
                DnsMessage::Response(r) => {
                    match r.rcode {
                        DnsRcode::NoError => dns_h.noerror.fetch_add(1, Ordering::Relaxed),
                        DnsRcode::NXDomain => dns_h.nxdomain.fetch_add(1, Ordering::Relaxed),
                        DnsRcode::ServFail => dns_h.servfail.fetch_add(1, Ordering::Relaxed),
                        DnsRcode::Refused => dns_h.refused.fetch_add(1, Ordering::Relaxed),
                        _ => dns_h.rcode_other.fetch_add(1, Ordering::Relaxed),
                    };
                    if let Some(rtt) = r.elapsed
                        && let Ok(mut v) = dns_h.rtt_ms.lock()
                        && v.len() < 100_000
                    {
                        v.push(rtt.as_millis() as u64);
                    }
                    // NXDOMAIN tally per SLD for the on-demand top-NXDOMAIN view.
                    if matches!(r.rcode, DnsRcode::NXDomain)
                        && let Some(question) = r.questions.first()
                        && let Some(sld) = map::dns_sld(&question.name)
                        && let Ok(mut inv) = dns_h.inventory.lock()
                        && let Some(e) = inv.get_mut(&sld)
                    {
                        e.1 += 1;
                    }
                }
                DnsMessage::Unanswered(_) => {
                    dns_h.unanswered.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
            Ok(())
        });
    }

    // L7 HTTP RED (issue #20) — cleartext only (TCP/80,8080).
    if cfg.collect.http {
        use flowscope::http::HttpMessage;
        b = b.protocol::<Http>();
        let http_h = http.clone();
        // Per-flow request-start timestamps (ms) keyed by flow, to derive
        // request→response latency, and the request's Host so the response can
        // attribute its status to the right host. Bounded; cleared on match.
        let pending: HttpPending = Arc::new(Mutex::new(HashMap::new()));
        b = b.on_ctx::<Http>(move |msg: &HttpMessage, ctx: &mut Ctx<'_>| {
            let now_ms = (ctx.ts.to_unix_f64() * 1000.0) as u64;
            let flow = ctx.flow;
            match msg {
                HttpMessage::Request(req) => {
                    http_h.requests.fetch_add(1, Ordering::Relaxed);
                    if let Some(method) = req.method_str() {
                        let m = method.to_ascii_lowercase();
                        if let Ok(mut mm) = http_h.methods.lock()
                            && (mm.contains_key(&m) || mm.len() < 32)
                        {
                            *mm.entry(m).or_insert(0) += 1;
                        }
                    }
                    let host = req.host().map(|h| h.to_string());
                    if let Some(host) = &host
                        && let Ok(mut inv) = http_h.inventory.lock()
                        && (inv.contains_key(host) || inv.len() < HTTP_INV_CAP)
                    {
                        inv.entry(host.clone()).or_insert((0, 0)).0 += 1;
                    }
                    if let (Some(k), Ok(mut p)) = (flow, pending.lock())
                        && p.len() < 65_536
                    {
                        p.insert(k, (now_ms, host));
                    }
                }
                HttpMessage::Response(resp) => {
                    match map::http_status_class(resp.status) {
                        "2xx" => http_h.status_2xx.fetch_add(1, Ordering::Relaxed),
                        "3xx" => http_h.status_3xx.fetch_add(1, Ordering::Relaxed),
                        "4xx" => http_h.status_4xx.fetch_add(1, Ordering::Relaxed),
                        "5xx" => http_h.status_5xx.fetch_add(1, Ordering::Relaxed),
                        _ => 0,
                    };
                    let is_err = matches!(map::http_status_class(resp.status), "4xx" | "5xx");
                    if let (Some(k), Ok(mut p)) = (flow, pending.lock())
                        && let Some((start, host)) = p.remove(&k)
                    {
                        let lat = now_ms.saturating_sub(start);
                        if let Ok(mut v) = http_h.latency_ms.lock()
                            && v.len() < 100_000
                        {
                            v.push(lat);
                        }
                        // Attribute a 4xx/5xx to the request's Host (top-hosts).
                        if is_err
                            && let Some(host) = host
                            && let Ok(mut inv) = http_h.inventory.lock()
                            && let Some(e) = inv.get_mut(&host)
                        {
                            e.1 += 1;
                        }
                    }
                }
            }
            Ok(())
        });
    }

    // Per-application bandwidth (periodic report).
    if cfg.collect.bandwidth {
        let tx = tel_tx.clone();
        let sensor_id = cfg.sensor_id.clone();
        b = b.on_bandwidth(
            Duration::from_secs(cfg.bandwidth_period_secs.max(1)),
            move |bw: &BandwidthReport<'_>| {
                for (app, bps) in bw.top(20) {
                    let _ = tx.send(crate::map::bandwidth_point(&sensor_id, app, bps));
                }
                Ok(())
            },
        );
    }

    // Capture self-health (live capture only).
    if cfg.collect.capture_stats {
        use netring::monitor::telemetry::CaptureTelemetry;
        let tx = tel_tx.clone();
        let sensor_id = cfg.sensor_id.clone();
        b = b.on_capture_stats(
            Duration::from_secs(cfg.bandwidth_period_secs.max(1)),
            move |t: &CaptureTelemetry, _ctx: &mut Ctx<'_>| {
                for p in crate::map::capture_points(
                    &sensor_id,
                    t.source.0,
                    t.packets,
                    t.drops,
                    t.freezes,
                    t.drop_rate,
                ) {
                    let _ = tx.send(p);
                }
                Ok(())
            },
        );
    }

    // Port-scan detector (Pillar A).
    if cfg.anomalies.port_scan {
        let scan = netring::pattern_detector! {
            name: "PortScanTRW",
            event: FlowEnded<Tcp>,
            detector: PortScan { detector: PortScanDetector::new(), last_score: None },
            feed: |evt, w| {
                let success = matches!(evt.reason, EndReason::Fin | EndReason::IdleTimeout);
                w.last_score = Some(w.detector.observe(evt.key, success));
            },
            verdict: |_evt, w| {
                w.last_score.as_ref().and_then(|s| {
                    if matches!(s.verdict, ScanVerdict::Scanner) {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
            },
        };
        b = b.detect(scan);
    }

    // RITA-style beaconing / C2 detector (issue #17).
    if cfg.anomalies.beaconing {
        let beacon = netring::pattern_detector! {
            name: "BeaconCv",
            event: FlowPacket,
            detector: Beacon {
                detector: BeaconDetector::new(),
                threshold: cfg.anomalies.beacon_threshold,
                allowlist: cfg.anomalies.allowlist.clone(),
                last_score: None,
            },
            feed: |evt, w| {
                if matches!(evt.proto, L4Proto::Tcp) {
                    w.last_score = w.detector.observe(evt.key, evt.ts, evt.len as u64);
                }
            },
            verdict: |_evt, w| {
                w.last_score.as_ref().and_then(|s| {
                    let dst = s.key.b.ip().to_string();
                    if s.score >= w.threshold && !allowlisted(&dst, &w.allowlist) {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
            },
        };
        b = b.detect(beacon);
    }

    // Connection-flood detector (issue #18).
    if cfg.anomalies.connection_flood {
        use netring::protocol::event_typed::FlowStarted;
        let flood = netring::pattern_detector! {
            name: "ConnectionFlood",
            event: FlowStarted<Tcp>,
            detector: Flood {
                counter: TimeBucketedCounter::new(Duration::from_secs(10), Duration::from_secs(1), 16_384),
                threshold: cfg.anomalies.flood_threshold,
                last_hit: None,
            },
            feed: |evt, w| {
                // Key by destination (ip:port) — many conns to one port = flood.
                let key = evt.key.b.to_string();
                w.counter.bump(key.clone(), evt.ts);
                let count = w.counter.count(&key, evt.ts);
                w.last_hit = Some((key, count));
            },
            verdict: |_evt, w| {
                w.last_hit.as_ref().and_then(|(dst, count)| {
                    if *count >= w.threshold {
                        Some(FloodScore { dst: dst.clone(), count: *count })
                    } else {
                        None
                    }
                })
            },
        };
        b = b.detect(flood);
    }

    // DGA / DNS-tunneling scorer (issue #18) — requires DNS collection.
    if cfg.anomalies.dga && cfg.collect.dns {
        use flowscope::dns::DnsMessage;
        let dga = netring::pattern_detector! {
            name: "DgaScorer",
            event: Dns,
            detector: Dga {
                scorer: DgaScorer::new(),
                threshold: cfg.anomalies.dga_threshold,
                allowlist: cfg.anomalies.allowlist.clone(),
                last_score: None,
            },
            feed: |msg, w| {
                w.last_score = None;
                if let DnsMessage::Query(q) = msg
                    && let Some(question) = q.questions.first()
                    && let Some(sld) = map::dns_sld(&question.name)
                    && !allowlisted(&sld, &w.allowlist)
                {
                    let sc = w.scorer.score(&sld);
                    w.last_score = Some(sc);
                }
            },
            verdict: |_msg, w| {
                w.last_score.as_ref().and_then(|s| {
                    let fire = (s.log_likelihood as f64) < w.threshold;
                    if fire { Some(*s) } else { None }
                })
            },
        };
        b = b.detect(dga);
    }

    // Threat-intel detection (netring 0.27). flow-risk / IOC / Sigma hits are
    // emitted as anomalies, so they ride the same ChannelSink → drain →
    // AlertReporter path as the built-in detectors — no extra plumbing.
    if cfg.threat.flow_risk {
        b = b.flow_risk();
        tracing::info!("netring: flow-risk scoring enabled");
    }
    let ioc = build_ioc_set(&cfg.threat.ioc);
    if !ioc.is_empty() {
        tracing::info!("netring: IOC matching enabled");
        b = b.ioc(ioc);
    }
    if cfg.threat.sigma.enabled {
        #[cfg(feature = "sigma")]
        if let Some(dir) = &cfg.threat.sigma.dir {
            match SigmaRuleSet::from_dir(dir) {
                Ok(rules) => {
                    b = b.sigma(rules);
                    tracing::info!(dir, "netring: Sigma rules loaded");
                }
                Err(e) => tracing::warn!(dir, error = %e, "netring: failed to load Sigma rules"),
            }
        }
        #[cfg(not(feature = "sigma"))]
        tracing::warn!("netring: threat.sigma is enabled but built without the `sigma` feature");
    }

    // Anomaly sink → channel → drain → AlertReporter.
    b = b.sink(ChannelSink::new(anom_tx));

    let monitor = b.build()?;
    let keepalive = tel_tx.clone();
    Ok((
        monitor,
        MonitorChannels {
            telemetry: tel_rx,
            anomalies: anom_rx,
            alerts: alert_rx,
            flow_started,
            flow_ended,
            flow_bytes,
            flow_packets,
            flow_retransmits,
            flow_durations_ms,
            flow_records,
            tcp_resets,
            tcp_refused,
            tls_handshakes,
            tls_inventory,
            l4,
            icmp,
            dns,
            http,
            talkers,
            elephants,
        },
        keepalive,
    ))
}

/// Local detector-score type for the connection-flood detector. Implements
/// flowscope's `DetectorScore` so `pattern_detector!`'s `verdict` can return it
/// and the macro publishes the resulting `OwnedAnomaly`.
struct FloodScore {
    dst: String,
    count: u64,
}

impl flowscope::DetectorScore for FloodScore {
    fn name(&self) -> &'static str {
        "ConnectionFlood"
    }
    fn into_anomaly(self, ts: flowscope::Timestamp) -> flowscope::OwnedAnomaly {
        flowscope::OwnedAnomaly::new("ConnectionFlood", flowscope::event::Severity::Warning, ts)
            .with_observation("dst", self.dst)
            .with_metric("connections", self.count as f64)
    }
}

/// Capture-path `FlowEnded<Tcp>` handler body — kept allocation-light: a handful
/// of atomic adds plus short-held `Mutex` pushes. No formatting beyond the
/// bounded detail rings, which are needed for the on-demand channels anyway.
#[allow(clippy::too_many_arguments)]
fn on_flow_ended(
    e: &FlowEnded<Tcp>,
    ended: &AtomicU64,
    bytes: &AtomicU64,
    packets: &AtomicU64,
    retransmits: &AtomicU64,
    durations: &Mutex<Vec<u64>>,
    records: &FlowRing,
    l4: &L4State,
    talkers: &TalkerHist,
    elephants: &ElephantRing,
    collect_talkers: bool,
) {
    let total_bytes = e.stats.total_bytes();
    let total_packets = e.stats.total_packets();
    ended.fetch_add(1, Ordering::Relaxed);
    bytes.fetch_add(total_bytes, Ordering::Relaxed);
    packets.fetch_add(total_packets, Ordering::Relaxed);
    retransmits.fetch_add(e.stats.total_retransmits(), Ordering::Relaxed);
    let duration_ms = e.stats.duration().as_millis() as u64;

    // Per-L4 (TCP) composition + connection-state bucketing.
    l4.tcp_bytes.fetch_add(total_bytes, Ordering::Relaxed);
    l4.tcp_flows.fetch_add(1, Ordering::Relaxed);
    match map::tcp_close_class(e.reason.as_str()) {
        "fin" => l4.closed_fin.fetch_add(1, Ordering::Relaxed),
        "rst" => l4.closed_rst.fetch_add(1, Ordering::Relaxed),
        _ => l4.closed_idle.fetch_add(1, Ordering::Relaxed),
    };

    if let Ok(mut d) = durations.lock()
        && d.len() < 1_000_000
    {
        d.push(duration_ms);
    }

    let proto = e.l4.map(|p| p.canonical_name()).unwrap_or("tcp");
    let reason = e.reason.as_str();
    let rec = crate::map::flow_record(
        e.key.a.to_string(),
        e.key.b.to_string(),
        proto,
        total_bytes,
        total_packets,
        duration_ms,
        reason,
    );
    if let Ok(mut r) = records.lock() {
        if r.len() == FLOW_RING_CAP {
            r.pop_front();
        }
        r.push_back(rec);
    }

    if collect_talkers {
        record_talker(talkers, &e.key.b.to_string(), &e.stats);
        // Elephant ring: keep the largest recent flows (push, trim by size).
        if let Ok(mut ring) = elephants.lock() {
            let er = crate::map::elephant_record(
                e.key.a.to_string(),
                e.key.b.to_string(),
                proto,
                total_bytes,
                total_packets,
                duration_ms,
            );
            ring.push_back(er);
            if ring.len() > ELEPHANT_RING_CAP {
                // Drop the smallest to keep the biggest (cheap: ring is small).
                if let Some((idx, _)) = ring.iter().enumerate().min_by_key(|(_, r)| r.bytes) {
                    ring.remove(idx);
                }
            }
        }
    }
}

/// Update the per-destination talker histogram (bounded by `TALKER_CAP`).
fn record_talker(talkers: &TalkerHist, dst: &str, stats: &flowscope::FlowStats) {
    if let Ok(mut t) = talkers.lock() {
        if let Some(e) = t.get_mut(dst) {
            e.0 += stats.total_bytes();
            e.1 += stats.total_packets();
            e.2 += 1;
        } else if t.len() < TALKER_CAP {
            t.insert(
                dst.to_string(),
                (stats.total_bytes(), stats.total_packets(), 1),
            );
        }
    }
}

/// Snapshot the DNS RED accumulators into the per-rcode tuple list + headline
/// counters for the drain (pure read; no clearing of the cumulative counters).
pub fn dns_snapshot(s: &DnsState) -> (u64, Vec<(DnsRcodeClass, u64)>, u64) {
    let queries = s.queries.load(Ordering::Relaxed);
    let unanswered = s.unanswered.load(Ordering::Relaxed);
    let by_rcode = vec![
        (DnsRcodeClass::NoError, s.noerror.load(Ordering::Relaxed)),
        (DnsRcodeClass::NxDomain, s.nxdomain.load(Ordering::Relaxed)),
        (DnsRcodeClass::ServFail, s.servfail.load(Ordering::Relaxed)),
        (DnsRcodeClass::Refused, s.refused.load(Ordering::Relaxed)),
        (DnsRcodeClass::Other, s.rcode_other.load(Ordering::Relaxed)),
    ];
    (queries, by_rcode, unanswered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IocConfig;
    use std::net::IpAddr;

    #[test]
    fn ioc_set_empty_by_default() {
        assert!(build_ioc_set(&IocConfig::default()).is_empty());
    }

    #[test]
    fn ioc_set_from_inline_lists() {
        let cfg = IocConfig {
            ips: vec!["10.0.0.2".into()],
            domains: vec!["evil.example".into()],
            ..Default::default()
        };
        let set = build_ioc_set(&cfg);
        assert!(!set.is_empty());
        assert!(set.matches_ip(&"10.0.0.2".parse::<IpAddr>().unwrap()));
        // subdomain-aware
        assert!(set.matches_domain("host.evil.example").is_some());
        assert!(set.matches_domain("good.example").is_none());
    }

    #[test]
    fn ioc_set_skips_unparseable_ip() {
        let cfg = IocConfig {
            ips: vec!["not-an-ip".into()],
            ..Default::default()
        };
        assert!(build_ioc_set(&cfg).is_empty());
    }

    #[test]
    fn ioc_set_loads_indicator_file() {
        let path = std::env::temp_dir().join(format!("zs-ioc-{}.txt", std::process::id()));
        std::fs::write(&path, "# bad stuff\n198.51.100.7\nmalware.test\n\n").unwrap();
        let cfg = IocConfig {
            files: vec![path.to_string_lossy().into_owned()],
            ..Default::default()
        };
        let set = build_ioc_set(&cfg);
        assert!(set.matches_ip(&"198.51.100.7".parse::<IpAddr>().unwrap()));
        assert!(set.matches_domain("sub.malware.test").is_some());
        let _ = std::fs::remove_file(&path);
    }
}
