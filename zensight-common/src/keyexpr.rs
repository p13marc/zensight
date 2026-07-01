use crate::telemetry::Protocol;

/// Default key expression prefix for all ZenSight telemetry.
pub const KEY_PREFIX: &str = "zensight";

/// Error type for key expression parsing.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("key expression too short: expected at least 4 segments, got {0}")]
    TooFewSegments(usize),
    #[error("invalid prefix: expected '{expected}', got '{actual}'")]
    InvalidPrefix {
        expected: &'static str,
        actual: String,
    },
    #[error("unknown protocol: '{0}'")]
    UnknownProtocol(String),
    #[error("empty source identifier")]
    EmptySource,
}

/// Builder for constructing ZenSight key expressions.
///
/// Key expressions follow the pattern:
/// `zensight/<protocol>/<source>/<metric_path>`
#[derive(Debug, Clone)]
pub struct KeyExprBuilder {
    prefix: String,
    protocol: Protocol,
}

impl KeyExprBuilder {
    /// Create a new key expression builder for a protocol.
    pub fn new(protocol: Protocol) -> Self {
        Self {
            prefix: KEY_PREFIX.to_string(),
            protocol,
        }
    }

    /// Create a builder with a custom prefix.
    pub fn with_prefix(prefix: impl Into<String>, protocol: Protocol) -> Self {
        Self {
            prefix: prefix.into(),
            protocol,
        }
    }

    /// Build a key expression for a specific source and metric.
    ///
    /// # Panics
    ///
    /// Debug-asserts that `source` and `metric` are non-empty and don't contain
    /// double slashes (`//`).
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Snmp);
    /// let key = builder.build("router01", "system/sysUpTime");
    /// assert_eq!(key, "zensight/snmp/router01/system/sysUpTime");
    /// ```
    pub fn build(&self, source: &str, metric: &str) -> String {
        debug_assert!(!source.is_empty(), "source must not be empty");
        debug_assert!(!metric.is_empty(), "metric must not be empty");
        debug_assert!(
            !source.contains("//") && !metric.contains("//"),
            "source and metric must not contain '//'"
        );
        format!(
            "{}/{}/{}/{}",
            self.prefix,
            self.protocol.as_str(),
            source,
            metric
        )
    }

    /// Build a wildcard key expression for all metrics from a source.
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Snmp);
    /// let key = builder.source_wildcard("router01");
    /// assert_eq!(key, "zensight/snmp/router01/**");
    /// ```
    pub fn source_wildcard(&self, source: &str) -> String {
        format!("{}/{}/{}/**", self.prefix, self.protocol.as_str(), source)
    }

    /// Build a wildcard key expression for all sources of this protocol.
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Snmp);
    /// let key = builder.protocol_wildcard();
    /// assert_eq!(key, "zensight/snmp/**");
    /// ```
    pub fn protocol_wildcard(&self) -> String {
        format!("{}/{}/**", self.prefix, self.protocol.as_str())
    }

    /// Build a key expression for sensor status.
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Snmp);
    /// let key = builder.status_key();
    /// assert_eq!(key, "zensight/snmp/@/status");
    /// ```
    pub fn status_key(&self) -> String {
        format!("{}/{}/@/status", self.prefix, self.protocol.as_str())
    }

    /// Build a key expression for a single keyed alert.
    ///
    /// Matches: `zensight/<protocol>/@/alerts/<alert_key>`
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Netlink);
    /// assert_eq!(
    ///     builder.alert_key_expr("ssh-listening-0011223344556677"),
    ///     "zensight/netlink/@/alerts/ssh-listening-0011223344556677"
    /// );
    /// ```
    pub fn alert_key_expr(&self, alert_key: &str) -> String {
        format!(
            "{}/{}/@/alerts/{}",
            self.prefix,
            self.protocol.as_str(),
            alert_key
        )
    }
}

/// Build a wildcard key expression for all ZenSight telemetry.
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_telemetry_wildcard;
///
/// assert_eq!(all_telemetry_wildcard(), "zensight/**");
/// ```
pub fn all_telemetry_wildcard() -> String {
    format!("{}/**", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor health data.
///
/// Matches: `zensight/<protocol>/@/health`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_health_wildcard;
///
/// assert_eq!(all_health_wildcard(), "zensight/*/@/health");
/// ```
pub fn all_health_wildcard() -> String {
    format!("{}/*/@/health", KEY_PREFIX)
}

/// Build a wildcard key expression for all device liveness data.
///
/// Matches: `zensight/<protocol>/@/devices/<device>/liveness`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_liveness_wildcard;
///
/// assert_eq!(all_liveness_wildcard(), "zensight/*/@/devices/*/liveness");
/// ```
pub fn all_liveness_wildcard() -> String {
    format!("{}/*/@/devices/*/liveness", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor error reports.
///
/// Matches: `zensight/<protocol>/@/errors`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_errors_wildcard;
///
/// assert_eq!(all_errors_wildcard(), "zensight/*/@/errors");
/// ```
pub fn all_errors_wildcard() -> String {
    format!("{}/*/@/errors", KEY_PREFIX)
}

/// Build a wildcard key expression for all correlation data.
///
/// Matches: `zensight/_meta/correlation/<ip>`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_correlation_wildcard;
///
/// assert_eq!(all_correlation_wildcard(), "zensight/_meta/correlation/*");
/// ```
pub fn all_correlation_wildcard() -> String {
    format!("{}/_meta/correlation/*", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor discovery data.
///
/// Matches: `zensight/_meta/sensors/<sensor_name>`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_sensors_wildcard;
///
/// assert_eq!(all_sensors_wildcard(), "zensight/_meta/sensors/*");
/// ```
pub fn all_sensors_wildcard() -> String {
    format!("{}/_meta/sensors/*", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor-emitted alerts.
///
/// Matches: `zensight/<protocol>/@/alerts/<alert_key>`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_alerts_wildcard;
///
/// assert_eq!(all_alerts_wildcard(), "zensight/*/@/alerts/*");
/// ```
pub fn all_alerts_wildcard() -> String {
    format!("{}/*/@/alerts/*", KEY_PREFIX)
}

/// Parse a key expression to extract protocol, source, and metric path.
///
/// Returns a descriptive error if the key expression doesn't match the expected pattern.
pub fn parse_key_expr(key: &str) -> Result<ParsedKeyExpr<'_>, ParseError> {
    let parts: Vec<&str> = key.split('/').collect();

    if parts.len() < 4 {
        return Err(ParseError::TooFewSegments(parts.len()));
    }

    if parts[0] != KEY_PREFIX {
        return Err(ParseError::InvalidPrefix {
            expected: KEY_PREFIX,
            actual: parts[0].to_string(),
        });
    }

    let protocol = match parts[1] {
        "snmp" => Protocol::Snmp,
        "logs" => Protocol::Logs,
        "gnmi" => Protocol::Gnmi,
        "netflow" => Protocol::Netflow,
        "opcua" => Protocol::Opcua,
        "modbus" => Protocol::Modbus,
        "sysinfo" => Protocol::Sysinfo,
        "netlink" => Protocol::Netlink,
        "netring" => Protocol::Netring,
        "systemd" => Protocol::Systemd,
        other => return Err(ParseError::UnknownProtocol(other.to_string())),
    };

    let source = parts[2];
    if source.is_empty() {
        return Err(ParseError::EmptySource);
    }

    let metric = parts[3..].join("/");

    Ok(ParsedKeyExpr {
        protocol,
        source,
        metric,
    })
}

/// Parsed components of a ZenSight key expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedKeyExpr<'a> {
    pub protocol: Protocol,
    pub source: &'a str,
    pub metric: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_builder() {
        let builder = KeyExprBuilder::new(Protocol::Snmp);

        assert_eq!(
            builder.build("router01", "system/sysUpTime"),
            "zensight/snmp/router01/system/sysUpTime"
        );

        assert_eq!(
            builder.source_wildcard("router01"),
            "zensight/snmp/router01/**"
        );

        assert_eq!(builder.protocol_wildcard(), "zensight/snmp/**");

        assert_eq!(builder.status_key(), "zensight/snmp/@/status");
    }

    #[test]
    fn test_parse_key_expr() {
        let parsed = parse_key_expr("zensight/snmp/router01/system/sysUpTime").unwrap();

        assert_eq!(parsed.protocol, Protocol::Snmp);
        assert_eq!(parsed.source, "router01");
        assert_eq!(parsed.metric, "system/sysUpTime");
    }

    #[test]
    fn test_parse_sysinfo_key_expr() {
        let parsed = parse_key_expr("zensight/sysinfo/server01/cpu/usage").unwrap();
        assert_eq!(parsed.protocol, Protocol::Sysinfo);
        assert_eq!(parsed.source, "server01");
        assert_eq!(parsed.metric, "cpu/usage");
    }

    #[test]
    fn test_parse_invalid_key() {
        assert!(matches!(
            parse_key_expr("invalid/key"),
            Err(ParseError::TooFewSegments(2))
        ));
        assert!(matches!(
            parse_key_expr("zensight/unknown/device/metric"),
            Err(ParseError::UnknownProtocol(_))
        ));
        assert!(matches!(
            parse_key_expr("other/snmp/device/metric"),
            Err(ParseError::InvalidPrefix { .. })
        ));
    }

    #[test]
    fn test_all_telemetry_wildcard() {
        assert_eq!(all_telemetry_wildcard(), "zensight/**");
    }
}
