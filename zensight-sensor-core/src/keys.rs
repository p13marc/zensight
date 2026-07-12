//! Control-plane key construction, centralized (#457).
//!
//! Every legacy control-plane key the framework emits is built here — one
//! module to edit when the key grammar moves (epic #453). Output is
//! byte-identical to the previous inline `format!` sites; see
//! `docs/KEYSPACE.md` for the shapes.

/// Host-scoped control prefix for one sensor instance:
/// `{key_prefix}/{source}` (e.g. `zensight/sysinfo/hostA`). All per-instance
/// state channels (`@/health`, `@/errors`, `@/status`, `@/alive`,
/// `@/devices/**`) hang off it.
pub(crate) fn control_prefix(key_prefix: &str, source: &str) -> String {
    format!("{key_prefix}/{source}")
}

/// Health key: `{prefix}/{source}/@/health` (legacy `{prefix}/@/health`
/// without a source).
pub(crate) fn health_key(prefix: &str, source: Option<&str>) -> String {
    match source {
        Some(s) => format!("{prefix}/{s}/@/health"),
        None => format!("{prefix}/@/health"),
    }
}

/// Errors key: `{prefix}/{source}/@/errors` (legacy `{prefix}/@/errors`
/// without a source).
pub(crate) fn errors_key(prefix: &str, source: Option<&str>) -> String {
    match source {
        Some(s) => format!("{prefix}/{s}/@/errors"),
        None => format!("{prefix}/@/errors"),
    }
}

/// Device-liveness document key:
/// `{prefix}[/{source}]/@/devices/{device}/liveness`.
pub(crate) fn device_liveness_key(prefix: &str, source: Option<&str>, device: &str) -> String {
    match source {
        Some(s) => format!("{prefix}/{s}/@/devices/{device}/liveness"),
        None => format!("{prefix}/@/devices/{device}/liveness"),
    }
}

/// Sensor liveliness token key: `{key_prefix}/@/alive`.
pub(crate) fn alive_key(key_prefix: &str) -> String {
    format!("{key_prefix}/@/alive")
}

/// Device liveliness token key: `{key_prefix}/@/devices/{device}/alive`.
pub(crate) fn device_alive_key(key_prefix: &str, device: &str) -> String {
    format!("{key_prefix}/@/devices/{device}/alive")
}

/// Status document key: `{control_prefix}/@/status`.
pub(crate) fn status_key(control_prefix: &str) -> String {
    format!("{control_prefix}/@/status")
}

/// Telemetry key: `{key_prefix}/{suffix}` (the suffix is the metric path).
pub(crate) fn telemetry_key(key_prefix: &str, suffix: &str) -> String {
    format!("{key_prefix}/{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin byte-identical output vs the pre-#457 inline sites.
    #[test]
    fn shapes_are_unchanged() {
        assert_eq!(
            control_prefix("zensight/sysinfo", "hosta"),
            "zensight/sysinfo/hosta"
        );
        assert_eq!(
            health_key("zensight/snmp", Some("poller01")),
            "zensight/snmp/poller01/@/health"
        );
        assert_eq!(health_key("zensight/snmp", None), "zensight/snmp/@/health");
        assert_eq!(
            errors_key("zensight/snmp", Some("poller01")),
            "zensight/snmp/poller01/@/errors"
        );
        assert_eq!(
            device_liveness_key("zensight/snmp", Some("poller01"), "router01"),
            "zensight/snmp/poller01/@/devices/router01/liveness"
        );
        assert_eq!(
            alive_key("zensight/snmp/poller01"),
            "zensight/snmp/poller01/@/alive"
        );
        assert_eq!(
            device_alive_key("zensight/snmp/poller01", "router01"),
            "zensight/snmp/poller01/@/devices/router01/alive"
        );
        assert_eq!(
            status_key("zensight/sysinfo/hosta"),
            "zensight/sysinfo/hosta/@/status"
        );
        assert_eq!(
            telemetry_key("zensight/sysinfo/hosta", "cpu/usage"),
            "zensight/sysinfo/hosta/cpu/usage"
        );
    }
}
