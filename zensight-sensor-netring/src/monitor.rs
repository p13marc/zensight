//! Builds the netring `Monitor` from config and wires its callbacks to ZenSight
//! publishing via channels (handlers stay cheap; publishing happens off the
//! capture path).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::sync::mpsc;
use zensight_common::{
    Alert, AssetRecord, ElephantRecord, FlowRecord, Ja4hRecord, QuicRecord, SshRecord,
    TelemetryPoint, TlsRecord,
};

use crate::command::DetectorHandle;
use crate::config::AnomalyConfig;

/// Lock-free live view of the tunable [`AnomalyConfig`] (#121). Detectors hold a
/// clone and `load()` the current config per scored candidate / DNS query, so a
/// runtime tuning command (allowlist / threshold / mute) takes effect with no
/// restart. The per-packet `feed` paths are left untouched.
type LiveConfig = Arc<ArcSwap<AnomalyConfig>>;

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

/// LRU cap of the Newly-Observed-Domain seen-set (issue #118) — bounds the
/// detector's memory to this many distinct second-level domains.
const NOD_SEEN_CAP: usize = 131_072;

/// Sliding-window parameters for the DNS-tunnel distinct-label set (issue #118):
/// 60 s window, 5 s buckets, capped at this many tracked (src, SLD) keys.
const DNS_TUNNEL_KEY_CAP: usize = 8192;

/// EWMA smoothing factor for the per-source data-exfil baseline (#123): ~10-flow
/// effective window, so a host's normal volume adapts but a single burst stands out.
const EXFIL_EWMA_ALPHA: f64 = 0.2;

use flowscope::EndReason;
use flowscope::detect::patterns::{
    BeaconDetector, BeaconScore, DgaScore, DgaScorer, PortScanDetector, RitaBeaconDetector,
    RitaBeaconScore, ScanScore, ScanVerdict,
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
/// Traffic-matrix histogram (#122): `(src, dst) -> (bytes, packets, flows)` — the
/// service-map data, "who talks to whom".
pub type MatrixHist = Arc<Mutex<HashMap<(String, String), (u64, u64, u64)>>>;
/// Bounded ring of recent elephant (large) flows.
pub type ElephantRing = Arc<Mutex<VecDeque<ElephantRecord>>>;
/// Passive TLS fingerprint inventory: (sni, ja4) → record with a hit count.
pub type TlsInventory = Arc<Mutex<HashMap<(String, String), TlsRecord>>>;
/// DNS SLD inventory: `sld -> (queries, nxdomain)` for `@/query/dns`.
pub type DnsInventory = Arc<Mutex<HashMap<String, (u64, u64)>>>;
/// HTTP host inventory: `host -> (requests, errors)` for `@/query/http`.
pub type HttpInventory = Arc<Mutex<HashMap<String, (u64, u64)>>>;
/// Passive asset inventory: `mac -> AssetRecord` for `@/query/assets` (issue #70).
pub type AssetInventory = Arc<Mutex<HashMap<String, AssetRecord>>>;
/// Per-flow in-flight HTTP request state: `flow -> (request_start_ms, host)`,
/// used to derive request→response latency and attribute response status.
type HttpPending = Arc<Mutex<HashMap<FiveTupleKey, (u64, Option<String>)>>>;
/// Passive QUIC SNI/ALPN inventory: (sni, version) → record for `@/query/quic` (#72).
pub type QuicInventory = Arc<Mutex<HashMap<(String, String), QuicRecord>>>;
/// Passive SSH/HASSH inventory: hassh → record for `@/query/ssh` (#72).
pub type SshInventory = Arc<Mutex<HashMap<String, SshRecord>>>;
/// Passive JA4H HTTP-fingerprint inventory: ja4h → record for `@/query/ja4h`
/// (#124, only populated with `--features ja4plus`).
pub type Ja4hInventory = Arc<Mutex<HashMap<String, Ja4hRecord>>>;
/// Per-flow SSH banner seen before the KEXINIT, to best-effort correlate a
/// HASSH fingerprint with its version banner: `flow -> banner`.
type SshPending = Arc<Mutex<HashMap<FiveTupleKey, String>>>;

/// Max distinct TLS fingerprints retained (cardinality guard).
const TLS_INVENTORY_CAP: usize = 4096;

/// Cardinality guards for the QUIC (sni,version) and SSH (hassh) inventories.
const QUIC_INVENTORY_CAP: usize = 4096;
const SSH_INVENTORY_CAP: usize = 4096;
/// Cardinality guard for the JA4H HTTP-fingerprint inventory (#124). Only the
/// `ja4plus`-gated capture path consults it; unused in the default build.
#[cfg(feature = "ja4plus")]
const JA4H_INVENTORY_CAP: usize = 4096;
/// LRU capacity of the passive asset inventory (MAC-keyed) — matches the bound
/// on the served `@/query/assets` map (issue #70).
const ASSET_INVENTORY_CAP: usize = 8192;

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
    /// `(src,dst)` traffic-matrix histogram, served on `@/query/matrix` (#122).
    pub matrix: MatrixHist,
    /// Recent elephant (large) flows ring (issue #21).
    pub elephants: ElephantRing,
    /// Passive QUIC SNI/ALPN inventory: served on `@/query/quic` (issue #72).
    pub quic: QuicInventory,
    /// Passive SSH/HASSH inventory: served on `@/query/ssh` (issue #72).
    pub ssh: SshInventory,
    /// Passive JA4H HTTP-fingerprint inventory: served on `@/query/ja4h` (#124).
    /// Stays empty unless built with `--features ja4plus` + `collect.http_fp`.
    pub ja4h_fp: Ja4hInventory,
    /// Passive asset inventory keyed by MAC: served on `@/query/assets` (#70).
    pub assets: AssetInventory,
}

/// Detector wrapper bridging `feed`→`verdict` for the TRW port-scan detector.
struct PortScan {
    detector: PortScanDetector<FiveTupleKey>,
    cfg: LiveConfig,
    last_score: Option<ScanScore<FiveTupleKey>>,
}

/// Detector wrapper for the RITA-style beaconing detector (issue #17). Reads its
/// threshold + allowlist live from `cfg` so they hot-swap at runtime (#121).
struct Beacon {
    detector: BeaconDetector<FiveTupleKey>,
    cfg: LiveConfig,
    last_score: Option<BeaconScore<FiveTupleKey>>,
}

/// Detector wrapper for the RITA-style ROBUST beaconing detector (issue #118):
/// Bowley skewness + MAD, bit-faithful to RITA, catches jittered C2 the CV
/// detector misses. Fed the same `FlowPacket` (key, ts, len) stream as `Beacon`.
struct RitaBeacon {
    detector: RitaBeaconDetector<FiveTupleKey>,
    cfg: LiveConfig,
    last_score: Option<RitaBeaconScore<FiveTupleKey>>,
}

/// Local detector-score wrapping a [`RitaBeaconScore`] so the published anomaly
/// carries the ZenSight kind slug `"RitaBeacon"` (the built-in `DetectorScore`
/// impl emits `"BeaconRita"`). `with_key` attaches the 5-tuple so the drain's
/// `anomaly_alert` derives src/dst labels + the cross-tool Community ID.
struct RitaBeaconHit(RitaBeaconScore<FiveTupleKey>);

impl flowscope::DetectorScore for RitaBeaconHit {
    fn name(&self) -> &'static str {
        "RitaBeacon"
    }
    fn into_anomaly(self, ts: flowscope::Timestamp) -> flowscope::OwnedAnomaly {
        let s = self.0;
        flowscope::OwnedAnomaly::new("RitaBeacon", flowscope::event::Severity::Warning, ts)
            .with_key(&s.key)
            .with_metric("score", s.score)
            .with_metric("ts_score", s.ts_score)
            .with_metric("ds_score", s.ds_score)
            .with_metric("dur_score", s.dur_score)
            .with_metric("mean_interval_secs", s.mean_interval.as_secs_f64())
            .with_metric("n", s.n as f64)
    }
}

/// Detector wrapper for the DGA scorer over DNS query SLDs (issue #18). Reads
/// its threshold + allowlist live from `cfg` (#121).
struct Dga {
    scorer: DgaScorer,
    cfg: LiveConfig,
    last_score: Option<DgaScore>,
}

/// Connection-flood detector (issue #18): counts TCP flow-starts per (dst,port)
/// in a sliding window via a `TimeBucketedCounter`, flagging once a single
/// (dst,port) crosses the threshold. Distinct from a port scan (many ports).
struct Flood {
    counter: TimeBucketedCounter<String>,
    cfg: LiveConfig,
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

/// Decompose netring's `DropBreakdown` into the pure [`map::CaptureDrops`] view,
/// keeping the netring capture type out of `map.rs` (issue #71).
fn drop_breakdown_view(d: &netring::stats::DropBreakdown) -> map::CaptureDrops {
    use netring::stats::DropBreakdown as D;
    match *d {
        D::AfPacket { freezes } => map::CaptureDrops::AfPacket { freezes },
        D::Xdp {
            rx_dropped,
            rx_invalid_descs,
            rx_ring_full,
            rx_fill_ring_empty_descs,
            tx_invalid_descs,
            tx_ring_empty_descs,
        } => map::CaptureDrops::Xdp {
            rx_dropped,
            rx_invalid_descs,
            rx_ring_full,
            rx_fill_ring_empty_descs,
            tx_invalid_descs,
            tx_ring_empty_descs,
        },
        // `DropBreakdown` is #[non_exhaustive]; map any future variant to an
        // empty AF_PACKET breakdown so the flat drop counters still publish.
        _ => map::CaptureDrops::AfPacket { freezes: 0 },
    }
}

/// Decode the asset capability bitmask into stable lowercase slugs (only the
/// set bits), for the on-demand `AssetRecord`. Order is deterministic.
fn capability_names(caps: flowscope::AssetCapabilities) -> Vec<String> {
    use flowscope::AssetCapabilities as C;
    [
        (C::BRIDGE, "bridge"),
        (C::ROUTER, "router"),
        (C::SWITCH, "switch"),
        (C::WLAN_AP, "wlan_ap"),
        (C::PHONE, "phone"),
        (C::IGMP, "igmp"),
        (C::REPEATER, "repeater"),
        (C::DOCSIS_CABLE, "docsis_cable"),
        (C::SOURCE_BRIDGE, "source_bridge"),
        (C::HOST, "host"),
        (C::REMOTELY_MANAGED, "remotely_managed"),
        (C::UPNP, "upnp"),
        (C::C_VLAN, "c_vlan"),
        (C::S_VLAN, "s_vlan"),
    ]
    .iter()
    .filter(|(bit, _)| caps.contains(*bit))
    .map(|(_, name)| name.to_string())
    .collect()
}

/// Decode the asset source bitmask into stable lowercase parser slugs.
fn source_names(set: flowscope::AssetSourceSet) -> Vec<String> {
    use flowscope::AssetSourceSet as S;
    [
        (S::ARP, "arp"),
        (S::NDP, "ndp"),
        (S::DHCP, "dhcp"),
        (S::LLDP, "lldp"),
        (S::CDP, "cdp"),
        (S::SSDP, "ssdp"),
        (S::MDNS, "mdns"),
        (S::NBNS, "nbns"),
    ]
    .iter()
    .filter(|(bit, _)| set.contains(*bit))
    .map(|(_, name)| name.to_string())
    .collect()
}

/// Flatten a `flowscope::Asset` into the transport-neutral [`AssetRecord`] DTO:
/// stringify the MAC/IPs, decode the capability + source bitmasks to slugs, and
/// carry the most-recent-observation timestamp as Unix epoch milliseconds.
fn asset_to_record(a: &flowscope::Asset) -> AssetRecord {
    AssetRecord {
        mac: a.mac.to_string(),
        ipv4: a.ipv4.iter().map(|ip| ip.to_string()).collect(),
        ipv6: a.ipv6.iter().map(|ip| ip.to_string()).collect(),
        hostname: a.hostname.clone(),
        vendor: a.vendor_banner.clone(),
        platform: a.platform.clone(),
        capabilities: capability_names(a.capabilities),
        seen_via: source_names(a.seen_via),
        last_seen: (a.last_seen.to_unix_f64() * 1000.0) as i64,
    }
}

/// What [`build`] returns: the monitor, its emit channels, the telemetry-channel
/// keepalive, and the runtime detection-tuning handle (#121).
pub type BuiltMonitor = (
    netring::monitor::Monitor,
    MonitorChannels,
    mpsc::UnboundedSender<TelemetryPoint>,
    DetectorHandle,
);

/// Build a netring `Monitor` from config plus the channels it emits on.
pub fn build(cfg: &NetringConfig) -> Result<BuiltMonitor, Box<dyn std::error::Error>> {
    // Live, hot-swappable anomaly config (#121). Detectors read `det_cfg`; the
    // command channel mutates it through the returned handle.
    let detector_handle = DetectorHandle::new(cfg.anomalies.clone());
    let det_cfg: LiveConfig = detector_handle.shared();
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
    let matrix: MatrixHist = Arc::new(Mutex::new(HashMap::new()));
    let elephants: ElephantRing = Arc::new(Mutex::new(VecDeque::with_capacity(ELEPHANT_RING_CAP)));
    let quic: QuicInventory = Arc::new(Mutex::new(HashMap::new()));
    let ssh: SshInventory = Arc::new(Mutex::new(HashMap::new()));
    let ja4h_fp: Ja4hInventory = Arc::new(Mutex::new(HashMap::new()));
    let assets: AssetInventory = Arc::new(Mutex::new(HashMap::new()));

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

    // TCP initiator inference (#122): recover the true initiator (SYN sender)
    // even on mid-handshake capture / SYN+ACK races, so flow direction is
    // capture-order-independent. No-op cost when off. TCP-only.
    b = b.infer_tcp_initiator(cfg.collect.infer_initiator);

    // Active load-shedding (#224): built only when armed AND detection is on.
    // When absent the flow handlers and capture-stats path behave byte-for-byte
    // as before (pure detection). Shared across the FlowStarted (admit),
    // FlowEnded (drop shed flows) and capture-stats (observe + telemetry) paths.
    let shed_ctl: Option<Arc<Mutex<crate::shed::ShedController>>> = (cfg.overload.enabled
        && cfg.overload.shed.enabled)
        .then(|| Arc::new(Mutex::new(crate::shed::ShedController::new(&cfg.overload))));

    // Flow lifecycle counters + per-L4/connection-state breakdown + top-talkers.
    if cfg.collect.flows {
        use netring::protocol::event_typed::FlowStarted;
        let started = flow_started.clone();
        let shed_fs = shed_ctl.clone();
        b = b.on_ctx::<FlowStarted<Tcp>>(move |e: &FlowStarted<Tcp>, _ctx: &mut Ctx<'_>| {
            started.fetch_add(1, Ordering::Relaxed);
            // Admission decision (deliberate, counted). A shed flow's hash is
            // remembered so its FlowEnded is dropped from telemetry below.
            if let Some(shed) = &shed_fs
                && let Ok(mut s) = shed.lock()
            {
                let h = crate::shed::ShedController::flow_hash(&e.key);
                s.admit(h);
            }
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
        let matrix_h = matrix.clone();
        let elephants_h = elephants.clone();
        let collect_talkers = cfg.collect.talkers;
        // Data-exfiltration baseline (#123) — built only when enabled at startup
        // (the #121 detectors follow the same rule). The closure feeds outbound
        // bytes per source and ships a finding on the typed alerts channel; sigma
        // / floor / mute / allowlist are read LIVE from the tunable config.
        let exfil = cfg.anomalies.data_exfil.then(|| {
            Arc::new(Mutex::new(crate::exfil::ExfilDetector::new(
                EXFIL_EWMA_ALPHA,
            )))
        });
        let exfil_h = exfil.clone();
        let exfil_alert_tx = alert_tx.clone();
        let exfil_cfg = det_cfg.clone();
        let exfil_sensor_id = cfg.sensor_id.clone();
        let shed_fe = shed_ctl.clone();
        b = b.on_ctx::<FlowEnded<Tcp>>(move |e: &FlowEnded<Tcp>, _ctx: &mut Ctx<'_>| {
            // Honest shed: a flow shed at start is dropped from telemetry here
            // (not silently retained), so "data is sampled" is the truth.
            if let Some(shed) = &shed_fe
                && let Ok(mut s) = shed.lock()
                && s.take_shed(crate::shed::ShedController::flow_hash(&e.key))
            {
                return Ok(());
            }
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
                &matrix_h,
                &elephants_h,
                collect_talkers,
            );
            if let Some(exfil) = &exfil_h {
                feed_exfil(exfil, e, &exfil_cfg, &exfil_alert_tx, &exfil_sensor_id);
            }
            Ok(())
        });

        // UDP + ICMP flow ends feed only the per-L4 composition (and talkers/matrix).
        let l4_udp = l4.clone();
        let talkers_udp = talkers.clone();
        let matrix_udp = matrix.clone();
        let collect_talkers_udp = cfg.collect.talkers;
        b = b.protocol::<Udp>();
        b = b.on_ctx::<FlowEnded<Udp>>(move |e: &FlowEnded<Udp>, _ctx: &mut Ctx<'_>| {
            l4_udp
                .udp_bytes
                .fetch_add(e.stats.total_bytes(), Ordering::Relaxed);
            l4_udp.udp_flows.fetch_add(1, Ordering::Relaxed);
            if collect_talkers_udp {
                // UDP has no handshake, so the initiator is the first-packet
                // sender (best-effort); still order initiator → responder so the
                // service map is directional (e.g. DNS client → resolver).
                let (ini, resp) =
                    map::initiator_responder(e.key.a, e.key.b, e.stats.initiator_orientation);
                record_talker(&talkers_udp, &resp.to_string(), &e.stats);
                record_matrix(&matrix_udp, &ini.to_string(), &resp.to_string(), &e.stats);
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
                // `HttpMessage` is `#[non_exhaustive]` as of flowscope 0.20 — we
                // only track requests/responses; ignore any future variants.
                _ => {}
            }
            Ok(())
        });
    }

    // L7 QUIC Initial visibility (issue #72) — passive SNI/ALPN/version from the
    // unprotected ClientHello (UDP/443). The QUIC analogue of TLS fingerprinting.
    if cfg.collect.quic {
        use flowscope::QuicInitial;
        b = b.protocol::<Quic>();
        let inv = quic.clone();
        b = b.on::<Quic>(move |q: &QuicInitial| {
            let version = q.version.to_string();
            let key = (q.sni.clone().unwrap_or_default(), version.clone());
            if let Ok(mut m) = inv.lock() {
                if let Some(rec) = m.get_mut(&key) {
                    rec.count += 1;
                } else if m.len() < QUIC_INVENTORY_CAP {
                    m.insert(
                        key,
                        QuicRecord {
                            sni: q.sni.clone(),
                            alpn: q.alpn.clone(),
                            version,
                            count: 1,
                        },
                    );
                }
            }
            Ok(())
        });
    }

    // L7 SSH/HASSH visibility (issue #72) — banner + KEXINIT HASSH fingerprints
    // (TCP/22). The banner precedes the KEXINIT on the same flow, so we stash it
    // per-flow and attach it to the fingerprint when the KEXINIT lands.
    if cfg.collect.ssh {
        use flowscope::ssh::SshMessage;
        b = b.protocol::<Ssh>();
        let inv = ssh.clone();
        let pending: SshPending = Arc::new(Mutex::new(HashMap::new()));
        b = b.on_ctx::<Ssh>(move |msg: &SshMessage, ctx: &mut Ctx<'_>| {
            match msg {
                SshMessage::Banner { banner } => {
                    if let (Some(k), Ok(mut p)) = (ctx.flow, pending.lock())
                        && p.len() < 65_536
                    {
                        p.insert(k, banner.clone());
                    }
                }
                SshMessage::KexInit(kex) => {
                    // Consume the pending banner for this flow (remove, not get):
                    // the entry is matched exactly once at KEXINIT, so leaving it
                    // in the map would leak entries up to the 64k cap, after which
                    // new flows' banners would be silently dropped.
                    let banner = ctx
                        .flow
                        .and_then(|k| pending.lock().ok().and_then(|mut p| p.remove(&k)));
                    let role = if kex.from_client { "client" } else { "server" };
                    if let Ok(mut m) = inv.lock() {
                        if let Some(rec) = m.get_mut(&kex.hassh) {
                            rec.count += 1;
                            if rec.banner.is_none() {
                                rec.banner = banner;
                            }
                        } else if m.len() < SSH_INVENTORY_CAP {
                            m.insert(
                                kex.hassh.clone(),
                                SshRecord {
                                    hassh: kex.hassh.clone(),
                                    role: role.to_string(),
                                    banner,
                                    count: 1,
                                },
                            );
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        });
    }

    // L7 JA4H HTTP-request fingerprinting (issue #124) — opt-in, behind the
    // `ja4plus` build feature (FoxIO License 1.1). netring computes the JA4H
    // fingerprint from the cleartext request; we accumulate a per-fingerprint
    // inventory served on `@/query/ja4h`. The hook auto-registers `Http`, which
    // netring de-dups against the `collect.http` RED handler's `.protocol::<Http>()`.
    #[cfg(feature = "ja4plus")]
    if cfg.collect.http_fp {
        use netring::monitor::fingerprint::HttpFingerprint;
        let inv = ja4h_fp.clone();
        b = b.on_http_fingerprint(move |fp: &HttpFingerprint, _ctx: &mut Ctx<'_>| {
            if let Ok(mut m) = inv.lock() {
                if let Some(rec) = m.get_mut(&fp.ja4h) {
                    rec.count += 1;
                    // Backfill best-effort context the first record may have missed.
                    if rec.host.is_none() {
                        rec.host = fp.host.clone();
                    }
                    if rec.user_agent.is_none() {
                        rec.user_agent = fp.user_agent.clone();
                    }
                } else if m.len() < JA4H_INVENTORY_CAP {
                    m.insert(
                        fp.ja4h.clone(),
                        Ja4hRecord {
                            ja4h: fp.ja4h.clone(),
                            host: fp.host.clone(),
                            method: fp.method.clone(),
                            user_agent: fp.user_agent.clone(),
                            count: 1,
                        },
                    );
                }
            }
            Ok(())
        });
    }

    // Cleartext SNMP community detection (issue #72) — opt-in, behind the `snmp`
    // build feature. v1/v2c carry the community string in cleartext: a classic
    // credential-exposure + lateral-movement signal. Flagged as an anomaly →
    // alert via the typed alerts channel.
    #[cfg(feature = "snmp")]
    if cfg.collect.snmp_cleartext {
        use flowscope::snmp::{SnmpMessage, SnmpVersion};
        b = b.protocol::<Snmp>();
        let alerts_h = alert_tx.clone();
        let sensor_id = cfg.sensor_id.clone();
        b = b.on_ctx::<Snmp>(move |msg: &SnmpMessage, ctx: &mut Ctx<'_>| {
            if matches!(msg.version, SnmpVersion::V1 | SnmpVersion::V2c) {
                let version = match msg.version {
                    SnmpVersion::V1 => "v1",
                    SnmpVersion::V2c => "v2c",
                    _ => "other",
                };
                let (src, dst) = ctx.flow.map(|k| (k.a.to_string(), k.b.to_string())).unzip();
                let _ = alerts_h.send(crate::map::snmp_cleartext_alert(
                    &sensor_id,
                    version,
                    &msg.community,
                    src,
                    dst,
                ));
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

    // Capture self-health (live capture only): honest per-source drop breakdown
    // + debounced overload detection (issue #71). Each sample's windowed
    // drop-rate feeds a per-source OverloadDetector; on a Normal↔Emergency
    // transition we ship a `capture-overload` SensorHealth alert on the alerts
    // channel ("the sensor is silently losing your packets").
    if cfg.collect.capture_stats {
        use netring::monitor::overload::{OverloadConfig, OverloadDetector, OverloadState};
        use netring::monitor::telemetry::CaptureTelemetry;
        let tx = tel_tx.clone();
        let sensor_id = cfg.sensor_id.clone();
        let overload_cfg = cfg.overload.clone();
        let alerts_h = alert_tx.clone();
        // When shedding is armed, a single controller drives detection + the
        // shed action (its hysteresis replaces the per-source detector — same
        // thresholds, no double-instantiation). Multi-source shedding is an
        // approximation today (worst source drives Emergency); per-NIC isolation
        // arrives with the multi-NIC issue (#226). When unarmed, the per-source
        // OverloadDetector path below is byte-for-byte today's behaviour.
        let shed_cs = shed_ctl.clone();
        let mut detectors: HashMap<u8, OverloadDetector> = HashMap::new();
        b = b.on_capture_stats(
            Duration::from_secs(cfg.bandwidth_period_secs.max(1)),
            move |t: &CaptureTelemetry, _ctx: &mut Ctx<'_>| {
                let source = t.source.0;
                for p in crate::map::capture_points(
                    &sensor_id,
                    source,
                    t.packets,
                    t.drops,
                    t.drop_rate,
                    &drop_breakdown_view(&t.detail),
                ) {
                    let _ = tx.send(p);
                }
                if overload_cfg.enabled {
                    if let Some(shed) = &shed_cs {
                        if let Ok(mut s) = shed.lock() {
                            if let Some(state) = s.observe(t.drop_rate) {
                                let firing = matches!(state, OverloadState::Emergency);
                                let policy = firing.then(|| s.policy_label());
                                let _ = alerts_h.send(crate::map::overload_alert_shed(
                                    &sensor_id,
                                    source,
                                    t.drop_rate,
                                    firing,
                                    policy,
                                ));
                            }
                            // Honest shed accounting: cumulative shed count +
                            // an `active` gauge, every tick.
                            for p in crate::map::shed_points(
                                &sensor_id,
                                source,
                                s.shed_total(),
                                s.is_shedding(),
                                s.policy_label(),
                            ) {
                                let _ = tx.send(p);
                            }
                        }
                    } else {
                        let det = detectors.entry(source).or_insert_with(|| {
                            OverloadDetector::new(
                                OverloadConfig::default()
                                    .enter_at(overload_cfg.enter_drop_rate)
                                    .recover_at(
                                        overload_cfg.recover_drop_rate,
                                        overload_cfg.recover_windows,
                                    ),
                            )
                        });
                        if let Some(state) = det.observe(t.drop_rate) {
                            let firing = matches!(state, OverloadState::Emergency);
                            let _ = alerts_h.send(crate::map::overload_alert(
                                &sensor_id,
                                source,
                                t.drop_rate,
                                firing,
                            ));
                        }
                    }
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
            detector: PortScan { detector: PortScanDetector::new(), cfg: det_cfg.clone(), last_score: None },
            feed: |evt, w| {
                let success = matches!(evt.reason, EndReason::Fin | EndReason::IdleTimeout);
                w.last_score = Some(w.detector.observe(evt.key, success));
            },
            verdict: |_evt, w| {
                // Muted at runtime? (#121)
                if !w.cfg.load().port_scan {
                    None
                } else {
                    w.last_score.as_ref().and_then(|s| {
                        if matches!(s.verdict, ScanVerdict::Scanner) {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                }
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
                cfg: det_cfg.clone(),
                last_score: None,
            },
            feed: |evt, w| {
                if matches!(evt.proto, L4Proto::Tcp) {
                    w.last_score = w.detector.observe(evt.key, evt.ts, evt.len as u64);
                }
            },
            verdict: |_evt, w| {
                let c = w.cfg.load();
                if !c.beaconing {
                    None // muted at runtime (#121)
                } else {
                    w.last_score.as_ref().and_then(|s| {
                        let dst = s.key.b.ip().to_string();
                        if s.score >= c.beacon_threshold && !allowlisted(&dst, &c.allowlist) {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                }
            },
        };
        b = b.detect(beacon);
    }

    // RITA robust beaconing detector (issue #118) — wired alongside the CV
    // beacon, fed the identical FlowPacket (key, ts, len) stream. Bowley-skew +
    // MAD survive jitter, so this catches periodic C2 the CV detector misses.
    if cfg.anomalies.rita_beacon {
        let rita = netring::pattern_detector! {
            name: "RitaBeacon",
            event: FlowPacket,
            detector: RitaBeacon {
                detector: RitaBeaconDetector::new(),
                cfg: det_cfg.clone(),
                last_score: None,
            },
            feed: |evt, w| {
                if matches!(evt.proto, L4Proto::Tcp) {
                    w.last_score = w.detector.observe(evt.key, evt.ts, evt.len as u64);
                }
            },
            verdict: |_evt, w| {
                let c = w.cfg.load();
                if !c.rita_beacon {
                    None // muted at runtime (#121)
                } else {
                    w.last_score.as_ref().and_then(|s| {
                        let dst = s.key.b.ip().to_string();
                        if s.score >= c.rita_beacon_threshold && !allowlisted(&dst, &c.allowlist) {
                            Some(RitaBeaconHit(s.clone()))
                        } else {
                            None
                        }
                    })
                }
            },
        };
        b = b.detect(rita);
    }

    // Connection-flood detector (issue #18).
    if cfg.anomalies.connection_flood {
        use netring::protocol::event_typed::FlowStarted;
        let flood = netring::pattern_detector! {
            name: "ConnectionFlood",
            event: FlowStarted<Tcp>,
            detector: Flood {
                counter: TimeBucketedCounter::new(Duration::from_secs(10), Duration::from_secs(1), 16_384),
                cfg: det_cfg.clone(),
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
                let c = w.cfg.load();
                if !c.connection_flood {
                    None // muted at runtime (#121)
                } else {
                    w.last_hit.as_ref().and_then(|(dst, count)| {
                        if *count >= c.flood_threshold {
                            Some(FloodScore { dst: dst.clone(), count: *count })
                        } else {
                            None
                        }
                    })
                }
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
                cfg: det_cfg.clone(),
                last_score: None,
            },
            feed: |msg, w| {
                w.last_score = None;
                if let DnsMessage::Query(q) = msg
                    && let Some(question) = q.questions.first()
                    && let Some(sld) = map::dns_sld(&question.name)
                    && !allowlisted(&sld, &w.cfg.load().allowlist)
                {
                    let sc = w.scorer.score(&sld);
                    w.last_score = Some(sc);
                }
            },
            verdict: |_msg, w| {
                let c = w.cfg.load();
                if !c.dga {
                    None // muted at runtime (#121)
                } else {
                    w.last_score.as_ref().and_then(|s| {
                        let fire = (s.log_likelihood as f64) < c.dga_threshold;
                        if fire { Some(*s) } else { None }
                    })
                }
            },
        };
        b = b.detect(dga);
    }

    // DNS tunneling + Newly-Observed-Domain detectors (issue #118) — both need
    // DNS parsing (`collect.dns`) for the qname and the flow ctx for src/ts, so
    // they ride a dedicated `on_ctx::<Dns>` handler (handlers append, so this
    // coexists with the collect.dns RED handler and the DGA detector). Hits are
    // emitted as anomaly alerts on the typed alerts channel via the shared
    // `map::anomaly_alert` path (kind → ATT&CK technique, Community ID). Detector
    // state lives behind a `Mutex` (on_ctx handlers are `Fn`), held briefly.
    if (cfg.anomalies.dns_tunnel || cfg.anomalies.nod) && cfg.collect.dns {
        use flowscope::dns::DnsMessage;
        let alerts_h = alert_tx.clone();
        let sensor_id = cfg.sensor_id.clone();
        // Read allowlist + per-detector enables/thresholds live so they hot-swap
        // at runtime (#121).
        let det = det_cfg.clone();
        let tunnel_set: Mutex<TimeBucketedSet<(String, String), String>> =
            Mutex::new(TimeBucketedSet::new(
                Duration::from_secs(60),
                Duration::from_secs(5),
                DNS_TUNNEL_KEY_CAP,
            ));
        let nod_seen: Mutex<map::SeenDomains> = Mutex::new(map::SeenDomains::new(NOD_SEEN_CAP));
        b = b.on_ctx::<Dns>(move |msg: &DnsMessage, ctx: &mut Ctx<'_>| {
            let DnsMessage::Query(q) = msg else {
                return Ok(());
            };
            let Some(question) = q.questions.first() else {
                return Ok(());
            };
            let Some(sld) = map::dns_sld(&question.name) else {
                return Ok(());
            };
            let c = det.load();
            if allowlisted(&sld, &c.allowlist) {
                return Ok(());
            }
            // Source HOST IP only (no ephemeral port) → stable (rule, src) bucket.
            let src = ctx.flow.map(|k| k.a.ip().to_string());

            // NOD: emit once on the first sight of this SLD on the wire.
            if c.nod
                && let Ok(mut seen) = nod_seen.lock()
                && seen.observe(&sld)
            {
                let view = map::nod_view(src.clone(), &sld);
                let _ = alerts_h.send(map::anomaly_alert(&sensor_id, &view));
            }

            // DNS tunnel: distinct subdomain labels per (src, SLD) + qname length.
            if c.dns_tunnel {
                let qname = question.name.trim_end_matches('.').to_ascii_lowercase();
                let key = (src.clone().unwrap_or_default(), sld.clone());
                let distinct = if let Ok(mut set) = tunnel_set.lock() {
                    set.insert(key.clone(), qname.clone(), ctx.ts);
                    set.cardinality(&key, ctx.ts)
                } else {
                    0
                };
                if map::dns_tunnel_fires(
                    distinct,
                    qname.len(),
                    c.dns_tunnel_distinct,
                    c.dns_tunnel_qname_len,
                ) {
                    let view = map::dns_tunnel_view(src, &sld, distinct, qname.len());
                    let _ = alerts_h.send(map::anomaly_alert(&sensor_id, &view));
                }
            }
            Ok(())
        });
        tracing::info!("netring: DNS tunnel / newly-observed-domain detection enabled");
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

    // Passive asset inventory (netring 0.27, issue #70). The discovery hooks
    // (ARP / NDP / LLDP, plus CDP behind its own flag) feed netring's MAC-keyed
    // inventory; `on_asset` fires on each inventory *event* (new or changed
    // asset) and folds the flattened record into the served map. The capture
    // path stays cheap — a single short-held lock + a struct build.
    if cfg.collect.assets {
        b = b.asset_inventory(ASSET_INVENTORY_CAP);
        // The inventory is fed by netring's per-frame discovery re-parse, which
        // is independent of these hooks — but the kernel prefilter only *admits*
        // the discovery traffic when the corresponding interest is armed. So we
        // arm ARP (EtherType 0x0806), NDP (ICMPv6), and LLDP (EtherType 0x88cc)
        // to add their precise prefilter terms; the empty closures exist only to
        // arm those interests. `arp_table()` arms ARP without an ARP handler.
        b = b.arp_table();
        b = b.on_ndp(|_m, _ctx| Ok(()));
        b = b.on_lldp(|_m, _ctx| Ok(()));
        if cfg.collect.asset_cdp {
            // CDP can't be expressed as a prefilter term → forces capture-all.
            b = b.on_cdp(|_m, _ctx| Ok(()));
        }
        let inv = assets.clone();
        b = b.on_asset(move |asset: &flowscope::Asset, _ctx: &mut Ctx<'_>| {
            let record = asset_to_record(asset);
            if let Ok(mut map) = inv.lock()
                && (map.contains_key(&record.mac) || map.len() < ASSET_INVENTORY_CAP)
            {
                map.insert(record.mac.clone(), record);
            }
            Ok(())
        });
        tracing::info!("netring: passive asset inventory enabled");
    }

    // Lateral-movement detection (#123): SMB admin-share / IPC$ service-pipe,
    // RDP connection requests, Kerberos kerberoast/weak-etype/brute-force. The
    // parsers are only compiled under the `lateral` feature; the per-message
    // enable + allowlist are read live (#121). Built once at startup like the
    // other detectors.
    #[cfg(feature = "lateral")]
    if cfg.anomalies.lateral_movement {
        use flowscope::kerberos::KerberosMessage;
        use flowscope::rdp::RdpMessage;
        use flowscope::smb::SmbMessage;

        // SMB → admin-share / service-pipe access.
        b = b.protocol::<Smb>();
        let (alerts, det, sid) = (alert_tx.clone(), det_cfg.clone(), cfg.sensor_id.clone());
        b = b.on_ctx::<Smb>(move |m: &SmbMessage, ctx: &mut Ctx<'_>| {
            if det.load().lateral_movement
                && let Some(f) = crate::lateral::smb_finding(
                    m.tree_connect_is_admin_share,
                    m.create_is_admin_named_pipe,
                    m.tree_connect_path.as_deref(),
                    m.create_path.as_deref(),
                    m.ntlm_auth.as_ref().and_then(|n| n.username.as_deref()),
                )
            {
                emit_lateral(&alerts, &sid, ctx, f);
            }
            Ok(())
        });

        // RDP → connection request between peers.
        b = b.protocol::<Rdp>();
        let (alerts, det, sid) = (alert_tx.clone(), det_cfg.clone(), cfg.sensor_id.clone());
        b = b.on_ctx::<Rdp>(move |m: &RdpMessage, ctx: &mut Ctx<'_>| {
            if det.load().lateral_movement
                && let RdpMessage::ConnectionRequest {
                    cookie_username, ..
                } = m
                && let Some(f) = crate::lateral::rdp_finding(cookie_username.as_deref())
            {
                emit_lateral(&alerts, &sid, ctx, f);
            }
            Ok(())
        });

        // Kerberos → kerberoasting / weak-etype / brute-force signals.
        b = b.protocol::<Kerberos>();
        let (alerts, det, sid) = (alert_tx.clone(), det_cfg.clone(), cfg.sensor_id.clone());
        b = b.on_ctx::<Kerberos>(move |m: &KerberosMessage, ctx: &mut Ctx<'_>| {
            if det.load().lateral_movement {
                let weak = m.etypes.iter().any(|e| e.is_weak());
                let brute = m
                    .error_code
                    .as_ref()
                    .is_some_and(|e| e.is_brute_force_signal());
                let realm = (!m.realm.is_empty()).then_some(m.realm.as_str());
                if let Some(f) = crate::lateral::kerberos_finding(
                    m.kerberoast_suspect,
                    brute,
                    weak,
                    realm,
                    m.sname.as_deref(),
                ) {
                    emit_lateral(&alerts, &sid, ctx, f);
                }
            }
            Ok(())
        });

        tracing::info!("netring: lateral-movement detection (SMB/RDP/Kerberos) enabled");
    }
    #[cfg(not(feature = "lateral"))]
    if cfg.anomalies.lateral_movement {
        tracing::warn!(
            "netring: anomalies.lateral_movement is set but the sensor was built without the `lateral` feature"
        );
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
            matrix,
            elephants,
            quic,
            ssh,
            ja4h_fp,
            assets,
        },
        keepalive,
        detector_handle,
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
    matrix: &MatrixHist,
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
    // Community ID v1 (#116) — directionless 5-tuple hash for cross-tool
    // correlation. Computed on the canonical (a, b) key; orientation must NOT
    // leak into the hash (regression-pinned by `community_id_symmetric`).
    let community_id =
        map::proto_number(proto).map(|n| map::community_id_v1(e.key.a, e.key.b, n, 0));
    // Direction (#122): present src/dst as initiator → responder. TCP is
    // handshake-aware, so the record is authoritatively `directed`.
    let (ini, resp) = map::initiator_responder(e.key.a, e.key.b, e.stats.initiator_orientation);
    let (ini_s, resp_s) = (ini.to_string(), resp.to_string());
    let rec = crate::map::flow_record(
        ini_s.clone(),
        resp_s.clone(),
        proto,
        total_bytes,
        total_packets,
        duration_ms,
        reason,
        community_id,
        true,
    );
    if let Ok(mut r) = records.lock() {
        if r.len() == FLOW_RING_CAP {
            r.pop_front();
        }
        r.push_back(rec);
    }

    if collect_talkers {
        record_talker(talkers, &resp_s, &e.stats);
        record_matrix(matrix, &ini_s, &resp_s, &e.stats);
        // Elephant ring: keep the largest recent flows (push, trim by size).
        if let Ok(mut ring) = elephants.lock() {
            let er = crate::map::elephant_record(
                ini_s.clone(),
                resp_s.clone(),
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

/// Feed a finished TCP flow's outbound volume into the data-exfil baseline (#123)
/// and ship a `DataExfiltration` alert when it stands out. Reads the live tunable
/// config so mute / sigma / floor / allowlist hot-swap at runtime (#121); the
/// outbound direction is the initiator (client → server) byte count.
fn feed_exfil(
    exfil: &Arc<Mutex<crate::exfil::ExfilDetector>>,
    e: &FlowEnded<Tcp>,
    det_cfg: &LiveConfig,
    alert_tx: &mpsc::UnboundedSender<Alert>,
    sensor_id: &str,
) {
    let c = det_cfg.load();
    if !c.data_exfil {
        return;
    }
    // Attribute the exfil baseline to the resolved initiator (the host pushing
    // bytes out), not the canonical key order (#122).
    let (initiator, responder) =
        map::initiator_responder(e.key.a, e.key.b, e.stats.initiator_orientation);
    let src_ip = initiator.ip();
    if allowlisted(&src_ip.to_string(), &c.allowlist) {
        return;
    }
    let bytes_out = e.stats.bytes_for(flowscope::FlowSide::Initiator);
    let finding = {
        let Ok(mut d) = exfil.lock() else {
            return;
        };
        d.observe(src_ip, bytes_out, c.exfil_sigma, c.exfil_min_bytes)
    };
    if let Some(f) = finding {
        let view = map::exfil_view(
            initiator.to_string(),
            responder.to_string(),
            f.bytes_out,
            f.zscore,
        );
        let _ = alert_tx.send(map::anomaly_alert(sensor_id, &view));
    }
}

/// Ship a lateral-movement [`LateralFinding`](crate::lateral::LateralFinding) as
/// a `zensight` alert (#123), attributing src/dst from the flow key in `ctx`.
#[cfg(feature = "lateral")]
fn emit_lateral(
    alerts: &mpsc::UnboundedSender<Alert>,
    sensor_id: &str,
    ctx: &Ctx<'_>,
    f: crate::lateral::LateralFinding,
) {
    let src = ctx.flow.map(|k| k.a.to_string());
    let dst = ctx.flow.map(|k| k.b.to_string());
    let view = map::lateral_view(f.kind, src, dst, f.severity, f.observations);
    let _ = alerts.send(map::anomaly_alert(sensor_id, &view));
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

/// Update the `(src,dst)` traffic-matrix histogram (#122), bounded by `TALKER_CAP`
/// like the talker map. Runs once per ended flow (not per packet), so building the
/// owned `(src,dst)` key here is cheap; the lock is held only for the update.
fn record_matrix(matrix: &MatrixHist, src: &str, dst: &str, stats: &flowscope::FlowStats) {
    if let Ok(mut m) = matrix.lock() {
        let key = (src.to_string(), dst.to_string());
        match m.get_mut(&key) {
            Some(e) => {
                e.0 += stats.total_bytes();
                e.1 += stats.total_packets();
                e.2 += 1;
            }
            None => {
                if m.len() < TALKER_CAP {
                    m.insert(key, (stats.total_bytes(), stats.total_packets(), 1));
                }
            }
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
    fn drop_breakdown_view_maps_both_backends() {
        use netring::stats::DropBreakdown as D;
        assert_eq!(
            drop_breakdown_view(&D::AfPacket { freezes: 7 }),
            map::CaptureDrops::AfPacket { freezes: 7 }
        );
        let xdp = D::Xdp {
            rx_dropped: 1,
            rx_invalid_descs: 2,
            rx_ring_full: 3,
            rx_fill_ring_empty_descs: 4,
            tx_invalid_descs: 5,
            tx_ring_empty_descs: 6,
        };
        assert_eq!(
            drop_breakdown_view(&xdp),
            map::CaptureDrops::Xdp {
                rx_dropped: 1,
                rx_invalid_descs: 2,
                rx_ring_full: 3,
                rx_fill_ring_empty_descs: 4,
                tx_invalid_descs: 5,
                tx_ring_empty_descs: 6,
            }
        );
    }

    #[test]
    fn capability_and_source_names_decode_set_bits_only() {
        use flowscope::{AssetCapabilities, AssetSourceSet};
        let caps = AssetCapabilities::ROUTER | AssetCapabilities::BRIDGE;
        let names = capability_names(caps);
        assert_eq!(names, vec!["bridge".to_string(), "router".to_string()]);
        assert!(capability_names(AssetCapabilities::empty()).is_empty());

        let set = AssetSourceSet::ARP | AssetSourceSet::LLDP;
        assert_eq!(
            source_names(set),
            vec!["arp".to_string(), "lldp".to_string()]
        );
    }

    #[test]
    fn overload_detector_from_config_fires_and_recovers() {
        use netring::monitor::overload::{OverloadConfig, OverloadDetector, OverloadState};
        // The sensor's config defaults (enter 5%, recover 1% for 3 windows)
        // must wire through `enter_at`/`recover_at` to reproduce the hysteresis.
        let cfg = crate::config::OverloadConfig::default();
        let mut det = OverloadDetector::new(
            OverloadConfig::default()
                .enter_at(cfg.enter_drop_rate)
                .recover_at(cfg.recover_drop_rate, cfg.recover_windows),
        );
        assert_eq!(det.observe(0.02), None, "below enter → stays Normal");
        assert_eq!(
            det.observe(0.06),
            Some(OverloadState::Emergency),
            "crosses 5%"
        );
        // Sustained calm required: 3 sub-1% windows before recovery.
        assert_eq!(det.observe(0.005), None);
        assert_eq!(det.observe(0.005), None);
        assert_eq!(det.observe(0.005), Some(OverloadState::Normal));
    }

    #[test]
    fn asset_to_record_flattens_a_discovery_asset() {
        use flowscope::{Asset, AssetCapabilities, AssetSourceSet, MacAddr, Timestamp};
        let mut a = Asset::new(MacAddr([0xaa, 0xbb, 0xcc, 0x11, 0x22, 0x33]));
        a.ipv4.push("10.0.0.5".parse().unwrap());
        a.hostname = Some("switch01".into());
        a.platform = Some("cisco WS-C2960X".into());
        a.capabilities |= AssetCapabilities::SWITCH | AssetCapabilities::BRIDGE;
        a.seen_via |= AssetSourceSet::LLDP;
        a.last_seen = Timestamp::new(1_700_000_000, 0);

        let rec = asset_to_record(&a);
        assert_eq!(rec.mac, "aa:bb:cc:11:22:33");
        assert_eq!(rec.ipv4, vec!["10.0.0.5".to_string()]);
        assert!(rec.ipv6.is_empty());
        assert_eq!(rec.hostname.as_deref(), Some("switch01"));
        assert_eq!(rec.platform.as_deref(), Some("cisco WS-C2960X"));
        assert_eq!(
            rec.capabilities,
            vec!["bridge".to_string(), "switch".to_string()]
        );
        assert_eq!(rec.seen_via, vec!["lldp".to_string()]);
        assert_eq!(rec.last_seen, 1_700_000_000_000);
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
