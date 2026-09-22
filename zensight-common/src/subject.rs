//! The typed spelling of a telemetry subject (#1274, RFC 08 §1).
//!
//! `zenkey-build` derives one `Subject` enum per producer from the registry
//! TOML, with a slugging constructor per declared path
//! (`sysinfo::Subject::cpu_core_usage(core)`) and a `key()` that renders it
//! under an origin. Until now every sensor still *spelled* its subject as a
//! string at the publish site and reused the string as `TelemetryPoint.metric`
//! — checked on every put by [`crate::metric_guard`], which is a runtime
//! answer to a question the registry can settle at compile time: a subject
//! the registry does not declare has no constructor.
//!
//! [`TelemetrySubject`] is the one trait every generated `Subject` implements,
//! so a publisher can take *any* producer's subject and render the key
//! itself; [`TelemetryPoint::for_subject`](crate::TelemetryPoint::for_subject)
//! builds the point with the subject's tail as its metric, so the metric
//! string and the key can no longer disagree.

use zenkey::grammar::{Class, Producer};
use zenkey::key::Key;
use zenkey::origin::LocalOrigin;

/// A registry-declared subject of one producer, as the generated
/// `registry::<producer>::Subject` enums implement it.
pub trait TelemetrySubject: std::fmt::Debug {
    /// The producer the registry declares this subject under.
    fn producer(&self) -> &'static str;
    /// The class the registry declares — a subject enum carries a producer's
    /// state and telemetry subjects alike, so a publisher checks.
    fn class(&self) -> Class;
    /// The full base-relative key under `origin`, as the producer's own
    /// registry name.
    fn key(&self, origin: &LocalOrigin) -> Key;
    /// The full base-relative key under `origin` as `producer` — the
    /// instance-suffixed spelling (`snmp-2`) when a host runs two.
    fn key_as(&self, origin: &LocalOrigin, producer: &Producer) -> Key;
    /// The subject's `{vars}`, bound: what a label that names the same thing
    /// as the key should carry (a `chassis` label is the `{chassis}` chunk).
    fn vars(&self) -> Vec<(&'static str, String)>;

    /// The subject tail — chunks 5.. of the key — which is what
    /// `TelemetryPoint.metric` carries and what a consumer names a series by.
    fn tail(&self) -> String {
        let key = self.key(&crate::PROFILE.local_origin());
        crate::keyexpr::parse_key(key.as_str())
            .map(|k| k.subject.join("/"))
            .unwrap_or_default()
    }
}

macro_rules! impl_telemetry_subject {
    ($($producer:ident),* $(,)?) => {
        $(
            impl TelemetrySubject for crate::registry::$producer::Subject {
                fn producer(&self) -> &'static str {
                    stringify!($producer)
                }
                fn class(&self) -> Class {
                    crate::registry::$producer::Subject::class(self)
                }
                fn key(&self, origin: &LocalOrigin) -> Key {
                    crate::registry::$producer::key(origin, self)
                }
                fn key_as(&self, origin: &LocalOrigin, producer: &Producer) -> Key {
                    crate::registry::$producer::key_as(origin, producer, self)
                }
                fn vars(&self) -> Vec<(&'static str, String)> {
                    crate::registry::$producer::Subject::vars(self)
                }
            }
        )*
    };
}

// Every producer registry that declares a telemetry subject.
impl_telemetry_subject!(
    bmc, container, gnmi, hostspec, logs, modbus, netflow, netlink, netring, parallax, probe, pve,
    snmp, sysinfo, systemd,
);

/// The key a telemetry subject is published under (#1274): rendered from
/// the subject as `producer` on this host, refused when the subject is not
/// telemetry — a state document has its own publish path, and rendering it
/// here would put a document on the telemetry plane under a metric name.
pub fn telemetry_key(subject: &impl TelemetrySubject, producer: &Producer) -> Result<Key, String> {
    let class = subject.class();
    if class != Class::Telemetry {
        return Err(format!(
            "{subject:?} is a {class:?} subject of {}, not telemetry — a document goes through \
             publish_json / publish_serializable, not publish_subject",
            subject.producer()
        ));
    }
    Ok(subject.key_as(&crate::PROFILE.local_origin(), producer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    /// A generated subject renders the tail its registry declares, and the
    /// key parses back to the same subject through the registry's parse
    /// direction (RFC 08 §1) — the round trip the guard checked at runtime.
    #[test]
    fn a_subject_round_trips_through_the_registry() {
        let subject = registry::bmc::Subject::fan_rpm("1", "3");
        assert_eq!(subject.producer(), "bmc");
        assert_eq!(subject.class(), Class::Telemetry);
        assert_eq!(subject.tail(), "1/fan/3/rpm");
        let key = subject.key(&crate::PROFILE.local_origin());
        let (_, producer, parsed) =
            crate::keyexpr::refine_key(key.as_str()).expect("a v1 key of a registered subject");
        assert_eq!(producer, "bmc");
        assert!(matches!(
            parsed,
            registry::AnySubject::Bmc(registry::bmc::Subject::FanRpm { .. })
        ));
        // And the guard has nothing to say about it.
        crate::metric_guard::check_telemetry_key(key.as_str());
    }

    /// A rest-var producer's subject renders its tail chunk by chunk — what
    /// the snmp poller spelled and split by hand.
    #[test]
    fn a_rest_var_subject_renders_its_tail() {
        let subject = registry::snmp::Subject::device_metric("sw1", ["if", "1", "in_octets"]);
        assert_eq!(subject.tail(), "sw1/if/1/in_octets");
        assert_eq!(subject.vars()[0].0, "device");
    }

    /// The builder slugs a foreign value once; the tail carries the chunk.
    #[test]
    fn a_foreign_value_is_slugged_by_the_builder() {
        let subject = registry::bmc::Subject::psu_present("Rack A-1", "PSU 0");
        let chassis = zenkey::Chunk::slug("Rack A-1");
        let psu = zenkey::Chunk::slug("PSU 0");
        assert_eq!(
            subject.tail(),
            format!("{}/psu/{}/present", chassis.as_str(), psu.as_str())
        );
        assert_eq!(
            subject.vars(),
            vec![
                ("chassis", chassis.as_str().to_string()),
                ("psu", psu.as_str().to_string())
            ]
        );
    }

    /// A state subject is refused by the telemetry path, by name.
    #[test]
    fn a_state_subject_is_not_a_telemetry_key() {
        let producer = Producer::new("bmc").unwrap();
        let err = telemetry_key(&registry::bmc::Subject::Health, &producer).unwrap_err();
        assert!(err.contains("State subject of bmc"), "{err}");
        assert!(telemetry_key(&registry::bmc::Subject::reachable("mgmt01"), &producer).is_ok());
        // An instance-suffixed producer renders under its own name.
        let two = Producer::with_instance("bmc", 2).unwrap();
        let key = telemetry_key(&registry::bmc::Subject::reachable("mgmt01"), &two).unwrap();
        assert!(
            key.as_str().contains("/telemetry/bmc-2/"),
            "{}",
            key.as_str()
        );
    }
}
