use crate::telemetry::Protocol;

/// Default key expression prefix for all ZenSight telemetry.
pub const KEY_PREFIX: &str = "zensight";

/// Build the v1 telemetry class selector (all producers, all origins).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_telemetry_wildcard;
///
/// assert_eq!(all_telemetry_wildcard(), "zensight/@v1/*/telemetry/**");
/// ```
pub fn all_telemetry_wildcard() -> String {
    // v1 (RFC 04): the telemetry class selector — nothing to discard
    // client-side (incumbent pain P6 retired).
    format!("{}/@v1/*/telemetry/**", KEY_PREFIX)
}

/// Caller-side fleet procedure selector (RFC 05 §2): GET
/// `<base>/@v1/*/@rpc/<producer>/<procedure...>` reaches every host serving
/// the producer. Callers MUST use query target `All` (RFC 05 §2.1) —
/// `BestMatching` can short-circuit the fan-in.
pub fn fleet_rpc_key(producer: &str, procedure: &str) -> String {
    format!("{}/@v1/*/@rpc/{}/{}", KEY_PREFIX, producer, procedure)
}

/// Caller-side fleet write selector: the `<topic>/set` procedure fleet-wide.
pub fn fleet_command_key(producer: &str, topic: &str) -> String {
    fleet_rpc_key(producer, &format!("{topic}/set"))
}

/// Caller-side single-host procedure key (RFC 05 §2): GET
/// `<base>/@v1/<origin>/@rpc/<producer>/<procedure...>` reaches exactly one
/// host's producer — use when the origin is already known (e.g. a drill-down
/// view), [`fleet_rpc_key`] otherwise.
pub fn origin_rpc_key(origin: &str, producer: &str, procedure: &str) -> String {
    format!(
        "{}/@v1/{}/@rpc/{}/{}",
        KEY_PREFIX, origin, producer, procedure
    )
}

/// Build a wildcard key expression for the whole fleet state plane.
pub fn all_state_wildcard() -> String {
    // v1 (RFC 04): the whole fleet state plane, one selector.
    format!("{}/@v1/*/state/**", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor health data.
///
/// Matches: `zensight/@v1/<origin>/state/<producer>/health`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_health_wildcard;
///
/// assert_eq!(all_health_wildcard(), "zensight/@v1/*/state/*/health");
/// ```
pub fn all_health_wildcard() -> String {
    format!("{}/@v1/*/state/*/health", KEY_PREFIX)
}

/// Build a wildcard key expression for all device liveness data.
///
/// Matches: `zensight/<protocol>/<source>/@/devices/<device>/liveness`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_liveness_wildcard;
///
/// assert_eq!(all_liveness_wildcard(), "zensight/@v1/*/state/*/device/*/liveness");
/// ```
pub fn all_liveness_wildcard() -> String {
    // v1 (RFC 04): device-liveness documents under every producer.
    format!("{}/@v1/*/state/*/device/*/liveness", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor error reports.
///
/// Matches: `zensight/@v1/<origin>/state/<producer>/errors`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_errors_wildcard;
///
/// assert_eq!(all_errors_wildcard(), "zensight/@v1/*/state/*/errors");
/// ```
pub fn all_errors_wildcard() -> String {
    format!("{}/@v1/*/state/*/errors", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor discovery data.
///
/// Matches: `zensight/@v1/<origin>/state/<producer>/sensor`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_sensors_wildcard;
///
/// assert_eq!(all_sensors_wildcard(), "zensight/@v1/*/state/*/sensor");
/// ```
pub fn all_sensors_wildcard() -> String {
    // v1: the registration document (state/<producer>/sensor).
    format!("{}/@v1/*/state/*/sensor", KEY_PREFIX)
}

/// Build this host's v1 evidence key for one observed device (RFC 06 §4):
/// `zensight/@v1/<local-origin>/state/<sensor>/evidence/device/<device>`.
/// The device chunk is slugged, so raw hostnames/IPs/MACs are safe inputs;
/// consumers read the observed identity from the payload, not the key.
pub fn host_evidence_key(sensor: &str, device: &str) -> String {
    format!(
        "{}/@v1/{}/state/{}/evidence/device/{}",
        KEY_PREFIX,
        zensight_keyspace::context::host_id().as_str(),
        sensor,
        zensight_keyspace::slug::chunk_slug(device)
    )
}

/// Build a wildcard key expression for the whole evidence keyspace
/// (`host/**` claims and `names/**` observation batches).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_evidence_wildcard;
///
/// assert_eq!(all_evidence_wildcard(), "zensight/@v1/*/state/*/evidence/**");
/// ```
pub fn all_evidence_wildcard() -> String {
    // v1 (RFC 06 §4): evidence is ordinary per-origin state.
    format!("{}/@v1/*/state/*/evidence/**", KEY_PREFIX)
}

/// Build this host's v1 name-observation key for one observed IP (#307,
/// RFC 06 §4): the ip is slugified (`.`/`:` → `-`) so updates for the same
/// IP replace in place:
/// `zensight/@v1/<local-origin>/state/<sensor>/evidence/names/<ip-slug>`.
pub fn name_observation_key(sensor: &str, ip_slug: &str) -> String {
    format!(
        "{}/@v1/{}/state/{}/evidence/names/{}",
        KEY_PREFIX,
        zensight_keyspace::context::host_id().as_str(),
        sensor,
        ip_slug
    )
}

/// Build a wildcard key expression for all passive-DNS name observations,
/// a subset of [`all_evidence_wildcard`] (#307).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_name_evidence_wildcard;
///
/// assert_eq!(
///     all_name_evidence_wildcard(),
///     "zensight/@v1/*/state/*/evidence/names/*"
/// );
/// ```
pub fn all_name_evidence_wildcard() -> String {
    format!("{}/@v1/*/state/*/evidence/names/*", KEY_PREFIX)
}

/// Build the entity key for one resolved host, published by the correlator on
/// `zensight/@v1/@catalog/state/entity/<entity_id>` (RFC 06 §5).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::entity_key;
///
/// assert_eq!(
///     entity_key("h-0123456789ab"),
///     "zensight/@v1/@catalog/state/entity/h-0123456789ab"
/// );
/// ```
pub fn entity_key(entity_id: &str) -> String {
    // v1 (RFC 06 §5): a catalog conclusion under the verbatim service origin.
    format!("{}/@v1/@catalog/state/entity/{}", KEY_PREFIX, entity_id)
}

/// Build the alias-record key (RFC 06 §5): old-id → entity-id re-pointing on
/// merges/upgrades, published by the catalog as its own key family.
pub fn alias_key(old_id: &str) -> String {
    format!("{}/@v1/@catalog/state/alias/{}", KEY_PREFIX, old_id)
}

/// Build a wildcard key expression for the whole entity keyspace — the
/// correlator's single-writer materialized view (#305).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_entity_wildcard;
///
/// assert_eq!(all_entity_wildcard(), "zensight/@v1/@catalog/state/entity/*");
/// ```
pub fn all_entity_wildcard() -> String {
    // v1 (RFC 06 §5): the catalog's entity documents.
    format!("{}/@v1/@catalog/state/entity/*", KEY_PREFIX)
}

/// Build the queryable key a late joiner GETs to seed the full current entity
/// set from the correlator (#305).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::entities_query_key;
///
/// assert_eq!(entities_query_key(), "zensight/@v1/@catalog/state/entity/*");
/// ```
pub fn entities_query_key() -> String {
    // v1 (RFC 05 §4): the seed IS the state selector — the catalog answers
    // it storage-shaped (one reply per entity on its concrete key).
    format!("{}/@v1/@catalog/state/entity/*", KEY_PREFIX)
}

/// Build the queryable key for on-demand IP→name resolution (selector
/// `?ip=<ip>`), served by the catalog so arbitrary/external IPs don't flood
/// the bus (#305).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::names_query_key;
///
/// assert_eq!(names_query_key(), "zensight/@v1/@catalog/@rpc/names");
/// ```
pub fn names_query_key() -> String {
    // v1 (RFC 06 §5): on-demand name resolution is a catalog procedure.
    format!("{}/@v1/@catalog/@rpc/names", KEY_PREFIX)
}

/// Build the correlator's liveliness-token key. A second correlator instance
/// GETs this to detect the first (single-writer guard) before declaring its own
/// token (#305).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::correlator_alive_key;
///
/// assert_eq!(correlator_alive_key(), "zensight/@v1/@catalog/state/alive");
/// ```
pub fn correlator_alive_key() -> String {
    // v1 (RFC 04 §5): declared by the elected catalog owner only.
    format!("{}/@v1/@catalog/state/alive", KEY_PREFIX)
}

/// Build a catalog ownership-claim token key (RFC 06 §5.3). Every candidate
/// declares one; the lexically-lowest claim chunk wins the election.
pub fn catalog_claim_key(zid: &str) -> String {
    format!(
        "{}/@v1/@catalog/state/claim/{}",
        KEY_PREFIX,
        zid.to_ascii_lowercase()
    )
}

/// The claim-set selector (liveliness) the election and standbys watch.
pub fn catalog_claims_wildcard() -> String {
    format!("{}/@v1/@catalog/state/claim/*", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor-emitted alerts.
///
/// Matches: `zensight/@v1/<origin>/state/<producer>/alert/<alert_key>`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_alerts_wildcard;
///
/// assert_eq!(all_alerts_wildcard(), "zensight/@v1/*/state/*/alert/*");
/// ```
pub fn all_alerts_wildcard() -> String {
    format!("{}/@v1/*/state/*/alert/*", KEY_PREFIX)
}

/// Build the v1 media-plane key for one video stream profile (RFC 07 §1):
/// `zensight/@v1/<origin>/@media/<producer>/<stream>/video/<codec>/<profile>`.
///
/// `@media` is an `@`-verbatim plane chunk — invisible to the telemetry and
/// state class selectors (D2). Samples on this key are **opaque**: raw
/// encoded access units with a Zenoh `Encoding` (e.g. `video/h264`) + a
/// frame-metadata attachment, never the `TelemetryPoint`/`Format` envelope.
///
/// Stream *control* rides the `@rpc` plane — the `stream`/`stream/set` and
/// `streams` procedures (see [`crate::command`] and [`crate::stream`]).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::media_video_key;
/// use zensight_common::telemetry::Protocol;
///
/// assert_eq!(
///     media_video_key(Protocol::Parallax, "h-3fa9c2d41b7e", "cam0", "h264", "main"),
///     "zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main"
/// );
/// ```
pub fn media_video_key(
    protocol: Protocol,
    origin: &str,
    stream: &str,
    codec: &str,
    profile: &str,
) -> String {
    format!(
        "{}/@v1/{}/@media/{}/{}/video/{}/{}",
        KEY_PREFIX,
        origin,
        protocol.as_str(),
        stream,
        codec,
        profile
    )
}

/// Build the v1 media-plane key for one stream's JPEG preview (RFC 07 §1):
/// `zensight/@v1/<origin>/@media/<producer>/<stream>/preview/jpeg`.
///
/// Same opaque, `@`-verbatim plane as [`media_video_key`] (no serialization
/// envelope, `QosClass::LiveVideo`); control rides the `@rpc` plane.
///
/// # Example
/// ```
/// use zensight_common::keyexpr::media_preview_key;
/// use zensight_common::telemetry::Protocol;
///
/// assert_eq!(
///     media_preview_key(Protocol::Parallax, "h-3fa9c2d41b7e", "cam0"),
///     "zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg"
/// );
/// ```
pub fn media_preview_key(protocol: Protocol, origin: &str, stream: &str) -> String {
    format!(
        "{}/@v1/{}/@media/{}/{}/preview/jpeg",
        KEY_PREFIX,
        origin,
        protocol.as_str(),
        stream
    )
}

/// Slugify an IP address into a single key chunk: `.` and `:` (IPv4 / IPv6
/// separators) become `-` so the address is one chunk with no embedded `/`.
/// Mirrors the `<ip-slug>` convention the passive-DNS name-observation keys
/// already use (#307).
fn ip_slug(ip: &str) -> String {
    ip.replace(['.', ':'], "-")
}

/// Build the durable historical passive-DNS key for one IP (#310):
/// `zensight/@pdns/<ip-slug>`.
///
/// `@pdns` is an `@`-verbatim chunk — a sibling of the per-sensor `@/` control
/// plane and the `@media` plane (#359), but a *different* chunk — so a durable
/// IP↔name record is invisible to BOTH the telemetry firehose (`zensight/**`)
/// and the per-sensor control-plane wildcard (`zensight/*/@/**`). These records
/// are published by the **correlator** off its accumulated per-IP
/// [`NameVal`](crate::NameVal) set (payload:
/// [`PdnsRecord`](crate::PdnsRecord)) and are meant to be captured by a
/// router-hosted storage backend (filesystem snapshot or InfluxDB time series)
/// into a historical tier — never by the live telemetry/exporter/GUI path.
///
/// # Example
/// ```
/// use zensight_common::keyexpr::pdns_key;
///
/// assert_eq!(pdns_key("10.0.0.9"), "zensight/@v1/@catalog/state/pdns/10-0-0-9");
/// assert_eq!(pdns_key("2001:db8::1"), "zensight/@v1/@catalog/state/pdns/2001-db8--1");
/// ```
pub fn pdns_key(ip: &str) -> String {
    // v1 (RFC 06 §5.2): catalog state; the historical tier is a storage choice.
    format!("{}/@v1/@catalog/state/pdns/{}", KEY_PREFIX, ip_slug(ip))
}

/// Build the wildcard key for the whole historical passive-DNS tier
/// (`zensight/@pdns/**`) — what a router-hosted storage backend subscribes to
/// to capture every IP↔name record (#310).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_pdns_wildcard;
///
/// assert_eq!(all_pdns_wildcard(), "zensight/@v1/@catalog/state/pdns/**");
/// ```
pub fn all_pdns_wildcard() -> String {
    format!("{}/@v1/@catalog/state/pdns/**", KEY_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #358 acceptance pin: per-line log events are served from the
    /// `@`-verbatim query key, which the `zensight/**` telemetry firehose can
    /// never match — so log lines no longer ride the streamed bus. The old
    /// streamed key shape *did* intersect (that's exactly what moved).
    #[test]
    fn log_events_query_key_is_off_the_telemetry_bus() {
        use zenoh::key_expr::KeyExpr;
        let telemetry = KeyExpr::try_from(all_telemetry_wildcard()).unwrap();
        use crate::command::query_key;
        // v1: the events read procedure lives on the verbatim @rpc plane —
        // invisible to the telemetry class selector by construction (D2).
        let query_key = KeyExpr::try_from(query_key("zensight/logs", "events")).unwrap();
        assert!(
            !telemetry.intersects(&query_key),
            "the events procedure must be invisible to the telemetry firehose"
        );
        // ...and a telemetry rollup key IS on the bus.
        let rollup =
            KeyExpr::try_from("zensight/@v1/h-3fa9c2d41b7e/telemetry/logs/by_severity/error")
                .unwrap();
        assert!(telemetry.intersects(&rollup));
    }

    /// #359 acceptance pin, v1: the media plane rides the `@media` verbatim
    /// plane chunk — invisible to BOTH the telemetry and state class
    /// selectors (D2). The video firehose can never leak into
    /// telemetry/exporter/GUI consumers.
    #[test]
    fn media_plane_is_off_the_telemetry_and_state_buses() {
        use zenoh::key_expr::KeyExpr;
        let telemetry = KeyExpr::try_from(all_telemetry_wildcard()).unwrap();
        let state = KeyExpr::try_from(all_state_wildcard()).unwrap();
        for media_key in [
            media_video_key(Protocol::Parallax, "h-3fa9c2d41b7e", "cam0", "h264", "main"),
            media_preview_key(Protocol::Parallax, "h-3fa9c2d41b7e", "cam0"),
        ] {
            let media = KeyExpr::try_from(media_key.clone()).unwrap();
            assert!(
                !telemetry.intersects(&media),
                "the telemetry class selector must not match {media_key}"
            );
            assert!(
                !state.intersects(&media),
                "the state class selector must not match {media_key} — @media is a verbatim plane"
            );
        }
        // A subscriber declared on the exact concrete key does receive it.
        let exact = KeyExpr::try_from(media_preview_key(
            Protocol::Parallax,
            "h-3fa9c2d41b7e",
            "cam0",
        ))
        .unwrap();
        let same = KeyExpr::try_from(media_preview_key(
            Protocol::Parallax,
            "h-3fa9c2d41b7e",
            "cam0",
        ))
        .unwrap();
        assert!(exact.intersects(&same));
    }

    /// #310 acceptance pin, v1: the historical passive-DNS tier rides
    /// `@catalog` state — captured by the dedicated pdns selector, invisible
    /// to the telemetry class selector, and NOT matched by the `*`-origin
    /// state selector (D4: `*` never matches the verbatim `@catalog` chunk),
    /// so the durable IP↔name records never leak into fleet-state consumers.
    #[test]
    fn pdns_tier_is_off_the_telemetry_and_fleet_state_buses() {
        use zenoh::key_expr::KeyExpr;
        let telemetry = KeyExpr::try_from(all_telemetry_wildcard()).unwrap();
        let fleet_state = KeyExpr::try_from(all_state_wildcard()).unwrap();
        for ip in ["10.0.0.9", "2001:db8::1"] {
            let k = pdns_key(ip);
            let pdns = KeyExpr::try_from(k.clone()).unwrap();
            assert!(
                !telemetry.intersects(&pdns),
                "the telemetry class selector must not match {k}"
            );
            assert!(
                !fleet_state.intersects(&pdns),
                "the *-origin state selector must not match {k} — @catalog is verbatim (D4)"
            );
        }
        // The dedicated historical-tier subscriber DOES match a concrete record.
        let tier = KeyExpr::try_from(all_pdns_wildcard()).unwrap();
        let one = KeyExpr::try_from(pdns_key("10.0.0.9")).unwrap();
        assert!(
            tier.intersects(&one),
            "the pdns selector must match a concrete @pdns record"
        );
    }

    /// Stream control rides the v1 `@rpc` plane (RFC 05; epic #453) —
    /// writes are `<topic>/set`, reads are `<topic>`.
    #[test]
    fn media_control_rides_the_rpc_plane() {
        use crate::command::{command_key, query_key, status_key};
        let prefix = "zensight/netring";
        let cmd = command_key(prefix, "stream");
        assert!(cmd.starts_with("zensight/@v1/h-"), "{cmd}");
        assert!(cmd.ends_with("/@rpc/netring/stream/set"), "{cmd}");
        assert!(query_key(prefix, "streams").ends_with("/@rpc/netring/streams"));
        assert_eq!(query_key(prefix, "streams"), status_key(prefix, "streams"));
    }

    #[test]
    fn test_all_telemetry_wildcard() {
        assert_eq!(all_telemetry_wildcard(), "zensight/@v1/*/telemetry/**");
    }
}
