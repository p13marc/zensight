//! Registry codegen round-trip tests (RFC 08 §1): build → parse identity,
//! parse precedence, procedure keys, introspection slice.

use zensight_keyspace::grammar::{self, Class, ClassOrPlane, Origin, Plane, Producer};
use zensight_keyspace::origin::HostId;
use zensight_keyspace::registry::{self, netring};

fn host() -> Origin {
    Origin::Host(HostId::parse("h-3fa9c2d41b7e").unwrap())
}

#[test]
fn build_parse_round_trip() {
    let cases = [
        netring::Subject::FlowRed {
            quantile: "p95_ms".into(),
        },
        netring::Subject::Bandwidth {
            iface: "eth0".into(),
            direction: "rx".into(),
        },
        netring::Subject::Health,
        netring::Subject::Alert {
            alert_key: "9f2c81ab04d7e3f1".into(),
        },
        netring::Subject::EvidenceSelf,
        netring::Subject::EvidenceNames {
            ip_slug: "10-0-0-7".into(),
        },
        netring::Subject::Capture {
            ulid: "01jgxqz4yqk8v6txw3m9f2a7cd".into(),
        },
    ];
    for subject in cases {
        let key = netring::key(&host(), &subject).unwrap();
        let parsed = grammar::parse(&key).unwrap();
        let ClassOrPlane::Class(class) = parsed.class else {
            panic!("data key parsed as plane: {key}");
        };
        assert_eq!(class, subject.class(), "{key}");
        let producer = parsed.producer.unwrap();
        assert_eq!(producer.name(), "netring");
        let tail: Vec<&str> = parsed.subject.iter().map(String::as_str).collect();
        let refined =
            netring::Subject::parse(class, &tail).unwrap_or_else(|| panic!("unparseable: {key}"));
        assert_eq!(refined, subject, "{key}");
        // Cross-producer dispatch agrees.
        match registry::parse_subject(producer.name(), class, &tail) {
            Some(registry::AnySubject::Netring(s)) => assert_eq!(s, subject),
            other => panic!("dispatch failed for {key}: {other:?}"),
        }
    }
}

#[test]
fn concrete_keys() {
    assert_eq!(
        netring::key(
            &host(),
            &netring::Subject::FlowRed {
                quantile: "p95_ms".into()
            }
        )
        .unwrap(),
        "@v1/h-3fa9c2d41b7e/telemetry/netring/flow/red/p95_ms"
    );
    assert_eq!(
        netring::key(
            &host(),
            &netring::Subject::Alert {
                alert_key: "9f2c81ab04d7e3f1".into()
            }
        )
        .unwrap(),
        "@v1/h-3fa9c2d41b7e/state/netring/alert/9f2c81ab04d7e3f1"
    );
}

#[test]
fn class_mismatch_does_not_parse() {
    // `health` is registered as state — a telemetry tail must not refine to it.
    assert!(netring::Subject::parse(Class::Telemetry, &["health"]).is_none());
    assert!(netring::Subject::parse(Class::State, &["health"]).is_some());
}

#[test]
fn literal_beats_var_precedence() {
    // `evidence/self` (all literal) vs `evidence/names/{ip_slug}` vs
    // `alert/{alert_key}`: the literal pattern must win its exact tail even
    // though var patterns of the same arity exist.
    assert_eq!(
        netring::Subject::parse(Class::State, &["evidence", "self"]),
        Some(netring::Subject::EvidenceSelf)
    );
    assert_eq!(
        netring::Subject::parse(Class::State, &["alert", "deadbeef00000000"]),
        Some(netring::Subject::Alert {
            alert_key: "deadbeef00000000".into()
        })
    );
    // Unregistered tails refine to nothing.
    assert!(netring::Subject::parse(Class::State, &["bogus"]).is_none());
}

#[test]
fn subject_metadata() {
    let alert = netring::Subject::Alert {
        alert_key: "x".into(),
    };
    assert_eq!(alert.qos(), zensight_keyspace::QosProfile::Alert);
    assert_eq!(alert.ttl_s(), Some(900));
    assert_eq!(alert.pattern(), "alert/{alert_key}");
    let flow = netring::Subject::FlowRed {
        quantile: "p50_ms".into(),
    };
    assert_eq!(flow.qos(), zensight_keyspace::QosProfile::Sampled);
    assert_eq!(flow.ttl_s(), None);
}

#[test]
fn procedures() {
    assert_eq!(
        netring::rpc_key(&host(), netring::ProcedureId::CaptureTrigger).unwrap(),
        "@v1/h-3fa9c2d41b7e/@rpc/netring/capture/trigger"
    );
    assert_eq!(netring::ProcedureId::CaptureTrigger.kind(), "write");
    assert_eq!(netring::ProcedureId::Flows.kind(), "read");
    assert!(netring::ProcedureId::ALL.contains(&netring::ProcedureId::Introspect));
    // The rpc key parses back structurally.
    let parsed = grammar::parse("@v1/h-3fa9c2d41b7e/@rpc/netring/capture/trigger").unwrap();
    assert_eq!(parsed.class, ClassOrPlane::Plane(Plane::Rpc));
}

#[test]
fn instance_suffixed_producer_keys() {
    let netring2 = Producer::with_instance("netring", 2).unwrap();
    let key = netring::key_as(&host(), &netring2, &netring::Subject::Health).unwrap();
    assert_eq!(key, "@v1/h-3fa9c2d41b7e/state/netring-2/health");
    // Parses back to base name + instance, so registry dispatch still works.
    let parsed = grammar::parse(&key).unwrap();
    let producer = parsed.producer.unwrap();
    assert_eq!((producer.name(), producer.instance()), ("netring", Some(2)));
}

#[test]
fn introspection_slice_is_the_registry_file() {
    assert!(netring::REGISTRY_TOML.contains("flow/red/{quantile}"));
    assert!(netring::REGISTRY_TOML.contains("[registry]"));
}
