//! The RFC 08 §5 type table and the RFC 08 §7 `describe` payload.
//!
//! RFC 08 §2 binds every registered subject/procedure to a payload type named
//! by a string in the registry TOML. Two mechanisms keep that column honest:
//!
//! - **Build time**: `zenkey-build`'s types.toml lint — every referenced name
//!   must appear in `registry/types.toml`, and the generated
//!   [`crate::registry::TYPE_NAMES`] is the sorted union of every reference.
//! - **Run time**: [`SCHEMAS`], the fleet-wide `SchemaSet` served on
//!   `@rpc/<producer>/describe`. It is `build_verified` against
//!   `TYPE_NAMES`, so a registry type with no schema entry aborts the first
//!   describe (and the `schema_set_covers_the_registry` test) rather than
//!   silently serving a partial table.
//!
//! Types defined in this crate get full derived JSON Schemas
//! (`schemars`). Names whose Rust definition lives in a sensor crate (or
//! does not exist yet — `Ack`, `TopicStatus`, … are declared-only) get
//! summary entries: honest about the shape's existence, explicit about where
//! the definition lives. Upgrading those to full schemas means either moving
//! the type into this crate or serving a per-producer extended set; both are
//! follow-up work, noted per entry.

use std::sync::LazyLock;

pub use zenkey::schema::{SchemaSet, TypeSchema};

/// The registry's type names minus the `toml` sentinel (`introspect`'s
/// raw-TOML reply is text, not a schema'd payload — RFC 08 §6).
pub fn schema_type_names() -> Vec<&'static str> {
    crate::registry::TYPE_NAMES
        .iter()
        .copied()
        .filter(|n| *n != "toml")
        .collect()
}

/// A summary entry for a type whose definition is not visible from this
/// crate: `where_` names the defining crate (or "declared only").
fn summary(description: &str) -> TypeSchema {
    TypeSchema::json_schema(serde_json::json!({
        "type": "object",
        "description": description,
    }))
}

/// The fleet-wide type table (RFC 08 §7), served by every producer's
/// `describe` procedure. One shared superset — a producer serving entries
/// beyond its own slice is legal (RFC 08 §7); consumers index by name.
pub static SCHEMAS: LazyLock<SchemaSet> = LazyLock::new(|| {
    SchemaSet::builder("zensight")
        // ── framework payloads (this crate, fully derived) ────────────────
        .json::<crate::TelemetryPoint>("TelemetryPoint")
        .json::<crate::Alert>("Alert")
        .json::<crate::HealthSnapshot>("HealthSnapshot")
        .json::<crate::ErrorReport>("ErrorReport")
        .json::<crate::SensorInfo>("SensorInfo")
        .json::<crate::ArtifactStatus>("ArtifactStatus")
        .json::<crate::ArtifactRequest>("ArtifactRequest")
        .json::<crate::DiscoveryReport>("DiscoveryReport")
        .json::<crate::EventRecord>("EventRecord")
        .json::<crate::HostEvidence>("HostEvidence")
        .json::<crate::InterfaceTable>("InterfaceTable")
        .json::<crate::NameObservation>("NameObservation")
        .json::<crate::HostEntity>("HostEntity")
        .json::<crate::AliasRecord>("AliasRecord")
        .json::<crate::OperatorAssertion>("OperatorAssertion")
        .json::<crate::PdnsRecord>("PdnsRecord")
        .json::<crate::StreamStatus>("StreamStatus")
        .json::<crate::stream::FrameMeta>("FrameMeta")
        .json::<crate::stream::MediaReceiverReport>("MediaReceiverReport")
        .json::<crate::command::Command<crate::stream::StreamControl>>("Command<StreamControl>")
        // ── service control (this crate, fully derived) ───────────────────
        .json::<crate::action::ActionCapability>("ActionCapability")
        .json::<crate::action::ActionStatus>("ActionStatus")
        .json::<crate::action::ServiceAction>("ServiceAction")
        .json::<Vec<crate::action::ActionStatus>>("Vec<ActionStatus>")
        // ── query-detail records (this crate, fully derived) ──────────────
        .json::<crate::query_detail::LatencyReport>("LatencyReport")
        .json::<crate::query_detail::UnitDetail>("UnitDetail")
        .json::<crate::query_detail::UnitFile>("UnitFile")
        .json::<Vec<crate::query_detail::CgroupNode>>("CgroupNode")
        .json::<Vec<crate::entity::NameVal>>("Vec<NameVal>")
        .json::<Vec<crate::stream::StreamDescriptor>>("Vec<StreamDescriptor>")
        .json::<Vec<crate::query_detail::AssetRecord>>("Vec<AssetRecord>")
        .json::<Vec<crate::query_detail::CaptureRecord>>("Vec<CaptureRecord>")
        .json::<Vec<crate::query_detail::ConnectionRecord>>("Vec<ConnectionRecord>")
        .json::<Vec<crate::query_detail::DnsRecord>>("Vec<DnsRecord>")
        .json::<Vec<crate::query_detail::EncryptedDnsRecord>>("Vec<EncryptedDnsRecord>")
        .json::<Vec<crate::query_detail::FlowRecord>>("Vec<FlowRecord>")
        .json::<Vec<crate::query_detail::Ja4hRecord>>("Vec<Ja4hRecord>")
        .json::<Vec<crate::query_detail::LogRecord>>("Vec<LogRecord>")
        .json::<Vec<crate::query_detail::MatrixRecord>>("Vec<MatrixRecord>")
        .json::<Vec<crate::query_detail::RetransmitRecord>>("Vec<RetransmitRecord>")
        .json::<Vec<crate::query_detail::NeighborRecord>>("Vec<NeighborRecord>")
        .json::<Vec<crate::query_detail::ProcessRecord>>("Vec<ProcessRecord>")
        .json::<Vec<crate::query_detail::QuicRecord>>("Vec<QuicRecord>")
        .json::<Vec<crate::query_detail::RouteRecord>>("Vec<RouteRecord>")
        .json::<Vec<crate::query_detail::SocketRecord>>("Vec<SocketRecord>")
        .json::<Vec<crate::query_detail::SshRecord>>("Vec<SshRecord>")
        .json::<Vec<crate::query_detail::TalkerRecord>>("Vec<TalkerRecord>")
        .json::<Vec<crate::query_detail::TimerRecord>>("Vec<TimerRecord>")
        .json::<Vec<crate::query_detail::TlsRecord>>("Vec<TlsRecord>")
        .json::<Vec<crate::query_detail::UnitRecord>>("Vec<UnitRecord>")
        // ── registry drift the table makes visible (RFC 08 §5) ────────────
        // The registry says Vec<HttpRecord>/Vec<IpfixRecord>; the wire types
        // are HttpHostRecord/NetflowRecord. Served under the registry name so
        // the table is total; renaming the registry column is a wire change
        // for a follow-up.
        .json::<Vec<crate::query_detail::HttpHostRecord>>("Vec<HttpRecord>")
        .json::<Vec<crate::query_detail::NetflowRecord>>("Vec<IpfixRecord>")
        // ── defined in sensor crates (summary entries) ────────────────────
        .entry("CaptureDiskCommand", summary("netring capture-to-disk command — defined in zensight-sensor-netring::command"))
        .entry("CaptureDiskStatus", summary("netring capture-to-disk status — defined in zensight-sensor-netring::command"))
        .entry("ExpectationCommand", summary("netlink sentinel expectation command — defined in zensight-sensor-netlink::command"))
        .entry("ExpectationsConfig", summary("sentinel expectations config — defined in zensight-sensor-{netlink,systemd}::sentinel"))
        .entry("LogRulesConfig", summary("log sentinel ruleset (pattern→alert rules) — defined in zensight-sensor-logs::sentinel"))
        .entry("RulesStatus", summary("log sentinel ruleset + per-rule hit counters — defined in zensight-sensor-logs::sentinel"))
        .entry("Vec<EventRecord>", summary("event ring records — defined in zensight-sensor-{netlink,systemd}::events"))
        .entry("Vec<AddressRecord>", summary("netlink address records — defined in zensight-sensor-netlink"))
        .entry("Vec<BandwidthRecord>", summary("bandwidth records — defined in zensight-sensor-{netlink,netring}"))
        .entry("Vec<NftRecord>", summary("nftables records — defined in zensight-sensor-netlink"))
        .entry("Vec<RouteChangeRecord>", summary("route-change records — defined in zensight-sensor-netlink"))
        .entry("Vec<TcRecord>", summary("traffic-control records — defined in zensight-sensor-netlink"))
        .entry("Vec<XfrmRecord>", summary("IPsec xfrm records — defined in zensight-sensor-netlink"))
        // ── declared-only names (RFC 08 §5 debt: no Rust definition) ──────
        .entry("Ack", summary("generic write acknowledgement — declared only, ad-hoc JSON on the wire"))
        .entry("ArtifactAck", summary("artifact request acknowledgement { id } — declared only, ad-hoc JSON on the wire"))
        .entry("TopicStatus", summary("topic subscription status — declared only, ad-hoc JSON on the wire"))
        .entry("TopicConfig", summary("topic subscription config — declared only, ad-hoc JSON on the wire"))
        .entry("DetectorConfig", summary("netring detector config — declared only, ad-hoc JSON on the wire"))
        .entry("ThreatIntelConfig", summary("netring threat-intel config — declared only, ad-hoc JSON on the wire"))
        .entry("FilterConfig", summary("capture filter config — declared only, ad-hoc JSON on the wire"))
        .entry("CollectionConfig", summary("netlink collection config — declared only, ad-hoc JSON on the wire"))
        // ── meta entries ──────────────────────────────────────────────────
        .entry(
            "RegistrySlice",
            summary("RFC 08 §6 introspect reply (raw registry TOML envelope) — zenkey::slice::RegistrySlice"),
        )
        .entry(
            "SchemaSet",
            summary("RFC 08 §7 SchemaSet envelope (schema_version/app/types) — the describe reply itself"),
        )
        .build_verified(&schema_type_names())
});

/// The serialized `describe` reply, built once (RFC 08 §7). Every producer
/// serves this same superset next to its `introspect`.
pub static DESCRIBE_JSON: LazyLock<String> = LazyLock::new(|| SCHEMAS.to_json());

// ---------------------------------------------------------------------------
// Payload conformance (RFC 08 §7, #741)
//
// `SCHEMAS` above has always been *served* — every producer answers `describe`
// with it — and never *used*: nothing in this workspace validated a payload
// against it. `verdict_for` closes that, behind the `validate-json` feature.
//
// **The GUI wiring is deliberately NOT here yet.** #741 asks for the verdict to
// be surfaced in "the GUI's payload-inspection paths (subject/detail views,
// artifact/query paths)". Those paths do not exist: the GUI decodes bytes into
// typed structs at `zensight/src/subscription.rs`'s `decode_sample` and drops
// them, `zensight/src/store.rs` keeps numeric samples only, and no view renders
// a payload body except systemd's unit file (verbatim text, not a schema'd
// type). There is no place to put a verdict chip, so wiring one in means
// building a payload inspector first — a feature, not an upgrade consequence.
//
// What that change needs, when it happens: a reply's declared type name, which
// the generated registry already carries (`ProcedureId::reply_type()`); the
// ~10 `String::from_utf8_lossy(&sample.payload()…)` RPC-reply parse sites in
// `zensight/src/app.rs`; and a three-state chip built on
// `view::components::kit::badge` with `theme::colors(..).status_healthy()` /
// `status_error()` / `status_unknown()` — never a two-state check mark, since
// the whole point is that `NotValidated` must read as absent rather than as a
// pass.
// ---------------------------------------------------------------------------

/// Did a payload conform to the schema its registry type declares?
///
/// Re-exported so a consumer gets the three-state answer without a direct
/// `zenkey` dependency, the same way [`crate::CommonState`] is.
pub use zenkey::schema::validate::{NotValidated, Verdict};

/// Compiled JSON Schema validators, keyed by schema hash — compiling one per
/// sample would put a schema compile on every row of a payload inspector.
#[cfg(feature = "validate-json")]
static VALIDATORS: LazyLock<zenkey::schema::compiled::CompiledCache<jsonschema::Validator>> =
    LazyLock::new(zenkey::schema::compiled::CompiledCache::new);

/// Validate `value` against the fleet type table's schema for `type_name`
/// (RFC 08 §7).
///
/// **Three states, never a boolean.** "I did not check" must never render like
/// "I checked and it passed", so the not-checked case says *why*:
///
/// | Answer | Meaning |
/// |---|---|
/// | [`Verdict::Valid`] | checked against a real draft-2020-12 schema, conformant |
/// | [`Verdict::Invalid`] | checked, with one sentence per violation and its instance path |
/// | [`NotValidated::FeatureOff`] | this binary was built without `validate-json` |
/// | [`NotValidated::NoSchema`] | the table was consulted and serves nothing for this type |
/// | [`NotValidated::KindUnsupported`] | the entry is a `protobuf`/`cdr` schema, which has no validator beyond its own decode |
/// | [`NotValidated::BadSchema`] | the served document does not compile as a schema |
///
/// `NoSchema` and `FeatureOff` are deliberately different answers, and neither
/// is `Valid`: one is "asked, and the type has none", the other is "nobody
/// looked" (RFC 09 §5.1 O4).
///
/// Note what a `Valid` from this table is worth for the *summary* entries in
/// [`SCHEMAS`] — the types whose Rust definition lives in a sensor crate get
/// `{"type": "object"}`, so conformance to one means "it is a JSON object" and
/// no more. That is honest, not a bug: the schema is thin, so the claim is
/// thin. Upgrading those is the same follow-up noted on each entry.
#[must_use]
pub fn verdict_for(type_name: &str, value: &serde_json::Value) -> Verdict {
    let Some(schema) = SCHEMAS.get(type_name) else {
        return Verdict::NotValidated(NotValidated::NoSchema);
    };
    verdict_against(schema, value)
}

/// [`verdict_for`] against an already-resolved schema entry — for a consumer
/// holding a [`SchemaSet`] parsed from a remote producer's `describe` reply
/// rather than this build's compiled-in table.
#[must_use]
#[cfg_attr(not(feature = "validate-json"), expect(unused_variables))]
pub fn verdict_against(schema: &TypeSchema, value: &serde_json::Value) -> Verdict {
    #[cfg(not(feature = "validate-json"))]
    {
        Verdict::NotValidated(NotValidated::FeatureOff)
    }
    #[cfg(feature = "validate-json")]
    {
        let Some(document) = schema.json_document() else {
            // protobuf / cdr: a successful decode already proves structural
            // conformance to the served descriptor, and there is no schema
            // language underneath to violate while still decoding.
            return Verdict::NotValidated(NotValidated::KindUnsupported);
        };
        let compiled = VALIDATORS.get_or_compile(schema, |_| jsonschema::validator_for(document));
        match compiled {
            Ok(validator) => zenkey::schema::validate::validate_json(&validator, value),
            Err(e) => {
                tracing::debug!(error = %e, "served schema does not compile");
                Verdict::NotValidated(NotValidated::BadSchema)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three states are three states. Whatever the build, a payload that
    /// was not checked must never come back as [`Verdict::Valid`] — that is
    /// the whole contract (#741, RFC 09 §5.1 O4).
    #[test]
    fn an_unknown_type_is_not_validated_rather_than_valid() {
        let v = verdict_for("NoSuchTypeName", &serde_json::json!({}));
        assert_eq!(v, Verdict::NotValidated(NotValidated::NoSchema));
        assert_ne!(v, Verdict::Valid);
        // The two silences must not share a spelling: "the table has nothing
        // for this type" is not "this binary cannot check".
        assert_ne!(
            NotValidated::NoSchema.to_string(),
            NotValidated::FeatureOff.to_string()
        );
    }

    /// Without the `validate-json` feature every checkable type answers
    /// `FeatureOff` — honest, and never a fake pass.
    #[cfg(not(feature = "validate-json"))]
    #[test]
    fn without_the_feature_a_real_type_is_feature_off() {
        let point = crate::TelemetryPoint::new(
            "h",
            crate::Protocol::Sysinfo,
            "m",
            crate::TelemetryValue::Gauge(1.0),
        );
        let v = verdict_for("TelemetryPoint", &serde_json::to_value(&point).unwrap());
        assert_eq!(v, Verdict::NotValidated(NotValidated::FeatureOff));
    }

    /// With the feature, a real payload validates against its own derived
    /// schema, and a malformed one is `Invalid` with a violation per problem.
    #[cfg(feature = "validate-json")]
    #[test]
    fn with_the_feature_valid_and_invalid_are_both_reachable() {
        let point = crate::TelemetryPoint::new(
            "h",
            crate::Protocol::Sysinfo,
            "m",
            crate::TelemetryValue::Gauge(1.0),
        );
        assert_eq!(
            verdict_for("TelemetryPoint", &serde_json::to_value(&point).unwrap()),
            Verdict::Valid
        );

        // A `TelemetryPoint` is an object with required fields; a bare array
        // is not one.
        match verdict_for("TelemetryPoint", &serde_json::json!([])) {
            Verdict::Invalid(errors) => assert!(!errors.is_empty(), "no violations reported"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// A summary entry (`{"type": "object"}`) validates thinly and says so by
    /// being thin, not by pretending. Pinned so nobody reads a `Valid` from
    /// one of these as a real conformance claim.
    #[cfg(feature = "validate-json")]
    #[test]
    fn a_summary_schema_is_a_thin_claim_not_a_missing_one() {
        assert_eq!(
            verdict_for("Ack", &serde_json::json!({"x": 1})),
            Verdict::Valid
        );
        match verdict_for("Ack", &serde_json::json!("not an object")) {
            Verdict::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    /// RFC 08 §5: "A `type` name not present in the type table fails CI."
    /// `build_verified` panics on a gap; instantiating is the assertion.
    #[test]
    fn schema_set_covers_the_registry() {
        assert!(SCHEMAS.len() >= schema_type_names().len());
        for name in schema_type_names() {
            assert!(
                SCHEMAS.get(name).is_some(),
                "registry type {name:?} has no schema entry"
            );
        }
    }

    /// The derived entries are real schemas, not stubs: spot-check that a
    /// field the GUI depends on is present in the emitted document.
    #[test]
    fn derived_schemas_carry_fields() {
        let point = SCHEMAS.get("TelemetryPoint").unwrap();
        let doc = point.json_document().expect("json schema");
        let props = doc["properties"].as_object().expect("object schema");
        assert!(
            props.contains_key("metric"),
            "TelemetryPoint.metric missing"
        );
        assert!(props.contains_key("value"), "TelemetryPoint.value missing");
    }

    /// The reply round-trips through the consumer-side parser (what zenctl /
    /// zenkey-fleet's SchemaStore does with the bytes).
    #[test]
    fn describe_reply_parses_back() {
        let parsed = SchemaSet::parse(&DESCRIBE_JSON).expect("describe JSON parses");
        assert_eq!(parsed.app(), "zensight");
        assert_eq!(parsed.len(), SCHEMAS.len());
    }
}
