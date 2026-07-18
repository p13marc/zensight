//! Reading a registry-refined telemetry subject in the views (issue #475).
//!
//! `TelemetryPoint::metric` **is** the telemetry subject tail, verbatim (the
//! publisher appends it to `telemetry/<producer>`), so a view can refine a
//! metric name straight into its producer's typed `Subject` with
//! `Subject::parse_metric`.
//!
//! Two shapes show up in the views:
//!
//! * **A specific measurement** — "which mounts does this host report?" is
//!   `disk/{mount}/used`, and the typed match hands back `mount` directly:
//!   `Some(Subject::DiskUsed { mount }) => …`. No helper needed.
//!
//! * **A whole family as a table** — "every stat for every interface" spans
//!   fifteen registered subjects that share `iface/{iface}/…`. The view wants
//!   the *dimension* (`iface`) and the *measurement name* (`rx_bytes`), and
//!   both come out of the registry: the dimension from [`var`], the measurement
//!   from [`leaf`] (the last chunk of the registered pattern). That is the
//!   difference from the code this replaced, which read the dimension off
//!   `parts[1]` and the measurement off `parts[2]` and would have silently
//!   produced an empty table if the subject ever moved.

/// The measurement name at the end of a registered pattern — the literal leaf.
///
/// `iface/{iface}/rx_bytes` → `rx_bytes`. A pattern whose leaf is a variable
/// (`tcp/{state}`) has no literal measurement name, so this returns the
/// variable's placeholder; families like that are matched by variant instead.
pub fn leaf(pattern: &'static str) -> &'static str {
    pattern.rsplit('/').next().unwrap_or(pattern)
}

/// A named variable binding from a refined subject's [`vars()`].
///
/// The point of the parse direction: `{iface}` is read by *name*, never by
/// position.
pub fn var(vars: &[(&'static str, String)], name: &str) -> Option<String> {
    vars.iter()
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zensight_common::registry::netlink::Subject;

    #[test]
    fn leaf_is_the_registered_measurement_name() {
        let s = Subject::parse_metric("iface/eth0/rx_bytes").unwrap();
        assert_eq!(leaf(s.pattern()), "rx_bytes");
        assert_eq!(var(&s.vars(), "iface").as_deref(), Some("eth0"));
        assert_eq!(var(&s.vars(), "nope"), None);
    }

    #[test]
    fn a_deeper_family_still_names_its_dimension() {
        // `tc/{iface}/{kind}/drops` — two dimensions, one measurement.
        let s = Subject::parse_metric("tc/eth0/fq_codel/drops").unwrap();
        assert_eq!(leaf(s.pattern()), "drops");
        assert_eq!(var(&s.vars(), "iface").as_deref(), Some("eth0"));
        assert_eq!(var(&s.vars(), "kind").as_deref(), Some("fq_codel"));
    }

    /// An unregistered metric refines to nothing, so it never reaches a table.
    #[test]
    fn unregistered_metrics_are_not_in_any_family() {
        assert!(Subject::parse_metric("iface/eth0/rx_bytez").is_none());
    }
}
