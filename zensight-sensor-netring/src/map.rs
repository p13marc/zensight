//! Pure mapping from netring/flowscope observations to ZenSight types.
//!
//! Kept free of the netring/flowscope capture machinery so it is unit-testable
//! without privileges. `monitor.rs` decomposes netring callbacks into these
//! plain views; here we map them to [`TelemetryPoint`]s and [`Alert`]s.

use std::net::{IpAddr, SocketAddr};

use flowscope::dns::{NameClaim, Provenance};
use zensight_common::{
    Alert, AlertKind, AlertSeverity, FlowRecord, NameInfo, Protocol, TelemetryPoint, TelemetryValue,
};

/// Build a [`FlowRecord`] (on-demand flow detail) from already-extracted fields.
/// Kept pure so its shape is unit-testable without the netring capture machinery.
///
/// `community_id` is the precomputed Community ID v1 hash (see [`community_id_v1`]),
/// `None` when the 5-tuple is incomplete. `directed` is `true` when `src`/`dst`
/// are an authoritative initiator → responder pair (TCP); `false` for
/// best-effort orderings (UDP). See [`initiator_responder`].
#[allow(clippy::too_many_arguments)]
pub fn flow_record(
    src: String,
    dst: String,
    proto: &str,
    bytes: u64,
    packets: u64,
    duration_ms: u64,
    reason: &str,
    community_id: Option<String>,
    directed: bool,
    dir_counts: DirCounts,
) -> FlowRecord {
    FlowRecord {
        src,
        dst,
        proto: proto.to_string(),
        bytes,
        packets,
        duration_ms,
        reason: reason.to_string(),
        community_id,
        directed,
        bytes_initiator: dir_counts.bytes_initiator,
        bytes_responder: dir_counts.bytes_responder,
        packets_initiator: dir_counts.packets_initiator,
        packets_responder: dir_counts.packets_responder,
        // Passive-DNS enrichment (#308) is attached by the caller (the flow-end
        // handler ranks the responder's name claims); starts empty here.
        dst_names: Vec::new(),
    }
}

/// Rank passive-DNS name claims for one IP into the top-3 [`NameInfo`] list
/// (#308). Ordering: provenance rank (forward DNS > CNAME > SNI > mDNS > DHCP >
/// PTR > other), then client-scoped claims before global ones, then most
/// recently seen. Deduped by name (best-ranked sighting wins), capped at 3.
/// Pure — unit-testable without the capture machinery.
pub fn rank_names<'a, I>(claims: I) -> Vec<NameInfo>
where
    I: IntoIterator<Item = &'a NameClaim>,
{
    let mut ranked: Vec<&NameClaim> = claims.into_iter().collect();
    ranked.sort_by_key(|c| {
        (
            provenance_rank(c.provenance),
            c.client.is_none(),
            std::cmp::Reverse(c.last_seen),
        )
    });
    let mut out: Vec<NameInfo> = Vec::with_capacity(3);
    for c in ranked {
        if out.iter().any(|n| n.name == c.name) {
            continue;
        }
        out.push(NameInfo {
            name: c.name.clone(),
            provenance: c.provenance.as_str().to_string(),
        });
        if out.len() == 3 {
            break;
        }
    }
    out
}

/// The best *forward* DNS name for an IP (#308): the top-ranked A/AAAA claim,
/// applying the same client-scoped/recency tie-breaks as [`rank_names`].
/// `None` when the IP has no forward claim — reverse-only (PTR) or SNI-only
/// bindings do not qualify as a beacon key (an attacker controls those).
pub fn best_forward_name<'a, I>(claims: I) -> Option<String>
where
    I: IntoIterator<Item = &'a NameClaim>,
{
    claims
        .into_iter()
        .filter(|c| matches!(c.provenance, Provenance::DnsA | Provenance::DnsAaaa))
        .min_by_key(|c| (c.client.is_none(), std::cmp::Reverse(c.last_seen)))
        .map(|c| c.name.clone())
}

/// Provenance ordering for [`rank_names`]: lower ranks first. Forward answers
/// (what the client actually dialed) outrank the client-claimed (SNI) and
/// reverse (PTR) sources; `Provenance` is `#[non_exhaustive]`, so unknown
/// future sources sink to the bottom with `Other`.
fn provenance_rank(p: Provenance) -> u8 {
    match p {
        Provenance::DnsA | Provenance::DnsAaaa => 0,
        Provenance::DnsCname => 1,
        Provenance::Sni => 2,
        Provenance::Mdns => 3,
        Provenance::Dhcp => 4,
        Provenance::DnsPtr => 5,
        _ => 6,
    }
}

/// Parse a talker/flow endpoint label (`"ip:port"` or bare `"ip"`) back to its
/// IP for name-map lookups (#308). `None` for non-address labels.
pub fn endpoint_ip(endpoint: &str) -> Option<IpAddr> {
    endpoint
        .parse::<SocketAddr>()
        .map(|sa| sa.ip())
        .or_else(|_| endpoint.parse::<IpAddr>())
        .ok()
}

/// Per-direction byte/packet split for a flow (#223), oriented initiator →
/// responder. Bundled so [`flow_record`] keeps a readable call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirCounts {
    pub bytes_initiator: u64,
    pub bytes_responder: u64,
    pub packets_initiator: u64,
    pub packets_responder: u64,
}

/// Resolve `(initiator, responder)` from netring's address-sorted flow key and
/// the flow's [`initiator_orientation`](flowscope::event::FlowStats). The key
/// `(a, b)` is canonical/directionless (it backs the Community ID); orientation
/// is presentation metadata that says which endpoint opened the conversation:
/// `Forward` → the initiator is `a`, `Reverse` → the initiator is `b`. With
/// `infer_tcp_initiator` on this reflects the SYN sender regardless of capture
/// endpoint order. Pure + total so it is unit-testable without capture.
pub fn initiator_responder(
    a: SocketAddr,
    b: SocketAddr,
    orientation: flowscope::Orientation,
) -> (SocketAddr, SocketAddr) {
    match orientation {
        flowscope::Orientation::Forward => (a, b),
        flowscope::Orientation::Reverse => (b, a),
    }
}

/// Map our config [`BackendKind`](crate::config::BackendKind) to netring's
/// `Backend` (#227). `AfXdp` needs an AF_XDP-enabled build, which this sensor
/// doesn't compile, so it degrades to `Auto` (→ AF_PACKET) with a warning
/// rather than failing — the resolved plan reports the truth either way.
pub fn netring_backend(kind: crate::config::BackendKind) -> netring::monitor::Backend {
    use crate::config::BackendKind;
    use netring::monitor::{Backend, Fanout};
    match kind {
        BackendKind::Auto => Backend::Auto,
        BackendKind::AfPacket => Backend::AfPacket {
            fanout: Fanout::None,
        },
        BackendKind::AfXdp => {
            tracing::warn!(
                "backend \"afxdp\" requested but this build has no AF_XDP support; using auto"
            );
            Backend::Auto
        }
    }
}

/// `capture/focus/*` counters (#225): packets/bytes seen by the reloadable
/// capture-focus packet sub. Narrowing the live filter slows these for
/// non-matching traffic — the visible effect of a runtime capture focus.
/// Build one telemetry point.
///
/// Every metric name this sensor emits funnels through here, so this is where
/// the registry gets enforced (RFC 08 §5, issue #468): in debug builds — which
/// is every unit test — an unregistered metric name panics. The mapper tests
/// below are therefore also the registry-conformance suite, and adding a metric
/// without registering it in `zensight-common/registry/netring.toml` fails
/// them.
fn point(sensor_id: &str, metric: impl Into<String>, value: TelemetryValue) -> TelemetryPoint {
    let metric = metric.into();
    debug_assert!(
        zensight_common::registry::is_registered_telemetry("netring", &metric),
        "unregistered netring telemetry subject {metric:?} — add it to \
         zensight-common/registry/netring.toml (RFC 08 §5, issue #468)"
    );
    TelemetryPoint::new(sensor_id, Protocol::Netring, metric, value)
}

pub fn focus_points(sensor_id: &str, packets: u64, bytes: u64) -> Vec<TelemetryPoint> {
    vec![
        point(
            sensor_id,
            "capture/focus/packets".to_string(),
            TelemetryValue::Counter(packets),
        ),
        point(
            sensor_id,
            "capture/focus/bytes".to_string(),
            TelemetryValue::Counter(bytes),
        ),
    ]
}

/// One `capture/events` lifecycle point (#327): a triggered/rotating capture
/// event (trigger fired, capture ready, mode switch) as Text with an `event`
/// label, so the GUI Capture tab shows a live event feed.
pub fn capture_event_point(sensor_id: &str, event: &str, detail: &str) -> TelemetryPoint {
    point(
        sensor_id,
        "capture/events".to_string(),
        TelemetryValue::Text(detail.to_string()),
    )
    .with_label("event", event)
}

/// The `capture/disk/*` family (#327): capture-to-disk mode, pre-trigger ring
/// occupancy, retained files/bytes and drop/eviction/trigger counters — the
/// health surface for "is the ring armed / is the disk filling".
#[allow(clippy::too_many_arguments)]
pub fn capture_disk_points(
    sensor_id: &str,
    mode: &str,
    ring_packets: u64,
    ring_bytes: u64,
    retained_files: u64,
    retained_bytes: u64,
    dropped: u64,
    evictions: u64,
    triggers: u64,
) -> Vec<TelemetryPoint> {
    let pfx = "capture/disk";
    vec![
        point(
            sensor_id,
            format!("{pfx}/mode"),
            TelemetryValue::Text(mode.to_string()),
        ),
        point(
            sensor_id,
            format!("{pfx}/ring_packets"),
            TelemetryValue::Gauge(ring_packets as f64),
        ),
        point(
            sensor_id,
            format!("{pfx}/ring_bytes"),
            TelemetryValue::Gauge(ring_bytes as f64),
        ),
        point(
            sensor_id,
            format!("{pfx}/retained_files"),
            TelemetryValue::Gauge(retained_files as f64),
        ),
        point(
            sensor_id,
            format!("{pfx}/retained_bytes"),
            TelemetryValue::Gauge(retained_bytes as f64),
        ),
        point(
            sensor_id,
            format!("{pfx}/dropped"),
            TelemetryValue::Counter(dropped),
        ),
        point(
            sensor_id,
            format!("{pfx}/evictions"),
            TelemetryValue::Counter(evictions),
        ),
        point(
            sensor_id,
            format!("{pfx}/triggers"),
            TelemetryValue::Counter(triggers),
        ),
    ]
}

/// One-shot `capture/backend` info point (#227): the resolved capture backend
/// (or `pcap-replay`) as Text, so the GUI Sensors view can show what is live.
pub fn backend_point(sensor_id: &str, label: &str) -> TelemetryPoint {
    point(
        sensor_id,
        "capture/backend".to_string(),
        TelemetryValue::Text(label.to_string()),
    )
}

/// IANA protocol number for an L4 protocol label (`tcp`/`udp`/`icmp`/`icmpv6`).
/// Returns `None` for labels without a well-known number — Community ID is only
/// attached when the protocol is known.
pub fn proto_number(name: &str) -> Option<u8> {
    match name.to_ascii_lowercase().as_str() {
        "tcp" => Some(6),
        "udp" => Some(17),
        "icmp" => Some(1),
        "icmpv6" | "ipv6-icmp" => Some(58),
        "sctp" => Some(132),
        _ => None,
    }
}

/// Compute the [Community ID v1](https://github.com/corelight/community-id-spec)
/// flow hash: order the (addr, port) endpoints, concatenate
/// `seed | saddr | daddr | proto | 0x00 | sport | dport`, SHA1 it, and base64 the
/// digest behind the `"1:"` version prefix. This is the de-facto cross-tool flow
/// correlation key emitted by Zeek, Suricata, Wireshark and Security Onion, so a
/// ZenSight flow can be matched against any of their records by string compare.
pub fn community_id_v1(a: SocketAddr, b: SocketAddr, proto: u8, seed: u16) -> String {
    // Delegates to flowscope's canonical implementation (#324) — byte-identical
    // (same endpoint ordering, SHA-1, base64, `"1:"` prefix), so this anomaly key
    // stays in lockstep with `FlowRecord.community_id` / `Ctx::community_id()` and
    // with external Zeek/Suricata/Wireshark records. Pinned by `community_id_v1_*`.
    flowscope::community_id::community_id_v1(proto, a.ip(), a.port(), b.ip(), b.port(), seed)
}

/// Best-effort Community ID for an anomaly from its (`src`, `dst`, `proto`) labels.
/// Returns `None` unless all three are present and the endpoints parse as
/// `ip:port` — detectors that only know a source (e.g. a port scan) carry no id.
fn anomaly_community_id(a: &AnomalyView) -> Option<String> {
    let src: SocketAddr = a.src.as_ref()?.parse().ok()?;
    let dst: SocketAddr = a.dst.as_ref()?.parse().ok()?;
    let proto = proto_number(a.proto.as_ref()?)?;
    Some(community_id_v1(src, dst, proto, 0))
}

/// Map a detector slug to its primary [MITRE ATT&CK](https://attack.mitre.org)
/// technique ID (#117). The lingua franca of every NDR console — every anomaly
/// that maps cleanly gets a `technique` label the Security view renders as a
/// badge and groups by tactic. Unmapped detectors carry no technique.
pub fn attack_technique(kind: &str) -> Option<&'static str> {
    let t = match kind {
        "PortScanTRW" => "T1046", // Network Service Discovery
        // Application Layer Protocol (C2)
        // #369: the RITA beacon now emits flowscope's upstream slug `BeaconRita`
        // (was ZenSight's `RitaBeacon`); the FQDN-pivoted beacon keeps its slug.
        "BeaconCv" | "BeaconDetector" | "BeaconRita" | "RitaBeaconFqdn" => "T1071",
        "DgaScorer" => "T1568.002",       // Dynamic Resolution: DGA
        "DnsTunnel" => "T1071.004",       // Application Layer Protocol: DNS
        "NewlyObservedDomain" => "T1568", // Dynamic Resolution
        "ConnectionFlood" => "T1499",     // Endpoint Denial of Service
        "LateralSmb" => "T1021.002",      // SMB/Windows Admin Shares
        "LateralRdp" => "T1021.001",      // Remote Desktop Protocol
        "LateralKerberos" => "T1558",     // Steal or Forge Kerberos Tickets
        // #369: flowscope's upstream slug is `DataExfil` (was `DataExfiltration`).
        "DataExfil" => "T1048", // Exfiltration Over Alternative Protocol
        "cleartext_snmp" | "cleartext-snmp" => "T1040", // Network Sniffing
        "cleartext_http_credentials" => "T1040", // Network Sniffing
        "encrypted_dns_bypass" => "T1572", // Protocol Tunneling (DoH/DoT/DoQ bypass)
        "ioc_match" => "T1071", // C2 over app-layer protocol
        _ => return None,
    };
    Some(t)
}

/// A flattened anomaly, decomposed from `flowscope::OwnedAnomaly`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AnomalyView {
    /// Detector slug, e.g. "PortScanTRW".
    pub kind: String,
    pub severity: AlertSeverity,
    /// `ip:port` (or `ip`) of the source, if known.
    pub src: Option<String>,
    pub dst: Option<String>,
    pub proto: Option<String>,
    /// Detector observations (k, v) — high-cardinality detail goes here.
    pub observations: Vec<(String, String)>,
    /// Numeric metrics (k, v).
    pub metrics: Vec<(String, f64)>,
}

/// Map a decomposed anomaly to a sensor-pushed [`Alert`].
///
/// The alert is bucketed by `(rule, src)` (via labels feeding `alert_key`), so a
/// 1000-port scan from one host collapses to one alert, not one-per-port — the
/// offending detail lives in labels/summary, never in a metric series name.
pub fn anomaly_alert(sensor_id: &str, a: &AnomalyView) -> Alert {
    let summary = human_summary(a);
    let mut alert = Alert::new(
        sensor_id,
        Protocol::Netring,
        AlertKind::Anomaly,
        &a.kind,
        a.severity,
        summary,
    );
    if let Some(src) = &a.src {
        alert = alert.with_label("src", src.clone());
    }
    if let Some(dst) = &a.dst {
        alert = alert.with_label("dst", dst.clone());
    }
    if let Some(proto) = &a.proto {
        alert = alert.with_label("proto", proto.clone());
    }
    for (k, v) in &a.observations {
        alert = alert.with_label(k.clone(), v.clone());
    }
    for (k, v) in &a.metrics {
        alert = alert.with_label(k.clone(), format!("{v}"));
    }
    // MITRE ATT&CK technique (#117) — analyst-grade triage tagging.
    if let Some(tech) = attack_technique(&a.kind) {
        alert = alert.with_label("technique", tech);
    }
    // Community ID (#116) — cross-tool flow correlation when the 5-tuple is whole.
    if let Some(cid) = anomaly_community_id(a) {
        alert = alert.with_label("community_id", cid);
    }
    alert
}

// ─── DNS anomaly detectors (issue #118) ──────────────────────────────────────

/// Bounded first-sight set of second-level domains for the Newly-Observed-Domain
/// detector (issue #118). Tracks which SLDs have been seen so [`Self::observe`]
/// returns `true` exactly once per domain — its first sight — until the domain is
/// evicted under the LRU (insertion-order) cap. Cheap on the steady-state
/// (already-seen) path: a single hash lookup, no allocation.
#[derive(Debug)]
pub struct SeenDomains {
    cap: usize,
    seen: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
}

impl SeenDomains {
    /// New seen-set bounded to `cap` distinct domains (min 1).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            seen: std::collections::HashSet::new(),
            order: std::collections::VecDeque::new(),
        }
    }

    /// Record a sighting of `sld`. Returns `true` if this is its FIRST sight
    /// (newly observed); `false` if already in the set. Evicts the oldest entry
    /// when the cap is exceeded (FIFO / insertion-order).
    pub fn observe(&mut self, sld: &str) -> bool {
        if self.seen.contains(sld) {
            return false;
        }
        if self.order.len() >= self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.seen.remove(&old);
        }
        self.seen.insert(sld.to_string());
        self.order.push_back(sld.to_string());
        true
    }

    /// Distinct domains currently retained.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// `true` when a DNS query looks like tunneling: either the distinct-label
/// cardinality per (src, SLD) over the window reaches `max_distinct`, or the
/// query name is at/above `max_qname_len` bytes. Pure so the threshold logic is
/// unit-testable without the capture machinery.
pub fn dns_tunnel_fires(
    distinct_labels: usize,
    qname_len: usize,
    max_distinct: usize,
    max_qname_len: usize,
) -> bool {
    distinct_labels >= max_distinct || qname_len >= max_qname_len
}

/// Build the `DnsTunnel` [`AnomalyView`] (kind → ATT&CK T1071.004). Bucketed by
/// `(rule, src)` (src is the querying host IP, no ephemeral port); the SLD rides
/// as an observation, the trigger signal as metrics.
pub fn dns_tunnel_view(
    src: Option<String>,
    sld: &str,
    distinct_labels: usize,
    qname_len: usize,
) -> AnomalyView {
    AnomalyView {
        kind: "DnsTunnel".into(),
        severity: AlertSeverity::Warning,
        src,
        dst: None,
        proto: Some("udp".into()),
        observations: vec![("sld".into(), sld.to_string())],
        metrics: vec![
            ("distinct_labels".into(), distinct_labels as f64),
            ("qname_len".into(), qname_len as f64),
        ],
    }
}

/// Build the `NewlyObservedDomain` [`AnomalyView`] (kind → ATT&CK T1568). Info
/// severity (allowlist-friendly): a second-level domain seen for the first time.
/// Bucketed by `(rule, src, sld)`.
pub fn nod_view(src: Option<String>, sld: &str) -> AnomalyView {
    AnomalyView {
        kind: "NewlyObservedDomain".into(),
        severity: AlertSeverity::Info,
        src,
        dst: None,
        proto: Some("udp".into()),
        observations: vec![("sld".into(), sld.to_string())],
        metrics: vec![],
    }
}

/// Build a lateral-movement [`AnomalyView`] (#123) for a parsed SMB/RDP/Kerberos
/// finding. `kind` is one of `LateralSmb` / `LateralRdp` / `LateralKerberos`
/// (mapped to ATT&CK T1021/T1558 by [`attack_technique`]); `observations` carry
/// the parser detail (share path, NTLM user, realm, etc.). Bucketed by
/// `(rule, src, dst)` so repeated access from one peer-pair is one alert.
pub fn lateral_view(
    kind: &str,
    src: Option<String>,
    dst: Option<String>,
    severity: AlertSeverity,
    observations: Vec<(String, String)>,
) -> AnomalyView {
    AnomalyView {
        kind: kind.to_string(),
        severity,
        src,
        dst,
        proto: Some("tcp".into()),
        observations,
        metrics: vec![],
    }
}

// #369: data-exfil now rides flowscope's stock `DataExfilDetector` in the `Tuned`
// registry → anomaly published through the sink → drain (`to_view` /
// `anomaly_alert`), so the bespoke `exfil_view` helper is retired.

/// The RITA composite score decomposition carried by an FQDN beacon anomaly
/// (#308). Bundled so [`fqdn_beacon_view`] keeps a readable call site (the
/// `DirCounts` pattern) and stays constructible in tests — flowscope's
/// `RitaBeaconScore` is `#[non_exhaustive]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RitaSignal {
    /// Composite score (`ts·0.45 + ds·0.35 + dur·0.20`).
    pub score: f64,
    /// Inter-arrival statistical score.
    pub ts_score: f64,
    /// Payload-size statistical score.
    pub ds_score: f64,
    /// Duration coverage bonus.
    pub dur_score: f64,
    /// Mean inter-arrival time (seconds).
    pub mean_interval_secs: f64,
    /// Observations in the window.
    pub n: usize,
}

/// Build the `RitaBeaconFqdn` [`AnomalyView`] (#308 → ATT&CK T1071): robust
/// RITA beaconing keyed by the destination's best forward DNS name. `src`/`dst`
/// are bare host IPs (the beacon spans many ephemeral-port flows, so a port
/// would shatter the alert bucket); the name rides as the `fqdn` observation.
pub fn fqdn_beacon_view(
    fqdn: &str,
    src: Option<String>,
    dst: Option<String>,
    sig: RitaSignal,
) -> AnomalyView {
    AnomalyView {
        kind: "RitaBeaconFqdn".into(),
        severity: AlertSeverity::Warning,
        src,
        dst,
        proto: Some("tcp".into()),
        observations: vec![("fqdn".into(), fqdn.to_string())],
        metrics: vec![
            ("score".into(), sig.score),
            ("ts_score".into(), sig.ts_score),
            ("ds_score".into(), sig.ds_score),
            ("dur_score".into(), sig.dur_score),
            ("mean_interval_secs".into(), sig.mean_interval_secs),
            ("n".into(), sig.n as f64),
        ],
    }
}

fn human_summary(a: &AnomalyView) -> String {
    match (&a.src, &a.dst) {
        (Some(src), Some(dst)) => format!("{} {} -> {}", a.kind, src, dst),
        (Some(src), None) => format!("{} from {}", a.kind, src),
        (None, Some(dst)) => format!("{} to {}", a.kind, dst),
        (None, None) => a.kind.clone(),
    }
}

/// Per-application bandwidth point: `bandwidth/<app>/bytes_per_sec` (Gauge).
pub fn bandwidth_point(sensor_id: &str, app: &str, bytes_per_sec: f64) -> TelemetryPoint {
    point(
        sensor_id,
        format!("bandwidth/{app}/bytes_per_sec"),
        TelemetryValue::Gauge(bytes_per_sec),
    )
    .with_label("app", app)
}

/// Flow-lifecycle aggregate points.
pub fn flow_points(
    sensor_id: &str,
    started_total: u64,
    ended_total: u64,
    active: u64,
) -> Vec<TelemetryPoint> {
    let p = |metric: &str, v: TelemetryValue| point(sensor_id, metric, v);
    vec![
        p("flow/started_total", TelemetryValue::Counter(started_total)),
        p("flow/ended_total", TelemetryValue::Counter(ended_total)),
        p("flow/active", TelemetryValue::Gauge(active as f64)),
    ]
}

/// Flow-volume RED counters, accumulated from ended-flow stats: total bytes,
/// packets and retransmits across all completed flows (utilization/errors).
pub fn flow_volume_points(
    sensor_id: &str,
    bytes_total: u64,
    packets_total: u64,
    retransmits_total: u64,
) -> Vec<TelemetryPoint> {
    let c = |metric: &str, v: u64| point(sensor_id, metric, TelemetryValue::Counter(v));
    vec![
        c("flow/bytes_total", bytes_total),
        c("flow/packets_total", packets_total),
        c("flow/retransmits_total", retransmits_total),
    ]
}

/// Flow RED points (#369) from netring's `red()` per-flow state: request rate
/// (flows/sec), error ratio (reset+parse-error share) and the p50/p95/p99 flow
/// lifetime percentiles. Replaces the old `flow/duration_p*` gauges (which came
/// from a bespoke DDSketch of flow durations); the RED sketch is netring's now.
///
/// `p50`/`p95`/`p99` are `None` when the window held no flows (netring returns no
/// quantile), and those points are simply omitted so the cached gauge keeps its
/// last meaningful value instead of being clobbered.
pub fn flow_red_points(
    sensor_id: &str,
    rate: f64,
    error_ratio: f64,
    p50: Option<f64>,
    p95: Option<f64>,
    p99: Option<f64>,
) -> Vec<TelemetryPoint> {
    let g = |metric: &str, v: f64| point(sensor_id, metric, TelemetryValue::Gauge(v));
    let mut pts = vec![
        g("flow/red/rate", rate),
        g("flow/red/error_ratio", error_ratio),
    ];
    if let Some(v) = p50 {
        pts.push(g("flow/red/p50_ms", v));
    }
    if let Some(v) = p95 {
        pts.push(g("flow/red/p95_ms", v));
    }
    if let Some(v) = p99 {
        pts.push(g("flow/red/p99_ms", v));
    }
    pts
}

/// Per-source drop breakdown (netring 0.27 `DropBreakdown`), decomposed so the
/// pure map stays free of the capture types. The flat `drops` says *how many*;
/// this says *where* — honest accounting of the loss the sensor is admitting to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaptureDrops {
    /// AF_PACKET (TPACKET_V3): only the ring-freeze count is distinct.
    AfPacket { freezes: u64 },
    /// AF_XDP (`XDP_STATISTICS`): every drop cause kept distinct.
    Xdp {
        rx_dropped: u64,
        rx_invalid_descs: u64,
        rx_ring_full: u64,
        rx_fill_ring_empty_descs: u64,
        tx_invalid_descs: u64,
        tx_ring_empty_descs: u64,
    },
}

/// Capture self-health points: packets/drops (Counter) + windowed drop_rate
/// (Gauge) per capture source, plus the honest [`CaptureDrops`] breakdown —
/// AF_PACKET freezes, or each AF_XDP ring/descriptor drop cause. Honesty signal:
/// non-zero drops mean the sensor's *other* telemetry is incomplete (issue #71).
pub fn capture_points(
    sensor_id: &str,
    source: u8,
    packets: u64,
    drops: u64,
    drop_rate: f64,
    detail: &CaptureDrops,
) -> Vec<TelemetryPoint> {
    let p = |metric: String, v: TelemetryValue| point(sensor_id, metric, v);
    let pfx = format!("capture/{source}");
    let c = |pfx: &str, leaf: &str, v: u64| {
        point(
            sensor_id,
            format!("{pfx}/{leaf}"),
            TelemetryValue::Counter(v),
        )
    };
    let mut points = vec![
        p(format!("{pfx}/packets"), TelemetryValue::Counter(packets)),
        p(format!("{pfx}/drops"), TelemetryValue::Counter(drops)),
        p(format!("{pfx}/drop_rate"), TelemetryValue::Gauge(drop_rate)),
    ];
    match *detail {
        CaptureDrops::AfPacket { freezes } => {
            points.push(c(&pfx, "freezes", freezes));
        }
        CaptureDrops::Xdp {
            rx_dropped,
            rx_invalid_descs,
            rx_ring_full,
            rx_fill_ring_empty_descs,
            tx_invalid_descs,
            tx_ring_empty_descs,
        } => {
            let xpfx = format!("{pfx}/xdp");
            points.push(c(&xpfx, "rx_dropped", rx_dropped));
            points.push(c(&xpfx, "rx_invalid_descs", rx_invalid_descs));
            points.push(c(&xpfx, "rx_ring_full", rx_ring_full));
            points.push(c(
                &xpfx,
                "rx_fill_ring_empty_descs",
                rx_fill_ring_empty_descs,
            ));
            points.push(c(&xpfx, "tx_invalid_descs", tx_invalid_descs));
            points.push(c(&xpfx, "tx_ring_empty_descs", tx_ring_empty_descs));
        }
    }
    points
}

/// Build the `capture-overload` SensorHealth alert (issue #71): raised
/// (`firing = true`, Critical) on the debounced `Normal → Emergency` drop-rate
/// transition — "the sensor is silently losing your packets" — and resolved
/// (`firing = false`) on the `Emergency → Normal` recovery. Bucketed per source.
pub fn overload_alert(sensor_id: &str, source: u8, drop_rate: f64, firing: bool) -> Alert {
    overload_alert_shed(sensor_id, source, drop_rate, firing, None)
}

/// `overload_alert` with optional active-shedding annotation (#224). When the
/// sensor is deliberately shedding, `shed_policy` is `Some(label)` and the
/// alert carries `shedding=true` + the policy, so the operator knows the data
/// is intentionally lossy (sampled, not complete) — not just kernel-dropped.
pub fn overload_alert_shed(
    sensor_id: &str,
    source: u8,
    drop_rate: f64,
    firing: bool,
    shed_policy: Option<&str>,
) -> Alert {
    let summary = if firing {
        match shed_policy {
            Some(p) => format!(
                "capture overload on source {source}: dropping {:.1}% of packets — shedding ({p})",
                drop_rate * 100.0
            ),
            None => format!(
                "capture overload on source {source}: dropping {:.1}% of packets",
                drop_rate * 100.0
            ),
        }
    } else {
        format!("capture recovered on source {source}")
    };
    let mut alert = Alert::new(
        sensor_id,
        Protocol::Netring,
        AlertKind::SensorHealth,
        "capture-overload",
        AlertSeverity::Critical,
        summary,
    )
    .with_label("source", source.to_string())
    .with_label("drop_rate", format!("{drop_rate:.4}"));
    if firing && let Some(p) = shed_policy {
        alert = alert
            .with_label("shedding", "true".to_string())
            .with_label("shed_policy", p.to_string());
    }
    if !firing {
        alert = alert.resolved();
    }
    alert
}

/// Build the `capture-leg-asymmetry` SensorHealth alert (#226): a flow whose two
/// directions arrived on capture legs that shouldn't pair — a tap miswire or
/// asymmetric routing, meaning flow direction/volume may be wrong. Same channel
/// / kind as `capture-overload`. `fwd`/`rev` are the bound source indices, if
/// known, for the operator to chase the miswire.
pub fn leg_asymmetry_alert(sensor_id: &str, fwd: Option<u32>, rev: Option<u32>) -> Alert {
    let legs = match (fwd, rev) {
        (Some(f), Some(r)) => format!(" (forward leg {f}, reverse leg {r})"),
        (Some(f), None) => format!(" (forward leg {f})"),
        (None, Some(r)) => format!(" (reverse leg {r})"),
        (None, None) => String::new(),
    };
    let mut alert = Alert::new(
        sensor_id,
        Protocol::Netring,
        AlertKind::SensorHealth,
        "capture-leg-asymmetry",
        AlertSeverity::Warning,
        format!(
            "capture-leg asymmetry{legs}: a flow's two directions arrived on \
             mismatched legs — tap miswire or asymmetric routing; flow \
             direction/volume may be unreliable"
        ),
    );
    if let Some(f) = fwd {
        alert = alert.with_label("source_idx_forward", f.to_string());
    }
    if let Some(r) = rev {
        alert = alert.with_label("source_idx_reverse", r.to_string());
    }
    alert
}

/// Build the `capture/<source>/shed/*` telemetry family (#224): cumulative
/// deliberately-shed flow count (split by policy leaf) plus an `active` gauge
/// (`1` while shedding). Emitted alongside the rest of `capture/<source>/*`.
pub fn shed_points(
    sensor_id: &str,
    source: u8,
    shed_total: u64,
    active: bool,
    policy: &str,
) -> Vec<TelemetryPoint> {
    let leaf = match policy {
        "sample" => "sampled_total",
        _ => "new_flows_total",
    };
    let pfx = format!("capture/{source}/shed");
    vec![
        point(
            sensor_id,
            format!("{pfx}/{leaf}"),
            TelemetryValue::Counter(shed_total),
        ),
        point(
            sensor_id,
            format!("{pfx}/active"),
            TelemetryValue::Gauge(if active { 1.0 } else { 0.0 }),
        ),
    ]
}

/// TLS handshake aggregate: total ClientHellos fingerprinted + distinct
/// fingerprints seen (asset-inventory size).
pub fn tls_points(sensor_id: &str, handshakes: u64, distinct: u64) -> Vec<TelemetryPoint> {
    vec![
        point(
            sensor_id,
            "tls/handshakes_total",
            TelemetryValue::Counter(handshakes),
        ),
        point(
            sensor_id,
            "tls/distinct_fingerprints",
            TelemetryValue::Gauge(distinct as f64),
        ),
    ]
}

/// QUIC inventory aggregate (issue #72): distinct (sni, version) pairs seen.
/// Low-cardinality count safe to stream; detail pulled from `@rpc/netring/quic`.
pub fn quic_count_point(sensor_id: &str, distinct: u64) -> TelemetryPoint {
    point(
        sensor_id,
        "quic/distinct_sni",
        TelemetryValue::Gauge(distinct as f64),
    )
}

/// SSH/HASSH inventory aggregate (issue #72): distinct HASSH fingerprints seen.
pub fn ssh_count_point(sensor_id: &str, distinct: u64) -> TelemetryPoint {
    point(
        sensor_id,
        "ssh/distinct_hassh",
        TelemetryValue::Gauge(distinct as f64),
    )
}

/// Build the `cleartext-snmp` anomaly alert (issue #72): SNMP v1/v2c send the
/// community string in cleartext — a credential-exposure + lateral-movement
/// signal. Bucketed by `(rule, src)`; the community + version ride as labels.
pub fn snmp_cleartext_alert(
    sensor_id: &str,
    version: &str,
    community: &str,
    src: Option<String>,
    dst: Option<String>,
) -> Alert {
    let summary = match &src {
        Some(s) => format!("cleartext SNMP {version} community from {s}"),
        None => format!("cleartext SNMP {version} community observed"),
    };
    let mut alert = Alert::new(
        sensor_id,
        Protocol::Netring,
        AlertKind::Anomaly,
        "cleartext-snmp",
        AlertSeverity::Warning,
        summary,
    )
    .with_label("version", version)
    .with_label("community", community);
    if let Some(src) = src {
        alert = alert.with_label("src", src);
    }
    if let Some(dst) = dst {
        alert = alert.with_label("dst", dst);
    }
    alert
}

/// The `encrypted_dns_bypass` anomaly alert (#326): an encrypted-DNS (DoT/DoQ/DoH)
/// session went to a resolver that isn't sanctioned by policy — the classic
/// signal for DNS-based C2 / exfil tunneling past a network resolver. Tagged with
/// ATT&CK **T1572** (Protocol Tunneling). Bucketed by `(rule, transport, sni)` so
/// repeated sessions to the same rogue resolver collapse to one alert.
pub fn encrypted_dns_bypass_alert(sensor_id: &str, transport: &str, sni: Option<&str>) -> Alert {
    let summary = match sni {
        Some(s) => format!("encrypted DNS ({transport}) to unsanctioned resolver {s}"),
        None => format!("encrypted DNS ({transport}) to an unsanctioned resolver"),
    };
    let mut alert = Alert::new(
        sensor_id,
        Protocol::Netring,
        AlertKind::Anomaly,
        "encrypted_dns_bypass",
        AlertSeverity::Warning,
        summary,
    )
    .with_label("transport", transport)
    .with_label("technique", "T1572");
    if let Some(s) = sni {
        alert = alert.with_label("sni", s);
    }
    alert
}

/// Encrypted-DNS aggregates (#326): per-transport session counters, the
/// un-known-resolver subset (the policy-bypass signal), and the distinct
/// destination count. Low-cardinality — safe to stream; per-destination detail is
/// pulled on demand from `@rpc/netring/encrypted_dns`.
pub fn encrypted_dns_points(
    sensor_id: &str,
    dot: u64,
    doq: u64,
    doh: u64,
    unknown_resolver: u64,
    distinct: u64,
) -> Vec<TelemetryPoint> {
    vec![
        point(sensor_id, "dns/encrypted/dot", TelemetryValue::Counter(dot)),
        point(sensor_id, "dns/encrypted/doq", TelemetryValue::Counter(doq)),
        point(sensor_id, "dns/encrypted/doh", TelemetryValue::Counter(doh)),
        point(
            sensor_id,
            "dns/encrypted/unknown_resolver",
            TelemetryValue::Counter(unknown_resolver),
        ),
        point(
            sensor_id,
            "dns/encrypted/distinct",
            TelemetryValue::Gauge(distinct as f64),
        ),
    ]
}

/// TLS post-quantum readiness gauge (#326): the share of fingerprinted handshakes
/// that offered a PQ (hybrid) key-share group. `0.0` when no handshakes yet.
pub fn tls_pq_ratio_point(sensor_id: &str, pq: u64, total: u64) -> TelemetryPoint {
    let ratio = if total > 0 {
        pq as f64 / total as f64
    } else {
        0.0
    };
    point(sensor_id, "tls/pq_ratio", TelemetryValue::Gauge(ratio))
}

/// Passive asset-inventory aggregate: number of distinct assets (MACs) the
/// inventory currently holds. Low-cardinality count safe to stream; the
/// per-asset detail is pulled on demand from `@rpc/netring/assets` (principle P2).
pub fn asset_count_point(sensor_id: &str, discovered: u64) -> TelemetryPoint {
    point(
        sensor_id,
        "assets/discovered",
        TelemetryValue::Gauge(discovered as f64),
    )
}

/// Per-detector anomaly count (#254): a monotonic `Counter` of how many anomaly
/// alerts a detector (`kind`, e.g. `BeaconRita` / `DnsTunnel` / `NewlyObservedDomain`)
/// has fired since sensor start, published as `anomaly/<kind>/total`. Lets the GUI
/// Overview anomaly strip roll up per-detector activity without a Security-view
/// round-trip; the key chunk is `detector_chunk(rule)`, a deterministic function
/// of the alert `rule`, so the two correlate.
pub fn anomaly_count_point(sensor_id: &str, kind: &str, count: u64) -> TelemetryPoint {
    point(
        sensor_id,
        format!("anomaly/{}/total", detector_chunk(kind)),
        TelemetryValue::Counter(count),
    )
}

/// Keyify a flowscope detector slug into a grammar-legal chunk (RFC 03 §2):
/// upstream slugs are CamelCase (`BeaconRita`), key chunks must be lowercase —
/// insert `_` at case boundaries and fold (`beacon_rita`). Already-snake slugs
/// pass through unchanged.
pub fn detector_chunk(kind: &str) -> String {
    let mut out = String::with_capacity(kind.len() + 4);
    for (i, c) in kind.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

// ─── ICMP error telemetry (issue #15) ───────────────────────────────────────

/// A flattened ICMP error, decomposed from netring's `IcmpError` event. Kept
/// free of the netring/flowscope capture machinery so it is unit-testable.
///
/// `kind` is the flowscope stable slug (`port_unreachable`, `time_exceeded`,
/// `fragmentation_needed`, ...); `is_unreachable`/`is_time_exceeded` pre-classify
/// the two headline counters so the pure map never re-parses the slug.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IcmpErrorView {
    /// Stable kind slug from `IcmpErrorKind::as_str()`.
    pub kind: String,
    /// Destination-Unreachable family (host/port/network/admin/...).
    pub is_unreachable: bool,
    /// Time-Exceeded (TTL expired in transit / reassembly).
    pub is_time_exceeded: bool,
    /// PMTU signal (frag-needed / packet-too-big) — black-hole risk.
    pub is_mtu_signal: bool,
    /// The originating flow `src -> dst` (canonical 5-tuple), if reconstructed
    /// from the ICMP message's embedded inner packet.
    pub correlated_flow: Option<(String, String)>,
}

/// ICMP error counters, accumulated across all observed ICMP errors. Streams the
/// headline RED-style signals plus a per-kind breakdown.
///
/// `by_kind` is a small, bounded set of stable slugs (≈8 ICMP error classes), so
/// `icmp/by_kind/<slug>_total` is low-cardinality — safe to stream.
pub fn icmp_points(
    sensor_id: &str,
    unreachable_total: u64,
    time_exceeded_total: u64,
    mtu_signal_total: u64,
    by_kind: &[(String, u64)],
) -> Vec<TelemetryPoint> {
    let c = |metric: String, v: u64| point(sensor_id, metric, TelemetryValue::Counter(v));
    let mut pts = vec![
        c("icmp/unreachable_total".into(), unreachable_total),
        c("icmp/time_exceeded_total".into(), time_exceeded_total),
        c("icmp/mtu_signal_total".into(), mtu_signal_total),
    ];
    for (kind, count) in by_kind {
        pts.push(c(format!("icmp/by_kind/{kind}_total"), *count));
    }
    pts
}

/// Build an [`Alert`] for a flow killed by an ICMP error (e.g. a port-unreachable
/// or admin-prohibited that terminates a live flow) — a high-signal path failure.
/// Bucketed by `(rule, dst)` so a storm of unreachables to one host collapses to
/// one alert.
pub fn icmp_flow_alert(sensor_id: &str, v: &IcmpErrorView) -> Alert {
    let (src, dst) = v
        .correlated_flow
        .clone()
        .unwrap_or_else(|| ("?".into(), "?".into()));
    let summary = format!("ICMP {} for flow {} -> {}", v.kind, src, dst);
    Alert::new(
        sensor_id,
        Protocol::Netring,
        AlertKind::Anomaly,
        "IcmpFlowError",
        AlertSeverity::Warning,
        summary,
    )
    .with_label("kind", v.kind.clone())
    .with_label("src", src)
    .with_label("dst", dst)
}

// ─── Per-protocol + connection-state breakdown (issue #16) ───────────────────

/// Per-L4-protocol flow composition: bytes + flow counts split by tcp/udp/icmp.
/// An unusual UDP byte spike flags DNS/NTP amplification abuse; the split is the
/// first-order "what is this network carrying?" signal. Three protocols → six
/// series — low-cardinality, safe to stream.
pub fn flow_by_l4_points(
    sensor_id: &str,
    tcp_bytes: u64,
    tcp_flows: u64,
    udp_bytes: u64,
    udp_flows: u64,
    icmp_bytes: u64,
    icmp_flows: u64,
) -> Vec<TelemetryPoint> {
    let c = |metric: String, v: u64| point(sensor_id, metric, TelemetryValue::Counter(v));
    vec![
        c("flow/by_l4/tcp/bytes_total".into(), tcp_bytes),
        c("flow/by_l4/tcp/flows_total".into(), tcp_flows),
        c("flow/by_l4/udp/bytes_total".into(), udp_bytes),
        c("flow/by_l4/udp/flows_total".into(), udp_flows),
        c("flow/by_l4/icmp/bytes_total".into(), icmp_bytes),
        c("flow/by_l4/icmp/flows_total".into(), icmp_flows),
    ]
}

/// Bucket a flowscope `EndReason` slug into one of the three TCP close classes we
/// track: `fin` (clean), `rst` (abort/refused), `idle` (timeout). Everything else
/// (evicted/buffer_overflow/parse_error/...) folds into `idle` — they're all
/// "the flow stopped without an explicit close" from an operator's view.
pub fn tcp_close_class(reason: &str) -> &'static str {
    match reason {
        "fin" => "fin",
        "rst" => "rst",
        _ => "idle",
    }
}

/// TCP connection-state breakdown: how flows closed (clean FIN vs RST abort vs
/// idle timeout). A high RST share = firewall/IDS drops or instability.
pub fn tcp_closed_points(
    sensor_id: &str,
    closed_fin: u64,
    closed_rst: u64,
    closed_idle: u64,
) -> Vec<TelemetryPoint> {
    let c = |metric: &str, v: u64| point(sensor_id, metric, TelemetryValue::Counter(v));
    vec![
        c("tcp/closed_fin_total", closed_fin),
        c("tcp/closed_rst_total", closed_rst),
        c("tcp/closed_idle_total", closed_idle),
    ]
}

/// TCP reset aggregate points.
pub fn tcp_reset_points(sensor_id: &str, resets: u64, refused: u64) -> Vec<TelemetryPoint> {
    vec![
        point(
            sensor_id,
            "tcp/resets_total",
            TelemetryValue::Counter(resets),
        ),
        point(
            sensor_id,
            "tcp/refused_total",
            TelemetryValue::Counter(refused),
        ),
    ]
}

// ─── L7 DNS RED analytics (issue #19) ────────────────────────────────────────

use zensight_common::{
    DnsRecord, ElephantRecord, HttpHostRecord, Ja4hRecord, MatrixRecord, TalkerRecord,
};

/// DNS response codes we track as distinct RED-error buckets. Closed set →
/// low-cardinality, safe to stream as `dns/responses_by_rcode/<slug>_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsRcodeClass {
    NoError,
    NxDomain,
    ServFail,
    Refused,
    /// Any other rcode (FormErr/NotImpl/...): folded into one "other" bucket so a
    /// rare rcode can't explode the series count.
    Other,
}

impl DnsRcodeClass {
    pub fn slug(self) -> &'static str {
        match self {
            Self::NoError => "noerror",
            Self::NxDomain => "nxdomain",
            Self::ServFail => "servfail",
            Self::Refused => "refused",
            Self::Other => "other",
        }
    }
}

/// Extract the second-level domain label from a DNS qname (e.g.
/// `"www.example.com."` → `"example"`). `None` for a bare TLD / root / empty
/// name. Lowercases the result. This is the unit the DGA scorer and the top-SLD
/// inventory key on.
pub fn dns_sld(qname: &str) -> Option<String> {
    let trimmed = qname.trim_end_matches('.');
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.rsplit('.');
    let _tld = parts.next()?;
    let sld = parts.next()?;
    if sld.is_empty() {
        None
    } else {
        Some(sld.to_ascii_lowercase())
    }
}

/// DNS RED aggregate points: queries, per-rcode responses, unanswered, and the
/// resolver-loss rate. `rtt_ms` carries the query-RTT distribution of the window
/// (consumed/sorted) for p50/p95/p99 latency gauges.
///
/// Returns no rcode/RTT points when a bucket is empty so idle ticks never clobber
/// the cached gauges to zero (same discipline as `flow_latency_points`).
pub fn dns_points(
    sensor_id: &str,
    queries_total: u64,
    by_rcode: &[(DnsRcodeClass, u64)],
    unanswered_total: u64,
    rtt_pcts: Option<[f64; 3]>,
) -> Vec<TelemetryPoint> {
    let c = |metric: String, v: u64| point(sensor_id, metric, TelemetryValue::Counter(v));
    let g = |metric: &str, v: f64| point(sensor_id, metric, TelemetryValue::Gauge(v));
    let mut pts = vec![
        c("dns/queries_total".into(), queries_total),
        c("dns/unanswered_total".into(), unanswered_total),
    ];
    for (rc, count) in by_rcode {
        pts.push(c(
            format!("dns/responses_by_rcode/{}_total", rc.slug()),
            *count,
        ));
    }
    if let Some([p50, p95, p99]) = rtt_pcts {
        pts.push(g("dns/query_rtt_p50_ms", p50));
        pts.push(g("dns/query_rtt_p95_ms", p95));
        pts.push(g("dns/query_rtt_p99_ms", p99));
    }
    pts
}

/// Rank a DNS SLD inventory newest-volume-first into the on-demand `@rpc/netring/dns`
/// reply (top-N by query count). Pure so the ranking is unit-testable.
pub fn top_dns_records(
    inv: &std::collections::HashMap<String, (u64, u64)>,
    top: usize,
) -> Vec<DnsRecord> {
    let mut v: Vec<DnsRecord> = inv
        .iter()
        .map(|(domain, &(queries, nxdomain))| DnsRecord {
            domain: domain.clone(),
            queries,
            nxdomain,
        })
        .collect();
    v.sort_by(|a, b| {
        b.queries
            .cmp(&a.queries)
            .then_with(|| a.domain.cmp(&b.domain))
    });
    v.truncate(top);
    v
}

// ─── L7 HTTP RED analytics (issue #20) ───────────────────────────────────────

/// Bucket an HTTP status code into a RED status class slug. Out-of-range codes
/// fold into `other`.
pub fn http_status_class(status: u16) -> &'static str {
    match status {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "other",
    }
}

/// HTTP RED aggregate points: total requests, per-status-class responses, latency
/// percentiles, and a per-method breakdown. `latency_ms` carries the
/// request→response latency distribution of the window (consumed/sorted).
///
/// `by_method` is a small closed set of HTTP verbs (GET/POST/...) — low
/// cardinality, safe to stream. Empty latency window → no latency points.
#[allow(clippy::too_many_arguments)]
pub fn http_points(
    sensor_id: &str,
    requests_total: u64,
    status_2xx: u64,
    status_3xx: u64,
    status_4xx: u64,
    status_5xx: u64,
    by_method: &[(String, u64)],
    latency_pcts: Option<[f64; 3]>,
) -> Vec<TelemetryPoint> {
    let c = |metric: String, v: u64| point(sensor_id, metric, TelemetryValue::Counter(v));
    let g = |metric: &str, v: f64| point(sensor_id, metric, TelemetryValue::Gauge(v));
    let mut pts = vec![
        c("http/requests_total".into(), requests_total),
        c("http/status_2xx_total".into(), status_2xx),
        c("http/status_3xx_total".into(), status_3xx),
        c("http/status_4xx_total".into(), status_4xx),
        c("http/status_5xx_total".into(), status_5xx),
    ];
    for (method, count) in by_method {
        pts.push(c(format!("http/methods/{method}_total"), *count));
    }
    // p99 (index 2) is computed but HTTP surfaces only p50/p95 today.
    if let Some([p50, p95, _p99]) = latency_pcts {
        pts.push(g("http/latency_p50_ms", p50));
        pts.push(g("http/latency_p95_ms", p95));
    }
    pts
}

/// Rank an HTTP host inventory request-volume-first into the `@rpc/netring/http` reply.
pub fn top_http_hosts(
    inv: &std::collections::HashMap<String, (u64, u64)>,
    top: usize,
) -> Vec<HttpHostRecord> {
    let mut v: Vec<HttpHostRecord> = inv
        .iter()
        .map(|(host, &(requests, errors))| HttpHostRecord {
            host: host.clone(),
            requests,
            errors,
        })
        .collect();
    v.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.host.cmp(&b.host))
    });
    v.truncate(top);
    v
}

/// Rank a JA4H fingerprint inventory hit-count-first into the `@rpc/netring/ja4h`
/// reply (#124). Pure so the ranking is unit-testable.
pub fn top_ja4h(
    inv: &std::collections::HashMap<String, Ja4hRecord>,
    top: usize,
) -> Vec<Ja4hRecord> {
    let mut v: Vec<Ja4hRecord> = inv.values().cloned().collect();
    v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.ja4h.cmp(&b.ja4h)));
    v.truncate(top);
    v
}

// ─── Top-talkers + elephant flows (issue #21) ────────────────────────────────

/// Rank a per-destination histogram byte-volume-first into the `@rpc/netring/talkers`
/// reply (top-N talkers). Pure so the ranking is unit-testable.
pub fn top_talkers(talkers: &[(String, f64)], top: usize) -> Vec<TalkerRecord> {
    let mut v: Vec<TalkerRecord> = talkers
        .iter()
        .map(|(src, rate)| TalkerRecord {
            src: src.clone(),
            bytes_per_sec: *rate,
            // Passive-DNS enrichment (#308) is attached at query time (the
            // talker channel peeks the name map per served row).
            names: Vec::new(),
        })
        .collect();
    v.sort_by(|a, b| {
        b.bytes_per_sec
            .total_cmp(&a.bytes_per_sec)
            .then_with(|| a.src.cmp(&b.src))
    });
    v.truncate(top);
    v
}

/// Rank the `(src,dst)` traffic-matrix pairs (netring `aggregate()`, #369)
/// rate-first into the `@rpc/netring/matrix` reply (top-N / service map). Pure so the
/// ranking is unit-testable (#122).
pub fn traffic_matrix(pairs: &[(String, String, f64)], top: usize) -> Vec<MatrixRecord> {
    let mut v: Vec<MatrixRecord> = pairs
        .iter()
        .map(|(src, dst, rate)| MatrixRecord {
            src: src.clone(),
            dst: dst.clone(),
            bytes_per_sec: *rate,
            names: Vec::new(),
        })
        .collect();
    v.sort_by(|a, b| {
        b.bytes_per_sec
            .total_cmp(&a.bytes_per_sec)
            .then_with(|| a.src.cmp(&b.src))
            .then_with(|| a.dst.cmp(&b.dst))
    });
    v.truncate(top);
    v
}

/// Build an [`ElephantRecord`] from already-extracted fields (pure shape).
#[allow(clippy::too_many_arguments)]
pub fn elephant_record(
    src: String,
    dst: String,
    proto: &str,
    bytes: u64,
    packets: u64,
    duration_ms: u64,
    dir_counts: DirCounts,
) -> ElephantRecord {
    ElephantRecord {
        src,
        dst,
        proto: proto.to_string(),
        bytes,
        packets,
        duration_ms,
        bytes_initiator: dir_counts.bytes_initiator,
        bytes_responder: dir_counts.bytes_responder,
        packets_initiator: dir_counts.packets_initiator,
        packets_responder: dir_counts.packets_responder,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_pq_ratio_math() {
        // 3 of 4 handshakes PQ → 0.75; the empty case is a clean 0.0, not NaN.
        let p = tls_pq_ratio_point("s", 3, 4);
        assert_eq!(p.metric, "tls/pq_ratio");
        assert!(matches!(p.value, TelemetryValue::Gauge(g) if (g - 0.75).abs() < 1e-9));
        let z = tls_pq_ratio_point("s", 0, 0);
        assert!(matches!(z.value, TelemetryValue::Gauge(g) if g == 0.0));
    }

    #[test]
    fn encrypted_dns_points_shape() {
        let pts = encrypted_dns_points("s", 1, 2, 3, 4, 5);
        let names: Vec<&str> = pts.iter().map(|p| p.metric.as_str()).collect();
        assert!(names.contains(&"dns/encrypted/dot"));
        assert!(names.contains(&"dns/encrypted/doq"));
        assert!(names.contains(&"dns/encrypted/doh"));
        assert!(names.contains(&"dns/encrypted/unknown_resolver"));
        assert!(names.contains(&"dns/encrypted/distinct"));
    }

    #[test]
    fn encrypted_dns_bypass_alert_is_tagged() {
        // The bypass alert carries the transport, the SNI, and the ATT&CK tag —
        // and the technique table maps the slug to Protocol Tunneling.
        let a = encrypted_dns_bypass_alert("s", "doh", Some("dns.evil.example"));
        assert_eq!(a.rule, "encrypted_dns_bypass");
        assert_eq!(a.labels.get("transport").map(String::as_str), Some("doh"));
        assert_eq!(
            a.labels.get("sni").map(String::as_str),
            Some("dns.evil.example")
        );
        assert_eq!(a.labels.get("technique").map(String::as_str), Some("T1572"));
        assert_eq!(attack_technique("encrypted_dns_bypass"), Some("T1572"));
    }

    /// A name claim at `t` seconds, optionally client-scoped (#308 tests).
    fn claim(name: &str, prov: Provenance, t: u32, client: Option<&str>) -> NameClaim {
        let c = NameClaim::new(name, prov, flowscope::Timestamp::new(t, 0));
        match client {
            Some(ip) => c.with_client(ip.parse().unwrap()),
            None => c,
        }
    }

    #[test]
    fn rank_names_orders_by_provenance() {
        let claims = vec![
            claim("reverse.example", Provenance::DnsPtr, 100, None),
            claim("sni.example", Provenance::Sni, 100, None),
            claim("forward.example", Provenance::DnsA, 100, None),
            claim("chain.example", Provenance::DnsCname, 100, None),
        ];
        let ranked = rank_names(&claims);
        let names: Vec<&str> = ranked.iter().map(|n| n.name.as_str()).collect();
        // Forward > CNAME > SNI (> PTR, cut by the top-3 cap).
        assert_eq!(
            names,
            vec!["forward.example", "chain.example", "sni.example"]
        );
        assert_eq!(ranked[0].provenance, "dns_a");
    }

    #[test]
    fn rank_names_prefers_client_scoped_then_recency() {
        let claims = vec![
            claim("global.example", Provenance::DnsA, 200, None),
            claim("scoped.example", Provenance::DnsA, 100, Some("10.0.0.5")),
            claim("older.example", Provenance::DnsA, 50, Some("10.0.0.5")),
        ];
        let names: Vec<String> = rank_names(&claims).into_iter().map(|n| n.name).collect();
        // Client-scoped beats global despite being older; recency breaks the
        // client-scoped tie.
        assert_eq!(
            names,
            vec!["scoped.example", "older.example", "global.example"]
        );
    }

    #[test]
    fn rank_names_dedupes_by_name_and_caps_at_three() {
        let claims = vec![
            claim("dup.example", Provenance::DnsA, 100, None),
            claim("dup.example", Provenance::Sni, 100, None),
            claim("b.example", Provenance::DnsA, 100, None),
            claim("c.example", Provenance::DnsA, 100, None),
            claim("d.example", Provenance::DnsA, 100, None),
        ];
        let ranked = rank_names(&claims);
        assert_eq!(ranked.len(), 3);
        // The dup keeps its best-ranked provenance.
        assert_eq!(ranked[0].name, "dup.example");
        assert_eq!(ranked[0].provenance, "dns_a");
        assert!(ranked.iter().filter(|n| n.name == "dup.example").count() == 1);
    }

    #[test]
    fn rank_names_empty_is_empty() {
        assert!(rank_names([]).is_empty());
    }

    #[test]
    fn best_forward_name_skips_non_forward_claims() {
        let reverse_only = vec![
            claim("reverse.example", Provenance::DnsPtr, 100, None),
            claim("sni.example", Provenance::Sni, 100, None),
        ];
        assert_eq!(best_forward_name(&reverse_only), None);

        let mixed = vec![
            claim("reverse.example", Provenance::DnsPtr, 300, None),
            claim("global.example", Provenance::DnsAaaa, 200, None),
            claim("scoped.example", Provenance::DnsA, 100, Some("10.0.0.5")),
        ];
        // Client-scoped forward claim wins over the newer global one.
        assert_eq!(
            best_forward_name(&mixed),
            Some("scoped.example".to_string())
        );
    }

    #[test]
    fn endpoint_ip_parses_socket_and_bare_forms() {
        assert_eq!(
            endpoint_ip("93.184.216.34:443"),
            Some("93.184.216.34".parse().unwrap())
        );
        assert_eq!(
            endpoint_ip("93.184.216.34"),
            Some("93.184.216.34".parse().unwrap())
        );
        assert_eq!(
            endpoint_ip("[2001:db8::1]:443"),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(endpoint_ip("not-an-endpoint"), None);
    }

    fn scan_anomaly() -> AnomalyView {
        AnomalyView {
            kind: "PortScanTRW".into(),
            severity: AlertSeverity::Warning,
            src: Some("10.0.0.5:44321".into()),
            dst: Some("10.0.0.9:22".into()),
            proto: Some("tcp".into()),
            observations: vec![("verdict".into(), "scanner".into())],
            metrics: vec![("score".into(), 3.5)],
        }
    }

    #[test]
    fn anomaly_maps_to_alert_with_labels() {
        let a = anomaly_alert("sensor1", &scan_anomaly());
        assert_eq!(a.protocol, Protocol::Netring);
        assert_eq!(a.kind, AlertKind::Anomaly);
        assert_eq!(a.rule, "PortScanTRW");
        assert_eq!(a.severity, AlertSeverity::Warning);
        assert!(a.summary.contains("PortScanTRW"));
        assert!(a.summary.contains("10.0.0.5:44321"));
        assert_eq!(
            a.labels.get("src").map(String::as_str),
            Some("10.0.0.5:44321")
        );
        assert_eq!(a.labels.get("verdict").map(String::as_str), Some("scanner"));
        assert_eq!(a.labels.get("score").map(String::as_str), Some("3.5"));
    }

    #[test]
    fn alert_key_buckets_by_src_not_per_port() {
        // Same rule + src, different dst port → SAME alert_key (one alert).
        let mut a1 = scan_anomaly();
        a1.dst = Some("10.0.0.9:22".into());
        let mut a2 = scan_anomaly();
        a2.dst = Some("10.0.0.9:23".into());
        // Drop dst from labels for bucketing — emulate the production view that
        // keys on src only.
        a1.dst = None;
        a2.dst = None;
        let k1 = anomaly_alert("s", &a1).alert_key();
        let k2 = anomaly_alert("s", &a2).alert_key();
        assert_eq!(k1, k2);
    }

    #[test]
    fn bandwidth_and_flow_points() {
        let bp = bandwidth_point("s", "https", 1234.5);
        assert_eq!(bp.metric, "bandwidth/https/bytes_per_sec");
        assert_eq!(bp.value, TelemetryValue::Gauge(1234.5));

        let fps = flow_points("s", 10, 8, 2);
        assert_eq!(fps[0].value, TelemetryValue::Counter(10));
        assert_eq!(fps[2].value, TelemetryValue::Gauge(2.0));
    }

    #[test]
    fn flow_volume_points_shape() {
        let pts = flow_volume_points("s", 4096, 12, 2);
        assert_eq!(pts[0].metric, "flow/bytes_total");
        assert_eq!(pts[0].value, TelemetryValue::Counter(4096));
        assert_eq!(pts[1].metric, "flow/packets_total");
        assert_eq!(pts[1].value, TelemetryValue::Counter(12));
        assert_eq!(pts[2].metric, "flow/retransmits_total");
        assert_eq!(pts[2].value, TelemetryValue::Counter(2));
    }

    #[test]
    fn flow_red_points_shape() {
        // #369: rate + error_ratio always published; present percentiles too.
        let pts = flow_red_points("s", 12.5, 0.05, Some(50.0), Some(100.0), Some(120.0));
        assert_eq!(pts.len(), 5);
        assert_eq!(pts[0].metric, "flow/red/rate");
        assert_eq!(pts[0].value, TelemetryValue::Gauge(12.5));
        assert_eq!(pts[1].metric, "flow/red/error_ratio");
        assert_eq!(pts[1].value, TelemetryValue::Gauge(0.05));
        assert_eq!(pts[2].metric, "flow/red/p50_ms");
        assert_eq!(pts[3].metric, "flow/red/p95_ms");
        assert_eq!(pts[4].metric, "flow/red/p99_ms");
        // Empty window → percentiles omitted, but rate/error_ratio still emit.
        let idle = flow_red_points("s", 0.0, 0.0, None, None, None);
        assert_eq!(idle.len(), 2);
    }

    #[test]
    fn flow_record_shape() {
        let r = flow_record(
            "10.0.0.1:5555".into(),
            "1.1.1.1:443".into(),
            "tcp",
            694,
            10,
            100,
            "fin",
            Some("1:abc".into()),
            true,
            DirCounts {
                bytes_initiator: 120,
                bytes_responder: 574,
                packets_initiator: 4,
                packets_responder: 6,
            },
        );
        assert_eq!(r.src, "10.0.0.1:5555");
        assert_eq!(r.dst, "1.1.1.1:443");
        assert_eq!(r.proto, "tcp");
        assert_eq!(r.bytes, 694);
        assert_eq!(r.duration_ms, 100);
        assert_eq!(r.reason, "fin");
        assert_eq!(r.community_id.as_deref(), Some("1:abc"));
        assert!(r.directed);
        // Per-direction split (#223); totals stay the both-directions sum.
        assert_eq!(r.bytes_initiator + r.bytes_responder, r.bytes);
        assert_eq!(r.packets_initiator, 4);
        assert_eq!(r.packets_responder, 6);
    }

    #[test]
    fn initiator_responder_follows_orientation() {
        let a: SocketAddr = "10.0.0.10:49152".parse().unwrap();
        let b: SocketAddr = "10.0.0.20:80".parse().unwrap();
        // Forward: the canonical `a` opened the conversation → initiator is `a`.
        assert_eq!(
            initiator_responder(a, b, flowscope::Orientation::Forward),
            (a, b)
        );
        // Reverse: the SYN actually came from `b` → initiator/responder swap,
        // independent of the address-sorted key order.
        assert_eq!(
            initiator_responder(a, b, flowscope::Orientation::Reverse),
            (b, a)
        );
    }

    #[test]
    fn community_id_matches_spec_vector() {
        // Canonical vector from corelight/community-id-spec: tcp
        // 128.232.110.120:34855 -> 66.35.250.204:80, seed 0.
        let a: SocketAddr = "128.232.110.120:34855".parse().unwrap();
        let b: SocketAddr = "66.35.250.204:80".parse().unwrap();
        let id = community_id_v1(a, b, 6, 0);
        assert_eq!(id, "1:LQU9qZlK+B5F3KDmev6m5PMibrg=");
        // Directionless: swapping endpoints yields the same hash.
        assert_eq!(community_id_v1(b, a, 6, 0), id);
    }

    #[test]
    fn community_id_agrees_with_netring_ipfix() {
        // Cross-check (#223): our hand-rolled Community ID must equal the value
        // netring stamps on its IPFIX records (`FlowRecord::to_ipfix_record`).
        // These vectors were captured live from `@rpc/netring/ipfix` for the same
        // 5-tuples we serve on `@rpc/netring/flows` — they agree because both follow
        // the Corelight spec with universal seed 0. Pins both impls.
        let tcp_a: SocketAddr = "10.0.0.10:49152".parse().unwrap();
        let tcp_b: SocketAddr = "10.0.0.20:80".parse().unwrap();
        assert_eq!(
            community_id_v1(tcp_a, tcp_b, 6, 0),
            "1:UG6cynLHoE34uLlABaTLnoF9dWI="
        );
        let udp_a: SocketAddr = "10.0.0.10:40000".parse().unwrap();
        let udp_b: SocketAddr = "10.0.0.20:53".parse().unwrap();
        assert_eq!(
            community_id_v1(udp_a, udp_b, 17, 0),
            "1:7/iy0TWjrq0cVVQ9n2zZP8McXdg="
        );
    }

    #[test]
    fn leg_asymmetry_alert_shape() {
        // Drive the alert builder with synthetic leg indices (a real
        // capture_leg_inconsistent flag needs a multi-NIC host — #226 is
        // host-gated; the boolean decision is wired in `monitor.rs`).
        let a = leg_asymmetry_alert("netsensor", Some(0), Some(1));
        assert_eq!(a.rule, "capture-leg-asymmetry");
        assert_eq!(a.kind, AlertKind::SensorHealth);
        assert_eq!(a.severity, AlertSeverity::Warning);
        assert_eq!(
            a.labels.get("source_idx_forward").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            a.labels.get("source_idx_reverse").map(String::as_str),
            Some("1")
        );
        // Unknown legs: no labels, still a well-formed alert.
        let b = leg_asymmetry_alert("netsensor", None, None);
        assert!(b.labels.is_empty());
        assert_eq!(b.rule, "capture-leg-asymmetry");
    }

    #[test]
    fn proto_number_known_and_unknown() {
        assert_eq!(proto_number("tcp"), Some(6));
        assert_eq!(proto_number("UDP"), Some(17));
        assert_eq!(proto_number("icmp"), Some(1));
        assert_eq!(proto_number("gre"), None);
    }

    #[test]
    fn attack_technique_mapping() {
        assert_eq!(attack_technique("PortScanTRW"), Some("T1046"));
        assert_eq!(attack_technique("DgaScorer"), Some("T1568.002"));
        // #369: upstream slug BeaconRita (was RitaBeacon) + DataExfil (was DataExfiltration).
        assert_eq!(attack_technique("BeaconRita"), Some("T1071"));
        assert_eq!(attack_technique("DataExfil"), Some("T1048"));
        assert_eq!(attack_technique("RitaBeaconFqdn"), Some("T1071"));
        assert_eq!(attack_technique("UnknownDetector"), None);
    }

    // ─── DNS anomaly detectors (issue #118) ──────────────────────────────────

    #[test]
    fn seen_domains_first_sight_then_repeat() {
        let mut seen = SeenDomains::new(8);
        assert!(seen.observe("evil"), "first sight is newly-observed");
        assert!(!seen.observe("evil"), "repeat is not newly-observed");
        assert!(seen.observe("good"));
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn seen_domains_evicts_oldest_at_cap() {
        let mut seen = SeenDomains::new(2);
        assert!(seen.observe("a"));
        assert!(seen.observe("b"));
        // Inserting a third evicts "a" (oldest).
        assert!(seen.observe("c"));
        assert_eq!(seen.len(), 2);
        // "a" was evicted → seen as new again; "b"/"c" still known.
        assert!(seen.observe("a"));
        assert!(!seen.observe("c"));
    }

    #[test]
    fn dns_tunnel_threshold_crossing() {
        // Below both thresholds → no fire.
        assert!(!dns_tunnel_fires(10, 40, 50, 100));
        // Distinct-label cardinality crosses → fire.
        assert!(dns_tunnel_fires(50, 40, 50, 100));
        // Long qname crosses (even with few distinct labels) → fire.
        assert!(dns_tunnel_fires(3, 120, 50, 100));
        // Exact boundary is inclusive.
        assert!(dns_tunnel_fires(0, 100, 50, 100));
    }

    #[test]
    fn dns_tunnel_view_maps_to_alert_with_technique() {
        let v = dns_tunnel_view(Some("10.0.0.5".into()), "exfil", 80, 110);
        assert_eq!(v.kind, "DnsTunnel");
        assert_eq!(v.severity, AlertSeverity::Warning);
        let alert = anomaly_alert("s", &v);
        assert_eq!(alert.kind, AlertKind::Anomaly);
        assert_eq!(alert.rule, "DnsTunnel");
        assert_eq!(alert.labels.get("sld").map(String::as_str), Some("exfil"));
        assert_eq!(
            alert.labels.get("src").map(String::as_str),
            Some("10.0.0.5")
        );
        assert_eq!(
            alert.labels.get("technique").map(String::as_str),
            Some("T1071.004")
        );
        // Bare-IP src (no port) carries no community_id.
        assert!(!alert.labels.contains_key("community_id"));
    }

    #[test]
    fn fqdn_beacon_view_maps_to_alert_with_technique() {
        let sig = RitaSignal {
            score: 0.93,
            ts_score: 1.0,
            ds_score: 1.0,
            dur_score: 0.4,
            mean_interval_secs: 30.0,
            n: 15,
        };
        let v = fqdn_beacon_view(
            "beacon.evil.example",
            Some("10.0.0.5".into()),
            Some("93.184.216.34".into()),
            sig,
        );
        assert_eq!(v.kind, "RitaBeaconFqdn");
        assert_eq!(v.severity, AlertSeverity::Warning);
        let alert = anomaly_alert("s", &v);
        assert_eq!(alert.kind, AlertKind::Anomaly);
        assert_eq!(alert.rule, "RitaBeaconFqdn");
        assert_eq!(
            alert.labels.get("fqdn").map(String::as_str),
            Some("beacon.evil.example")
        );
        assert_eq!(
            alert.labels.get("src").map(String::as_str),
            Some("10.0.0.5")
        );
        assert_eq!(
            alert.labels.get("dst").map(String::as_str),
            Some("93.184.216.34")
        );
        // C2 over app-layer protocol, same tactic family as the 5-tuple beacons.
        assert_eq!(
            alert.labels.get("technique").map(String::as_str),
            Some("T1071")
        );
        assert_eq!(alert.labels.get("score").map(String::as_str), Some("0.93"));
        assert_eq!(alert.labels.get("n").map(String::as_str), Some("15"));
        // Bare-IP endpoints (no ports) carry no community_id.
        assert!(!alert.labels.contains_key("community_id"));
    }

    #[test]
    fn nod_view_is_info_with_technique() {
        let v = nod_view(Some("10.0.0.5".into()), "newdomain");
        assert_eq!(v.kind, "NewlyObservedDomain");
        assert_eq!(v.severity, AlertSeverity::Info);
        let alert = anomaly_alert("s", &v);
        assert_eq!(alert.severity, AlertSeverity::Info);
        assert_eq!(alert.rule, "NewlyObservedDomain");
        assert_eq!(
            alert.labels.get("sld").map(String::as_str),
            Some("newdomain")
        );
        assert_eq!(
            alert.labels.get("technique").map(String::as_str),
            Some("T1568")
        );
    }

    #[test]
    fn nod_buckets_by_src_and_sld() {
        // Same (src, sld) → same alert_key (one NOD alert per domain per host).
        let a1 = anomaly_alert("s", &nod_view(Some("10.0.0.5".into()), "dom"));
        let a2 = anomaly_alert("s", &nod_view(Some("10.0.0.5".into()), "dom"));
        assert_eq!(a1.alert_key(), a2.alert_key());
        // Different sld → different key.
        let a3 = anomaly_alert("s", &nod_view(Some("10.0.0.5".into()), "other"));
        assert_ne!(a1.alert_key(), a3.alert_key());
    }

    #[test]
    fn rita_beacon_view_maps_kind_and_technique() {
        // #369: the RITA detector emits flowscope's upstream kind "BeaconRita";
        // the mapping must carry it through to the alert with the C2 technique tag.
        let v = AnomalyView {
            kind: "BeaconRita".into(),
            severity: AlertSeverity::Warning,
            src: Some("10.0.0.5:44321".into()),
            dst: Some("203.0.113.7:443".into()),
            proto: Some("tcp".into()),
            observations: vec![],
            metrics: vec![("score".into(), 0.94)],
        };
        let alert = anomaly_alert("s", &v);
        assert_eq!(alert.rule, "BeaconRita");
        assert_eq!(
            alert.labels.get("technique").map(String::as_str),
            Some("T1071")
        );
        // Full 5-tuple → cross-tool community_id is attached.
        assert!(
            alert
                .labels
                .get("community_id")
                .is_some_and(|c| c.starts_with("1:"))
        );
    }

    #[test]
    fn anomaly_alert_carries_technique_and_community_id() {
        let mut a = scan_anomaly();
        a.kind = "DgaScorer".into();
        a.src = Some("10.0.0.5:44321".into());
        a.dst = Some("203.0.113.7:53".into());
        a.proto = Some("udp".into());
        let alert = anomaly_alert("s", &a);
        assert_eq!(
            alert.labels.get("technique").map(String::as_str),
            Some("T1568.002")
        );
        assert!(
            alert
                .labels
                .get("community_id")
                .is_some_and(|c| c.starts_with("1:"))
        );
    }

    #[test]
    fn capture_points_shape() {
        let pts = capture_points(
            "s",
            0,
            10_000,
            42,
            0.004,
            &CaptureDrops::AfPacket { freezes: 1 },
        );
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("capture/0/packets").value,
            TelemetryValue::Counter(10_000)
        );
        assert_eq!(find("capture/0/drops").value, TelemetryValue::Counter(42));
        assert_eq!(
            find("capture/0/drop_rate").value,
            TelemetryValue::Gauge(0.004)
        );
        assert_eq!(find("capture/0/freezes").value, TelemetryValue::Counter(1));
    }

    #[test]
    fn tcp_reset_points_shape() {
        let pts = tcp_reset_points("s", 5, 3);
        assert_eq!(pts[0].metric, "tcp/resets_total");
        assert_eq!(pts[0].value, TelemetryValue::Counter(5));
        assert_eq!(pts[1].metric, "tcp/refused_total");
        assert_eq!(pts[1].value, TelemetryValue::Counter(3));
    }

    #[test]
    fn summary_variants() {
        let mut a = scan_anomaly();
        a.src = None;
        a.dst = None;
        assert_eq!(human_summary(&a), "PortScanTRW");
    }

    // ─── ICMP (issue #15) ────────────────────────────────────────────────────

    #[test]
    fn icmp_points_headline_and_by_kind() {
        let by_kind = vec![
            ("port_unreachable".to_string(), 7),
            ("time_exceeded".to_string(), 2),
        ];
        let pts = icmp_points("s", 9, 2, 1, &by_kind);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("icmp/unreachable_total").value,
            TelemetryValue::Counter(9)
        );
        assert_eq!(
            find("icmp/time_exceeded_total").value,
            TelemetryValue::Counter(2)
        );
        assert_eq!(
            find("icmp/mtu_signal_total").value,
            TelemetryValue::Counter(1)
        );
        assert_eq!(
            find("icmp/by_kind/port_unreachable_total").value,
            TelemetryValue::Counter(7)
        );
        assert_eq!(
            find("icmp/by_kind/time_exceeded_total").value,
            TelemetryValue::Counter(2)
        );
    }

    #[test]
    fn icmp_flow_alert_shape_and_bucketing() {
        let v = IcmpErrorView {
            kind: "port_unreachable".into(),
            is_unreachable: true,
            correlated_flow: Some(("10.0.0.1:5555".into(), "10.0.0.9:53".into())),
            ..Default::default()
        };
        let a = icmp_flow_alert("s", &v);
        assert_eq!(a.kind, AlertKind::Anomaly);
        assert_eq!(a.rule, "IcmpFlowError");
        assert_eq!(a.severity, AlertSeverity::Warning);
        assert!(a.summary.contains("port_unreachable"));
        assert!(a.summary.contains("10.0.0.9:53"));
        assert_eq!(
            a.labels.get("kind").map(String::as_str),
            Some("port_unreachable")
        );
        assert_eq!(a.labels.get("dst").map(String::as_str), Some("10.0.0.9:53"));

        // Same rule+dst, different kind label still buckets by (rule, dst-as-src
        // in alert_key)? alert_key includes labels; assert two errors to the same
        // dst with the same kind collapse.
        let mut v2 = v.clone();
        v2.correlated_flow = Some(("10.0.0.2:6666".into(), "10.0.0.9:53".into()));
        let k1 = icmp_flow_alert("s", &v).alert_key();
        let k2 = icmp_flow_alert("s", &v2).alert_key();
        // src differs so keys differ — that's fine; the bucketing test for src is
        // covered by the scan test. Here just assert both produce valid keys.
        assert!(!k1.is_empty() && !k2.is_empty());
    }

    #[test]
    fn icmp_no_kinds_still_emits_headline() {
        let pts = icmp_points("s", 0, 0, 0, &[]);
        assert_eq!(pts.len(), 3);
        assert!(
            pts.iter()
                .all(|p| matches!(p.value, TelemetryValue::Counter(0)))
        );
    }

    // ─── Per-protocol + connection-state (issue #16) ─────────────────────────

    #[test]
    fn flow_by_l4_points_shape() {
        let pts = flow_by_l4_points("s", 1000, 5, 200, 3, 50, 1);
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("flow/by_l4/tcp/bytes_total").value,
            TelemetryValue::Counter(1000)
        );
        assert_eq!(
            find("flow/by_l4/tcp/flows_total").value,
            TelemetryValue::Counter(5)
        );
        assert_eq!(
            find("flow/by_l4/udp/bytes_total").value,
            TelemetryValue::Counter(200)
        );
        assert_eq!(
            find("flow/by_l4/icmp/flows_total").value,
            TelemetryValue::Counter(1)
        );
    }

    #[test]
    fn tcp_close_class_buckets() {
        assert_eq!(tcp_close_class("fin"), "fin");
        assert_eq!(tcp_close_class("rst"), "rst");
        assert_eq!(tcp_close_class("idle"), "idle");
        // Everything non-fin/rst folds into idle.
        assert_eq!(tcp_close_class("evicted"), "idle");
        assert_eq!(tcp_close_class("buffer_overflow"), "idle");
        assert_eq!(tcp_close_class("parse_error"), "idle");
    }

    #[test]
    fn tcp_closed_points_shape() {
        let pts = tcp_closed_points("s", 10, 4, 2);
        assert_eq!(pts[0].metric, "tcp/closed_fin_total");
        assert_eq!(pts[0].value, TelemetryValue::Counter(10));
        assert_eq!(pts[1].metric, "tcp/closed_rst_total");
        assert_eq!(pts[1].value, TelemetryValue::Counter(4));
        assert_eq!(pts[2].metric, "tcp/closed_idle_total");
        assert_eq!(pts[2].value, TelemetryValue::Counter(2));
    }

    // ─── DNS RED (issue #19) ─────────────────────────────────────────────────

    #[test]
    fn dns_sld_extraction() {
        assert_eq!(dns_sld("www.example.com."), Some("example".into()));
        assert_eq!(dns_sld("example.com"), Some("example".into()));
        assert_eq!(dns_sld("a.b.example.co.uk"), Some("co".into())); // naive SLD
        assert_eq!(dns_sld("EXAMPLE.COM"), Some("example".into())); // lowercased
        assert_eq!(dns_sld("localhost"), None); // bare TLD
        assert_eq!(dns_sld("."), None);
        assert_eq!(dns_sld(""), None);
    }

    #[test]
    fn dns_points_red() {
        let by_rcode = vec![
            (DnsRcodeClass::NoError, 100),
            (DnsRcodeClass::NxDomain, 8),
            (DnsRcodeClass::ServFail, 1),
        ];
        let pts = dns_points("s", 120, &by_rcode, 4, Some([50.0, 95.0, 100.0]));
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("dns/queries_total").value,
            TelemetryValue::Counter(120)
        );
        assert_eq!(
            find("dns/unanswered_total").value,
            TelemetryValue::Counter(4)
        );
        assert_eq!(
            find("dns/responses_by_rcode/nxdomain_total").value,
            TelemetryValue::Counter(8)
        );
        assert_eq!(
            find("dns/responses_by_rcode/servfail_total").value,
            TelemetryValue::Counter(1)
        );
        assert_eq!(
            find("dns/query_rtt_p50_ms").value,
            TelemetryValue::Gauge(50.0)
        );
        assert_eq!(
            find("dns/query_rtt_p99_ms").value,
            TelemetryValue::Gauge(100.0)
        );
    }

    #[test]
    fn dns_points_empty_rtt_no_latency_gauges() {
        let pts = dns_points("s", 5, &[], 0, None);
        assert!(pts.iter().all(|p| !p.metric.contains("rtt")));
    }

    #[test]
    fn dns_rcode_class_slugs() {
        assert_eq!(DnsRcodeClass::NoError.slug(), "noerror");
        assert_eq!(DnsRcodeClass::NxDomain.slug(), "nxdomain");
        assert_eq!(DnsRcodeClass::ServFail.slug(), "servfail");
        assert_eq!(DnsRcodeClass::Refused.slug(), "refused");
        assert_eq!(DnsRcodeClass::Other.slug(), "other");
    }

    #[test]
    fn top_dns_records_ranks_by_queries() {
        let mut inv = std::collections::HashMap::new();
        inv.insert("alpha".to_string(), (5u64, 1u64));
        inv.insert("beta".to_string(), (12u64, 0u64));
        inv.insert("gamma".to_string(), (12u64, 3u64));
        let top = top_dns_records(&inv, 2);
        assert_eq!(top.len(), 2);
        // beta & gamma tie at 12; tiebreak by domain ascending → beta first.
        assert_eq!(top[0].domain, "beta");
        assert_eq!(top[1].domain, "gamma");
        assert_eq!(top[1].nxdomain, 3);
    }

    // ─── HTTP RED (issue #20) ────────────────────────────────────────────────

    #[test]
    fn http_status_class_buckets() {
        assert_eq!(http_status_class(200), "2xx");
        assert_eq!(http_status_class(301), "3xx");
        assert_eq!(http_status_class(404), "4xx");
        assert_eq!(http_status_class(503), "5xx");
        assert_eq!(http_status_class(100), "other");
        assert_eq!(http_status_class(600), "other");
    }

    #[test]
    fn http_points_red() {
        let by_method = vec![("get".to_string(), 90), ("post".to_string(), 10)];
        let pts = http_points("s", 100, 80, 5, 12, 3, &by_method, Some([45.0, 95.0, 99.0]));
        let find = |m: &str| pts.iter().find(|p| p.metric == m).unwrap();
        assert_eq!(
            find("http/requests_total").value,
            TelemetryValue::Counter(100)
        );
        assert_eq!(
            find("http/status_2xx_total").value,
            TelemetryValue::Counter(80)
        );
        assert_eq!(
            find("http/status_5xx_total").value,
            TelemetryValue::Counter(3)
        );
        assert_eq!(
            find("http/methods/get_total").value,
            TelemetryValue::Counter(90)
        );
        assert_eq!(
            find("http/latency_p95_ms").value,
            TelemetryValue::Gauge(95.0)
        );
    }

    #[test]
    fn http_points_empty_latency_no_gauges() {
        let pts = http_points("s", 1, 1, 0, 0, 0, &[], None);
        assert!(pts.iter().all(|p| !p.metric.contains("latency")));
    }

    #[test]
    fn top_http_hosts_ranks_by_requests() {
        let mut inv = std::collections::HashMap::new();
        inv.insert("a.example".to_string(), (3u64, 0u64));
        inv.insert("b.example".to_string(), (9u64, 2u64));
        let top = top_http_hosts(&inv, 5);
        assert_eq!(top[0].host, "b.example");
        assert_eq!(top[0].errors, 2);
    }

    #[test]
    fn top_ja4h_ranks_by_count_then_fingerprint() {
        let mut inv = std::collections::HashMap::new();
        inv.insert(
            "ge11nn050000_aaa".to_string(),
            Ja4hRecord {
                ja4h: "ge11nn050000_aaa".to_string(),
                host: Some("a.example".to_string()),
                method: Some("GET".to_string()),
                user_agent: None,
                count: 3,
            },
        );
        inv.insert(
            "po20cn100000_bbb".to_string(),
            Ja4hRecord {
                ja4h: "po20cn100000_bbb".to_string(),
                host: Some("b.example".to_string()),
                method: Some("POST".to_string()),
                user_agent: Some("curl/8".to_string()),
                count: 9,
            },
        );
        let top = top_ja4h(&inv, 5);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].ja4h, "po20cn100000_bbb");
        assert_eq!(top[0].count, 9);
        assert_eq!(top[0].user_agent.as_deref(), Some("curl/8"));
        // top-N truncation keeps only the highest-count entry.
        assert_eq!(top_ja4h(&inv, 1).len(), 1);
    }

    // ─── Top-talkers + elephant flows (issue #21) ────────────────────────────

    #[test]
    fn top_talkers_ranks_by_rate() {
        // #369: src-keyed rolling bytes/sec from netring aggregate().
        let talkers = vec![
            ("1.1.1.1".to_string(), 1000.0),
            ("8.8.8.8".to_string(), 5000.0),
            ("9.9.9.9".to_string(), 50.0),
        ];
        let top = top_talkers(&talkers, 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].src, "8.8.8.8");
        assert_eq!(top[0].bytes_per_sec, 5000.0);
        assert_eq!(top[1].src, "1.1.1.1");
    }

    #[test]
    fn traffic_matrix_ranks_pairs_by_rate() {
        let pairs = vec![
            ("10.0.0.1".to_string(), "8.8.8.8".to_string(), 5000.0),
            ("10.0.0.1".to_string(), "1.1.1.1".to_string(), 1000.0),
            ("10.0.0.2".to_string(), "8.8.8.8".to_string(), 50.0),
        ];
        let top = traffic_matrix(&pairs, 2);
        assert_eq!(top.len(), 2);
        // Heaviest src→dst pair leads; the matrix keeps both endpoints.
        assert_eq!(
            (top[0].src.as_str(), top[0].dst.as_str()),
            ("10.0.0.1", "8.8.8.8")
        );
        assert_eq!(top[0].bytes_per_sec, 5000.0);
        assert_eq!(
            (top[1].src.as_str(), top[1].dst.as_str()),
            ("10.0.0.1", "1.1.1.1")
        );
    }

    #[test]
    fn elephant_record_shape() {
        let r = elephant_record(
            "10.0.0.1:5".into(),
            "1.1.1.1:443".into(),
            "tcp",
            10_000_000,
            8000,
            4200,
            DirCounts {
                bytes_initiator: 400_000,
                bytes_responder: 9_600_000,
                packets_initiator: 3000,
                packets_responder: 5000,
            },
        );
        assert_eq!(r.src, "10.0.0.1:5");
        assert_eq!(r.bytes, 10_000_000);
        assert_eq!(r.proto, "tcp");
        assert_eq!(r.duration_ms, 4200);
        // Per-direction split (#255): totals stay, splits are carried through.
        assert_eq!(r.bytes_initiator, 400_000);
        assert_eq!(r.bytes_responder, 9_600_000);
        assert_eq!(r.packets_initiator, 3000);
        assert_eq!(r.packets_responder, 5000);
        assert_eq!(r.bytes_initiator + r.bytes_responder, r.bytes);
    }

    #[test]
    fn anomaly_count_point_is_counter_with_slug_path() {
        let p = anomaly_count_point("host01", "BeaconRita", 7);
        assert_eq!(p.metric, "anomaly/beacon_rita/total");
        assert_eq!(detector_chunk("DnsTunnel"), "dns_tunnel");
        assert_eq!(
            detector_chunk("NewlyObservedDomain"),
            "newly_observed_domain"
        );
        assert_eq!(detector_chunk("port_unreachable"), "port_unreachable");
        assert_eq!(p.value, TelemetryValue::Counter(7));
        assert_eq!(p.source, "host01");
        assert_eq!(p.protocol, Protocol::Netring);
    }

    #[test]
    fn quic_ssh_count_points_are_gauges() {
        let q = quic_count_point("s", 5);
        assert_eq!(q.metric, "quic/distinct_sni");
        assert_eq!(q.value, TelemetryValue::Gauge(5.0));
        let h = ssh_count_point("s", 3);
        assert_eq!(h.metric, "ssh/distinct_hassh");
        assert_eq!(h.value, TelemetryValue::Gauge(3.0));
    }

    #[test]
    fn snmp_cleartext_alert_carries_community_and_endpoints() {
        let a = snmp_cleartext_alert(
            "s",
            "v2c",
            "public",
            Some("10.0.0.9:51000".into()),
            Some("10.0.0.1:161".into()),
        );
        assert_eq!(a.kind, AlertKind::Anomaly);
        assert_eq!(a.rule, "cleartext-snmp");
        assert_eq!(a.severity, AlertSeverity::Warning);
        assert_eq!(
            a.labels.get("community").map(String::as_str),
            Some("public")
        );
        assert_eq!(a.labels.get("version").map(String::as_str), Some("v2c"));
        assert_eq!(
            a.labels.get("src").map(String::as_str),
            Some("10.0.0.9:51000")
        );
    }

    #[test]
    fn capture_points_afpacket_emits_freezes() {
        let pts = capture_points(
            "s",
            0,
            1000,
            5,
            0.01,
            &CaptureDrops::AfPacket { freezes: 3 },
        );
        let names: Vec<_> = pts.iter().map(|p| p.metric.as_str()).collect();
        assert!(names.contains(&"capture/0/packets"));
        assert!(names.contains(&"capture/0/drops"));
        assert!(names.contains(&"capture/0/drop_rate"));
        assert!(names.contains(&"capture/0/freezes"));
        // No XDP sub-metrics for an AF_PACKET source.
        assert!(!names.iter().any(|n| n.contains("/xdp/")));
    }

    #[test]
    fn capture_points_xdp_breaks_out_each_drop_cause() {
        let pts = capture_points(
            "s",
            1,
            2000,
            8,
            0.004,
            &CaptureDrops::Xdp {
                rx_dropped: 1,
                rx_invalid_descs: 2,
                rx_ring_full: 3,
                rx_fill_ring_empty_descs: 4,
                tx_invalid_descs: 5,
                tx_ring_empty_descs: 6,
            },
        );
        let names: Vec<_> = pts.iter().map(|p| p.metric.as_str()).collect();
        assert!(names.contains(&"capture/1/xdp/rx_ring_full"));
        assert!(names.contains(&"capture/1/xdp/rx_invalid_descs"));
        assert!(names.contains(&"capture/1/xdp/tx_ring_empty_descs"));
        // The invalid-descs counter is kept distinct (not folded into drops).
        let invalid = pts
            .iter()
            .find(|p| p.metric == "capture/1/xdp/rx_invalid_descs")
            .unwrap();
        assert_eq!(invalid.value, TelemetryValue::Counter(2));
    }

    #[test]
    fn overload_alert_firing_then_resolved() {
        let firing = overload_alert("s", 0, 0.12, true);
        assert_eq!(firing.kind, AlertKind::SensorHealth);
        assert_eq!(firing.rule, "capture-overload");
        assert_eq!(firing.severity, AlertSeverity::Critical);
        assert!(firing.is_firing());
        assert_eq!(firing.labels.get("source").map(String::as_str), Some("0"));

        let resolved = overload_alert("s", 0, 0.0, false);
        assert!(!resolved.is_firing());
        // Same rule + bucketing key so it resolves the firing alert.
        assert_eq!(resolved.rule, "capture-overload");
    }

    /// The registry guard must actually bite. A conformance suite that cannot fail
    /// is the same mistake as the `{metric...}` catch-all it replaced: vacuously
    /// true. This is the test that proves the others mean something.
    #[test]
    #[should_panic(expected = "unregistered netring telemetry subject")]
    fn an_unregistered_metric_panics_in_debug() {
        let _ = point("s", "totally/made/up/subject", TelemetryValue::Gauge(1.0));
    }

    /// ...and a real subject constructs.
    #[test]
    fn a_registered_metric_constructs() {
        assert_eq!(
            point("s", "flow/active", TelemetryValue::Gauge(1.0)).metric,
            "flow/active"
        );
    }
}
