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
}

/// SNMP-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnmpConfig {
    /// Override the agent-host source id (default: the local hostname).
    #[serde(default)]
    pub source: Option<String>,

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

    /// Additional MIB files to load (legacy JSON pseudo-MIB format).
    ///
    /// Deprecated (#532): put standard SMI `.mib`/`.txt` files in `dirs`
    /// instead. Still honored for one release; a startup warning points
    /// here.
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
                if let Some(community) = &set.community {
                    device.community = community.clone();
                }
                if let Some(security) = &set.security {
                    device.security = Some(security.clone());
                }
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
    /// AES-192 encryption (non-standard).
    #[serde(rename = "AES192")]
    Aes192,
    /// AES-256 encryption (non-standard).
    #[serde(rename = "AES256")]
    Aes256,
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

    fn validate(&self) -> zensight_sensor_core::Result<()> {
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
