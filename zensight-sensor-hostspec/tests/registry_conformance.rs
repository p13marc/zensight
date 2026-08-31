//! Reverse registry conformance for hostspec (#654 pattern, RFC 08 §6.1).
//!
//! Forward (*published ⊆ registered*) is `telemetry_guard::checked_point`'s
//! debug-assert. This asserts the reverse: *registered ⊆ emittable* — a
//! family the registry advertises that no build can publish is a surface
//! `introspect` promises the fleet and nobody serves. hostspec registers
//! exactly one telemetry family (the failing-count gauge, #821), and this is
//! the test that keeps that "exactly one" honest in both directions.

use zensight_common::registry::hostspec::Subject;
use zensight_common::registry_audit;
use zensight_sensor_hostspec::map;

const CONDITIONAL_FAMILIES: &[(&str, &str)] = &[];

#[test]
fn every_registered_family_has_an_emitter() {
    let emitted: Vec<String> = vec![map::failing_point("h", 0).metric];
    registry_audit::assert_families_covered(
        "hostspec",
        emitted,
        |m| Subject::parse_metric(m).map(|s| s.pattern()),
        CONDITIONAL_FAMILIES,
    );
}
