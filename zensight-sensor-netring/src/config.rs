//! Configuration for the netring sensor.

use serde::{Deserialize, Serialize};
use zensight_sensor_core::{LoggingConfig, SensorConfig, ZenohConfig};

fn default_key_prefix() -> String {
    "zensight/netring".to_string()
}
fn default_sensor_id() -> String {
    "auto".to_string()
}
fn default_bw_period() -> u64 {
    5
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetringSensorConfig {
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    pub netring: NetringConfig,
    /// On-demand debug-report (`@/report`) limits. Disabled by default.
    #[serde(default)]
    pub report: zensight_sensor_core::ReportLimits,
    /// Tier-2 directory-snapshot (`@/snapshot`) limits. Disabled by default.
    #[serde(default)]
    pub snapshot: zensight_sensor_core::SnapshotLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetringConfig {
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    /// Sensor identifier used as telemetry `source`. "auto" → hostname.
    #[serde(default = "default_sensor_id")]
    pub sensor_id: String,
    /// Capture interfaces (e.g. ["eth0"]). Ignored when `pcap` is set.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// Capture backend selection (netring 0.28, issue #227). `auto` (default)
    /// probes the host/interface and picks the best available backend, logging
    /// the choice; explicit values force it. `pcap` always overrides (replay).
    #[serde(default)]
    pub backend: BackendKind,
    /// Replay an offline pcap instead of live capture (no privileges needed).
    #[serde(default)]
    pub pcap: Option<String>,
    #[serde(default)]
    pub collect: CollectConfig,
    #[serde(default = "default_bw_period")]
    pub bandwidth_period_secs: u64,
    #[serde(default)]
    pub anomalies: AnomalyConfig,
    /// Threat-intel detection (netring 0.27): flow-risk scoring, IOC matching,
    /// Sigma rules. Hits surface as anomalies → alerts via the same path as the
    /// built-in detectors.
    #[serde(default)]
    pub threat: ThreatConfig,
    /// Capture-overload detection (netring 0.27): watch the windowed capture
    /// drop-rate and raise/clear a `capture-overload` SensorHealth alert on the
    /// debounced Normal↔Emergency transition. Needs `collect.capture_stats`.
    #[serde(default)]
    pub overload: OverloadConfig,
    /// Runtime capture-focus filter (netring 0.28, issue #225). Off by default.
    /// When enabled, registers a reloadable packet-tier subscription whose BPF
    /// filter can be hot-swapped at runtime via `@/commands/capture_filter`
    /// (no capture restart) to narrow attention to a host/port under
    /// investigation, with focused packet/byte counters as the visible effect.
    #[serde(default)]
    pub capture_focus: CaptureFocusConfig,
}

/// Runtime capture-focus config (netring 0.28, issue #225). Opt-in because the
/// packet-tier handler runs in the zero-copy drain (a per-frame cost the default
/// build avoids).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureFocusConfig {
    /// Arm the reloadable capture-focus subscription. `false` (default) → no
    /// packet sub, zero-cost hot loop, `@/commands/capture_filter` is a no-op.
    #[serde(default)]
    pub enabled: bool,
    /// Base packet filter (netring `.expr()` / tcpdump-like grammar) installed
    /// at startup and restored by `clear_packet_filter`. Permissive by default
    /// so a runtime narrow can focus within it.
    #[serde(default = "default_focus_expr")]
    pub base_expr: String,
}

fn default_focus_expr() -> String {
    // netring `.expr()` grammar: `tcp|udp|icmp`, `dir? port N`, `dir? host IP`,
    // `dir? net CIDR`, combined with and/or/!/parens. This permissive base
    // matches all L4 we track, leaving room to narrow at runtime.
    "tcp or udp or icmp".to_string()
}

impl Default for CaptureFocusConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_expr: default_focus_expr(),
        }
    }
}

/// Capture backend selection (netring 0.28, issue #227). Maps to netring's
/// `Backend`. `AfXdp` only takes effect in a build with AF_XDP support; this
/// sensor doesn't enable those netring features, so it falls back to `Auto`
/// (which resolves to AF_PACKET) with a warning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Probe + auto-select (logs the choice). Resolves to AF_PACKET here.
    #[default]
    Auto,
    /// Force AF_PACKET (TPACKET_v3).
    #[serde(rename = "afpacket")]
    AfPacket,
    /// Request AF_XDP — needs an AF_XDP-enabled build; falls back to Auto here.
    #[serde(rename = "afxdp")]
    AfXdp,
}

/// Capture-overload detection config (netring 0.27). Drives an
/// `OverloadDetector` off the windowed drop-rate with Suricata-style hysteresis
/// (enter high, recover low after N calm windows) so it doesn't flap.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverloadConfig {
    /// Enable overload detection (no-op without `collect.capture_stats`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Enter Emergency when the windowed drop-rate reaches this fraction
    /// (`0.0..=1.0`). Default 0.05 (5%).
    #[serde(default = "default_enter_drop_rate")]
    pub enter_drop_rate: f64,
    /// Recover to Normal only after the drop-rate stays below this for
    /// `recover_windows` samples. Default 0.01 (1%).
    #[serde(default = "default_recover_drop_rate")]
    pub recover_drop_rate: f64,
    /// Consecutive calm windows required to recover. Default 3.
    #[serde(default = "default_recover_windows")]
    pub recover_windows: u32,
    /// Active load-shedding under overload (netring 0.28, issue #224). Off by
    /// default: when disabled the sensor only *detects* overload (today's
    /// behaviour). When enabled it *deliberately* sheds new flows at the
    /// dispatch boundary while in Emergency — honest, counted drops instead of
    /// opaque kernel loss. Needs `enabled` (detection) to fire.
    #[serde(default)]
    pub shed: ShedConfig,
}

/// Load-shedding policy + arming (netring 0.28, issue #224). Couples with the
/// surrounding [`OverloadConfig`] hysteresis: shedding is only ever *active*
/// while the detector is in Emergency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShedConfig {
    /// Arm shedding. `false` (default) → detection only, byte-for-byte today.
    #[serde(default)]
    pub enabled: bool,
    /// `"new_flows"` → admit no new flows while overloaded (keep tracked ones);
    /// `"sample"` → admit a deterministic `sample_rate` fraction of new flows.
    #[serde(default = "default_shed_policy")]
    pub policy: ShedPolicyKind,
    /// Fraction of new flows to keep under the `"sample"` policy (`0.0..=1.0`).
    /// Ignored by `"new_flows"`. Default 0.5.
    #[serde(default = "default_sample_rate")]
    pub sample_rate: f64,
}

/// Which shed policy to apply while overloaded (issue #224).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShedPolicyKind {
    /// Drop every new flow while in Emergency (keep already-tracked flows).
    NewFlows,
    /// Keep a deterministic `sample_rate` fraction of new flows; shed the rest.
    Sample,
}

fn default_shed_policy() -> ShedPolicyKind {
    ShedPolicyKind::NewFlows
}
fn default_sample_rate() -> f64 {
    0.5
}

impl Default for ShedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            policy: default_shed_policy(),
            sample_rate: default_sample_rate(),
        }
    }
}

fn default_enter_drop_rate() -> f64 {
    0.05
}
fn default_recover_drop_rate() -> f64 {
    0.01
}
fn default_recover_windows() -> u32 {
    3
}

impl Default for OverloadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enter_drop_rate: default_enter_drop_rate(),
            recover_drop_rate: default_recover_drop_rate(),
            recover_windows: default_recover_windows(),
            shed: ShedConfig::default(),
        }
    }
}

/// Threat-intel detection config (netring 0.27).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreatConfig {
    /// nDPI-style passive flow-risk scoring (obsolete TLS, cleartext HTTP
    /// credentials). Requires the `tls`/`http` collectors for the respective arms.
    #[serde(default)]
    pub flow_risk: bool,
    /// Indicator-of-compromise matching (bad IPs / domains / JA3 / JA4).
    #[serde(default)]
    pub ioc: IocConfig,
    /// Sigma rule evaluation (needs the `sigma` build feature).
    #[serde(default)]
    pub sigma: SigmaConfig,
}

/// Indicator-of-compromise indicator set, from inline lists and/or files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IocConfig {
    /// Bad host IPs (matched against flow src/dst).
    #[serde(default)]
    pub ips: Vec<String>,
    /// Bad domains (subdomain-aware; matched against DNS qname / TLS SNI / HTTP Host).
    #[serde(default)]
    pub domains: Vec<String>,
    /// Bad JA4 TLS client fingerprints.
    #[serde(default)]
    pub ja4: Vec<String>,
    /// Bad JA3 TLS client fingerprints.
    #[serde(default)]
    pub ja3: Vec<String>,
    /// Files of newline-separated indicators (IP or domain inferred per line;
    /// `#` comments allowed). Useful for external IOC feeds.
    #[serde(default)]
    pub files: Vec<String>,
}

/// Sigma rule evaluation config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SigmaConfig {
    /// Enable Sigma evaluation (no-op unless built with the `sigma` feature).
    #[serde(default)]
    pub enabled: bool,
    /// Directory of `.yml` Sigma rules to load.
    #[serde(default)]
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectConfig {
    /// Per-application bandwidth (bytes/sec).
    #[serde(default = "default_true")]
    pub bandwidth: bool,
    /// Flow lifecycle aggregates.
    #[serde(default = "default_true")]
    pub flows: bool,
    /// TCP reset counters (resets + connection refusals).
    #[serde(default = "default_true")]
    pub tcp_resets: bool,
    /// Passive TLS fingerprinting (SNI + JA3/JA4 asset inventory). Needs capture
    /// (CAP_NET_RAW), same as all netring collection.
    #[serde(default = "default_true")]
    pub tls: bool,
    /// Capture self-health (packets/drops/drop_rate per source). Only fires on
    /// LIVE capture — the kernel ring has no drops to report under pcap replay.
    #[serde(default = "default_true")]
    pub capture_stats: bool,
    /// ICMP error telemetry (unreachable/time-exceeded/PMTU). Synthesised from
    /// the embedded inner packet — needs LIVE capture with real kernel ICMP to
    /// correlate; a synthetic pcap rarely triggers it (issue #15). Degrades to
    /// silent (zero counters) under replay. Default OFF (live-gated).
    #[serde(default)]
    pub icmp: bool,
    /// L7 DNS RED analytics (queries/rcodes/RTT/unanswered + top SLDs). Cleartext
    /// UDP/53 only. Default OFF (opt-in L7).
    #[serde(default)]
    pub dns: bool,
    /// L7 HTTP RED analytics (requests/status/methods/latency + top hosts).
    /// CLEARTEXT only (TCP/80,8080); TLS is opaque. Default OFF (opt-in L7).
    #[serde(default)]
    pub http: bool,
    /// Top-talkers + elephant-flows on-demand query channels (issue #21).
    #[serde(default = "default_true")]
    pub talkers: bool,
    /// L7 QUIC Initial visibility (netring 0.27): passive SNI/ALPN/version from
    /// the unprotected ClientHello on UDP/443 — the QUIC analogue of TLS SNI.
    /// Served on `@/query/quic`. Default OFF (opt-in L7).
    #[serde(default)]
    pub quic: bool,
    /// L7 SSH/HASSH visibility (netring 0.27): banner + KEXINIT HASSH handshake
    /// fingerprints on TCP/22, served on `@/query/ssh`. Default OFF (opt-in L7).
    #[serde(default)]
    pub ssh: bool,
    /// JA4H HTTP-request fingerprinting (issue #124). No-op unless built with
    /// `--features ja4plus` (FoxIO License 1.1 — non-OSI; default build stays
    /// OSI-clean). Cleartext HTTP only; served on `@/query/ja4h`. Default OFF.
    #[serde(default)]
    pub http_fp: bool,
    /// Flag cleartext SNMP v1/v2c community strings (netring 0.27) as anomalies
    /// → alerts. No-op unless built with `--features snmp`. Default OFF (opt-in).
    #[serde(default)]
    pub snmp_cleartext: bool,
    /// Passive asset inventory (netring 0.27): discover hosts on the wire from
    /// L2/L3 discovery traffic (ARP / NDP / LLDP / CDP) into a MAC-keyed
    /// inventory served on `@/query/assets`. Arming the discovery hooks narrows
    /// the kernel prefilter accordingly; needs capture (CAP_NET_RAW). Default
    /// OFF — opt-in, and CDP forces a capture-all prefilter (see `asset_cdp`).
    #[serde(default)]
    pub assets: bool,
    /// Also feed the asset inventory from CDP (Cisco Discovery Protocol).
    /// Separate flag because CDP rides 802.3 LLC/SNAP, which can't be expressed
    /// in the kernel prefilter — arming it forces capture-all (fail-open), so
    /// it's opt-in on top of `assets`. No effect unless `assets` is also set.
    #[serde(default)]
    pub asset_cdp: bool,
    /// Canonical IPFIX flow export (netring 0.28, issue #223). No-op unless
    /// built with `--features ipfix`. When set, serves IANA-IE-keyed flow
    /// records (`FlowRecord::to_ipfix_record`) on `@/query/ipfix` — per-direction
    /// deltas, both-direction totals, precise `flowEndReason`, Community ID — so
    /// a SIEM / flow collector consumes standard fields without re-deriving them.
    /// Default OFF (opt-in, standards export).
    #[serde(default)]
    pub ipfix: bool,
    /// TCP initiator inference (netring 0.28, issue #122). When on, the tracker
    /// uses SYN / SYN+ACK analysis to recover the true flow initiator even when
    /// the capture starts mid-handshake or the SYN+ACK races ahead — so flow
    /// records / matrix / talkers are labelled client → server regardless of
    /// capture endpoint order. Zero cost when off (falls back to first-packet
    /// order). Default ON; TCP-only (UDP flows stay first-packet best-effort).
    #[serde(default = "default_true")]
    pub infer_initiator: bool,
}

impl Default for CollectConfig {
    fn default() -> Self {
        Self {
            bandwidth: true,
            flows: true,
            tcp_resets: true,
            tls: true,
            capture_stats: true,
            icmp: false,
            dns: false,
            http: false,
            talkers: true,
            quic: false,
            ssh: false,
            http_fp: false,
            snmp_cleartext: false,
            assets: false,
            asset_cdp: false,
            ipfix: false,
            infer_initiator: true,
        }
    }
}

/// Anomaly detectors to run (Pillar A).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AnomalyConfig {
    /// Port-scan detection (TRW).
    #[serde(default)]
    pub port_scan: bool,
    /// RITA-style beaconing / C2 detection (issue #17). Flags periodic,
    /// size-consistent TCP flows (`score >= beacon_threshold`).
    #[serde(default)]
    pub beaconing: bool,
    /// Beaconing score threshold (0.0–1.0); higher = stricter. Default 0.8.
    #[serde(default = "default_beacon_threshold")]
    pub beacon_threshold: f64,
    /// RITA-style ROBUST beaconing detector (issue #118): Bowley skewness + MAD,
    /// bit-faithful to RITA — catches jittered C2 (e.g. Cobalt Strike jitter)
    /// that the coarser CV `beaconing` detector misses. Flags TCP flows whose
    /// composite RITA score `>= rita_beacon_threshold`. Independent of `beaconing`.
    #[serde(default)]
    pub rita_beacon: bool,
    /// RITA beacon score threshold (0.0–1.0); higher = stricter. Default 0.9.
    #[serde(default = "default_rita_beacon_threshold")]
    pub rita_beacon_threshold: f64,
    /// DNS tunneling detection (issue #118): flags a (src, SLD) whose distinct
    /// subdomain-label cardinality over a sliding window crosses
    /// `dns_tunnel_distinct`, or any single query name at/above
    /// `dns_tunnel_qname_len` bytes (classic exfil-via-qname). Requires
    /// `collect.dns`.
    #[serde(default)]
    pub dns_tunnel: bool,
    /// Distinct subdomain labels per (src, SLD) over the window before flagging
    /// a tunnel. Default 50.
    #[serde(default = "default_dns_tunnel_distinct")]
    pub dns_tunnel_distinct: usize,
    /// Query-name length (bytes) at/above which a single query is flagged as
    /// tunnel-shaped. Default 100.
    #[serde(default = "default_dns_tunnel_qname_len")]
    pub dns_tunnel_qname_len: usize,
    /// Newly-Observed-Domain detection (issue #118): emit an Info anomaly on the
    /// first sight of a second-level domain (allowlist-friendly). Bounded LRU
    /// seen-set. Requires `collect.dns`.
    #[serde(default)]
    pub nod: bool,
    /// Connection-flood detection (issue #18): many TCP connections to one
    /// (dst,port) in a short window — distinct from a port scan (many ports).
    #[serde(default)]
    pub connection_flood: bool,
    /// Connection-flood threshold: connections to one (dst,port) per window.
    #[serde(default = "default_flood_threshold")]
    pub flood_threshold: u64,
    /// DGA / DNS-tunneling scoring on each query SLD (issue #18). Requires
    /// `collect.dns`. Flags queries whose bigram log-likelihood is below
    /// `dga_threshold` (more negative = more random-looking).
    #[serde(default)]
    pub dga: bool,
    /// DGA log-likelihood threshold (negative). Default -8.0 (moderately
    /// aggressive — matches netring's `dga_query` example).
    #[serde(default = "default_dga_threshold")]
    pub dga_threshold: f64,
    /// Lateral-movement detection (#123): SMB admin-share / IPC$ service-pipe
    /// access, RDP connection requests, and Kerberos kerberoast/weak-etype/
    /// brute-force signals → alerts. Requires the `lateral` build feature (pulls
    /// the SMB/RDP/Kerberos parsers); a no-op when built without it. Default off.
    #[serde(default)]
    pub lateral_movement: bool,
    /// Data-exfiltration detection (#123): flags a flow whose outbound bytes
    /// exceed its per-source learned baseline by `exfil_sigma` standard
    /// deviations (and the `exfil_min_bytes` floor). Requires `collect.flows`.
    /// Default off.
    #[serde(default)]
    pub data_exfil: bool,
    /// Sigma multiplier a flow must exceed its source baseline by to flag exfil.
    /// Default 4.0.
    #[serde(default = "default_exfil_sigma")]
    pub exfil_sigma: f64,
    /// Absolute outbound-byte floor below which exfil never fires (so a quiet
    /// host's first modest upload can't trip a near-zero baseline). Default 10MB.
    #[serde(default = "default_exfil_min_bytes")]
    pub exfil_min_bytes: u64,
    /// Hostnames/SLDs to never alert on (allowlist for the noisy detectors:
    /// beaconing telemetry agents + DGA-scored CDN/randomised-but-benign SLDs).
    #[serde(default)]
    pub allowlist: Vec<String>,
}

fn default_beacon_threshold() -> f64 {
    0.8
}
fn default_rita_beacon_threshold() -> f64 {
    0.9
}
fn default_dns_tunnel_distinct() -> usize {
    50
}
fn default_dns_tunnel_qname_len() -> usize {
    100
}
fn default_flood_threshold() -> u64 {
    100
}
fn default_dga_threshold() -> f64 {
    -8.0
}
fn default_exfil_sigma() -> f64 {
    4.0
}
fn default_exfil_min_bytes() -> u64 {
    10 * 1024 * 1024
}

impl NetringConfig {
    pub fn resolved_sensor_id(&self) -> String {
        if self.sensor_id == "auto" {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .unwrap_or_else(|| "unknown".to_string())
        } else {
            self.sensor_id.clone()
        }
    }
}

impl SensorConfig for NetringSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }
    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }
    fn key_prefix(&self) -> &str {
        &self.netring.key_prefix
    }
    fn report_limits(&self) -> zensight_sensor_core::ReportLimits {
        self.report.clone()
    }

    fn snapshot_limits(&self) -> zensight_sensor_core::SnapshotLimits {
        self.snapshot.clone()
    }
    fn validate(&self) -> zensight_sensor_core::Result<()> {
        if self.netring.pcap.is_none() && self.netring.interfaces.is_empty() {
            return Err(zensight_sensor_core::SensorError::config(
                "netring: configure at least one interface, or set `pcap` for replay",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_interface() {
        let cfg: NetringSensorConfig =
            json5::from_str(r#"{ netring: { sensor_id: "s1", interfaces: ["eth0"] } }"#).unwrap();
        assert_eq!(cfg.netring.key_prefix, "zensight/netring");
        assert_eq!(cfg.netring.resolved_sensor_id(), "s1");
        assert!(cfg.netring.collect.bandwidth);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_requires_source() {
        let cfg: NetringSensorConfig = json5::from_str(r#"{ netring: {} }"#).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn new_anomaly_detectors_off_by_default_with_thresholds() {
        // `anomalies: {}` present → serde applies each field's documented
        // default (omitting the table entirely would use the derived `Default`).
        let cfg: NetringSensorConfig =
            json5::from_str(r#"{ netring: { interfaces: ["eth0"], anomalies: {} } }"#).unwrap();
        let a = &cfg.netring.anomalies;
        // Issue #118 detectors are opt-in.
        assert!(!a.rita_beacon);
        assert!(!a.dns_tunnel);
        assert!(!a.nod);
        // Thresholds carry their documented defaults.
        assert_eq!(a.rita_beacon_threshold, 0.9);
        assert_eq!(a.dns_tunnel_distinct, 50);
        assert_eq!(a.dns_tunnel_qname_len, 100);
    }

    #[test]
    fn pcap_satisfies_validation() {
        let cfg: NetringSensorConfig =
            json5::from_str(r#"{ netring: { pcap: "/tmp/x.pcap" } }"#).unwrap();
        assert!(cfg.validate().is_ok());
    }
}
