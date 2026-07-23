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
        .json::<crate::command::Command<crate::stream::StreamControl>>("Command<StreamControl>")
        // ── query-detail records (this crate, fully derived) ──────────────
        .json::<crate::query_detail::LatencyReport>("LatencyReport")
        .json::<crate::query_detail::UnitDetail>("UnitDetail")
        .json::<Vec<crate::query_detail::CgroupNode>>("CgroupNode")
        .json::<Vec<crate::entity::NameVal>>("Vec<NameVal>")
        .json::<Vec<crate::stream::StreamDescriptor>>("Vec<StreamDescriptor>")
        .json::<Vec<crate::query_detail::AssetRecord>>("Vec<AssetRecord>")
        .json::<Vec<crate::query_detail::CaptureRecord>>("Vec<CaptureRecord>")
        .json::<Vec<crate::query_detail::DnsRecord>>("Vec<DnsRecord>")
        .json::<Vec<crate::query_detail::EncryptedDnsRecord>>("Vec<EncryptedDnsRecord>")
        .json::<Vec<crate::query_detail::FlowRecord>>("Vec<FlowRecord>")
        .json::<Vec<crate::query_detail::Ja4hRecord>>("Vec<Ja4hRecord>")
        .json::<Vec<crate::query_detail::LogRecord>>("Vec<LogRecord>")
        .json::<Vec<crate::query_detail::MatrixRecord>>("Vec<MatrixRecord>")
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
        .entry("ActionStatus", summary("systemd action outcome — defined in zensight-sensor-systemd::action"))
        .entry("CaptureDiskCommand", summary("netring capture-to-disk command — defined in zensight-sensor-netring::command"))
        .entry("CaptureDiskStatus", summary("netring capture-to-disk status — defined in zensight-sensor-netring::command"))
        .entry("ExpectationCommand", summary("netlink sentinel expectation command — defined in zensight-sensor-netlink::command"))
        .entry("ExpectationsConfig", summary("sentinel expectations config — defined in zensight-sensor-{netlink,systemd}::sentinel"))
        .entry("Vec<EventRecord>", summary("event ring records — defined in zensight-sensor-{netlink,systemd}::events"))
        .entry("Vec<AddressRecord>", summary("netlink address records — defined in zensight-sensor-netlink"))
        .entry("Vec<BandwidthRecord>", summary("bandwidth records — defined in zensight-sensor-{netlink,netring}"))
        .entry("Vec<ConnectionRecord>", summary("conntrack records — defined in zensight-sensor-netlink"))
        .entry("Vec<NftRecord>", summary("nftables records — defined in zensight-sensor-netlink"))
        .entry("Vec<RetransmitRecord>", summary("TCP retransmit records — defined in zensight-sensor-netlink"))
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
        .entry("ServiceAction", summary("systemd service action request — declared only, ad-hoc JSON on the wire"))
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

#[cfg(test)]
mod tests {
    use super::*;

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
