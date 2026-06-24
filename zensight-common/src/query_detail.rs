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
}
