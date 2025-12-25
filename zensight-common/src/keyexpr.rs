use crate::telemetry::Protocol;

/// Default key expression prefix for all ZenSight telemetry.
pub const KEY_PREFIX: &str = "zensight";

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

    /// Build a key expression for bridge status.
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

/// Parse a key expression to extract protocol, source, and metric path.
///
/// Returns `None` if the key expression doesn't match the expected pattern.
pub fn parse_key_expr(key: &str) -> Option<ParsedKeyExpr<'_>> {
    let parts: Vec<&str> = key.split('/').collect();

    if parts.len() < 4 || parts[0] != KEY_PREFIX {
        return None;
    }

    let protocol = match parts[1] {
        "snmp" => Protocol::Snmp,
        "syslog" => Protocol::Syslog,
        "gnmi" => Protocol::Gnmi,
        "netflow" => Protocol::Netflow,
        "opcua" => Protocol::Opcua,
        "modbus" => Protocol::Modbus,
        _ => return None,
    };

    let source = parts[2];
    let metric = parts[3..].join("/");

    Some(ParsedKeyExpr {
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
    fn test_parse_invalid_key() {
        assert!(parse_key_expr("invalid/key").is_none());
        assert!(parse_key_expr("zensight/unknown/device/metric").is_none());
        assert!(parse_key_expr("other/snmp/device/metric").is_none());
    }

    #[test]
    fn test_all_telemetry_wildcard() {
        assert_eq!(all_telemetry_wildcard(), "zensight/**");
    }
}
