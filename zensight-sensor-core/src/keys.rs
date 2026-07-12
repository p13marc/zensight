//! LEGACY control-plane key construction (#457). Only the channels not
//! yet migrated to v1 build keys here (status, alerts, commands, artifacts
//! — they move with #460/#461); everything else derives from
//! [`crate::v1::V1Context`].
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
            status_key("zensight/sysinfo/hosta"),
            "zensight/sysinfo/hosta/@/status"
        );
        assert_eq!(
            telemetry_key("zensight/sysinfo/hosta", "cpu/usage"),
            "zensight/sysinfo/hosta/cpu/usage"
        );
    }
}
