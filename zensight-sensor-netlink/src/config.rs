//! Configuration for the netlink sensor.

use serde::{Deserialize, Serialize};
use zensight_sensor_core::{LoggingConfig, SensorConfig, ZenohConfig};

fn default_key_prefix() -> String {
    "zensight/netlink".to_string()
}
fn default_hostname() -> String {
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
    #[serde(default)]
    pub logging: LoggingConfig,
    /// On-demand debug-report (`@/report`) limits. Disabled by default.
    #[serde(default)]
    pub report: zensight_sensor_core::ReportLimits,
    /// Tier-2 directory-snapshot (`@/snapshot`) limits. Disabled by default.
    #[serde(default)]
    pub snapshot: zensight_sensor_core::SnapshotLimits,
    pub netlink: NetlinkConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetlinkConfig {
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    /// Host identifier used as telemetry `source`. "auto" detects the hostname.
    #[serde(default = "default_hostname")]
    pub hostname: String,
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
}

/// Tuning for the opt-in eBPF module (issue #114).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EbpfConfig {
    /// Capacity of the recent-connections ring served via `@/query/connections`.
    #[serde(default = "default_conn_ring")]
    pub conn_ring_capacity: usize,
    /// Number of top retransmit peers returned by `@/query/retransmits`.
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
    /// Capacity of the recent-events ring served via `@/query/events`.
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
    /// attribution (`@/query/retransmits`) + tcplife connection records
    /// (`@/query/connections`). OFF by default. NO-OP unless the binary was
    /// built with `--features ebpf` AND holds CAP_BPF/CAP_NET_ADMIN.
    #[serde(default)]
    pub ebpf: bool,
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
    /// Resolve the configured hostname, detecting it when set to "auto".
    pub fn resolved_hostname(&self) -> String {
        if self.hostname == "auto" {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.hostname.clone()
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
    fn key_prefix(&self) -> &str {
        &self.netlink.key_prefix
    }
    fn report_limits(&self) -> zensight_sensor_core::ReportLimits {
        self.report.clone()
    }

    fn snapshot_limits(&self) -> zensight_sensor_core::SnapshotLimits {
        self.snapshot.clone()
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
        let cfg: NetlinkSensorConfig =
            json5::from_str(r#"{ netlink: { hostname: "h1" } }"#).unwrap();
        assert_eq!(cfg.netlink.key_prefix, "zensight/netlink");
        assert_eq!(cfg.netlink.resolved_hostname(), "h1");
        assert!(cfg.netlink.collect.interfaces);
    }
}
