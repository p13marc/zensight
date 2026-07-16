//! Configuration for the netlink sensor.

use serde::{Deserialize, Serialize};
use zensight_sensor_core::{LoggingConfig, SensorConfig, ZenohConfig};

fn default_source() -> String {
    "auto".to_string()
}
fn default_poll() -> u64 {
    5
}
fn default_true() -> bool {
    true
}

/// Root configuration loaded from JSON5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetlinkSensorConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    /// Serialization format for telemetry.
    #[serde(default)]
    pub serialization: zensight_common::serialization::Format,
    #[serde(default)]
    pub logging: LoggingConfig,
    /// On-demand artifact channel (`@rpc/netlink/artifact/*`) limits — report + snapshot.
    /// Every kind disabled by default.
    #[serde(default)]
    pub artifacts: zensight_sensor_core::ArtifactLimits,
    pub netlink: NetlinkConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetlinkConfig {
    /// Host identifier used as telemetry `source`. "auto" detects the hostname.
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub collect: CollectConfig,
    /// Real-time RTNETLINK event stream tuning (recent-events ring size).
    #[serde(default)]
    pub events: EventsConfig,
    #[serde(default)]
    pub interfaces: IfaceFilter,
    /// WireGuard peer monitoring (handshake age, rx/tx, up/down). Needs the
    /// `wireguard` kernel module; full peer data needs CAP_NET_ADMIN.
    #[serde(default)]
    pub wireguard: WireguardConfig,
    /// Pillar B — declared expectations for this host (sentinel). When present,
    /// the sensor evaluates them and emits alerts on deviation.
    #[serde(default)]
    pub expectations: Option<crate::sentinel::ExpectationsConfig>,
    /// Opt-in eBPF module tuning (#114). Only used when `collect.ebpf` is set on
    /// a binary built with `--features ebpf`.
    #[serde(default)]
    pub ebpf: EbpfConfig,
    /// Host-evidence feed (#307): republish observed neighbors as third-party
    /// identity evidence on `state/netlink/evidence/**` for the correlator.
    #[serde(default)]
    pub evidence: EvidenceConfig,
}

/// Host-evidence feed tuning (#307). Governs how the sensor republishes
/// observed neighbors (ARP/NDP cache) as third-party identity claims onto the
/// `state/netlink/evidence/**` keyspace for the correlator (epic #312).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceConfig {
    /// Publish observed-neighbor evidence at all. `true` by default.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Minimum seconds between evidence feed runs — the neighbor poll cadence is
    /// usually faster, so this floors publish churn per source. Default 60.
    #[serde(default = "default_evidence_min_interval")]
    pub min_interval_secs: u64,
    /// Re-emit every live claim at least this often (liveness refresh) so the
    /// correlator's TTL never lapses. Default 420 (≤ evidence_ttl/2). Changed
    /// claims publish on the next feed run regardless.
    #[serde(default = "default_evidence_refresh")]
    pub refresh_secs: u64,
    /// Hard cap on evidence records emitted per feed run; the remainder waits for
    /// the next run. Default 200.
    #[serde(default = "default_evidence_max_per_tick")]
    pub max_per_tick: usize,
}

fn default_evidence_min_interval() -> u64 {
    60
}
fn default_evidence_refresh() -> u64 {
    420
}
fn default_evidence_max_per_tick() -> usize {
    200
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_interval_secs: default_evidence_min_interval(),
            refresh_secs: default_evidence_refresh(),
            max_per_tick: default_evidence_max_per_tick(),
        }
    }
}

/// Tuning for the opt-in eBPF module (issue #114).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EbpfConfig {
    /// Capacity of the recent-connections ring served via `@rpc/netlink/connections`.
    #[serde(default = "default_conn_ring")]
    pub conn_ring_capacity: usize,
    /// Number of top retransmit peers returned by `@rpc/netlink/retransmits`.
    #[serde(default = "default_top_k")]
    pub retransmit_top_k: usize,
}

fn default_conn_ring() -> usize {
    256
}
fn default_top_k() -> usize {
    20
}

impl Default for EbpfConfig {
    fn default() -> Self {
        Self {
            conn_ring_capacity: default_conn_ring(),
            retransmit_top_k: default_top_k(),
        }
    }
}

/// WireGuard monitoring config. Lists the WG interfaces to poll (empty = off).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WireguardConfig {
    /// WG interface names to monitor, e.g. `["wg0"]`.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// A peer is "up" when its last handshake is within this many seconds.
    #[serde(default = "default_wg_stale")]
    pub stale_after_secs: u64,
    /// Paths to `wg-quick` config files (`*.conf`) used to enrich peer labels
    /// with their AllowedIPs / endpoint for readable GUI display (#268). Peers
    /// not present in any config keep their short-pubkey label. Empty = disabled.
    #[serde(default)]
    pub wg_quick_configs: Vec<String>,
}

fn default_wg_stale() -> u64 {
    180
}

/// Tuning for the real-time RTNETLINK event stream (issue #8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsConfig {
    /// Capacity of the recent-events ring served via `@rpc/netlink/events`.
    #[serde(default = "default_event_ring")]
    pub ring_capacity: usize,
}

fn default_event_ring() -> usize {
    256
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            ring_capacity: default_event_ring(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectConfig {
    /// Per-interface counters + state.
    #[serde(default = "default_true")]
    pub interfaces: bool,
    /// TCP socket-state aggregates (sockdiag).
    #[serde(default = "default_true")]
    pub sockets: bool,
    /// ARP/NDP neighbor state summary.
    #[serde(default = "default_true")]
    pub neighbors: bool,
    /// Routing-table summary.
    #[serde(default = "default_true")]
    pub routes: bool,
    /// nlink built-in diagnostics scan (bottleneck score + issue counts).
    #[serde(default = "default_true")]
    pub diagnostics: bool,
    /// Real-time RTNETLINK event stream (link/addr/route/neighbor add/del):
    /// event counters + recent-events ring + instant sentinel re-eval (#8).
    #[serde(default = "default_true")]
    pub events: bool,
    /// ethtool link speed/duplex/autoneg, ring sizes, offloads, pause (#9).
    #[serde(default = "default_true")]
    pub ethtool: bool,
    /// IP address inventory summary (per-family + global counts) (#10).
    #[serde(default = "default_true")]
    pub addresses: bool,
    /// TC/QoS qdisc stats (drops/overlimits/backlog) per interface (#12). Read is
    /// unprivileged; absent where no qdiscs are configured.
    #[serde(default = "default_true")]
    pub tc: bool,
    /// XFRM/IPsec SA + policy health (#13). Read is unprivileged; empty where no
    /// IPsec is configured.
    #[serde(default = "default_true")]
    pub xfrm: bool,
    /// nftables table/chain/rule counters (#14). Listing rules typically needs
    /// CAP_NET_ADMIN, so OFF by default — enable on a firewall host.
    #[serde(default)]
    pub nftables: bool,
    /// Netfilter conntrack table summary (entries/proto/utilization). Requires
    /// CAP_NET_ADMIN, so OFF by default — enable on a NAT gateway / firewall.
    #[serde(default)]
    pub conntrack: bool,
    /// Opt-in eBPF module (#114): connect-latency gauges + per-peer retransmit
    /// attribution (`@rpc/netlink/retransmits`) + tcplife connection records
    /// (`@rpc/netlink/connections`). OFF by default. NO-OP unless the binary was
    /// built with `--features ebpf` AND holds CAP_BPF + CAP_PERFMON (loading a
    /// tracing program needs those; CAP_NET_ADMIN gates *networking* program
    /// types, which this module does not use).
    #[serde(default)]
    pub ebpf: bool,
    /// Socket→process attribution for the `@rpc/netlink/sockets` drill-down (#304):
    /// one `/proc/<pid>/fd` walk per query joins each socket inode to its
    /// owning process (pid/comm/start_time) and resolves the cgroup v2 id to
    /// its path. Unprivileged reads; other users' processes are skipped
    /// without CAP_SYS_PTRACE (run privileged for whole-system attribution).
    #[serde(default = "default_true")]
    pub socket_processes: bool,
    /// Skip the `/proc` fd walk (and reply unattributed) when the host has
    /// more than this many processes — a query-time cost ceiling (#304).
    #[serde(default = "default_socket_process_max_procs")]
    pub socket_process_max_procs: usize,
    /// Per-process TCP bandwidth via sock_diag goodput deltas (#317, epic #320):
    /// sample `tcp_info` byte counters on a cadence, diff per cookie, and serve
    /// per-process rate on `@rpc/netlink/bandwidth`. Unprivileged, **TCP-only**
    /// (`udp_diag` has no per-socket byte counters). The pid join reuses
    /// `socket_processes` (attribution off ⇒ everything folds into the
    /// `unattributed` bucket). ON by default.
    #[serde(default = "default_true")]
    pub bandwidth: bool,
}

fn default_socket_process_max_procs() -> usize {
    4096
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            interfaces: true,
            sockets: true,
            neighbors: true,
            routes: true,
            diagnostics: true,
            events: true,
            ethtool: true,
            addresses: true,
            tc: true,
            xfrm: true,
            nftables: false,
            conntrack: false,
            ebpf: false,
            socket_processes: true,
            socket_process_max_procs: default_socket_process_max_procs(),
            bandwidth: true,
        }
    }
}

/// Interface include/exclude filtering.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IfaceFilter {
    /// Only include these interfaces (empty = all).
    #[serde(default)]
    pub include: Vec<String>,
    /// Exclude these interfaces.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Exclude the loopback interface.
    #[serde(default)]
    pub exclude_loopback: bool,
    /// Exclude common virtual interfaces (docker*, veth*, br-*, virbr*, vnet*).
    #[serde(default)]
    pub exclude_virtual: bool,
}

impl IfaceFilter {
    /// Whether an interface name passes the filter.
    pub fn should_include(&self, name: &str) -> bool {
        if self.exclude_loopback && name == "lo" {
            return false;
        }
        if self.exclude_virtual && is_virtual(name) {
            return false;
        }
        if self.exclude.iter().any(|e| e == name) {
            return false;
        }
        if !self.include.is_empty() {
            return self.include.iter().any(|i| i == name);
        }
        true
    }
}

fn is_virtual(name: &str) -> bool {
    const PREFIXES: &[&str] = &["docker", "veth", "br-", "virbr", "vnet", "tap"];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

impl NetlinkConfig {
    /// Resolve the configured source id, detecting the hostname when set to "auto".
    pub fn resolved_source(&self) -> String {
        if self.source == "auto" {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.source.clone()
        }
    }
}

impl SensorConfig for NetlinkSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }
    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }
    fn producer(&self) -> &str {
        "netlink"
    }
    fn artifact_limits(&self) -> zensight_sensor_core::ArtifactLimits {
        self.artifacts.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_loopback_and_virtual() {
        let f = IfaceFilter {
            exclude_loopback: true,
            exclude_virtual: true,
            ..Default::default()
        };
        assert!(!f.should_include("lo"));
        assert!(!f.should_include("docker0"));
        assert!(!f.should_include("veth1234"));
        assert!(f.should_include("eth0"));
    }

    #[test]
    fn filter_include_list() {
        let f = IfaceFilter {
            include: vec!["eth0".into()],
            ..Default::default()
        };
        assert!(f.should_include("eth0"));
        assert!(!f.should_include("eth1"));
    }

    #[test]
    fn filter_exclude_list() {
        let f = IfaceFilter {
            exclude: vec!["eth1".into()],
            ..Default::default()
        };
        assert!(f.should_include("eth0"));
        assert!(!f.should_include("eth1"));
    }

    #[test]
    fn parse_minimal_config() {
        let cfg: NetlinkSensorConfig = json5::from_str(r#"{ netlink: { source: "h1" } }"#).unwrap();
        assert_eq!(cfg.netlink.resolved_source(), "h1");
        assert!(cfg.netlink.collect.interfaces);
    }
}
