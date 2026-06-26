//! On-demand detail DTOs: the JSON reply shapes for the sensors' `@/query/*`
//! detail channels (full route/neighbor/socket tables). Shared so the producing
//! sensor and the consuming frontend agree on one definition (no drift).
//!
//! These are higher-cardinality tables served only on demand (principle P2) —
//! they are never streamed onto the telemetry bus.

use serde::{Deserialize, Serialize};

/// One row of the routing table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteRecord {
    /// IP family: 4 or 6.
    pub family: u8,
    /// Destination: `"default"` or `"<cidr>"`.
    pub dst: String,
    pub gateway: Option<String>,
    /// Output interface index.
    pub oif: Option<u32>,
    pub priority: Option<u32>,
    pub protocol: String,
    pub scope: String,
    pub table: u32,
}

/// One ARP/NDP neighbor entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeighborRecord {
    pub family: u8,
    pub ip: Option<String>,
    pub mac: Option<String>,
    pub ifindex: u32,
    pub state: String,
    pub is_router: bool,
}

/// One recent network flow (netring), served on demand from a bounded ring of
/// the most-recently-ended flows. The 5-tuple + volume + lifetime + close reason
/// — the NetFlow/IPFIX-style detail behind the streamed flow aggregates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlowRecord {
    /// Initiator endpoint `ip:port`.
    pub src: String,
    /// Responder endpoint `ip:port`.
    pub dst: String,
    /// L4 protocol label (e.g. `"tcp"`).
    pub proto: String,
    pub bytes: u64,
    pub packets: u64,
    pub duration_ms: u64,
    /// How the flow ended: `fin` / `rst` / `idle_timeout` / `evicted` / ...
    pub reason: String,
    /// Community ID v1 flow hash (`1:<base64-sha1>`) — the de-facto cross-tool
    /// flow-correlation key (Zeek/Suricata/Wireshark/Security Onion). `None` when
    /// the 5-tuple is incomplete. Additive (`#[serde(default)]` for old records).
    #[serde(default)]
    pub community_id: Option<String>,
}

/// One observed TLS client fingerprint (passive asset inventory from netring's
/// ClientHello parsing) — SNI + JA3/JA4 + negotiated ALPN, with how many
/// handshakes matched it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TlsRecord {
    pub sni: Option<String>,
    pub alpn: Option<String>,
    pub ja3: Option<String>,
    pub ja4: Option<String>,
    pub count: u64,
}

/// One observed QUIC Initial (netring, passive). QUIC carries the destination
/// hostname (SNI) and ALPN in the *unprotected* Initial ClientHello, so this is
/// the QUIC analogue of TLS SNI visibility — for the growing share of HTTPS that
/// has moved off TCP+TLS onto QUIC/h3. Served on demand from `@/query/quic`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuicRecord {
    /// Server Name Indication from the ClientHello (the dialed hostname).
    pub sni: Option<String>,
    /// ALPN protocol identifiers (e.g. `["h3"]`).
    #[serde(default)]
    pub alpn: Vec<String>,
    /// QUIC version label (e.g. `"v1"`, `"v2"`, `"draft-29"`).
    pub version: String,
    /// Number of Initials matching this (sni, version).
    pub count: u64,
}

/// One observed SSH handshake fingerprint (netring, passive), keyed by HASSH.
/// HASSH (client) / HASSHServer fingerprints the SSH implementation from its
/// KEXINIT algorithm lists — fleet fingerprinting + rogue-client detection
/// without touching the (encrypted) session. Served on demand from `@/query/ssh`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshRecord {
    /// HASSH / HASSHServer fingerprint (lowercase-hex MD5).
    pub hassh: String,
    /// `"client"` or `"server"` — which side this fingerprint is from.
    pub role: String,
    /// Version banner seen on the same flow (e.g. `"SSH-2.0-OpenSSH_9.6"`),
    /// best-effort correlated; `None` if the banner wasn't observed.
    pub banner: Option<String>,
    /// Number of handshakes matching this fingerprint.
    pub count: u64,
}

/// One top-talker destination (netring), served on demand from a per-destination
/// histogram updated as flows end. "Who are the major backends?" — bytes/packets/
/// flows aggregated per remote endpoint, the operational view distinct from
/// per-app bandwidth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TalkerRecord {
    /// Remote endpoint (`ip` or `ip:port`, depending on aggregation key).
    pub dst: String,
    pub bytes: u64,
    pub packets: u64,
    pub flows: u64,
}

/// One cell of the netring traffic matrix (#122): a directed `src → dst` pair with
/// aggregated byte/packet/flow volume, served on demand from an `(src,dst)`-keyed
/// histogram updated as flows end. This is the service-map data — "who talks to
/// whom, and how much" — distinct from the per-destination [`TalkerRecord`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatrixRecord {
    pub src: String,
    pub dst: String,
    pub bytes: u64,
    pub packets: u64,
    pub flows: u64,
}

/// One recent elephant (large) flow (netring), served on demand from a bounded
/// ring of the biggest recently-ended flows. "What were the biggest transfers?"
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElephantRecord {
    pub src: String,
    pub dst: String,
    pub proto: String,
    pub bytes: u64,
    pub packets: u64,
    pub duration_ms: u64,
}

/// One observed DNS second-level domain (netring), served on demand. Carries the
/// query count and an NXDOMAIN tally — the high-cardinality detail behind the
/// streamed DNS RED aggregates (top SLDs / top-NXDOMAIN).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DnsRecord {
    /// Second-level domain (e.g. `example` for `example.com`), lowercased.
    pub domain: String,
    pub queries: u64,
    /// Responses for this domain that returned NXDOMAIN.
    pub nxdomain: u64,
}

/// One observed HTTP host (netring, cleartext), served on demand. Carries request
/// count and an error tally — the detail behind the streamed HTTP RED aggregates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HttpHostRecord {
    pub host: String,
    pub requests: u64,
    /// Responses for this host with a 4xx/5xx status.
    pub errors: u64,
}

/// One passively-discovered network asset (netring), served on demand from the
/// MAC-keyed inventory netring builds off L2/L3 discovery traffic (ARP / NDP /
/// LLDP / CDP). "Who is on my network, and what are they?" — surfaced without
/// any active probing, so it covers hosts that emit no ZenSight telemetry of
/// their own. The high-cardinality detail is pulled on demand (principle P2),
/// never streamed; only an aggregate asset count rides the telemetry bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetRecord {
    /// L2 address (the inventory's primary key), e.g. `"aa:bb:cc:dd:ee:ff"`.
    pub mac: String,
    /// IPv4 addresses observed bound to this MAC (bounded, oldest-evicted).
    #[serde(default)]
    pub ipv4: Vec<String>,
    /// IPv6 addresses observed bound to this MAC.
    #[serde(default)]
    pub ipv6: Vec<String>,
    /// Hostname (DHCP option 12 / LLDP / CDP system name / mDNS).
    pub hostname: Option<String>,
    /// Vendor / OS banner (DHCP option 60, LLDP system-description, CDP
    /// software-version, SSDP `SERVER`).
    pub vendor: Option<String>,
    /// Hardware platform (LLDP / CDP platform TLV), e.g. `"cisco WS-C2960X"`.
    pub platform: Option<String>,
    /// Decoded device capabilities (e.g. `["router", "bridge"]`).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Which discovery parsers contributed (e.g. `["arp", "lldp"]`) — a
    /// confidence/freshness signal per source.
    #[serde(default)]
    pub seen_via: Vec<String>,
    /// Most-recent observation timestamp (Unix epoch milliseconds).
    pub last_seen: i64,
}

/// One process row (sysinfo), served on demand sorted/filtered by the caller
/// (`@/query/processes?sort=cpu|mem|io&top=N`). The high-cardinality per-pid
/// firehose behind the streamed `system/processes_{total,zombie}` aggregates —
/// never streamed as per-pid metric series (principle P2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub pid: i32,
    pub name: String,
    /// CPU usage percent (single-core normalized, as sysinfo reports it).
    pub cpu: f32,
    /// Resident memory in bytes.
    pub rss: u64,
    /// Virtual memory in bytes.
    pub vsz: u64,
    /// Thread/task count, if available.
    pub threads: Option<usize>,
    /// Process state label (e.g. `Run`, `Sleep`, `Zombie`).
    pub state: String,
    /// Cumulative bytes read (disk), if available.
    pub io_read: u64,
    /// Cumulative bytes written (disk), if available.
    pub io_write: u64,
    /// Owning user id, if available.
    pub uid: Option<u32>,
}

/// One TCP socket (served filterable by state/port).
///
/// The richer fields (congestion control, congestion window, socket-memory
/// buffers) are populated when the sensor requests the sockdiag mem/congestion
/// extensions; they default to absent/zero for older producers (issue #11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SocketRecord {
    pub local: String,
    pub remote: String,
    pub state: String,
    pub uid: u32,
    pub recv_q: u32,
    pub send_q: u32,
    pub rtt_us: u32,
    pub retrans: u32,
    pub inode: u32,
    /// TCP congestion-control algorithm in use (e.g. "cubic", "bbr"), if known.
    #[serde(default)]
    pub congestion: Option<String>,
    /// Sender congestion window (packets), if known.
    #[serde(default)]
    pub snd_cwnd: u32,
    /// Send-buffer size in bytes (sndbuf), if mem info was requested.
    #[serde(default)]
    pub snd_buf: u32,
    /// Receive-buffer size in bytes (rcvbuf), if mem info was requested.
    #[serde(default)]
    pub rcv_buf: u32,
    /// Recent delivery rate in bytes/sec (kernel `tcp_info.delivery_rate`, #108) —
    /// "is this flow actually moving data". 0 when unknown.
    #[serde(default)]
    pub delivery_rate: u64,
    /// Pacing rate in bytes/sec (`tcp_info.pacing_rate`, #108); 0 when unknown,
    /// `u64::MAX` (unpaced) is normalized to 0 by the producer.
    #[serde(default)]
    pub pacing_rate: u64,
    /// Total bytes retransmitted on this socket (`tcp_info.bytes_retrans`, #108).
    #[serde(default)]
    pub bytes_retrans: u64,
    /// Lifetime segment retransmits (`tcp_info.total_retrans`, #108) — distinct
    /// from `retrans` (the current/outstanding count).
    #[serde(default)]
    pub total_retrans: u32,
    /// Receiver-side RTT estimate in microseconds (`tcp_info.rcv_rtt`, #108).
    #[serde(default)]
    pub rcv_rtt_us: u32,
    /// Currently lost (presumed-lost, unacked) segments (`tcp_info.lost`, #108).
    #[serde(default)]
    pub lost: u32,
    /// Reordering events observed on this socket (`tcp_info.reord_seen`, #108).
    #[serde(default)]
    pub reord_seen: u32,
}
