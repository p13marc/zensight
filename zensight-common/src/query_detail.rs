//! On-demand detail DTOs: the JSON reply shapes for the sensors'
//! `@rpc/<producer>/<topic>` detail read procedures (full route/neighbor/socket
//! tables). Shared so the producing
//! sensor and the consuming frontend agree on one definition (no drift).
//!
//! These are higher-cardinality tables served only on demand (principle P2) —
//! they are never streamed onto the telemetry bus.

use serde::{Deserialize, Serialize};

/// One row of the routing table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NeighborRecord {
    pub family: u8,
    pub ip: Option<String>,
    pub mac: Option<String>,
    pub ifindex: u32,
    pub state: String,
    pub is_router: bool,
}

/// One resolved name for a flow endpoint (netring passive DNS, #308): the
/// hostname a client actually used to reach an IP, tagged with where the
/// binding came from so a drill-down can weigh it (a forward DNS answer
/// outranks a reverse PTR guess).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NameInfo {
    /// Canonical name — lowercased, no trailing dot.
    pub name: String,
    /// Provenance slug: `dns_a` / `dns_aaaa` / `dns_cname` / `dns_ptr` /
    /// `mdns` / `sni` / `dhcp` / `other`.
    pub provenance: String,
}

/// One recent network flow (netring), served on demand from a bounded ring of
/// the most-recently-ended flows. The 5-tuple + volume + lifetime + close reason
/// — the NetFlow/IPFIX-style detail behind the streamed flow aggregates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Names the initiator resolved for the responder IP (netring passive DNS,
    /// #308) — best-ranked first, at most 3. Empty when the sensor's name cache
    /// is off / cold. Additive (`#[serde(default)]` for old records).
    #[serde(default)]
    pub dst_names: Vec<NameInfo>,
}

/// One observed TLS client fingerprint (passive asset inventory from netring's
/// ClientHello parsing) — SNI + JA3/JA4 + negotiated ALPN, with how many
/// handshakes matched it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct TlsRecord {
    pub sni: Option<String>,
    pub alpn: Option<String>,
    pub ja3: Option<String>,
    pub ja4: Option<String>,
    pub count: u64,
    /// The ClientHello offered a post-quantum (hybrid) key-share group (#326).
    /// Fleet PQ-readiness signal; additive (`#[serde(default)]` for old records).
    #[serde(default)]
    pub pq_key_share: bool,
    /// Application protocol classified from ALPN + SNI + port (e.g. `h2`, `doh`),
    /// if known (#326). Additive.
    #[serde(default)]
    pub app_protocol: Option<String>,
}

/// One observed QUIC Initial (netring, passive). QUIC carries the destination
/// hostname (SNI) and ALPN in the *unprotected* Initial ClientHello, so this is
/// the QUIC analogue of TLS SNI visibility — for the growing share of HTTPS that
/// has moved off TCP+TLS onto QUIC/h3. Served on demand from `@rpc/netring/quic`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
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
    /// QUIC JA4 (`q`-prefixed FoxIO format, royalty-free — OSI-clean, #326), if
    /// recoverable from the Initial's embedded ClientHello. Additive.
    #[serde(default)]
    pub ja4: Option<String>,
    /// The Initial offered a post-quantum (hybrid) key-share group (#326). Additive.
    #[serde(default)]
    pub pq_key_share: bool,
    /// Application protocol classified from ALPN + SNI + port (e.g. `h3`, `doq`),
    /// if known (#326). Additive.
    #[serde(default)]
    pub app_protocol: Option<String>,
}

/// One observed SSH handshake fingerprint (netring, passive), keyed by HASSH.
/// HASSH (client) / HASSHServer fingerprints the SSH implementation from its
/// KEXINIT algorithm lists — fleet fingerprinting + rogue-client detection
/// without touching the (encrypted) session. Served on demand from `@rpc/netring/ssh`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
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
    /// KEXINIT key-exchange algorithms this side offered (#326, from netring's
    /// typed SSH-fingerprint handler). Bounded; additive (`#[serde(default)]`).
    #[serde(default)]
    pub kex_algorithms: Vec<String>,
}

/// One observed JA4H HTTP-request fingerprint (netring, passive — issue #124),
/// keyed by the JA4H string. JA4H fingerprints the HTTP client from its request
/// method, version, header set and cookie/language shape (FoxIO `a_b_c_d` form) —
/// cleartext HTTP only (TLS is opaque). Served on demand from `@rpc/netring/ja4h`.
/// Only populated when the sensor is built with `--features ja4plus` (FoxIO
/// License 1.1) and `collect.http_fp` is set. Note: JA4SSH is not yet available
/// upstream (flowscope 0.19 / netring 0.27 fingerprint SSH via HASSH — see
/// [`SshRecord`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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

/// One top-talker source host (netring), served on demand from netring's rolling
/// `aggregate()` state (#369). **Breaking change:** talkers are now keyed by
/// *source IP* with a rolling **bytes-per-second** rate over the last 60 s window
/// (netring `BandwidthByKey`), replacing the old per-*destination* cumulative
/// byte/packet/flow histogram. "Who is generating the most traffic right now?"
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TalkerRecord {
    /// Source host IP.
    pub src: String,
    /// Rolling throughput in bytes/second over netring's aggregate window.
    pub bytes_per_sec: f64,
    /// Names observed for `src` on the wire (netring passive DNS, #308) —
    /// best-ranked first, at most 3. Empty when the name cache is off / cold.
    /// Additive (`#[serde(default)]` for old records).
    #[serde(default)]
    pub names: Vec<NameInfo>,
}

/// One cell of the netring traffic matrix (#122/#369): a directed `src → dst` pair
/// with a rolling **bytes-per-second** rate, served on demand from netring's
/// `aggregate()` pair state. This is the service-map data — "who talks to whom,
/// and how fast" — distinct from the per-source [`TalkerRecord`]. **Breaking
/// change (#369):** rate replaces the old cumulative byte/packet/flow counters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MatrixRecord {
    pub src: String,
    pub dst: String,
    /// Rolling throughput in bytes/second for this `(src, dst)` pair.
    pub bytes_per_sec: f64,
    /// Names observed for `dst` on the wire (netring passive DNS, #308).
    #[serde(default)]
    pub names: Vec<NameInfo>,
}

/// One recent elephant (large) flow (netring), served on demand from a bounded
/// ring of the biggest recently-ended flows. "What were the biggest transfers?"
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ElephantRecord {
    pub src: String,
    pub dst: String,
    pub proto: String,
    pub bytes: u64,
    pub packets: u64,
    pub duration_ms: u64,
    /// Per-direction byte counts (#255): initiator = src→dst, responder = the
    /// reverse. `bytes` stays the both-directions total. Additive/backward-compatible.
    #[serde(default)]
    pub bytes_initiator: u64,
    #[serde(default)]
    pub bytes_responder: u64,
    /// Per-direction packet counts (#255). `packets` stays the total.
    #[serde(default)]
    pub packets_initiator: u64,
    #[serde(default)]
    pub packets_responder: u64,
}

/// One capture file written by the netring capture-to-disk engine (#327),
/// served on demand from `@rpc/netring/captures`. A `triggered`-mode file carries the
/// anomaly that fired it plus (while it lives) the artifact id to download the
/// bytes through the `@blob/artifact` path; a `rotating`-mode file is a local
/// spool entry (metadata only — retrieval needs host filesystem access).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct CaptureRecord {
    /// File name (not the full host path — only metadata rides the bus).
    pub filename: String,
    /// On-disk size, bytes (compressed size when the file is zstd-compressed).
    pub bytes: u64,
    /// Packet records written (0 when unknown, e.g. rotating spool files).
    #[serde(default)]
    pub packets: u64,
    /// Capture mode that produced the file: `"triggered"` or `"rotating"`.
    pub mode: String,
    /// Detector slug of the anomaly that fired the trigger (`"manual"` for a
    /// `capture_now` command); `None` for rotating spool files.
    #[serde(default)]
    pub trigger_kind: Option<String>,
    /// Operator tag from a manual `capture_now`, if any.
    #[serde(default)]
    pub tag: Option<String>,
    /// First / last packet wall-clock time, epoch ms (0 when unknown).
    #[serde(default)]
    pub start_ms: i64,
    #[serde(default)]
    pub end_ms: i64,
    /// Tier-1 blob id to download the file through `@blob/artifact/**` while it
    /// lives; `None` for rotating spool files or once the TTL reaped it.
    #[serde(default)]
    pub artifact_id: Option<String>,
    /// The **concrete** `@blob/artifact` prefix of the origin serving this
    /// capture — the host that recorded it.
    ///
    /// Carrying it is what lets a consumer fetch from a literal key instead of
    /// a `*`-origin one. RFC 07 §2.5/§3 permit a wildcard origin for *probing*
    /// (tiny replies) and forbid it for a bulk fetch, because every matching
    /// holder ships the full payload and Zenoh cannot cancel remote replies in
    /// flight. Through 0.10 the GUI wildcarded the fetch — while the origin
    /// was already known one hop upstream and simply thrown away here.
    #[serde(default)]
    pub artifact_prefix: Option<String>,
    /// BLAKE3 root of the capture file — the integrity anchor a downloader
    /// pins so a wrong or tampered reply is discarded *before* it reaches disk
    /// (RFC 07 §2.1) rather than assembled and detected afterwards. Typed as
    /// [`zenkey::ContentHash`] since zenkey 0.4, so a name or malformed hex
    /// fails at deserialization instead of at the consumer's re-parse; the
    /// wire shape stays the plain hex string 0.10 sensors sent.
    #[serde(default)]
    pub artifact_root: Option<zenkey::ContentHash>,
    /// When the served artifact expires (epoch ms); `None` when not served.
    #[serde(default)]
    pub expires_ms: Option<i64>,
    /// True while the byte cap truncated the capture before its post-trigger
    /// window elapsed.
    #[serde(default)]
    pub truncated: bool,
}

/// One observed DNS second-level domain (netring), served on demand. Carries the
/// query count and an NXDOMAIN tally — the high-cardinality detail behind the
/// streamed DNS RED aggregates (top SLDs / top-NXDOMAIN).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DnsRecord {
    /// Second-level domain (e.g. `example` for `example.com`), lowercased.
    pub domain: String,
    pub queries: u64,
    /// Responses for this domain that returned NXDOMAIN.
    pub nxdomain: u64,
}

/// One observed encrypted-DNS destination (netring, #326), served on demand from
/// `@rpc/netring/encrypted_dns`. Encrypted DNS (DoT/DoQ/DoH) hides resolution from
/// passive DNS RED and can tunnel/exfiltrate past a network resolver policy; this
/// surfaces where it's going and whether the resolver is a known/sanctioned one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
pub struct EncryptedDnsRecord {
    /// Transport slug: `dot` (DNS-over-TLS), `doq` (DNS-over-QUIC), or `doh`
    /// (DNS-over-HTTPS).
    pub transport: String,
    /// Resolver hostname (SNI), if the handshake carried one.
    pub sni: Option<String>,
    /// The destination matched netring's known-public-resolver set (Cloudflare /
    /// Google / Quad9 / …). `false` = an un-recognised (possibly rogue) resolver.
    pub via_known_resolver: bool,
    /// Number of encrypted-DNS sessions to this (transport, sni, resolver-class).
    pub count: u64,
}

/// One observed HTTP host (netring, cleartext), served on demand. Carries request
/// count and an error tally — the detail behind the streamed HTTP RED aggregates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, schemars::JsonSchema)]
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
    /// Classified device role (#329): `router` / `switch` / `access-point` /
    /// `phone` / `iot` / `host` / `unknown`, derived by netring from capabilities +
    /// discovery sources. Additive (`#[serde(default)]` for old records).
    #[serde(default)]
    pub role: String,
    /// First observation timestamp (Unix epoch ms) — the new-asset signal. Additive.
    #[serde(default)]
    pub first_seen: i64,
    /// How many distinct discovery parsers have contributed to this record — a
    /// confidence signal (#329). Additive.
    #[serde(default)]
    pub source_count: u32,
    /// Every distinct hostname ever bound to this MAC (bounded); `hostname` mirrors
    /// the most-recent (#329). Additive.
    #[serde(default)]
    pub hostnames: Vec<String>,
    /// Per-parser fingerprints for pivoting to the fingerprint explorer (#329):
    /// JA3 / JA4 (TLS), HASSH (SSH), p0f (TCP/IP). Additive.
    #[serde(default)]
    pub ja3: Option<String>,
    #[serde(default)]
    pub ja4: Option<String>,
    #[serde(default)]
    pub hassh: Option<String>,
    #[serde(default)]
    pub p0f: Option<String>,
    /// X.509 leaf-certificate subject CN + SAN DNS entries this asset presented as
    /// a TLS server (#329) — a cert-based pivot. Populated only on `ja4plus` builds;
    /// empty otherwise. Additive.
    #[serde(default)]
    pub x509_subject: Option<String>,
    #[serde(default)]
    pub x509_sans: Vec<String>,
}

/// One process row (sysinfo), served on demand sorted/filtered by the caller
/// (`@rpc/sysinfo/processes?sort=cpu|mem|io&top=N`). The high-cardinality per-pid
/// firehose behind the streamed `system/processes_{total,zombie}` aggregates —
/// never streamed as per-pid metric series (principle P2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Command line — **scrubbed** of secret-looking argv values before publish
    /// and truncated to a byte cap (#302). Empty when unavailable or when the
    /// sensor is configured to strip arguments.
    #[serde(default)]
    pub cmdline: String,
    /// Executable path (readlink of `/proc/<pid>/exe`); `None` without
    /// permission (other users' processes, unprivileged sensor).
    #[serde(default)]
    pub exe: Option<String>,
    /// Parent pid.
    #[serde(default)]
    pub ppid: Option<i32>,
    /// cgroup v2 unified path — the join key to systemd units
    /// (`process.cgroup == unit.control_group`).
    #[serde(default)]
    pub cgroup: Option<String>,
    /// `/proc/<pid>/stat` field 22: start time in clock ticks since boot.
    /// Process identity is the `(pid, start_time)` pair everywhere (PIDs get
    /// reused); matches nlink's `ProcessRef.start_time` exactly. `0` = unknown.
    #[serde(default)]
    pub start_time: u64,
    /// Resolved username for `uid`, if available.
    #[serde(default)]
    pub user: Option<String>,
}

/// One TCP socket (served filterable by state/port).
///
/// The richer fields (congestion control, congestion window, socket-memory
/// buffers) are populated when the sensor requests the sockdiag mem/congestion
/// extensions; they default to absent/zero for older producers (issue #11).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// BBR bottleneck-bandwidth estimate, bytes/sec (`INET_DIAG_BBRINFO`, #322).
    /// `None` unless the socket runs BBR (cubic/reno report no CC info).
    #[serde(default)]
    pub bbr_bw_bps: Option<u64>,
    /// BBR min-filtered RTT, µs (#322). `None` unless the socket runs BBR.
    #[serde(default)]
    pub cc_min_rtt_us: Option<u32>,
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
    /// Cumulative app-layer bytes ACKed by the peer (`tcp_info.bytes_acked`, #317)
    /// — TX **goodput**. Deltas on the socket `cookie` give per-process TX
    /// bandwidth (app-goodput, TCP-only). 0 when unknown.
    #[serde(default)]
    pub bytes_acked: u64,
    /// Cumulative app-layer bytes received in-order (`tcp_info.bytes_received`,
    /// #317) — RX **goodput**. 0 when unknown.
    #[serde(default)]
    pub bytes_received: u64,
    /// Cumulative app-layer bytes handed to the transport (`tcp_info.bytes_sent`,
    /// #317; includes retransmitted payload, so ≥ `bytes_acked`). 0 when unknown.
    #[serde(default)]
    pub bytes_sent: u64,
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
    /// Kernel socket cookie (`SO_COOKIE`, #304) — the **stable** per-socket id.
    /// Inodes get reused after close; cookies effectively don't. Prefer this
    /// over `inode` when correlating socket records across snapshots.
    #[serde(default)]
    pub cookie: u64,
    /// cgroup v2 ID of the owning cgroup (`INET_DIAG_CGROUP_ID`, #304).
    /// `None` on kernels/hosts that don't report it (or cgroup v1).
    #[serde(default)]
    pub cgroup_id: Option<u64>,
    /// Resolved cgroup v2 path relative to `/sys/fs/cgroup` (e.g.
    /// `system.slice/sshd.service`, #304) — the join key to systemd units.
    /// `None` when the id was absent or unresolved.
    #[serde(default)]
    pub cgroup: Option<String>,
    /// Owning process id from the `/proc/<pid>/fd` scan (#304). `None` when
    /// unattributed (scan disabled, socket closed, or unreadable process).
    /// Not a stable identity on its own — pair with `proc_start_time`.
    #[serde(default)]
    pub pid: Option<i32>,
    /// Owning process `comm` (kernel-truncated to 15 bytes, #304).
    #[serde(default)]
    pub process: Option<String>,
    /// Owning process start time (`/proc/<pid>/stat` field 22, clock ticks
    /// since boot, #304). `(pid, proc_start_time)` is the process-identity
    /// pair used everywhere in ZenSight (PIDs get reused).
    #[serde(default)]
    pub proc_start_time: Option<u64>,
}

/// One systemd unit inventory row (#274), served on demand from `@rpc/systemd/units`
/// / `@rpc/systemd/failed`. High-cardinality (hundreds per host) → never streamed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UnitRecord {
    pub name: String,
    /// Empty for an installed-but-unloaded unit: the description lives in the
    /// unit file, and reading it would mean loading every unit on the host.
    pub description: String,
    /// systemd's `LoadState` (`loaded`/`masked`/`not-found`/…), or the ZenSight
    /// value `not-loaded` for a unit that is installed on disk but that the
    /// manager has not loaded — those rows come from `ListUnitFiles`, not
    /// `ListUnits`.
    pub load_state: String,
    pub active_state: String,
    pub sub_state: String,
    /// Queued job type for this unit (`start`/`stop`/…), or `None` if idle.
    #[serde(default)]
    pub job: Option<String>,
    /// Unit-file enablement (`enabled`/`disabled`/`static`/`masked`/…) from
    /// `ListUnitFiles`, or `None` for a unit with no installed unit file (and
    /// from pre-1.4 sensors, which did not report it).
    #[serde(default)]
    pub unit_file_state: Option<String>,
}

/// One unit's on-disk definition, served from `@rpc/systemd/unit/file?name=<u>`.
///
/// Opt-in (`actions.expose_unit_files`) because unit files routinely carry
/// credentials in `Environment=` lines; the sensor redacts those before they
/// reach the bus. Paths are resolved from D-Bus `FragmentPath`/`DropInPaths`,
/// never from the request, so there is no traversal surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct UnitFile {
    pub name: String,
    /// The main unit file's path, if the unit has one (generated and transient
    /// units do not).
    #[serde(default)]
    pub fragment_path: Option<String>,
    #[serde(default)]
    pub fragment: Option<String>,
    /// Drop-ins, in systemd's own override order: `(path, contents)`.
    #[serde(default)]
    pub dropins: Vec<(String, String)>,
    /// Whether the size cap dropped content.
    #[serde(default)]
    pub truncated: bool,
    /// Whether any line was redacted, so the reader knows this is not verbatim.
    #[serde(default)]
    pub redacted: bool,
}

/// Full detail for one systemd unit (#274), served from `@rpc/systemd/unit?name=<u>`:
/// the inventory fields plus resource accounting, the unit file path, and the
/// dependency edges. Resource fields are `None` when accounting is off / the unit
/// isn't a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Main service PID (`None` when not running / not a service) (#303).
    #[serde(default)]
    pub main_pid: Option<u32>,
    /// `/proc/<main_pid>/stat` field 22 in clock ticks since boot — the
    /// `(pid, start_time)` identity pair with `main_pid` (matches
    /// `ProcessRecord.start_time` / nlink `ProcessRef.start_time`).
    #[serde(default)]
    pub main_pid_start_time: Option<u64>,
    /// Durable per-run identity, hex — matches journald's
    /// `_SYSTEMD_INVOCATION_ID` (label `sd.journald.invocation_id`), so a log
    /// line resolves to an exact unit *run*, not a time-window guess.
    #[serde(default)]
    pub invocation_id: Option<String>,
    /// The unit's cgroup path — the cross-sensor join key
    /// (`unit.control_group == process.cgroup`).
    #[serde(default)]
    pub control_group: Option<String>,
}

/// One systemd `.timer` unit row (#279), served on demand from `@rpc/systemd/timers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

/// One node of the systemd cgroup-v2 tree (#280), served from `@rpc/systemd/cgroups`.
/// A point-in-time snapshot keyed by `path` (transient scopes churn).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CgroupPid {
    pub pid: u32,
    pub comm: String,
}

/// The one canonical syslog severity model (#557): RFC 5424 0–7, with the slug
/// / display-label / OTel-number mappings every layer needs. Consumed by the
/// sensor parser, the wire `LogRecord`, and both GUI views (which previously
/// each defined their own copy, and had drifted). The wire stays slug+number.
///
/// Ordering follows the RFC number: `Emergency` (0) is the *most* severe, so
/// `a <= b` means "a is at least as severe as b".
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[repr(u8)]
pub enum LogSeverity {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Informational = 6,
    Debug = 7,
}

impl LogSeverity {
    /// Strict parse from a numeric code (0–7); `None` out of range.
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Emergency,
            1 => Self::Alert,
            2 => Self::Critical,
            3 => Self::Error,
            4 => Self::Warning,
            5 => Self::Notice,
            6 => Self::Informational,
            7 => Self::Debug,
            _ => return None,
        })
    }

    /// Lenient parse from a numeric value; out-of-range clamps to `Debug`.
    pub fn from_value(val: u64) -> Self {
        Self::from_code(val.min(7) as u8).unwrap_or(Self::Debug)
    }

    /// Parse from a severity slug (`emerg`/`err`/`warning`/…, plus common
    /// aliases). `None` for anything unrecognized.
    pub fn from_slug(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "emerg" | "emergency" => Self::Emergency,
            "alert" => Self::Alert,
            "crit" | "critical" => Self::Critical,
            "err" | "error" => Self::Error,
            "warning" | "warn" => Self::Warning,
            "notice" => Self::Notice,
            "info" | "informational" => Self::Informational,
            "debug" => Self::Debug,
            _ => return None,
        })
    }

    /// Alias of [`from_slug`](Self::from_slug) (the name the overview used).
    pub fn from_label(s: &str) -> Option<Self> {
        Self::from_slug(s)
    }

    /// The lowercase wire slug (`emerg`/`err`/`warning`/…).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Emergency => "emerg",
            Self::Alert => "alert",
            Self::Critical => "crit",
            Self::Error => "err",
            Self::Warning => "warning",
            Self::Notice => "notice",
            Self::Informational => "info",
            Self::Debug => "debug",
        }
    }

    /// The uppercase display label (`EMERG`/`ERR`/`WARN`/…) for badges.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Emergency => "EMERG",
            Self::Alert => "ALERT",
            Self::Critical => "CRIT",
            Self::Error => "ERR",
            Self::Warning => "WARN",
            Self::Notice => "NOTICE",
            Self::Informational => "INFO",
            Self::Debug => "DEBUG",
        }
    }

    /// OTel `severity_number` (1–24) for this syslog severity (#104).
    pub fn otel_severity_number(&self) -> u8 {
        match self {
            Self::Emergency => 24,
            Self::Alert => 23,
            Self::Critical => 22,
            Self::Error => 17,
            Self::Warning => 13,
            Self::Notice => 10,
            Self::Informational => 9,
            Self::Debug => 5,
        }
    }

    /// OTel `severity_text` coarse band.
    pub fn otel_severity_text(&self) -> &'static str {
        match self {
            Self::Emergency | Self::Alert | Self::Critical => "FATAL",
            Self::Error => "ERROR",
            Self::Warning => "WARN",
            Self::Notice | Self::Informational => "INFO",
            Self::Debug => "DEBUG",
        }
    }

    /// All eight severities, most-severe first — for building filter menus.
    pub fn all() -> &'static [LogSeverity] {
        &[
            Self::Emergency,
            Self::Alert,
            Self::Critical,
            Self::Error,
            Self::Warning,
            Self::Notice,
            Self::Informational,
            Self::Debug,
        ]
    }
}

/// One per-line log event, served on demand from the logs sensor's bounded
/// ring at `@rpc/logs/events` (#358). Replaces the old streamed
/// `zensight/logs/<host>/events/<uid>` keys — per-line events are
/// high-cardinality detail and ride the bus only as low-rate rollups.
///
/// Selector parameters: `since=<epoch_ms>` (inclusive lower bound),
/// `max=<n>` (reply cap, default 500), `host=<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LogRecord {
    /// Time-sortable event uid: `<13-digit ts_ms><12-digit seq>` (#104).
    pub uid: String,
    /// Event time (Unix epoch ms).
    pub ts: i64,
    /// Originating host (the resolved hostname / telemetry `source`).
    pub host: String,
    /// Syslog facility slug (e.g. `auth`).
    pub facility: String,
    /// Syslog severity slug (e.g. `err`).
    pub severity: String,
    /// OTel severity number (1–24).
    pub severity_number: u8,
    /// Application / program name, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Process id string, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<String>,
    /// The log message text.
    pub message: String,
    /// Every other label the event point carried (`sd.journald.*`,
    /// `template_id`/`template`, `msgid`, `source_type`, `severity_text`,
    /// `raw`/`log.record.original`, …) so structured drill-downs survive
    /// the query hop losslessly.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub labels: std::collections::HashMap<String, String>,
}

/// The labels [`LogRecord`] lifts into typed fields; everything else spills
/// into [`LogRecord::labels`].
const LOG_RECORD_TYPED_LABELS: &[&str] = &[
    "facility",
    "severity",
    "severity_number",
    "app",
    "pid",
    "log.record.uid",
];

impl LogRecord {
    /// Build a record from a per-line log-event [`TelemetryPoint`] (the
    /// `events/<uid>` shape from #104). Returns `None` for anything that
    /// isn't a `Protocol::Logs` Text event.
    pub fn from_point(point: &crate::TelemetryPoint) -> Option<LogRecord> {
        if point.protocol != crate::Protocol::Logs {
            return None;
        }
        let crate::TelemetryValue::Text(message) = &point.value else {
            return None;
        };
        let label = |k: &str| point.labels.get(k).cloned();
        let uid = label("log.record.uid")
            .or_else(|| point.metric.strip_prefix("events/").map(str::to_string))
            .unwrap_or_else(|| point.metric.clone());
        let severity_number = point
            .labels
            .get("severity_number")
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(9);
        let labels = point
            .labels
            .iter()
            .filter(|(k, _)| !LOG_RECORD_TYPED_LABELS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Some(LogRecord {
            uid,
            ts: point.timestamp,
            host: point.source.clone(),
            facility: label("facility").unwrap_or_else(|| "unknown".to_string()),
            severity: label("severity").unwrap_or_else(|| "info".to_string()),
            severity_number,
            app: label("app"),
            pid: label("pid"),
            message: message.clone(),
            labels,
        })
    }

    /// Reconstruct the per-line event [`TelemetryPoint`] this record was built
    /// from, so existing consumers (log-row decoding, cold-store persistence)
    /// keep one code path. Inverse of [`LogRecord::from_point`].
    pub fn to_point(&self) -> crate::TelemetryPoint {
        let mut labels = self.labels.clone();
        labels.insert("facility".to_string(), self.facility.clone());
        labels.insert("severity".to_string(), self.severity.clone());
        labels.insert(
            "severity_number".to_string(),
            self.severity_number.to_string(),
        );
        labels.insert("log.record.uid".to_string(), self.uid.clone());
        if let Some(app) = &self.app {
            labels.insert("app".to_string(), app.clone());
        }
        if let Some(pid) = &self.pid {
            labels.insert("pid".to_string(), pid.clone());
        }
        crate::TelemetryPoint {
            timestamp: self.ts,
            source: self.host.clone(),
            protocol: crate::Protocol::Logs,
            metric: format!("events/{}", self.uid),
            value: crate::TelemetryValue::Text(self.message.clone()),
            labels,
            unit: None,
        }
    }
}

/// One NetFlow/IPFIX flow record, served on demand from `@rpc/netflow/flows`
/// (#469).
///
/// The bounded ring behind that procedure is what keyspace-v2 put in place of
/// the per-flow-pair telemetry keys the old keyspace invited — a flow is an
/// event with unbounded cardinality, not a metric, so it is *pulled* as a
/// record and never published as a key (RFC 08 §4). This is the type that reply
/// carries.
///
/// `fields` stays a free-form map on purpose: the field set is whatever the
/// exporter's template says it is (v9/IPFIX templates are defined by the
/// device, not by us), which is the same reason netflow keeps a `{metric...}`
/// rest-var in the registry while the six host producers do not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NetflowRecord {
    /// Exporter IP address.
    pub exporter_ip: String,
    /// Resolved exporter name.
    pub exporter_name: String,
    /// NetFlow version (5, 7, 9, or 10 for IPFIX).
    pub version: u16,
    /// Flow fields as key-value pairs, per the exporter's template.
    pub fields: std::collections::HashMap<String, NetflowFieldValue>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A NetFlow field value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum NetflowFieldValue {
    Uint(u64),
    Int(i64),
    Float(f64),
    IpAddr(String),
    MacAddr(String),
    String(String),
    Bytes(Vec<u8>),
}

impl std::fmt::Display for NetflowFieldValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uint(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::IpAddr(v) | Self::MacAddr(v) | Self::String(v) => write!(f, "{v}"),
            Self::Bytes(b) => write!(f, "{} bytes", b.len()),
        }
    }
}

impl NetflowRecord {
    /// A field by name, rendered for display. The common 5-tuple names are
    /// `src_addr`/`dst_addr`/`src_port`/`dst_port`/`protocol`, but a template
    /// may name them anything, so a missing field is `None`, not an error.
    pub fn field(&self, name: &str) -> Option<String> {
        self.fields.get(name).map(|v| v.to_string())
    }
}

/// One log2 histogram bucket: count of samples with latency `< le_us` µs that
/// did not fall in a lower bucket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HistBucket {
    /// Upper bound of this bucket, in microseconds.
    pub le_us: u64,
    /// Number of samples in this bucket over the window.
    pub count: u64,
}

/// A latency histogram with derived percentiles (all µs).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Histogram {
    pub unit: String,
    pub buckets: Vec<HistBucket>,
    pub total: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
}

/// The `@rpc/sysinfo/latency` reply (#99): both saturation histograms over the
/// last window.
///
/// These are the two questions a load average cannot answer — *is the CPU
/// oversubscribed* (runqlat: how long a runnable task waits for a core) and *is
/// the disk the bottleneck* (biolatency: how long a block-I/O request takes).
/// They are histograms rather than gauges because the tail is the finding: a
/// p99 run-queue delay of 40 ms with a p50 of 20 µs is a stall that a mean
/// would hide completely.
///
/// The type lives here, not in the sensor, because a reply type only a producer
/// can name is a reply nobody can read — which is how this procedure went
/// unconsumed from the day it was written (#469).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LatencyReport {
    /// False when the eBPF collector could not load (no caps / unsupported
    /// kernel / not built with `--features ebpf`). The histograms are empty.
    pub available: bool,
    /// Window the bucket counts cover, in seconds.
    pub window_secs: u64,
    /// Scheduler run-queue latency (runqlat).
    pub runqlat: Histogram,
    /// Block-I/O latency (biolatency).
    pub biolatency: Histogram,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Old producers don't emit the #308 name fields — records must still
    /// decode (additive-compatibility pin for `dst_names` / `names`).
    #[test]
    fn flow_and_talker_records_decode_without_name_fields() {
        let flow: FlowRecord = serde_json::from_str(
            r#"{"src":"10.0.0.5:40000","dst":"93.184.216.34:443","proto":"tcp",
                "bytes":100,"packets":2,"duration_ms":5,"reason":"fin"}"#,
        )
        .unwrap();
        assert!(flow.dst_names.is_empty());

        let talker: TalkerRecord =
            serde_json::from_str(r#"{"src":"10.0.0.5","bytes_per_sec":12345.0}"#).unwrap();
        assert!(talker.names.is_empty());
    }

    /// The #308 name fields round-trip with their provenance slugs intact.
    #[test]
    fn name_info_round_trips() {
        let rec = TalkerRecord {
            src: "10.0.0.5".into(),
            bytes_per_sec: 4096.0,
            names: vec![NameInfo {
                name: "beacon.evil.example".into(),
                provenance: "dns_a".into(),
            }],
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<TalkerRecord>(&json).unwrap(), rec);
    }

    /// A `LogRecord` round-trips through its `TelemetryPoint` bridge without
    /// losing typed fields or spillover labels (`sd.journald.*`, templates,
    /// raw) — the losslessness contract behind `@rpc/logs/events` (#358).
    #[test]
    fn log_record_point_round_trip() {
        let mut labels = std::collections::HashMap::new();
        labels.insert("facility".to_string(), "auth".to_string());
        labels.insert("severity".to_string(), "err".to_string());
        labels.insert("severity_number".to_string(), "17".to_string());
        labels.insert("severity_text".to_string(), "ERROR".to_string());
        labels.insert(
            "log.record.uid".to_string(),
            "0000001719999000000000000042".to_string(),
        );
        labels.insert("app".to_string(), "sshd".to_string());
        labels.insert("pid".to_string(), "4242".to_string());
        labels.insert("sd.journald.unit".to_string(), "sshd.service".to_string());
        labels.insert("template_id".to_string(), "t-77".to_string());
        labels.insert("raw".to_string(), "<38>raw line".to_string());
        let point = crate::TelemetryPoint {
            timestamp: 1_719_999_000_000,
            source: "web01".to_string(),
            protocol: crate::Protocol::Logs,
            metric: "events/0000001719999000000000000042".to_string(),
            value: crate::TelemetryValue::Text("Failed password for root".to_string()),
            labels,
            unit: None,
        };

        let rec = LogRecord::from_point(&point).expect("log line converts");
        assert_eq!(rec.uid, "0000001719999000000000000042");
        assert_eq!(rec.host, "web01");
        assert_eq!(rec.severity_number, 17);
        assert_eq!(rec.app.as_deref(), Some("sshd"));
        assert_eq!(rec.pid.as_deref(), Some("4242"));
        // Untyped labels spill over.
        assert_eq!(
            rec.labels.get("sd.journald.unit").map(String::as_str),
            Some("sshd.service")
        );
        assert_eq!(
            rec.labels.get("severity_text").map(String::as_str),
            Some("ERROR")
        );

        // The bridge back reproduces the original point exactly.
        // (TelemetryPoint doesn't derive PartialEq — compare field-wise.)
        let back = rec.to_point();
        assert_eq!(back.timestamp, point.timestamp);
        assert_eq!(back.source, point.source);
        assert_eq!(back.metric, point.metric);
        assert_eq!(back.value, point.value);
        assert_eq!(back.labels, point.labels);

        // And the record itself round-trips as JSON.
        let json = serde_json::to_string(&rec).unwrap();
        assert_eq!(serde_json::from_str::<LogRecord>(&json).unwrap(), rec);
    }

    /// Rollup/derived points (non-Text payloads) are not log lines.
    #[test]
    fn log_record_rejects_rollups() {
        let point = crate::TelemetryPoint {
            timestamp: 1,
            source: "web01".to_string(),
            protocol: crate::Protocol::Logs,
            metric: "logs/errors_total".to_string(),
            value: crate::TelemetryValue::Counter(5),
            labels: Default::default(),
            unit: None,
        };
        assert!(LogRecord::from_point(&point).is_none());
    }
}
