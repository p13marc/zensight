//! Bandwidth-by-process / service vocabulary (epic #320).
//!
//! No single OS source can own per-process/per-service network bandwidth, so the
//! feature tiers by measurement source and **labels every value with its
//! byte-semantics** — app-goodput (socket hooks / `tcp_info`: no headers, no
//! retransmits) < wire-L3 (cgroup_skb / systemd: no L2) < wire-L2 (capture: full
//! frame). Two differently-tagged numbers must never be summed or compared
//! without the semantics shown. This module is the shared vocabulary (enums +
//! labels + the `bandwidth` read-procedure record shape) so the sensor tiers and the
//! GUI can't drift.
//!
//! Per-**service** bandwidth (systemd units, low cardinality) is **streamed** as
//! `unit/<name>/{ip_ingress_bps,ip_egress_bps}` telemetry with the labels below.
//! Per-**process** bandwidth (netlink sock_diag / eBPF, high cardinality) is
//! **query-only** — a [`BandwidthRecord`] served on `@rpc/<producer>/bandwidth` (principle
//! P2: high-cardinality tables are never streamed onto the telemetry bus).

use serde::{Deserialize, Serialize};

use crate::telemetry::Protocol;

/// Telemetry/record label key naming the measurement source (`bw.source`).
pub const LABEL_SOURCE: &str = "bw.source";
/// Telemetry/record label key naming the byte-semantics (`bw.semantics`).
pub const LABEL_SEMANTICS: &str = "bw.semantics";

/// Where a bandwidth number was measured. Ordered cheapest→completest is not
/// meaningful; this only names the source so the GUI can badge it and pick the
/// best per host (eBPF > sock_diag for per-process; systemd for per-service).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandwidthSource {
    /// Kernel eBPF socket/UDP hooks — race-free, per-process, TCP+UDP (#316).
    Ebpf,
    /// sock_diag `tcp_info` byte deltas — unprivileged, per-process, TCP-only (#317).
    SockDiag,
    /// systemd IPAccounting (cgroup_skb) — per-service/cgroup, all protocols (#315).
    Systemd,
    /// Wire capture flow⋈socket attribution — per-process, best-effort (#318).
    Netring,
}

impl BandwidthSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            BandwidthSource::Ebpf => "ebpf",
            BandwidthSource::SockDiag => "sock_diag",
            BandwidthSource::Systemd => "systemd",
            BandwidthSource::Netring => "netring",
        }
    }
}

/// What the counted bytes include — the crux of "never compare blindly".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ByteSemantics {
    /// Application payload only (socket hooks / `tcp_info`) — no headers, no
    /// retransmits. Reads **below** NIC/wire counters.
    AppGoodput,
    /// L3+ bytes (cgroup_skb / systemd IPAccounting) — includes IP/transport
    /// headers and retransmits but **not** L2 framing.
    WireL3,
    /// Full frame incl. Ethernet (packet capture) — the highest count.
    WireL2,
}

impl ByteSemantics {
    pub fn as_str(&self) -> &'static str {
        match self {
            ByteSemantics::AppGoodput => "app-goodput",
            ByteSemantics::WireL3 => "wire-l3",
            ByteSemantics::WireL2 => "wire-l2",
        }
    }

    /// Short human label for the GUI badge.
    pub fn badge(&self) -> &'static str {
        match self {
            ByteSemantics::AppGoodput => "goodput",
            ByteSemantics::WireL3 => "wire-L3",
            ByteSemantics::WireL2 => "wire-L2",
        }
    }
}

/// Which protocols a bandwidth number covers (sock_diag is structurally TCP-only:
/// `udp_diag` has no per-socket byte counters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtoScope {
    /// TCP only (the sock_diag limit).
    Tcp,
    /// TCP and UDP (the eBPF tier).
    TcpUdp,
    /// All protocols (cgroup_skb / capture).
    All,
}

impl ProtoScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProtoScope::Tcp => "tcp",
            ProtoScope::TcpUdp => "tcp+udp",
            ProtoScope::All => "all",
        }
    }
}

/// What a [`BandwidthRecord`] is keyed by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BandwidthKey {
    /// A process — identified by `(pid, start_time)` (PIDs get reused), with the
    /// kernel `comm` for display.
    Process {
        pid: i32,
        start_time: u64,
        comm: String,
    },
    /// A systemd unit / cgroup service.
    Service { unit: String },
    /// A raw cgroup path (`system.slice/<...>`), when not a named unit.
    Cgroup { path: String },
}

/// One row of the on-demand `@rpc/<producer>/bandwidth` per-process table (#317/#316).
/// `tx`/`rx` are bytes-per-second derived from cumulative deltas by the producer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandwidthRecord {
    pub key: BandwidthKey,
    /// Transmit rate, bytes/sec.
    pub tx_bps: f64,
    /// Receive rate, bytes/sec.
    pub rx_bps: f64,
    pub source: BandwidthSource,
    pub semantics: ByteSemantics,
    pub proto: ProtoScope,
    /// Owning host `source`/entity id, for cross-host rollup in the GUI. `None`
    /// when the producer didn't stamp it (single-host context).
    #[serde(default)]
    pub host: Option<String>,
}

/// The fleet-wide v1 `bandwidth` read procedure for on-demand per-process
/// bandwidth, served by the netlink/eBPF/netring tiers. GET it with a
/// `?by=process|socket;top=<N>` selector and query target `All` (RFC 05 §2).
///
/// ```
/// use zensight_common::bandwidth::bandwidth_query_key;
/// use zensight_common::Protocol;
/// assert_eq!(
///     bandwidth_query_key(Protocol::Netlink),
///     "zensight/@v1/*/@rpc/netlink/bandwidth"
/// );
/// ```
pub fn bandwidth_query_key(protocol: Protocol) -> String {
    crate::keyexpr::fleet_rpc_key(protocol.as_str(), "bandwidth")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_as_str_is_stable() {
        assert_eq!(BandwidthSource::SockDiag.as_str(), "sock_diag");
        assert_eq!(ByteSemantics::AppGoodput.as_str(), "app-goodput");
        assert_eq!(ByteSemantics::WireL3.as_str(), "wire-l3");
        assert_eq!(ProtoScope::Tcp.as_str(), "tcp");
        assert_eq!(ProtoScope::TcpUdp.as_str(), "tcp+udp");
    }

    #[test]
    fn record_round_trips_json() {
        let rec = BandwidthRecord {
            key: BandwidthKey::Process {
                pid: 42,
                start_time: 12345,
                comm: "curl".into(),
            },
            tx_bps: 1024.0,
            rx_bps: 2_000_000.5,
            source: BandwidthSource::SockDiag,
            semantics: ByteSemantics::AppGoodput,
            proto: ProtoScope::Tcp,
            host: Some("web01".into()),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let back: BandwidthRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
        // The tagged key survives.
        assert!(json.contains("\"kind\":\"process\""));
    }

    #[test]
    fn service_key_round_trips() {
        let rec = BandwidthRecord {
            key: BandwidthKey::Service {
                unit: "nginx.service".into(),
            },
            tx_bps: 0.0,
            rx_bps: 0.0,
            source: BandwidthSource::Systemd,
            semantics: ByteSemantics::WireL3,
            proto: ProtoScope::All,
            host: None,
        };
        let back: BandwidthRecord =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn query_key_is_the_fleet_procedure_selector() {
        assert_eq!(
            bandwidth_query_key(Protocol::Netlink),
            "zensight/@v1/*/@rpc/netlink/bandwidth"
        );
    }
}
