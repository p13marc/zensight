//! Syslog message filtering.
//!
//! Provides configurable filtering by severity, facility, app name,
//! hostname, and message content using glob or regex patterns.

use crate::parser::SyslogMessage;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;

/// Pattern type for filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PatternType {
    /// Glob pattern (e.g., "systemd-*").
    #[default]
    Glob,
    /// Regular expression.
    Regex,
}

/// A filter pattern with its type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternFilter {
    /// The pattern string.
    pub pattern: String,
    /// Pattern type (glob or regex).
    #[serde(default)]
    pub pattern_type: PatternType,
}

/// Syslog filter configuration (serializable for config + commands).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyslogFilterConfig {
    /// Minimum severity level (0=emergency, 7=debug).
    /// Messages with higher severity numbers (less severe) are filtered out.
    #[serde(default)]
    pub min_severity: Option<u8>,

    /// Only include messages from these facilities.
    /// If empty, all facilities are allowed.
    #[serde(default)]
    pub include_facilities: Vec<String>,

    /// Exclude messages from these facilities.
    #[serde(default)]
    pub exclude_facilities: Vec<String>,

    /// Include messages matching these app name patterns.
    #[serde(default)]
    pub include_app_patterns: Vec<PatternFilter>,

    /// Exclude messages matching these app name patterns.
    #[serde(default)]
    pub exclude_app_patterns: Vec<PatternFilter>,

    /// Include messages matching these hostname patterns.
    #[serde(default)]
    pub include_hostname_patterns: Vec<PatternFilter>,

    /// Exclude messages matching these hostname patterns.
    #[serde(default)]
    pub exclude_hostname_patterns: Vec<PatternFilter>,

    /// Include messages matching these message content patterns.
    #[serde(default)]
    pub include_message_patterns: Vec<PatternFilter>,

    /// Exclude messages matching these message content patterns.
    #[serde(default)]
    pub exclude_message_patterns: Vec<PatternFilter>,
}

impl SyslogFilterConfig {
    /// Check if this filter is empty (passes all messages).
    pub fn is_empty(&self) -> bool {
        self.min_severity.is_none()
            && self.include_facilities.is_empty()
            && self.exclude_facilities.is_empty()
            && self.include_app_patterns.is_empty()
            && self.exclude_app_patterns.is_empty()
            && self.include_hostname_patterns.is_empty()
            && self.exclude_hostname_patterns.is_empty()
            && self.include_message_patterns.is_empty()
            && self.exclude_message_patterns.is_empty()
    }
}

/// Compiled pattern for efficient runtime matching.
#[derive(Debug)]
enum CompiledPattern {
    /// Glob pattern compiled to regex.
    Glob(Regex),
    /// User-provided regex.
    Regex(Regex),
}

impl CompiledPattern {
    /// Compile a pattern filter.
    fn compile(filter: &PatternFilter) -> Result<Self, regex::Error> {
        match filter.pattern_type {
            PatternType::Glob => {
                // Convert glob to regex
                let regex_pattern = glob_to_regex(&filter.pattern);
                Ok(CompiledPattern::Glob(Regex::new(&regex_pattern)?))
            }
            PatternType::Regex => Ok(CompiledPattern::Regex(Regex::new(&filter.pattern)?)),
        }
    }

    /// Check if the pattern matches the input.
    fn is_match(&self, input: &str) -> bool {
        match self {
            CompiledPattern::Glob(re) | CompiledPattern::Regex(re) => re.is_match(input),
        }
    }
}

/// Convert a glob pattern to a regex pattern.
fn glob_to_regex(glob: &str) -> String {
    let mut regex = String::with_capacity(glob.len() * 2 + 2);
    regex.push('^');

    for c in glob.chars() {
        match c {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                regex.push('\\');
                regex.push(c);
            }
            _ => regex.push(c),
        }
    }

    regex.push('$');
    regex
}

/// Compiled syslog filter for efficient runtime matching.
pub struct CompiledSyslogFilter {
    /// Original configuration for serialization.
    config: SyslogFilterConfig,
    /// Minimum severity (lower = more severe).
    min_severity: Option<u8>,
    /// Included facilities (lowercase).
    include_facilities: Vec<String>,
    /// Excluded facilities (lowercase).
    exclude_facilities: Vec<String>,
    /// Compiled app name include patterns.
    include_app_patterns: Vec<CompiledPattern>,
    /// Compiled app name exclude patterns.
    exclude_app_patterns: Vec<CompiledPattern>,
    /// Compiled hostname include patterns.
    include_hostname_patterns: Vec<CompiledPattern>,
    /// Compiled hostname exclude patterns.
    exclude_hostname_patterns: Vec<CompiledPattern>,
    /// Compiled message include patterns.
    include_message_patterns: Vec<CompiledPattern>,
    /// Compiled message exclude patterns.
    exclude_message_patterns: Vec<CompiledPattern>,
}

impl std::fmt::Debug for CompiledSyslogFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledSyslogFilter")
            .field("config", &self.config)
            .finish()
    }
}

impl CompiledSyslogFilter {
    /// Compile a filter configuration.
    pub fn compile(config: &SyslogFilterConfig) -> Result<Self, FilterCompileError> {
        let compile_patterns =
            |patterns: &[PatternFilter]| -> Result<Vec<CompiledPattern>, FilterCompileError> {
                patterns
                    .iter()
                    .map(|p| {
                        CompiledPattern::compile(p).map_err(|e| FilterCompileError {
                            pattern: p.pattern.clone(),
                            error: e.to_string(),
                        })
                    })
                    .collect()
            };

        Ok(Self {
            config: config.clone(),
            min_severity: config.min_severity,
            include_facilities: config
                .include_facilities
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            exclude_facilities: config
                .exclude_facilities
                .iter()
                .map(|s| s.to_lowercase())
                .collect(),
            include_app_patterns: compile_patterns(&config.include_app_patterns)?,
            exclude_app_patterns: compile_patterns(&config.exclude_app_patterns)?,
            include_hostname_patterns: compile_patterns(&config.include_hostname_patterns)?,
            exclude_hostname_patterns: compile_patterns(&config.exclude_hostname_patterns)?,
            include_message_patterns: compile_patterns(&config.include_message_patterns)?,
            exclude_message_patterns: compile_patterns(&config.exclude_message_patterns)?,
        })
    }

    /// Get the original configuration.
    pub fn config(&self) -> &SyslogFilterConfig {
        &self.config
    }

    /// Check if a message passes the filter.
    ///
    /// Returns `true` if the message should be published, `false` if filtered out.
    pub fn matches(&self, msg: &SyslogMessage, hostname: &str) -> bool {
        // 1. Severity check (most common filter, check first)
        // Lower severity number = more severe (0=emergency, 7=debug)
        if let Some(min) = self.min_severity {
            let msg_severity = msg.severity as u8;
            if msg_severity > min {
                return false;
            }
        }

        // 2. Facility include/exclude
        let facility = msg.facility.as_str().to_lowercase();

        if !self.include_facilities.is_empty() && !self.include_facilities.contains(&facility) {
            return false;
        }

        if self.exclude_facilities.contains(&facility) {
            return false;
        }

        // 3. App name patterns
        if let Some(app) = &msg.app_name {
            if !check_patterns(app, &self.include_app_patterns, &self.exclude_app_patterns) {
                return false;
            }
        } else if !self.include_app_patterns.is_empty() {
            // If we require app patterns but message has no app name, exclude it
            return false;
        }

        // 4. Hostname patterns
        if !check_patterns(
            hostname,
            &self.include_hostname_patterns,
            &self.exclude_hostname_patterns,
        ) {
            return false;
        }

        // 5. Message content patterns
        if !check_patterns(
            &msg.message,
            &self.include_message_patterns,
            &self.exclude_message_patterns,
        ) {
            return false;
        }

        true
    }

    /// Create an empty filter that passes all messages.
    pub fn pass_all() -> Self {
        Self {
            config: SyslogFilterConfig::default(),
            min_severity: None,
            include_facilities: Vec::new(),
            exclude_facilities: Vec::new(),
            include_app_patterns: Vec::new(),
            exclude_app_patterns: Vec::new(),
            include_hostname_patterns: Vec::new(),
            exclude_hostname_patterns: Vec::new(),
            include_message_patterns: Vec::new(),
            exclude_message_patterns: Vec::new(),
        }
    }
}

/// Check if a value passes include/exclude pattern filters.
fn check_patterns(
    value: &str,
    include_patterns: &[CompiledPattern],
    exclude_patterns: &[CompiledPattern],
) -> bool {
    // Check exclude patterns first (any match = exclude)
    for pattern in exclude_patterns {
        if pattern.is_match(value) {
            return false;
        }
    }

    // If include patterns exist, at least one must match
    if !include_patterns.is_empty() {
        return include_patterns.iter().any(|p| p.is_match(value));
    }

    true
}

/// Error when compiling a filter pattern.
#[derive(Debug, Clone)]
pub struct FilterCompileError {
    /// The pattern that failed to compile.
    pub pattern: String,
    /// Error message.
    pub error: String,
}

impl std::fmt::Display for FilterCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to compile pattern '{}': {}",
            self.pattern, self.error
        )
    }
}

impl std::error::Error for FilterCompileError {}

/// Filter statistics.
#[derive(Debug, Default)]
pub struct FilterStats {
    /// Total messages received.
    pub messages_received: AtomicU64,
    /// Messages that passed filters.
    pub messages_passed: AtomicU64,
    /// Messages filtered out.
    pub messages_filtered: AtomicU64,
}

impl FilterStats {
    /// Record a message that passed the filter.
    pub fn record_passed(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
        self.messages_passed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a message that was filtered out.
    pub fn record_filtered(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
        self.messages_filtered.fetch_add(1, Ordering::Relaxed);
    }

    /// Get current statistics.
    pub fn snapshot(&self) -> FilterStatsSnapshot {
        FilterStatsSnapshot {
            messages_received: self.messages_received.load(Ordering::Relaxed),
            messages_passed: self.messages_passed.load(Ordering::Relaxed),
            messages_filtered: self.messages_filtered.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of filter statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterStatsSnapshot {
    /// Total messages received.
    pub messages_received: u64,
    /// Messages that passed filters.
    pub messages_passed: u64,
    /// Messages filtered out.
    pub messages_filtered: u64,
}

/// Information about a dynamic filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicFilterInfo {
    /// Filter ID.
    pub id: String,
    /// Filter configuration.
    pub config: SyslogFilterConfig,
}

/// Thread-safe filter manager with base and dynamic filters.
pub struct FilterManager {
    /// Base filter from configuration.
    base_filter: CompiledSyslogFilter,
    /// Dynamic filters added at runtime.
    dynamic_filters: Arc<RwLock<HashMap<String, CompiledSyslogFilter>>>,
    /// Filter statistics.
    stats: Arc<FilterStats>,
}

impl FilterManager {
    /// Create a new filter manager with a base filter.
    pub fn new(base_config: &SyslogFilterConfig) -> Result<Self, FilterCompileError> {
        let base_filter = CompiledSyslogFilter::compile(base_config)?;

        Ok(Self {
            base_filter,
            dynamic_filters: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(FilterStats::default()),
        })
    }

    /// Create a filter manager that passes all messages.
    pub fn pass_all() -> Self {
        Self {
            base_filter: CompiledSyslogFilter::pass_all(),
            dynamic_filters: Arc::new(RwLock::new(HashMap::new())),
            stats: Arc::new(FilterStats::default()),
        }
    }

    /// Check if a message passes all filters.
    ///
    /// The message must pass the base filter AND all dynamic filters.
    pub async fn matches(&self, msg: &SyslogMessage, hostname: &str) -> bool {
        // Check base filter first
        if !self.base_filter.matches(msg, hostname) {
            self.stats.record_filtered();
            return false;
        }

        // Check dynamic filters
        let dynamic = self.dynamic_filters.read().await;
        for filter in dynamic.values() {
            if !filter.matches(msg, hostname) {
                self.stats.record_filtered();
                return false;
            }
        }

        self.stats.record_passed();
        true
    }

    /// Add a dynamic filter.
    pub async fn add_filter(
        &self,
        id: String,
        config: &SyslogFilterConfig,
    ) -> Result<(), FilterCompileError> {
        let compiled = CompiledSyslogFilter::compile(config)?;
        let mut filters = self.dynamic_filters.write().await;
        filters.insert(id, compiled);
        Ok(())
    }

    /// Remove a dynamic filter by ID.
    pub async fn remove_filter(&self, id: &str) -> bool {
        let mut filters = self.dynamic_filters.write().await;
        filters.remove(id).is_some()
    }

    /// Clear all dynamic filters.
    pub async fn clear_filters(&self) {
        let mut filters = self.dynamic_filters.write().await;
        filters.clear();
    }

    /// Get the base filter configuration.
    pub fn base_config(&self) -> &SyslogFilterConfig {
        self.base_filter.config()
    }

    /// Get information about all dynamic filters.
    pub async fn dynamic_filter_info(&self) -> Vec<DynamicFilterInfo> {
        let filters = self.dynamic_filters.read().await;
        filters
            .iter()
            .map(|(id, filter)| DynamicFilterInfo {
                id: id.clone(),
                config: filter.config().clone(),
            })
            .collect()
    }

    /// Get filter statistics.
    pub fn stats(&self) -> FilterStatsSnapshot {
        self.stats.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    fn parse_msg(s: &str) -> SyslogMessage {
        parser::parse(s).expect("Failed to parse test message")
    }

    #[test]
    fn test_glob_to_regex() {
        assert_eq!(glob_to_regex("*"), "^.*$");
        assert_eq!(glob_to_regex("systemd-*"), "^systemd-.*$");
        assert_eq!(glob_to_regex("*.log"), "^.*\\.log$");
        assert_eq!(glob_to_regex("test?"), "^test.$");
    }

    #[test]
    fn test_empty_filter_passes_all() {
        let config = SyslogFilterConfig::default();
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        let msg = parse_msg("<14>test message");
        assert!(filter.matches(&msg, "localhost"));
    }

    #[test]
    fn test_severity_filter() {
        let config = SyslogFilterConfig {
            min_severity: Some(4), // Warning and above
            ..Default::default()
        };
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        // Warning (4) - should pass
        let msg = parse_msg("<12>warning message"); // 12 = user.warning
        assert!(filter.matches(&msg, "localhost"));

        // Error (3) - should pass (more severe)
        let msg = parse_msg("<11>error message"); // 11 = user.error
        assert!(filter.matches(&msg, "localhost"));

        // Info (6) - should be filtered (less severe)
        let msg = parse_msg("<14>info message"); // 14 = user.info
        assert!(!filter.matches(&msg, "localhost"));

        // Debug (7) - should be filtered
        let msg = parse_msg("<15>debug message"); // 15 = user.debug
        assert!(!filter.matches(&msg, "localhost"));
    }

    #[test]
    fn test_facility_include() {
        let config = SyslogFilterConfig {
            include_facilities: vec!["auth".to_string(), "daemon".to_string()],
            ..Default::default()
        };
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        // Auth facility - should pass
        let msg = parse_msg("<34>Jan  5 14:30:00 host sshd: test"); // auth.crit
        assert!(filter.matches(&msg, "host"));

        // Daemon facility - should pass
        let msg = parse_msg("<30>Jan  5 14:30:00 host cron: test"); // daemon.info
        assert!(filter.matches(&msg, "host"));

        // User facility - should be filtered
        let msg = parse_msg("<14>user message"); // user.info
        assert!(!filter.matches(&msg, "host"));
    }

    #[test]
    fn test_facility_exclude() {
        let config = SyslogFilterConfig {
            exclude_facilities: vec!["local7".to_string()],
            ..Default::default()
        };
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        // User facility - should pass
        let msg = parse_msg("<14>user message");
        assert!(filter.matches(&msg, "host"));

        // Local7 facility - should be filtered
        let msg = parse_msg("<190>local7 message"); // 23*8 + 6 = 190 = local7.info
        assert!(!filter.matches(&msg, "host"));
    }

    #[test]
    fn test_app_pattern_glob() {
        let config = SyslogFilterConfig {
            exclude_app_patterns: vec![PatternFilter {
                pattern: "systemd-*".to_string(),
                pattern_type: PatternType::Glob,
            }],
            ..Default::default()
        };
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        // Regular app - should pass
        let msg = parse_msg("<14>Jan  5 14:30:00 host sshd: test");
        assert!(filter.matches(&msg, "host"));

        // systemd-journald - should be filtered
        let msg = parse_msg("<14>Jan  5 14:30:00 host systemd-journald: test");
        assert!(!filter.matches(&msg, "host"));
    }

    #[test]
    fn test_app_pattern_regex() {
        let config = SyslogFilterConfig {
            include_app_patterns: vec![PatternFilter {
                pattern: "^(sshd|nginx|apache)$".to_string(),
                pattern_type: PatternType::Regex,
            }],
            ..Default::default()
        };
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        // sshd - should pass
        let msg = parse_msg("<14>Jan  5 14:30:00 host sshd: test");
        assert!(filter.matches(&msg, "host"));

        // cron - should be filtered
        let msg = parse_msg("<14>Jan  5 14:30:00 host cron: test");
        assert!(!filter.matches(&msg, "host"));
    }

    #[test]
    fn test_hostname_pattern() {
        let config = SyslogFilterConfig {
            include_hostname_patterns: vec![PatternFilter {
                pattern: "web-*".to_string(),
                pattern_type: PatternType::Glob,
            }],
            ..Default::default()
        };
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        // Matching hostname
        let msg = parse_msg("<14>test message");
        assert!(filter.matches(&msg, "web-01"));
        assert!(filter.matches(&msg, "web-server"));

        // Non-matching hostname
        assert!(!filter.matches(&msg, "db-01"));
    }

    #[test]
    fn test_message_pattern() {
        let config = SyslogFilterConfig {
            exclude_message_patterns: vec![PatternFilter {
                pattern: "*HEALTHCHECK*".to_string(),
                pattern_type: PatternType::Glob,
            }],
            ..Default::default()
        };
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        // Regular message - should pass
        let msg = parse_msg("<14>Jan  5 14:30:00 host app: User logged in");
        assert!(filter.matches(&msg, "host"));

        // Health check - should be filtered
        let msg = parse_msg("<14>Jan  5 14:30:00 host app: HEALTHCHECK OK");
        assert!(!filter.matches(&msg, "host"));
    }

    #[test]
    fn test_combined_filters() {
        let config = SyslogFilterConfig {
            min_severity: Some(4), // Warning and above
            exclude_facilities: vec!["local7".to_string()],
            exclude_app_patterns: vec![PatternFilter {
                pattern: "systemd-*".to_string(),
                pattern_type: PatternType::Glob,
            }],
            ..Default::default()
        };
        let filter = CompiledSyslogFilter::compile(&config).unwrap();

        // Passes all filters
        let msg = parse_msg("<12>Jan  5 14:30:00 host sshd: warning message"); // user.warning
        assert!(filter.matches(&msg, "host"));

        // Fails severity (info)
        let msg = parse_msg("<14>Jan  5 14:30:00 host sshd: info message"); // user.info
        assert!(!filter.matches(&msg, "host"));

        // Fails app pattern
        let msg = parse_msg("<12>Jan  5 14:30:00 host systemd-journald: warning");
        assert!(!filter.matches(&msg, "host"));
    }

    #[tokio::test]
    async fn test_filter_manager() {
        let base_config = SyslogFilterConfig {
            min_severity: Some(6), // Info and above
            ..Default::default()
        };

        let manager = FilterManager::new(&base_config).unwrap();

        // Info message passes
        let msg = parse_msg("<14>info message");
        assert!(manager.matches(&msg, "host").await);

        // Debug message filtered
        let msg = parse_msg("<15>debug message");
        assert!(!manager.matches(&msg, "host").await);

        // Add dynamic filter
        let dynamic_config = SyslogFilterConfig {
            exclude_app_patterns: vec![PatternFilter {
                pattern: "noisy-*".to_string(),
                pattern_type: PatternType::Glob,
            }],
            ..Default::default()
        };
        manager
            .add_filter("filter1".to_string(), &dynamic_config)
            .await
            .unwrap();

        // Info from normal app still passes
        let msg = parse_msg("<14>Jan  5 14:30:00 host app: info");
        assert!(manager.matches(&msg, "host").await);

        // Info from noisy app now filtered
        let msg = parse_msg("<14>Jan  5 14:30:00 host noisy-service: info");
        assert!(!manager.matches(&msg, "host").await);

        // Remove dynamic filter
        manager.remove_filter("filter1").await;

        // Noisy app passes again
        let msg = parse_msg("<14>Jan  5 14:30:00 host noisy-service: info");
        assert!(manager.matches(&msg, "host").await);
    }

    #[tokio::test]
    async fn test_filter_stats() {
        let manager = FilterManager::new(&SyslogFilterConfig {
            min_severity: Some(4),
            ..Default::default()
        })
        .unwrap();

        let msg_pass = parse_msg("<12>warning message"); // warning
        let msg_fail = parse_msg("<14>info message"); // info

        manager.matches(&msg_pass, "host").await;
        manager.matches(&msg_fail, "host").await;
        manager.matches(&msg_pass, "host").await;

        let stats = manager.stats();
        assert_eq!(stats.messages_received, 3);
        assert_eq!(stats.messages_passed, 2);
        assert_eq!(stats.messages_filtered, 1);
    }

    #[test]
    fn test_filter_config_is_empty() {
        let empty = SyslogFilterConfig::default();
        assert!(empty.is_empty());

        let with_severity = SyslogFilterConfig {
            min_severity: Some(4),
            ..Default::default()
        };
        assert!(!with_severity.is_empty());
    }
}
