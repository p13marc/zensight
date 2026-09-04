//! The wire target type and the file target type must not drift (#936).
//!
//! `zensight_common::targets::ProbeTarget` is deliberately the file-config
//! `Target` **minus `headers`** — the field that carries an
//! `Authorization: Bearer …` in practice, does not go through
//! `zensight_sensor_core::secret`, and must therefore never reach the bus.
//!
//! Two types with one shape is a drift hazard, and the drift is silent in the
//! direction that matters: a field added to the file type and not the wire
//! type is a knob a fleet quietly cannot set, with no error anywhere. This
//! test is the reason that split was affordable at all — the alternative,
//! `#[serde(flatten)]`, moved ninety field accesses in this crate for the same
//! guarantee.

use zensight_common::probe::ProbeKind;
use zensight_common::targets::ProbeTarget;
use zensight_sensor_probe::config::Target;

/// Populate every field so nothing is elided by `skip_serializing_if`, then
/// compare the two key sets.
fn file_target() -> Target {
    Target {
        name: "site".into(),
        kind: ProbeKind::Http,
        target: "https://example.com".into(),
        interval_secs: Some(30),
        timeout_secs: Some(5),
        expect_status: vec![200],
        expect_body: Some("ok".into()),
        follow_redirects: true,
        allow_offhost_redirect: false,
        method: Some("GET".into()),
        headers: vec![("Authorization".into(), "Bearer hunter2".into())],
        count: Some(10),
        spacing_ms: Some(100),
        transport: Some("tcp".into()),
        server_name: Some("example.com".into()),
        inspect_untrusted: true,
        resolver: Some("1.1.1.1:53".into()),
        expect_addrs: vec!["93.184.216.34".into()],
        enabled: true,
    }
}

fn wire_target() -> ProbeTarget {
    ProbeTarget {
        name: "site".into(),
        kind: ProbeKind::Http,
        target: "https://example.com".into(),
        interval_secs: Some(30),
        timeout_secs: Some(5),
        expect_status: vec![200],
        expect_body: Some("ok".into()),
        follow_redirects: true,
        allow_offhost_redirect: false,
        method: Some("GET".into()),
        count: Some(10),
        spacing_ms: Some(100),
        transport: Some("tcp".into()),
        server_name: Some("example.com".into()),
        inspect_untrusted: true,
        resolver: Some("1.1.1.1:53".into()),
        expect_addrs: vec!["93.184.216.34".into()],
        enabled: true,
    }
}

fn keys(v: &serde_json::Value) -> std::collections::BTreeSet<String> {
    v.as_object().expect("object").keys().cloned().collect()
}

#[test]
fn probe_target_spec_is_the_file_target_minus_headers() {
    let file = keys(&serde_json::to_value(file_target()).expect("encode file target"));
    let wire = keys(&serde_json::to_value(wire_target()).expect("encode wire target"));

    let only_in_file: Vec<&String> = file.difference(&wire).collect();
    assert_eq!(
        only_in_file,
        vec![&"headers".to_string()],
        "the ONLY field the file target may have that the wire target does not is `headers`. \
         Anything else here is a knob a fleet cannot set, with no error to say so"
    );
    let only_in_wire: Vec<&String> = wire.difference(&file).collect();
    assert!(
        only_in_wire.is_empty(),
        "the wire target grew a field the sensor cannot apply: {only_in_wire:?}"
    );
}

/// The wire type refuses the field rather than dropping it silently — a
/// payload that carries `headers` fails to decode, so an operator learns
/// where headers belong instead of watching a probe run without them.
#[test]
fn a_wire_payload_carrying_headers_is_refused() {
    let mut doc = serde_json::to_value(wire_target()).expect("encode");
    doc.as_object_mut().unwrap().insert(
        "headers".into(),
        serde_json::json!([["Authorization", "Bearer hunter2"]]),
    );
    let err = serde_json::from_value::<ProbeTarget>(doc)
        .expect_err("a header on the wire must not decode");
    assert!(
        err.to_string().contains("headers"),
        "the error should name the field: {err}"
    );
}
