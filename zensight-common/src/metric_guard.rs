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

/// Run `warn` the first time `entry` is seen in this process — a drift in the
/// field is loud, not a log flood.
fn warn_once(entry: String, warn: impl FnOnce()) {
    let mut seen = warned().lock().unwrap_or_else(|e| e.into_inner());
    if seen.insert(entry) {
        warn();
    }
}

/// Check that a key we are about to publish is buildable from a registry entry.
///
/// Non-telemetry keys and unknown producers pass silently: this guards the
/// telemetry tree, which is the one #468 refined. An illegal chunk inside a
/// registered or rest-var subject already trips it — `parse_subject` checks
/// the grammar chunk by chunk (#559) — so a foreign value that skipped
/// [`Chunk::slug`] with a space or an upper-case letter is caught below as
/// "unregistered" — and so is an empty chunk. **A key that is not a v1 key at
/// all does not pass either** (#1153): the wrong class word, a malformed
/// origin, a bare name. Those used to return early here and be *waved
/// through*, so the one production-time check on the wire said nothing about
/// exactly the keys no `v1/**` pattern subscriber would ever match. Debug
/// builds panic; release builds warn once per key.
///
/// [`Chunk::slug`]: zenkey::Chunk::slug Producers
/// whose metric tree is defined by the polled device (`snmp`, `modbus`,
/// `gnmi`, `netflow`) keep a rest-var subject by design, so every
/// *grammar-valid* name they publish is registered. The rest-var does NOT
/// excuse a chunk-grammar violation: a mixed-case metric name fails
/// `parse_subject` chunk-by-chunk and trips the guard like any unregistered
/// subject (#559) — which is why those producers slug at the publish boundary.
pub fn check_telemetry_key(key: &str) {
    // The publish path is base-relative (#466): the session namespace adds the
    // base. A full key here would still *parse* if we searched for "v1/" —
    // which is how this guard used to work, and would mean it kept passing
    // while the publisher put keys somewhere nothing is listening. Fail on it
    // instead: a full key reaching a publisher is a bug in the caller. The
    // check is heuristic — it only catches the conventional base spelling.
    debug_assert!(
        !key.starts_with(crate::CONVENTIONAL_BASE),
        "the publish path was handed a FULL key {key:?} — application keys are base-relative \
         (#466) and the session namespace supplies the base"
    );
    let parsed = match grammar::parse(key) {
        Ok(parsed) => parsed,
        Err(e) => {
            debug_assert!(
                false,
                "publishing a key that does not parse as v1 ({e}): {key:?} — a chunk built from \
                 a foreign value must go through `device_chunk` (#1153)"
            );
            warn_once(key.to_string(), || {
                tracing::warn!(
                    key = %key,
                    error = %e,
                    "publishing a key outside the v1 grammar — no pattern subscriber will match it (#1153)"
                );
            });
            return;
        }
    };
    if !matches!(parsed.class, ClassOrPlane::Class(Class::Telemetry)) {
        return;
    }
    let Some(producer) = parsed.producer() else {
        return;
    };
    let tail: &[&str] = &parsed.subject;
    if crate::registry::parse_subject(producer.name(), Class::Telemetry, tail).is_some() {
        return;
    }

    let name = producer.name();
    let metric = tail.join("/");
    debug_assert!(
        false,
        "unregistered telemetry subject {metric:?} — add it to \
         zensight-common/registry/{name}.toml (RFC 08 §5, issue #468)"
    );
    warn_once(format!("{name}/{metric}"), || {
        tracing::warn!(
            producer = %name,
            metric = %metric,
            "publishing an unregistered telemetry subject — introspect cannot describe it (RFC 08 §5)"
        );
    });
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
        //
        // Only *half* true since #779 and #783: the four indexed tables
        // (ifTable, ifXTable, hrProcessorTable, ipAddrTable, hrStorageTable)
        // are now registered column by column, so those keys match a real
        // pattern and the catch-all is what everything *else* rides. Both
        // routes pass this guard, which is the point — the guard asks whether
        // a key is buildable, not which entry built it.
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/if/1/in_octets");
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/storage/1/size");
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/cpu/1/load");
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/ip/1/netmask");
        // …and the ip group's scalars, which stay on the catch-all.
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/ip/in_receives.rate");
        // `.rate` siblings (#527), trap counters, and the dotted-OID fallback
        // are all grammar-valid chunks and ride the same rest-var.
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/if/1/in_octets.rate");
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/trap/link_down");
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/1.3.6.1.4.1.9.9.999.0");
    }

    /// #559: a rest-var subject does not excuse the chunk grammar — the
    /// mixed-case names the builtin MIB table used to emit trip the guard.
    #[test]
    #[should_panic(expected = "unregistered telemetry subject")]
    fn rest_var_grammar_violation_panics_in_debug() {
        check_telemetry_key("v1/h-0123456789ab/telemetry/snmp/router1/sysUpTime.0");
    }

    #[test]
    fn non_telemetry_passes() {
        check_telemetry_key("v1/h-0123456789ab/state/sysinfo/health");
        check_telemetry_key("v1/h-0123456789ab/state/sysinfo/alert/0123456789abcdef");
        check_telemetry_key("v1/h-0123456789ab/events/snmp/router1/trap/01HZ");
    }

    /// An illegal chunk in a *registered* subject is already refused (#559):
    /// the structural parse accepts it, the registry match does not. A raw
    /// `format!` of a mount point with a space lands here.
    #[test]
    #[should_panic(expected = "unregistered telemetry subject")]
    fn an_illegal_chunk_in_a_registered_subject_panics_in_debug() {
        check_telemetry_key("v1/h-0123456789ab/telemetry/sysinfo/disk/Var Lib/usage");
    }

    /// An empty chunk — what a raw `format!("{prefix}/{name}")` produces for
    /// an empty name — parses structurally and is refused by the registry
    /// match, like any other illegal chunk.
    #[test]
    #[should_panic(expected = "unregistered telemetry subject")]
    fn an_empty_chunk_is_refused_in_debug() {
        check_telemetry_key("v1/h-0123456789ab/telemetry/sysinfo/disk//usage");
    }

    /// #1153: a key that does not parse as v1 at all used to return early and
    /// pass — the only production-time check on the wire waved through
    /// exactly the keys nothing could subscribe to.
    #[test]
    #[should_panic(expected = "does not parse as v1")]
    fn a_non_v1_key_panics_in_debug() {
        check_telemetry_key("not-a-v1-key");
    }

    #[test]
    #[should_panic(expected = "does not parse as v1")]
    fn a_malformed_origin_panics_in_debug() {
        check_telemetry_key("v1/not-an-origin/telemetry/sysinfo/cpu/usage");
    }

    /// The slug boundary's output always passes: whatever the foreign value,
    /// `Chunk::slug` yields a chunk the guard accepts — shown on a rest-var
    /// producer, whose whole tail is the foreign value.
    #[test]
    fn a_slugged_chunk_always_passes() {
        for v in [
            "/var/lib/docker",
            "NetworkManager.service",
            "a b/c",
            "::1",
            "-x",
            "",
        ] {
            let chunk = zenkey::Chunk::slug(v);
            check_telemetry_key(&format!(
                "v1/h-0123456789ab/telemetry/snmp/router1/{}/in_octets",
                chunk.as_str()
            ));
        }
    }

    #[test]
    #[should_panic(expected = "unregistered telemetry subject")]
    fn unregistered_metric_panics_in_debug() {
        check_telemetry_key("v1/h-0123456789ab/telemetry/sysinfo/not/a/real/metric");
    }
}
