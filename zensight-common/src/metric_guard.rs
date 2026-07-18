//! Publish-time registry conformance for telemetry (RFC 08 §5, issue #468).
//!
//! The registry's central promise is *"a subject that is not registered does
//! not exist"* — which is worth nothing unless somebody checks. Sensors build
//! telemetry keys by appending the metric name to `telemetry/<producer>` as a
//! raw string, so the two publish paths — [`crate::PublisherRegistry::put`]
//! (baseline tier) and `AdvancedPublisherRegistry::build_key` (advanced tier)
//! — are the only places where the registry and the wire can be compared.
//!
//! Debug builds panic: a test that publishes an unregistered metric fails,
//! which is exactly the lint #468 says does not exist today. Release builds
//! warn once per metric name — a drift in the field is loud, not a log flood.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use zenkey::grammar::{self, Class, ClassOrPlane};

fn warned() -> &'static Mutex<HashSet<String>> {
    static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    WARNED.get_or_init(Default::default)
}

/// Check that a key we are about to publish is buildable from a registry entry.
///
/// Non-telemetry keys, unparseable keys and unknown producers pass silently:
/// this guards the telemetry tree, which is the one #468 refined. Producers
/// whose metric tree is defined by the polled device (`snmp`, `modbus`,
/// `gnmi`, `netflow`) keep a rest-var subject by design, so everything they
/// publish is registered and this is a no-op for them.
pub fn check_telemetry_key(key: &str) {
    // The publish path is base-relative (#466): the session namespace adds the
    // base. A full key here would still *parse* if we searched for "v1/" —
    // which is how this guard used to work, and would mean it kept passing
    // while the publisher put keys somewhere nothing is listening. Fail on it
    // instead: a full key reaching a publisher is a bug in the caller.
    debug_assert!(
        !key.starts_with(zenkey::DEFAULT_BASE),
        "the publish path was handed a FULL key {key:?} — application keys are base-relative \
         (#466) and the session namespace supplies the base"
    );
    let Ok(parsed) = grammar::parse(key) else {
        return;
    };
    if !matches!(parsed.class, ClassOrPlane::Class(Class::Telemetry)) {
        return;
    }
    let Some(producer) = parsed.producer.as_ref() else {
        return;
    };
    let tail: Vec<&str> = parsed.subject.iter().map(String::as_str).collect();
    if zenkey::registry::parse_subject(producer.name(), Class::Telemetry, &tail).is_some() {
        return;
    }

    let name = producer.name();
    let metric = tail.join("/");
    debug_assert!(
        false,
        "unregistered telemetry subject {metric:?} — add it to \
         zenkey/registry/ (zenkey repo){name}.toml (RFC 08 §5, issue #468)"
    );
    let mut seen = warned().lock().unwrap_or_else(|e| e.into_inner());
    if seen.insert(format!("{name}/{metric}")) {
        tracing::warn!(
            producer = %name,
            metric = %metric,
            "publishing an unregistered telemetry subject — introspect cannot describe it (RFC 08 §5)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_metric_passes() {
        check_telemetry_key("v1/h-0123456789ab/telemetry/sysinfo/cpu/usage");
        check_telemetry_key("v1/h-0123456789ab/telemetry/sysinfo/disk/sda/io/read_bytes");
    }

    #[test]
    fn rest_var_producers_pass() {
        // snmp's tree is whatever the device exposes — a rest-var by design.
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/if/1/in_octets");
    }

    #[test]
    fn non_telemetry_and_junk_pass() {
        check_telemetry_key("v1/h-0123456789ab/state/sysinfo/health");
        check_telemetry_key("not-a-v1-key");
    }

    #[test]
    #[should_panic(expected = "unregistered telemetry subject")]
    fn unregistered_metric_panics_in_debug() {
        check_telemetry_key("v1/h-0123456789ab/telemetry/sysinfo/not/a/real/metric");
    }
}
