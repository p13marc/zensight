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
    /// Whether `src`/`dst` are an authoritative initiator → responder pair
    /// (netring 0.28 orientation, issue #122). `true` for TCP, where the
    /// handshake-aware tracker resolves the initiator (SYN sender). `false` for
    /// UDP and other handshake-less flows, where the ordering is a first-packet
    /// best-effort guess — render those as undirected (`↔`). Additive
    /// (`#[serde(default)]` → old records decode as undirected).
    #[serde(default)]
    pub directed: bool,
    /// Per-direction byte counts (netring 0.28, issue #223): `initiator` =
    /// `src`→`dst` (IPFIX IE 1), `responder` = the reverse (IE 1 reverse). The
    /// `bytes` field above stays the both-directions total (IE 85). `0` on old
    /// records. Lets a drill-down show flow asymmetry (e.g. a tiny request, a
    /// huge response).
    #[serde(default)]
    pub bytes_initiator: u64,
    #[serde(default)]
    pub bytes_responder: u64,
    /// Per-direction packet counts (IPFIX IE 2 / IE 2 reverse). `packets` stays
    /// the total (IE 86).
    #[serde(default)]
    pub packets_initiator: u64,
    #[serde(default)]
    pub packets_responder: u64,
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

/// One observed JA4H HTTP-request fingerprint (netring, passive — issue #124),
/// keyed by the JA4H string. JA4H fingerprints the HTTP client from its request
/// method, version, header set and cookie/language shape (FoxIO `a_b_c_d` form) —
/// cleartext HTTP only (TLS is opaque). Served on demand from `@/query/ja4h`.
/// Only populated when the sensor is built with `--features ja4plus` (FoxIO
/// License 1.1) and `collect.http_fp` is set. Note: JA4SSH is not yet available
/// upstream (flowscope 0.19 / netring 0.27 fingerprint SSH via HASSH — see
/// [`SshRecord`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ja4hRecord {
    /// JA4H fingerprint (`a_b_c_d` FoxIO format).
    pub ja4h: String,
    /// First `Host` header value seen for this fingerprint, if any.
    pub host: Option<String>,
    /// Request method (`GET`, `POST`, …), if it was valid ASCII.
    pub method: Option<String>,
    /// First `User-Agent` header value seen for this fingerprint, if any.
    pub user_agent: Option<String>,
    /// Number of requests matching this fingerprint.
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

/// One systemd unit inventory row (#274), served on demand from `@/query/units`
/// / `@/query/failed`. High-cardinality (hundreds per host) → never streamed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitRecord {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    /// Queued job type for this unit (`start`/`stop`/…), or `None` if idle.
    #[serde(default)]
    pub job: Option<String>,
}

/// Full detail for one systemd unit (#274), served from `@/query/unit?name=<u>`:
/// the inventory fields plus resource accounting, the unit file path, and the
/// dependency edges. Resource fields are `None` when accounting is off / the unit
/// isn't a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitDetail {
    pub name: String,
    pub description: String,
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    #[serde(default)]
    pub fragment_path: Option<String>,
    pub active_enter_usec: u64,
    pub n_restarts: u32,
    #[serde(default)]
    pub mem_bytes: Option<u64>,
    #[serde(default)]
    pub cpu_usec: Option<u64>,
    #[serde(default)]
    pub tasks: Option<u64>,
    pub exec_main_status: i32,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub wants: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub before: Vec<String>,
    /// Recent state-change lines from the event ring (#275); empty until events
    /// land or when the querier didn't ask for them.
    #[serde(default)]
    pub recent_changes: Vec<String>,
}

/// One systemd `.timer` unit row (#279), served on demand from `@/query/timers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerRecord {
    pub name: String,
    pub active_state: String,
    /// Wall-clock µs of the last trigger (0 = never fired).
    pub last_trigger_usec: u64,
    /// Wall-clock µs of the next scheduled elapse (0 / `u64::MAX` = none).
    pub next_elapse_usec: u64,
    /// The next elapse is in the past (a run is overdue / the timer is behind).
    pub overdue: bool,
}

/// One node of the systemd cgroup-v2 tree (#280), served from `@/query/cgroups`.
/// A point-in-time snapshot keyed by `path` (transient scopes churn).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CgroupNode {
    /// Path relative to the cgroup root, e.g. `system.slice/sshd.service`.
    pub path: String,
    /// The leaf name, e.g. `sshd.service`.
    pub name: String,
    /// Owning unit name when the node maps to one (`.service`/`.scope`/`.slice`).
    #[serde(default)]
    pub unit: Option<String>,
    /// Node classification derived from the name: `slice` / `service` / `scope` /
    /// `other`.
    pub kind: String,
    #[serde(default)]
    pub mem_bytes: Option<u64>,
    #[serde(default)]
    pub cpu_usec: Option<u64>,
    #[serde(default)]
    pub tasks: Option<u64>,
    #[serde(default)]
    pub io_read_bytes: Option<u64>,
    #[serde(default)]
    pub io_write_bytes: Option<u64>,
    /// Direct-member processes (leaves only, per the cgroup-v2 no-internal-process
    /// rule): `(pid, comm)`.
    #[serde(default)]
    pub pids: Vec<CgroupPid>,
    #[serde(default)]
    pub children: Vec<CgroupNode>,
}

/// A process member of a cgroup node (#280).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CgroupPid {
    pub pid: u32,
    pub comm: String,
}
