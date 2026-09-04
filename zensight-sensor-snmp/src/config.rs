use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use zensight_common::{Format, ZenohConfig};

// Re-export LoggingConfig from the framework for compatibility
pub use zensight_sensor_core::LoggingConfig;

/// Root configuration for the SNMP sensor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnmpSensorConfig {
    /// Zenoh connection settings.
    #[serde(default)]
    pub zenoh: ZenohConfig,

    /// Serialization format for telemetry.
    #[serde(default)]
    pub serialization: Format,

    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,

    /// SNMP-specific settings.
    pub snmp: SnmpConfig,

    /// On-demand artifact channel (`@rpc/snmp/artifact/*`) limits — report + snapshot.
    /// Every kind disabled by default.
    #[serde(default)]
    pub artifacts: zensight_sensor_core::ArtifactLimits,

    /// `@desired` reconcile settings (#931): the kill switch and refresh
    /// cadence. File config on purpose — the mechanism that could misbehave
    /// must be disarmable from outside itself.
    #[serde(default)]
    pub desired: zensight_common::desired::DesiredConfig,

    /// Operator-authored threshold rules over this sensor's own telemetry
    /// (#931). **Empty by default** — this build ships no threshold that
    /// fires. Also authorable fleet-wide on `@desired` and per-host over
    /// `@rpc/snmp/thresholds/set`; `state/snmp/applied/thresholds`
    /// says which of the three is in force.
    #[serde(default)]
    pub thresholds: zensight_common::threshold::ThresholdsConfig,
}

/// SNMP-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnmpConfig {
    /// Override the agent-host source id (default: the local hostname).
    #[serde(default)]
    pub source: Option<String>,

    /// Opt-in for SNMP v1/v2c devices (#825): a community string is a
    /// CLEARTEXT credential on the wire — netring even ships a detector for
    /// exactly this. `false` (the default) refuses to start with any
    /// v1/v2c-versioned device or trap community configured; setting it
    /// `true` is the explicit, logged acknowledgement of what it costs.
    /// SNMPv3 authPriv is the supported default and what the shipped config
    /// leads with.
    #[serde(default)]
    pub allow_insecure_versions: bool,

    /// SNMP trap listener configuration.
    #[serde(default)]
    pub trap_listener: TrapListenerConfig,

    /// Devices to poll.
    #[serde(default)]
    pub devices: Vec<DeviceConfig>,

    /// Predefined OID groups (reusable across devices).
    #[serde(default)]
    pub oid_groups: HashMap<String, OidGroup>,

    /// OID to human-readable name mapping.
    #[serde(default)]
    pub oid_names: HashMap<String, String>,

    /// MIB configuration.
    #[serde(default)]
    pub mib: MibConfig,

    /// Threshold alerting (#528). On by default; individual rules and the
    /// whole engine can be disabled, and any device can carry a full
    /// replacement block in `devices[].alerts`.
    #[serde(default)]
    pub alerts: crate::alerts::SnmpAlertsConfig,

    /// Publish the joined per-device `InterfaceTable` state doc (#529) from
    /// whatever IF-MIB columns each cycle walks. On by default.
    #[serde(default = "default_true")]
    pub publish_interfaces: bool,

    /// Device profiles (#531): curated OID sets matched by sysObjectID.
    #[serde(default)]
    pub profiles: ProfilesConfig,

    /// Observed-device identity evidence (#537).
    #[serde(default)]
    pub evidence: EvidenceConfig,

    /// Named credential sets (#538): one place to rotate a shared community
    /// or v3 user, referenced per device via `devices[].credentials`.
    #[serde(default)]
    pub credentials: HashMap<String, CredentialSet>,

    /// Gated PDU outlet control (#956, SYS-SUP-003). **Default off**, so
    /// every existing deployment is unaffected by this feature existing.
    #[serde(default)]
    pub actions: ActionsConfig,

    /// Resilience tuning (#539): backoff, circuit breaker, jitter.
    #[serde(default)]
    pub resilience: ResilienceConfig,

    /// Subnet auto-discovery (#541). Absent = no scanning, ever.
    #[serde(default)]
    pub discovery: Option<crate::discovery::DiscoveryConfig>,
}

/// Resilience configuration (#539).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ResilienceConfig {
    /// Poll-interval backoff cap as a multiple of the base interval
    /// (default 10× — exponential doubling stops here).
    #[serde(default = "default_backoff_cap")]
    pub backoff_cap: u32,

    /// Consecutive fully-failed cycles before the circuit breaker opens
    /// (probe-only polling; default 3).
    #[serde(default = "default_breaker_after")]
    pub breaker_after: u32,

    /// Per-cycle scheduling jitter in percent of the interval (default 10);
    /// the initial phase is randomized over the whole interval regardless.
    #[serde(default = "default_jitter_percent")]
    pub jitter_percent: u8,
}

fn default_backoff_cap() -> u32 {
    10
}

fn default_breaker_after() -> u32 {
    3
}

fn default_jitter_percent() -> u8 {
    10
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            backoff_cap: default_backoff_cap(),
            breaker_after: default_breaker_after(),
            jitter_percent: default_jitter_percent(),
        }
    }
}

/// Gated PDU outlet control (#956) — the strictest gate in the tree, and it
/// is arranged so the default configuration **cannot act at all**.
///
/// Four independent things must be true before an outlet cycles:
///
/// 1. `enabled` is set (default `false`);
/// 2. the target matches a pattern in `allow_outlets` (default **empty**,
///    which accepts nothing even with the switch on);
/// 3. `credentials` names a **separate write credential set** — startup
///    refuses `enabled` without one, because a read community that can reach a
///    SET is how a monitoring credential quietly becomes a control one;
/// 4. the device is pinned to a PDU profile whose control OIDs this build has
///    actually verified.
///
/// Any of them missing is a refusal that **names the switch that refused**
/// (#866), so an operator learns which of the four from the answer rather than
/// from the source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionsConfig {
    /// The master switch.
    #[serde(default)]
    pub enabled: bool,
    /// `<device>/<outlet>` glob patterns. **Empty rejects everything**, and
    /// there is deliberately no `allow_all`: a wildcard an operator typed is a
    /// decision, a wildcard a default provided is an accident.
    #[serde(default)]
    pub allow_outlets: Vec<String>,
    /// The name of a `snmp.credentials` set with **write** access. Never the
    /// device's read credential, and startup refuses `enabled` without it.
    #[serde(default)]
    pub credentials: Option<String>,
    /// How many outcomes the in-sensor ring keeps for `@rpc/snmp/actions`.
    #[serde(default = "default_action_history")]
    pub history_capacity: usize,
}

fn default_action_history() -> usize {
    64
}

impl Default for ActionsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_outlets: Vec::new(),
            credentials: None,
            history_capacity: default_action_history(),
        }
    }
}

/// One named credential set (#538). Either kind (or both) may be present;
/// values support `${ENV}` / `file:/path` indirection.
#[derive(Clone, Serialize, Deserialize)]
pub struct CredentialSet {
    /// v1/v2c community.
    #[serde(default)]
    pub community: Option<String>,

    /// SNMPv3 USM credentials.
    #[serde(default)]
    pub security: Option<SnmpV3Security>,
}

impl std::fmt::Debug for CredentialSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSet")
            .field("community", &self.community.as_ref().map(|_| "<redacted>"))
            .field("security", &self.security)
            .finish()
    }
}

/// MIB loading configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MibConfig {
    /// Load built-in MIB definitions (SNMPv2-MIB, IF-MIB, etc.).
    #[serde(default = "default_true")]
    pub load_builtin: bool,

    /// **Removed** (#580, deprecated in #532): the legacy JSON pseudo-MIB
    /// format is gone. The field is still parsed so a config that sets it
    /// fails startup with a pointer to `dirs` instead of being silently
    /// ignored; it goes away entirely next release.
    #[serde(default)]
    pub files: Vec<String>,

    /// Directories of standard SMI MIB files (`.mib`/`.txt`, vendor files
    /// drop in unmodified). Parsed with a real SMI parser (#532); malformed
    /// modules fail startup.
    #[serde(default)]
    pub dirs: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for MibConfig {
    fn default() -> Self {
        Self {
            load_builtin: true,
            files: Vec::new(),
            dirs: Vec::new(),
        }
    }
}

impl SnmpConfig {
    /// Apply named credential sets and resolve `${ENV}` / `file:` secret
    /// indirection (#538). Called once at startup; unknown set names and
    /// missing env/files are hard errors.
    pub fn resolve_credentials(&mut self) -> Result<(), zensight_sensor_core::SensorError> {
        use zensight_sensor_core::SensorError;
        use zensight_sensor_core::secret::{resolve_secret, resolve_secret_opt};

        // Resolve indirection inside the sets themselves first.
        for set in self.credentials.values_mut() {
            resolve_secret_opt(&mut set.community)?;
            if let Some(sec) = &mut set.security {
                resolve_secret_opt(&mut sec.auth_password)?;
                resolve_secret_opt(&mut sec.priv_password)?;
            }
        }

        for device in &mut self.devices {
            if let Some(name) = &device.credentials {
                let set = self.credentials.get(name).ok_or_else(|| {
                    SensorError::Config(format!(
                        "device {:?}: unknown credential set {name:?}",
                        device.name
                    ))
                })?;
                apply_credential_set(device, set);
            }
            // Inline values may use indirection too.
            device.community = resolve_secret(&device.community)?;
            if let Some(sec) = &mut device.security {
                resolve_secret_opt(&mut sec.auth_password)?;
                resolve_secret_opt(&mut sec.priv_password)?;
            }
        }

        for user in &mut self.trap_listener.users {
            resolve_secret_opt(&mut user.auth_password)?;
            resolve_secret_opt(&mut user.priv_password)?;
        }
        for community in &mut self.trap_listener.communities {
            *community = resolve_secret(community)?;
        }
        Ok(())
    }

    /// The agent host's unified source id: the `source` override, else the hostname.
    pub fn resolved_source(&self) -> String {
        self.source.clone().unwrap_or_else(|| {
            hostname::get()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "unknown".to_string())
        })
    }
}

/// SNMP trap listener configuration (#535).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrapListenerConfig {
    /// Enable trap listener.
    #[serde(default)]
    pub enabled: bool,

    /// Address to bind (e.g., "0.0.0.0:162"). Binding 162 needs privileges
    /// (or CAP_NET_BIND_SERVICE); an unprivileged deployment binds 1162 and
    /// redirects — see docs/reference.md.
    #[serde(default = "default_trap_bind")]
    pub bind: String,

    /// Accepted v1/v2c communities. Empty (default) accepts any community —
    /// the pre-#535 behavior.
    #[serde(default)]
    pub communities: Vec<String>,

    /// SNMPv3 notification users (traps + informs). Same schema as device
    /// `security`; `engine_id` is ignored here (the receiver is
    /// authoritative and generates its own).
    #[serde(default)]
    pub users: Vec<SnmpV3Security>,

    /// Trap → alert mappings: a `fire` trap OID raises the alert, the
    /// optional `resolve` OID clears it (per device + interface).
    #[serde(default)]
    pub alerts: Vec<TrapAlertRule>,

    /// Include the built-in linkDown/linkUp mapping (default true).
    #[serde(default = "default_true")]
    pub builtin_rules: bool,

    /// Where the v3 authoritative engine identity (`snmpEngineID`) and its
    /// `snmpEngineBoots` counter are persisted (#650, RFC 3414 §2.2).
    ///
    /// `None` (the default) resolves the systemd `STATE_DIRECTORY` / XDG state
    /// location, the same scheme the logs sensor uses for its cursor and
    /// offsets. If **no** location resolves at all, the receiver keeps a
    /// per-start ephemeral identity and says so once — see
    /// `docs/reference.md`. A location that resolves but cannot be written is
    /// a different matter: v3 receiving is refused rather than silently
    /// downgraded, because an operator who asked for durability and did not
    /// get it should not find out from an inform sender.
    #[serde(default)]
    pub engine_state_path: Option<std::path::PathBuf>,
}

fn default_trap_bind() -> String {
    "0.0.0.0:162".to_string()
}

impl Default for TrapListenerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_trap_bind(),
            communities: Vec::new(),
            users: Vec::new(),
            alerts: Vec::new(),
            builtin_rules: true,
            engine_state_path: None,
        }
    }
}

impl TrapListenerConfig {
    /// The effective alert-mapping rules: configured ones plus (unless
    /// disabled) the built-in linkDown/linkUp pair.
    pub fn effective_rules(&self) -> Vec<TrapAlertRule> {
        let mut rules = self.alerts.clone();
        if self.builtin_rules {
            rules.push(TrapAlertRule {
                rule: "trap_link_down".to_string(),
                fire: "1.3.6.1.6.3.1.1.5.3".to_string(),
                resolve: Some("1.3.6.1.6.3.1.1.5.4".to_string()),
                severity: "warning".to_string(),
            });
        }
        rules
    }
}

/// One trap → alert mapping (#535).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrapAlertRule {
    /// Stable rule slug (alert `rule` field).
    pub rule: String,
    /// Trap OID that fires the alert.
    pub fire: String,
    /// Trap OID that resolves it (same device + interface labels).
    #[serde(default)]
    pub resolve: Option<String>,
    /// `info` / `warning` / `critical` (default warning).
    #[serde(default = "default_severity")]
    pub severity: String,
}

fn default_severity() -> String {
    "warning".to_string()
}

/// Configuration for a single SNMP device.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    /// Device name (used in key expressions).
    pub name: String,

    /// Device address (e.g., "192.168.1.1:161").
    pub address: String,

    /// SNMP community string (for v1/v2c).
    #[serde(default = "default_community")]
    pub community: String,

    /// SNMP version ("v1", "v2c", or "v3").
    #[serde(default = "default_version")]
    pub version: SnmpVersion,

    /// SNMPv3 security settings (required if version is "v3").
    #[serde(default)]
    pub security: Option<SnmpV3Security>,

    /// Polling interval in seconds.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Per-request timeout in seconds (per attempt, not per poll cycle).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,

    /// Retransmissions after a timed-out request (0 = single attempt).
    #[serde(default = "default_retries")]
    pub retries: u32,

    /// GETBULK max-repetitions for table walks (v2c/v3).
    #[serde(default = "default_max_repetitions")]
    pub max_repetitions: u32,
    /// **Per-device PDU ceiling** (#825 item 2). An SNMP sensor's
    /// characteristic failure is hammering a device weaker than itself — an
    /// eight-year-old switch CPU, or a UPS card that reboots under load — and
    /// one device's tolerance says nothing about another's, so this is
    /// declared per device rather than per sensor.
    ///
    /// A GET is charged one token before it is issued. A **walk is charged
    /// after it completes**, from the rows it really returned
    /// (`ceil(rows / max_repetitions) + 1` for GETBULK): how many PDUs a walk
    /// takes is not knowable before the table is read, and estimating it would
    /// make this number mean something other than what it says. Over budget
    /// the poller **waits** rather than dropping the poll — a sensor that
    /// skips work to stay under budget has traded the device's health for a
    /// gap in its own telemetry.
    ///
    /// Absent or 0 = no ceiling, which is what every deployment before #825
    /// had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pdus_per_sec: Option<f64>,
    /// **Outstanding operations against this device** (#825 item 2). A walk
    /// holds its slot for its whole duration, which is the part that bounds
    /// concurrent load. Absent or 0 = unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<usize>,

    /// Individual OIDs to poll with GET.
    #[serde(default)]
    pub oids: Vec<String>,

    /// OID subtrees to poll with WALK (GETNEXT/GETBULK).
    #[serde(default)]
    pub walks: Vec<String>,

    /// Reference to a predefined OID group.
    #[serde(default)]
    pub oid_group: Option<String>,

    /// Per-device alerting override: replaces the global `snmp.alerts`
    /// block for this device when present.
    #[serde(default)]
    pub alerts: Option<crate::alerts::SnmpAlertsConfig>,

    /// Pin a specific device profile by name (#531) instead of sysObjectID
    /// matching. Default profiles still apply.
    #[serde(default)]
    pub profile: Option<String>,

    /// Reference a named `snmp.credentials` set (#538): its community and/or
    /// v3 security replace this device's own; rotate once, apply everywhere.
    #[serde(default)]
    pub credentials: Option<String>,
}

/// Observed-device evidence configuration (#537).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceConfig {
    /// Publish per-device `HostEvidence` claims (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Refresh cadence in poll cycles (default 10; the first successful
    /// cycle always publishes).
    #[serde(default = "default_evidence_refresh")]
    pub refresh_cycles: u32,
}

fn default_evidence_refresh() -> u32 {
    10
}

impl Default for EvidenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_cycles: default_evidence_refresh(),
        }
    }
}

/// Device-profile configuration (#531).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilesConfig {
    /// Apply profiles at all (default true). Off restores explicit-only
    /// polling from `oids`/`walks`/`oid_group`.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Extra profile directories (`*.toml`); same-name profiles override
    /// the shipped ones. Bad files fail startup loudly.
    #[serde(default)]
    pub dirs: Vec<String>,
}

impl Default for ProfilesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dirs: Vec::new(),
        }
    }
}

impl std::fmt::Debug for DeviceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The community string is a credential (#538).
        f.debug_struct("DeviceConfig")
            .field("name", &self.name)
            .field("address", &self.address)
            .field("community", &"<redacted>")
            .field("version", &self.version)
            .field("security", &self.security)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("credentials", &self.credentials)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

fn default_community() -> String {
    "public".to_string()
}

fn default_version() -> SnmpVersion {
    SnmpVersion::V2c
}

fn default_poll_interval() -> u64 {
    30
}

fn default_timeout_secs() -> u64 {
    5
}

fn default_retries() -> u32 {
    2
}

fn default_max_repetitions() -> u32 {
    20
}

/// SNMP protocol version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnmpVersion {
    #[serde(rename = "v1")]
    V1,
    #[default]
    #[serde(rename = "v2c")]
    V2c,
    #[serde(rename = "v3")]
    V3,
}

/// SNMPv3 security configuration (USM - User Security Model).
#[derive(Clone, Serialize, Deserialize)]
pub struct SnmpV3Security {
    /// SNMPv3 username.
    pub username: String,

    /// Authentication protocol.
    #[serde(default)]
    pub auth_protocol: AuthProtocol,

    /// Authentication password (required if auth_protocol is not None).
    #[serde(default)]
    pub auth_password: Option<String>,

    /// Privacy/encryption protocol.
    #[serde(default)]
    pub priv_protocol: PrivProtocol,

    /// Privacy password (required if priv_protocol is not None).
    #[serde(default)]
    pub priv_password: Option<String>,

    /// Optional pre-configured engine ID (hex string).
    /// If not provided, will be discovered automatically.
    #[serde(default)]
    pub engine_id: Option<String>,
}

impl std::fmt::Debug for SnmpV3Security {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Passwords never reach logs, even at trace level (#538).
        f.debug_struct("SnmpV3Security")
            .field("username", &self.username)
            .field("auth_protocol", &self.auth_protocol)
            .field(
                "auth_password",
                &self.auth_password.as_ref().map(|_| "<redacted>"),
            )
            .field("priv_protocol", &self.priv_protocol)
            .field(
                "priv_password",
                &self.priv_password.as_ref().map(|_| "<redacted>"),
            )
            .field("engine_id", &self.engine_id)
            .finish()
    }
}

/// SNMPv3 authentication protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AuthProtocol {
    /// No authentication (noAuthNoPriv).
    #[default]
    #[serde(rename = "none")]
    None,
    /// MD5 authentication (RFC 3414).
    #[serde(rename = "MD5")]
    Md5,
    /// SHA-1 authentication (RFC 3414).
    #[serde(rename = "SHA")]
    Sha1,
    /// SHA-224 authentication (non-standard).
    #[serde(rename = "SHA224")]
    Sha224,
    /// SHA-256 authentication (non-standard).
    #[serde(rename = "SHA256")]
    Sha256,
    /// SHA-384 authentication (non-standard).
    #[serde(rename = "SHA384")]
    Sha384,
    /// SHA-512 authentication (non-standard).
    #[serde(rename = "SHA512")]
    Sha512,
}

/// SNMPv3 privacy/encryption protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PrivProtocol {
    /// No encryption (noPriv).
    #[default]
    #[serde(rename = "none")]
    None,
    /// DES encryption (RFC 3414) - may not be available.
    #[serde(rename = "DES")]
    Des,
    /// AES-128 encryption (RFC 3826).
    #[serde(rename = "AES")]
    Aes128,
    /// AES-192 (non-standard). The localized key is extended with the
    /// Blumenthal algorithm (draft-blumenthal-aes-usm-04) when the auth
    /// digest is too short — what net-snmp does, and what this sensor has
    /// always done. Cisco gear extends the Reeder way: `AES192-REEDER`.
    #[serde(rename = "AES192")]
    Aes192,
    /// AES-256 (non-standard), Blumenthal key extension — see `AES192`.
    #[serde(rename = "AES256")]
    Aes256,
    /// AES-192 with the Reeder (Cisco) key extension. `AES192-CISCO` is
    /// accepted as a synonym.
    #[serde(rename = "AES192-REEDER", alias = "AES192-CISCO")]
    Aes192Reeder,
    /// AES-256 with the Reeder (Cisco) key extension. `AES256-CISCO` is
    /// accepted as a synonym.
    #[serde(rename = "AES256-REEDER", alias = "AES256-CISCO")]
    Aes256Reeder,
    /// 3DES-EDE (draft-reeder-snmpv3-usm-3desede-00). Slow, no hardware
    /// acceleration; for gear that offers nothing better.
    #[serde(rename = "3DES")]
    Des3,
}

/// A group of OIDs that can be referenced by devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidGroup {
    /// Individual OIDs to poll with GET.
    #[serde(default)]
    pub oids: Vec<String>,

    /// OID subtrees to poll with WALK.
    #[serde(default)]
    pub walks: Vec<String>,
}

impl SnmpSensorConfig {
    /// Load configuration from a JSON5 file.
    pub fn load(path: impl AsRef<Path>) -> zensight_common::Result<Self> {
        zensight_common::load_config(path)
    }

    /// Parse configuration from a JSON5 string.
    #[cfg(test)]
    pub fn parse(content: &str) -> zensight_common::Result<Self> {
        zensight_common::parse_config(content)
    }
}

impl zensight_sensor_core::SensorConfig for SnmpSensorConfig {
    fn zenoh(&self) -> &ZenohConfig {
        &self.zenoh
    }

    fn logging(&self) -> &LoggingConfig {
        &self.logging
    }

    fn producer(&self) -> &str {
        "snmp"
    }

    fn desired(&self) -> zensight_common::desired::DesiredConfig {
        self.desired.clone()
    }

    fn thresholds(&self) -> zensight_common::threshold::ThresholdsConfig {
        self.thresholds.clone()
    }

    fn validate(&self) -> zensight_sensor_core::Result<()> {
        // v1/v2c gate (#825): cleartext credentials need the explicit flag.
        if !self.snmp.allow_insecure_versions {
            for device in &self.snmp.devices {
                if matches!(device.version, SnmpVersion::V1 | SnmpVersion::V2c) {
                    return Err(zensight_sensor_core::SensorError::config(format!(
                        "Device '{}' uses SNMP {} — a cleartext community on the wire.                          Prefer v3 authPriv; if this device genuinely cannot, set                          snmp.allow_insecure_versions = true to accept the cost (#825)",
                        device.name,
                        match device.version {
                            SnmpVersion::V1 => "v1",
                            _ => "v2c",
                        }
                    )));
                }
            }
            if !self.snmp.trap_listener.communities.is_empty() {
                return Err(zensight_sensor_core::SensorError::config(
                    "trap_listener.communities configures v1/v2c trap senders — cleartext                      communities on the wire. Prefer v3 users; set                      snmp.allow_insecure_versions = true to accept the cost (#825)",
                ));
            }
        }
        // Validate that devices have required fields
        for device in &self.snmp.devices {
            if device.name.is_empty() {
                return Err(zensight_sensor_core::SensorError::config(
                    "Device name cannot be empty",
                ));
            }
            if device.address.is_empty() {
                return Err(zensight_sensor_core::SensorError::config(format!(
                    "Device '{}' has no address",
                    device.name
                )));
            }
            // Validate SNMPv3 security if specified
            if device.version == SnmpVersion::V3 && device.security.is_none() {
                return Err(zensight_sensor_core::SensorError::config(format!(
                    "Device '{}' uses SNMPv3 but has no security configuration",
                    device.name
                )));
            }
        }

        // ── Gated outlet control (#956) ──────────────────────────────────
        //
        // Refused at STARTUP rather than at the first request, because the two
        // failures below are configuration mistakes an operator would
        // otherwise discover by trying to power-cycle a server.
        if self.snmp.actions.enabled {
            let Some(name) = self.snmp.actions.credentials.as_deref() else {
                return Err(zensight_sensor_core::SensorError::config(
                    "snmp.actions.enabled is true but snmp.actions.credentials names no \
                     credential set. An outlet cycle is an SNMP SET, and it must not ride \
                     the read credential: a monitoring community that can reach a SET is a \
                     control credential nobody decided to grant. Add a write-access \
                     credential set and name it here (#956)",
                ));
            };
            if !self.snmp.credentials.contains_key(name) {
                return Err(zensight_sensor_core::SensorError::config(format!(
                    "snmp.actions.credentials names {name:?}, which is not a defined \
                     snmp.credentials set"
                )));
            }
            // A cleartext community that can CUT POWER is a different
            // proposition from one that can read a counter, so it needs the
            // #825 flag said out loud even though the device-level gate above
            // may already have accepted the read path.
            let write = &self.snmp.credentials[name];
            if write.community.is_some()
                && write.security.is_none()
                && !self.snmp.allow_insecure_versions
            {
                return Err(zensight_sensor_core::SensorError::config(format!(
                    "the write credential set {name:?} is a v1/v2c community — a cleartext \
                     string on the wire that can CUT POWER. Prefer a v3 authPriv user; if \
                     this PDU genuinely cannot, set snmp.allow_insecure_versions = true to \
                     accept the cost (#825, #956)"
                )));
            }
            if self.snmp.actions.allow_outlets.is_empty() {
                // Not an error: "on with an empty allowlist" is a legitimate,
                // fully-refusing state, and it is what the capability
                // advertises. But it reads as a working gate until you try it,
                // so it is said once at startup.
                tracing::warn!(
                    "snmp.actions.enabled is true but snmp.actions.allow_outlets is empty, \
                     so every outlet request will be refused. Add <device>/<outlet> globs \
                     to permit control (#956)"
                );
            }
        }
        Ok(())
    }

    fn artifact_limits(&self) -> zensight_sensor_core::ArtifactLimits {
        self.artifacts.clone()
    }
}

impl DeviceConfig {
    /// Get all OIDs to poll (including from referenced group).
    pub fn all_oids(&self, groups: &HashMap<String, OidGroup>) -> Vec<String> {
        let mut oids = self.oids.clone();

        if let Some(group_name) = &self.oid_group
            && let Some(group) = groups.get(group_name)
        {
            oids.extend(group.oids.clone());
        }

        oids
    }

    /// Get all OID subtrees to walk (including from referenced group).
    pub fn all_walks(&self, groups: &HashMap<String, OidGroup>) -> Vec<String> {
        let mut walks = self.walks.clone();

        if let Some(group_name) = &self.oid_group
            && let Some(group) = groups.get(group_name)
        {
            walks.extend(group.walks.clone());
        }

        walks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let json5 = r#"
        {
            zenoh: {
                mode: "peer",
            },
            serialization: "json",
            snmp: {
                devices: [
                    {
                        name: "router01",
                        address: "192.168.1.1:161",
                        community: "public",
                        version: "v2c",
                        poll_interval_secs: 30,
                        oids: ["1.3.6.1.2.1.1.3.0"],
                        walks: ["1.3.6.1.2.1.2.2.1"],
                    },
                ],
                oid_groups: {
                    system_info: {
                        oids: ["1.3.6.1.2.1.1.1.0", "1.3.6.1.2.1.1.3.0"],
                        walks: [],
                    },
                },
                oid_names: {
                    "1.3.6.1.2.1.1.3.0": "system/sysUpTime",
                },
            },
            logging: { level: "info" },
        }
        "#;

        let config = SnmpSensorConfig::parse(json5).unwrap();

        assert_eq!(config.zenoh.mode, "peer");
        assert_eq!(config.serialization, Format::Json);
        assert_eq!(config.snmp.devices.len(), 1);
        assert_eq!(config.snmp.devices[0].name, "router01");
        assert_eq!(config.snmp.devices[0].version, SnmpVersion::V2c);
        assert_eq!(config.snmp.oid_groups.len(), 1);
        assert!(config.snmp.oid_groups.contains_key("system_info"));

        // Transport tuning fields default when absent (config compatibility).
        assert_eq!(config.snmp.devices[0].timeout_secs, 5);
        assert_eq!(config.snmp.devices[0].retries, 2);
        assert_eq!(config.snmp.devices[0].max_repetitions, 20);
    }

    #[test]
    fn test_credential_sets_and_indirection() {
        // SAFETY: test-local variable, single-threaded use.
        unsafe { std::env::set_var("ZENSIGHT_TEST_SNMP_PW", "envpass-538") };
        let dir = std::env::temp_dir().join(format!("zensight-cred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let community_file = dir.join("community");
        std::fs::write(&community_file, "filepass-538\n").unwrap();

        let json5 = format!(
            r#"
        {{
            zenoh: {{ mode: "peer" }},
            snmp: {{
                credentials: {{
                    "readonly-v2c": {{ community: "file:{}" }},
                    "netops-v3": {{
                        security: {{
                            username: "netops",
                            auth_protocol: "SHA256",
                            auth_password: "${{ZENSIGHT_TEST_SNMP_PW}}",
                        }},
                    }},
                }},
                devices: [
                    {{ name: "sw1", address: "10.0.0.1:161", credentials: "readonly-v2c" }},
                    {{ name: "r1", address: "10.0.0.2:161", version: "v3", credentials: "netops-v3" }},
                    {{ name: "inline1", address: "10.0.0.3:161", community: "${{ZENSIGHT_TEST_SNMP_PW}}" }},
                ]
            }},
            logging: {{ level: "info" }},
        }}
        "#,
            community_file.display()
        );

        let mut config = SnmpSensorConfig::parse(&json5).unwrap();
        config.snmp.resolve_credentials().unwrap();

        // Named set + file indirection.
        assert_eq!(config.snmp.devices[0].community, "filepass-538");
        // Named v3 set + env indirection.
        let sec = config.snmp.devices[1].security.as_ref().unwrap();
        assert_eq!(sec.username, "netops");
        assert_eq!(sec.auth_password.as_deref(), Some("envpass-538"));
        // Inline env indirection (the escape hatch keeps working).
        assert_eq!(config.snmp.devices[2].community, "envpass-538");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_unknown_credential_set_fails_loudly() {
        let json5 = r#"
        {
            zenoh: { mode: "peer" },
            snmp: {
                devices: [
                    { name: "sw1", address: "10.0.0.1:161", credentials: "nope" },
                ]
            },
            logging: { level: "info" },
        }
        "#;
        let mut config = SnmpSensorConfig::parse(json5).unwrap();
        let err = config.snmp.resolve_credentials().unwrap_err();
        assert!(err.to_string().contains("unknown credential set"), "{err}");
    }

    /// #538 scrubbing audit: neither Debug formatting nor the redacted
    /// debug-bundle transform may leak a configured secret.
    #[test]
    fn test_secrets_never_leak() {
        const SECRETS: [&str; 3] = ["hunter2-community", "hunter2-auth", "hunter2-priv"];
        let json5 = r#"
        {
            zenoh: { mode: "peer" },
            snmp: {
                credentials: {
                    "set-a": { community: "hunter2-community" },
                },
                trap_listener: {
                    enabled: true,
                    communities: ["hunter2-community"],
                    users: [{ username: "u", auth_protocol: "SHA256",
                              auth_password: "hunter2-auth",
                              priv_protocol: "AES", priv_password: "hunter2-priv" }],
                },
                devices: [
                    {
                        name: "r1", address: "10.0.0.1:161",
                        community: "hunter2-community",
                        version: "v3",
                        security: {
                            username: "monitor",
                            auth_protocol: "SHA256", auth_password: "hunter2-auth",
                            priv_protocol: "AES", priv_password: "hunter2-priv",
                        },
                    },
                ]
            },
            logging: { level: "info" },
        }
        "#;
        let config = SnmpSensorConfig::parse(json5).unwrap();

        // Debug formatting (what a stray `{:?}` log line would print).
        let debugged = format!("{:?} {:?}", config.snmp.devices, config.snmp.credentials);
        for secret in SECRETS {
            assert!(
                !debugged.contains(secret),
                "Debug leaked {secret}: {debugged}"
            );
        }

        // The debug-report bundle's redaction transform, applied to the full
        // serialized config exactly as build_debug_bundle does.
        let mut json = serde_json::to_value(&config).unwrap();
        zensight_sensor_core::redact(&mut json, &[]);
        let bundle = serde_json::to_string(&json).unwrap();
        for secret in SECRETS {
            assert!(!bundle.contains(secret), "bundle leaked {secret}");
        }
        // Non-secrets survive redaction (the report stays useful).
        assert!(bundle.contains("10.0.0.1:161"));
        assert!(bundle.contains("monitor"));
    }

    #[test]
    fn test_parse_alerts_block() {
        let json5 = r#"
        {
            zenoh: { mode: "peer" },
            snmp: {
                alerts: {
                    for_secs: 30,
                    unreachable: { cycles: 5 },
                    utilization: { percent: 80.0 },
                    interface_errors: { enabled: false },
                },
                devices: [
                    {
                        name: "quiet01",
                        address: "192.168.1.7:161",
                        alerts: { enabled: false },
                    },
                ],
            },
            logging: { level: "info" },
        }
        "#;

        let config = SnmpSensorConfig::parse(json5).unwrap();
        let alerts = &config.snmp.alerts;
        assert!(alerts.enabled);
        assert_eq!(alerts.for_secs, 30);
        assert_eq!(alerts.unreachable.cycles, 5);
        assert_eq!(alerts.utilization.percent, 80.0);
        assert!(!alerts.interface_errors.enabled);
        assert!(alerts.interface_down.enabled); // untouched default
        let dev = &config.snmp.devices[0];
        assert!(!dev.alerts.as_ref().unwrap().enabled);
    }

    #[test]
    fn test_parse_transport_tuning() {
        let json5 = r#"
        {
            zenoh: { mode: "peer" },
            snmp: {
                devices: [
                    {
                        name: "slow01",
                        address: "192.168.1.9:161",
                        timeout_secs: 10,
                        retries: 4,
                        max_repetitions: 50,
                    },
                ],
            },
            logging: { level: "info" },
        }
        "#;

        let config = SnmpSensorConfig::parse(json5).unwrap();
        assert_eq!(config.snmp.devices[0].timeout_secs, 10);
        assert_eq!(config.snmp.devices[0].retries, 4);
        assert_eq!(config.snmp.devices[0].max_repetitions, 50);
    }

    #[test]
    fn test_device_all_oids() {
        let mut groups = HashMap::new();
        groups.insert(
            "system_info".to_string(),
            OidGroup {
                oids: vec!["1.3.6.1.2.1.1.1.0".to_string()],
                walks: vec!["1.3.6.1.2.1.2.2.1".to_string()],
            },
        );

        let device = DeviceConfig {
            max_pdus_per_sec: None,
            max_concurrent: None,
            name: "test".to_string(),
            address: "127.0.0.1:161".to_string(),
            community: "public".to_string(),
            version: SnmpVersion::V2c,
            security: None,
            poll_interval_secs: 30,
            timeout_secs: 5,
            retries: 2,
            max_repetitions: 20,
            oids: vec!["1.3.6.1.2.1.1.3.0".to_string()],
            walks: vec![],
            oid_group: Some("system_info".to_string()),
            alerts: None,
            profile: None,
            credentials: None,
        };

        let all_oids = device.all_oids(&groups);
        assert_eq!(all_oids.len(), 2);

        let all_walks = device.all_walks(&groups);
        assert_eq!(all_walks.len(), 1);
    }

    #[test]
    fn test_parse_snmpv3_config() {
        let json5 = r#"
        {
            zenoh: { mode: "peer" },
            snmp: {
                devices: [
                    {
                        name: "secure-router",
                        address: "192.168.1.1:161",
                        version: "v3",
                        security: {
                            username: "admin",
                            auth_protocol: "SHA256",
                            auth_password: "authpass123",
                            priv_protocol: "AES",
                            priv_password: "privpass456",
                        },
                        poll_interval_secs: 60,
                        oids: ["1.3.6.1.2.1.1.3.0"],
                    },
                ],
            },
        }
        "#;

        let config = SnmpSensorConfig::parse(json5).unwrap();

        assert_eq!(config.snmp.devices.len(), 1);
        let device = &config.snmp.devices[0];
        assert_eq!(device.name, "secure-router");
        assert_eq!(device.version, SnmpVersion::V3);

        let security = device.security.as_ref().unwrap();
        assert_eq!(security.username, "admin");
        assert_eq!(security.auth_protocol, AuthProtocol::Sha256);
        assert_eq!(security.auth_password, Some("authpass123".to_string()));
        assert_eq!(security.priv_protocol, PrivProtocol::Aes128);
        assert_eq!(security.priv_password, Some("privpass456".to_string()));
    }

    #[test]
    fn test_snmpv3_noauth_config() {
        let json5 = r#"
        {
            zenoh: { mode: "peer" },
            snmp: {
                devices: [
                    {
                        name: "public-device",
                        address: "192.168.1.2:161",
                        version: "v3",
                        security: {
                            username: "public",
                        },
                        oids: ["1.3.6.1.2.1.1.1.0"],
                    },
                ],
            },
        }
        "#;

        let config = SnmpSensorConfig::parse(json5).unwrap();

        let device = &config.snmp.devices[0];
        assert_eq!(device.version, SnmpVersion::V3);

        let security = device.security.as_ref().unwrap();
        assert_eq!(security.username, "public");
        assert_eq!(security.auth_protocol, AuthProtocol::None);
        assert_eq!(security.priv_protocol, PrivProtocol::None);
    }
}

#[cfg(test)]
mod insecure_gate_tests {
    use zensight_sensor_core::SensorConfig as _;

    /// #825: a v1/v2c device is a cleartext credential on the wire and needs
    /// the explicit opt-in; the refusal names the device and the flag.
    #[test]
    fn v2c_refuses_without_the_flag_and_starts_with_it() {
        let base = r#"{ zenoh: { mode: "peer" }, snmp: { devices: [
            { name: "legacy", address: "10.0.0.9:161", community: "public", version: "v2c" }
        ] } }"#;
        let cfg: crate::config::SnmpSensorConfig = json5::from_str(base).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("legacy") && err.contains("allow_insecure_versions"),
            "{err}"
        );

        let with_flag = base.replace("snmp: {", "snmp: { allow_insecure_versions: true,");
        let cfg: crate::config::SnmpSensorConfig = json5::from_str(&with_flag).unwrap();
        cfg.validate().expect("explicit opt-in starts");
    }

    /// Same gate for trap communities (v1/v2c senders).
    #[test]
    fn trap_communities_need_the_flag_too() {
        let cfg: crate::config::SnmpSensorConfig = json5::from_str(
            r#"{ zenoh: { mode: "peer" },
                 snmp: { trap_listener: { enabled: true, communities: ["public"] } } }"#,
        )
        .unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("allow_insecure_versions"), "{err}");
    }

    /// v3 needs no flag — it is the supported default.
    #[test]
    fn v3_needs_no_flag() {
        let cfg: crate::config::SnmpSensorConfig = json5::from_str(
            r#"{ zenoh: { mode: "peer" }, snmp: { devices: [
                { name: "r1", address: "10.0.0.1:161", version: "v3",
                  security: { username: "ro", auth_protocol: "SHA256", auth_password: "x",
                              priv_protocol: "AES", priv_password: "y" } }
            ] } }"#,
        )
        .unwrap();
        cfg.validate().expect("v3 authPriv is the default path");
    }
}

/// The shipped example config must load (#845): it ships in the release
/// tarball and the container image, and nothing else in CI ever parsed it —
/// so a renamed field silently reverted to its serde default in production
/// (`gen-configs.sh` documents the identical hazard for the demo profile).
/// Precedent: parallax/logs guard their shipped configs the same way.
#[cfg(test)]
mod shipped_config {
    #[test]
    fn shipped_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../configs/snmp.json5");
        let _config =
            crate::config::SnmpSensorConfig::load(path).expect("configs/snmp.json5 must load");
    }
}

/// Copy a named credential set onto a device.
///
/// Shared by the file path (`resolve_credentials`) and the wire path
/// (`devices_from_wire`) since #936, so a fleet-authored device and a
/// file-configured one that name the same set are credentialed identically.
/// Two copies of this would be two chances for them to differ, and the
/// difference would show as one device mysteriously not answering.
fn apply_credential_set(device: &mut DeviceConfig, set: &CredentialSet) {
    if let Some(community) = &set.community {
        device.community = community.clone();
    }
    if let Some(security) = &set.security {
        device.security = Some(security.clone());
    }
}

// ── the wire set, resolved against local credentials (#936) ─────────────────

/// Turn a fleet-authored device set into pollable devices.
///
/// **Credentials are resolved here, from this host's own file config.** The
/// wire carries a *name*; the community string and the v3 passphrases live in
/// `snmp.credentials` and never leave the machine. A name this host does not
/// have is an error, not a fallback to the default community — polling with
/// the wrong credential reads, on every chart, as a device that stopped
/// answering, and that is the most expensive way to be told about a typo.
///
/// Unnamed fields (`oids`, `walks`, timeouts, rate caps) come from the file
/// config's device of the same name when there is one, so a fleet may retarget
/// or reprofile a device an operator tuned locally without discarding the
/// tuning.
pub fn devices_from_wire(
    wire: &zensight_common::targets::SnmpTargets,
    file_baseline: &[DeviceConfig],
    credentials: &HashMap<String, CredentialSet>,
) -> Result<Vec<DeviceConfig>, String> {
    wire.validate()?;
    let mut out = Vec::with_capacity(wire.targets.len());
    for t in &wire.targets {
        let Some(cred) = credentials.get(&t.credentials) else {
            let mut known: Vec<&str> = credentials.keys().map(String::as_str).collect();
            known.sort();
            return Err(format!(
                "target {:?} names credential set {:?}, which this host does not have.                  It has {:?}. The wire carries a NAME; the secret stays in this host's                  own `snmp.credentials`",
                t.name, t.credentials, known
            ));
        };
        // Start from the local device of the same name when there is one, so
        // locally-tuned oids/walks/limits survive a fleet retarget.
        let mut d = file_baseline
            .iter()
            .find(|d| d.name == t.name)
            .cloned()
            .unwrap_or_else(|| DeviceConfig {
                name: t.name.clone(),
                address: t.address.clone(),
                ..default_device()
            });
        d.name = t.name.clone();
        d.address = t.address.clone();
        d.credentials = Some(t.credentials.clone());
        if let Some(p) = &t.profile {
            d.profile = Some(p.clone());
        }
        if let Some(g) = &t.oid_group {
            d.oid_group = Some(g.clone());
        }
        if let Some(i) = t.poll_interval_secs {
            d.poll_interval_secs = i;
        }
        apply_credential_set(&mut d, cred);
        out.push(d);
    }
    Ok(out)
}

/// The device set as a wire set, for `@rpc/snmp/targets` to answer with.
///
/// Every field that could hold a secret is dropped on the way out, not just on
/// the way in: a read procedure returning `community` would put every
/// configured community string on the bus in reply to a GET — the same leak as
/// publishing them, reached from the other direction.
pub fn devices_to_wire(devices: &[DeviceConfig]) -> zensight_common::targets::SnmpTargets {
    zensight_common::targets::SnmpTargets {
        targets: devices
            .iter()
            .map(|d| zensight_common::targets::SnmpTarget {
                name: d.name.clone(),
                address: d.address.clone(),
                credentials: d.credentials.clone().unwrap_or_default(),
                profile: d.profile.clone(),
                oid_group: d.oid_group.clone(),
                poll_interval_secs: Some(d.poll_interval_secs),
            })
            .collect(),
    }
}

fn default_device() -> DeviceConfig {
    serde_json::from_value(serde_json::json!({ "name": "", "address": "" }))
        .expect("a device with a name and an address is valid")
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use zensight_common::targets::{SnmpTarget, SnmpTargets};

    fn creds() -> HashMap<String, CredentialSet> {
        let mut m = HashMap::new();
        m.insert(
            "ro".to_string(),
            serde_json::from_value(serde_json::json!({ "community": "s3cret" }))
                .expect("credential set"),
        );
        m
    }

    fn target(name: &str, cred: &str) -> SnmpTarget {
        SnmpTarget {
            name: name.into(),
            address: "10.0.0.1:161".into(),
            credentials: cred.into(),
            profile: None,
            oid_group: None,
            poll_interval_secs: Some(60),
        }
    }

    #[test]
    fn a_credential_name_resolves_from_local_config() {
        let out = devices_from_wire(
            &SnmpTargets {
                targets: vec![target("sw1", "ro")],
            },
            &[],
            &creds(),
        )
        .expect("resolves");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].community, "s3cret", "the secret came from the host");
        assert_eq!(out[0].poll_interval_secs, 60);
    }

    /// Falling back to the default community would poll every device with the
    /// wrong credential and read, on every chart, as a device that stopped
    /// answering — the most expensive possible way to learn about a typo.
    #[test]
    fn an_unknown_credential_name_is_refused_and_lists_what_exists() {
        let err = devices_from_wire(
            &SnmpTargets {
                targets: vec![target("sw1", "typo")],
            },
            &[],
            &creds(),
        )
        .expect_err("unknown credential");
        assert!(err.contains("typo"), "{err}");
        assert!(
            err.contains("\"ro\""),
            "it should say what IS available: {err}"
        );
    }

    /// A fleet may retarget a device without discarding the oids an operator
    /// tuned on that host.
    #[test]
    fn local_tuning_survives_a_fleet_retarget() {
        let mut local: DeviceConfig = serde_json::from_value(serde_json::json!({
            "name": "sw1", "address": "10.0.0.99", "oids": ["1.3.6.1.2.1.1.3.0"]
        }))
        .expect("local device");
        local.max_repetitions = 42;

        let out = devices_from_wire(
            &SnmpTargets {
                targets: vec![target("sw1", "ro")],
            },
            std::slice::from_ref(&local),
            &creds(),
        )
        .expect("resolves");
        assert_eq!(out[0].address, "10.0.0.1:161", "the wire retargeted it");
        assert_eq!(out[0].oids, local.oids, "and kept the local oid list");
        assert_eq!(out[0].max_repetitions, 42);
    }

    #[test]
    fn the_read_procedure_returns_no_secret() {
        let devices = devices_from_wire(
            &SnmpTargets {
                targets: vec![target("sw1", "ro")],
            },
            &[],
            &creds(),
        )
        .expect("resolves");
        let json = serde_json::to_string(&devices_to_wire(&devices)).expect("encode");
        assert!(
            !json.contains("s3cret"),
            "a community string reached a reply: {json}"
        );
        assert!(json.contains("\"ro\""), "the NAME is what a reader gets");
    }
}
