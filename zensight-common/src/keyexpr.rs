use crate::telemetry::Protocol;

/// Default key expression prefix for all ZenSight telemetry.
pub const KEY_PREFIX: &str = "zensight";

/// Error type for key expression parsing.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("key expression too short: expected at least 4 segments, got {0}")]
    TooFewSegments(usize),
    #[error("invalid prefix: expected '{expected}', got '{actual}'")]
    InvalidPrefix {
        expected: &'static str,
        actual: String,
    },
    #[error("unknown protocol: '{0}'")]
    UnknownProtocol(String),
    #[error("empty source identifier")]
    EmptySource,
}

/// Builder for constructing ZenSight key expressions.
///
/// Key expressions follow the pattern:
/// `zensight/<protocol>/<source>/<metric_path>`
#[derive(Debug, Clone)]
pub struct KeyExprBuilder {
    prefix: String,
    protocol: Protocol,
}

impl KeyExprBuilder {
    /// Create a new key expression builder for a protocol.
    pub fn new(protocol: Protocol) -> Self {
        Self {
            prefix: KEY_PREFIX.to_string(),
            protocol,
        }
    }

    /// Create a builder with a custom prefix.
    pub fn with_prefix(prefix: impl Into<String>, protocol: Protocol) -> Self {
        Self {
            prefix: prefix.into(),
            protocol,
        }
    }

    /// Build a key expression for a specific source and metric.
    ///
    /// # Panics
    ///
    /// Debug-asserts that `source` and `metric` are non-empty and don't contain
    /// double slashes (`//`).
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Snmp);
    /// let key = builder.build("router01", "system/sysUpTime");
    /// assert_eq!(key, "zensight/snmp/router01/system/sysUpTime");
    /// ```
    pub fn build(&self, source: &str, metric: &str) -> String {
        debug_assert!(!source.is_empty(), "source must not be empty");
        debug_assert!(!metric.is_empty(), "metric must not be empty");
        debug_assert!(
            !source.contains("//") && !metric.contains("//"),
            "source and metric must not contain '//'"
        );
        format!(
            "{}/{}/{}/{}",
            self.prefix,
            self.protocol.as_str(),
            source,
            metric
        )
    }

    /// Build a wildcard key expression for all metrics from a source.
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Snmp);
    /// let key = builder.source_wildcard("router01");
    /// assert_eq!(key, "zensight/snmp/router01/**");
    /// ```
    pub fn source_wildcard(&self, source: &str) -> String {
        format!("{}/{}/{}/**", self.prefix, self.protocol.as_str(), source)
    }

    /// Build a wildcard key expression for all sources of this protocol.
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Snmp);
    /// let key = builder.protocol_wildcard();
    /// assert_eq!(key, "zensight/snmp/**");
    /// ```
    pub fn protocol_wildcard(&self) -> String {
        format!("{}/{}/**", self.prefix, self.protocol.as_str())
    }

    /// Build a key expression for one sensor instance's lifecycle status.
    ///
    /// Host-scoped: two hosts running the same protocol publish distinct
    /// status keys (see `docs/KEYSPACE.md`).
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Snmp);
    /// let key = builder.status_key("poller01");
    /// assert_eq!(key, "zensight/snmp/poller01/@/status");
    /// ```
    pub fn status_key(&self, source: &str) -> String {
        format!(
            "{}/{}/{}/@/status",
            self.prefix,
            self.protocol.as_str(),
            source
        )
    }

    /// Build a key expression for a single keyed alert.
    ///
    /// Matches: `zensight/<protocol>/@/alerts/<alert_key>`
    ///
    /// # Example
    /// ```
    /// use zensight_common::keyexpr::KeyExprBuilder;
    /// use zensight_common::telemetry::Protocol;
    ///
    /// let builder = KeyExprBuilder::new(Protocol::Netlink);
    /// assert_eq!(
    ///     builder.alert_key_expr("ssh-listening-0011223344556677"),
    ///     "zensight/netlink/@/alerts/ssh-listening-0011223344556677"
    /// );
    /// ```
    pub fn alert_key_expr(&self, alert_key: &str) -> String {
        format!(
            "{}/{}/@/alerts/{}",
            self.prefix,
            self.protocol.as_str(),
            alert_key
        )
    }
}

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

/// Build the control prefix for one sensor *instance*: `zensight/<protocol>/<source>`.
///
/// Every per-instance state channel (`@/health`, `@/errors`, `@/status`,
/// `@/alive`, `@/devices/**`) hangs off this prefix, so two hosts running the
/// same protocol never collide (they publish e.g.
/// `zensight/sysinfo/hostA/@/health` vs `zensight/sysinfo/hostB/@/health`).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::sensor_control_prefix;
///
/// assert_eq!(sensor_control_prefix("sysinfo", "host1"), "zensight/sysinfo/host1");
/// ```
pub fn sensor_control_prefix(protocol: &str, source: &str) -> String {
    format!("{}/{}/{}", KEY_PREFIX, protocol, source)
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

/// Build a wildcard key expression for every host-scoped control-plane key
/// (`@/health`, `@/errors`, `@/status`, `@/alive`, `@/devices/**`, …).
///
/// Matches: `zensight/<protocol>/<source>/@/**`. Does NOT match the
/// protocol-scoped channels (`zensight/<protocol>/@/alerts/*`), the telemetry
/// firehose, or the `@media`/`@pdns` planes — pinned by tests below.
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_control_wildcard;
///
/// assert_eq!(all_control_wildcard(), "zensight/*/*/@/**");
/// ```
pub fn all_control_wildcard() -> String {
    format!("{}/*/*/@/**", KEY_PREFIX)
}

/// Build a wildcard key expression for all sensor-instance liveliness tokens.
///
/// Matches: `zensight/<protocol>/<source>/@/alive`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_sensor_alive_wildcard;
///
/// assert_eq!(all_sensor_alive_wildcard(), "zensight/*/*/@/alive");
/// ```
pub fn all_sensor_alive_wildcard() -> String {
    format!("{}/*/*/@/alive", KEY_PREFIX)
}

/// Build a wildcard key expression for all device liveliness tokens.
///
/// Matches: `zensight/<protocol>/<source>/@/devices/<device>/alive`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_device_alive_wildcard;
///
/// assert_eq!(all_device_alive_wildcard(), "zensight/*/*/@/devices/*/alive");
/// ```
pub fn all_device_alive_wildcard() -> String {
    format!("{}/*/*/@/devices/*/alive", KEY_PREFIX)
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

/// Build the sensor-registration key for one sensor instance. Keyed by
/// `<name>/<source>` — per-name keys would collide across hosts running the
/// same sensor.
///
/// # Example
/// ```
/// use zensight_common::keyexpr::sensor_info_key;
///
/// assert_eq!(sensor_info_key("sysinfo", "host1"), "zensight/_meta/sensors/sysinfo/host1");
/// ```
pub fn sensor_info_key(name: &str, source: &str) -> String {
    format!("{}/_meta/sensors/{}/{}", KEY_PREFIX, name, source)
}

/// Build the host-evidence key for one `(sensor, source)` claim.
///
/// # Example
/// ```
/// use zensight_common::keyexpr::host_evidence_key;
///
/// assert_eq!(
///     host_evidence_key("netlink", "host1"),
///     "zensight/_meta/evidence/host/netlink/host1"
/// );
/// ```
pub fn host_evidence_key(sensor: &str, source: &str) -> String {
    format!("{}/_meta/evidence/host/{}/{}", KEY_PREFIX, sensor, source)
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

/// Build the name-observation key for one `(sensor, source)` claim, where
/// `source` is the observed IP slugified (`.`/`:` → `-`) so updates for the
/// same IP replace in place (#307).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::name_observation_key;
///
/// assert_eq!(
///     name_observation_key("netring", "10-0-0-9"),
///     "zensight/_meta/evidence/names/netring/10-0-0-9"
/// );
/// ```
pub fn name_observation_key(sensor: &str, source: &str) -> String {
    format!("{}/_meta/evidence/names/{}/{}", KEY_PREFIX, sensor, source)
}

/// Build a wildcard key expression for all passive-DNS name observations
/// (`zensight/_meta/evidence/names/**`), a subset of [`all_evidence_wildcard`]
/// (#307).
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

/// Build the queryable key for on-demand IP→name resolution
/// (`zensight/_meta/query/names`, selector `?ip=<ip>`), served by the correlator
/// so arbitrary/external IPs don't flood the bus (#305).
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

/// Build the media-plane key for one video stream profile (#359):
/// `zensight/<protocol>/<source>/@media/<stream>/video/<codec>/<profile>`.
///
/// `@media` is an `@`-verbatim chunk — a sibling of the `@/` control plane —
/// so the video firehose is invisible to both the telemetry wildcard
/// (`zensight/**`) and the control wildcard (`zensight/*/@/**`). Samples on
/// this key are **opaque**: raw encoded access units with a Zenoh `Encoding`
/// (e.g. `video/h264`) + a frame-metadata attachment, never the
/// `TelemetryPoint`/`Format` envelope.
///
/// Stream *control* stays on the ordinary `@/` channels — reuse
/// [`crate::command::command_key`] / [`crate::command::query_key`] /
/// [`crate::command::status_key`] with topics `stream` (commands:
/// [`crate::stream::StreamControl`]) and `streams` (query: list of
/// [`crate::stream::StreamDescriptor`]; status:
/// [`crate::stream::StreamStatus`]).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::media_video_key;
/// use zensight_common::telemetry::Protocol;
///
/// assert_eq!(
///     media_video_key(Protocol::Netring, "host01", "cam0", "h264", "main"),
///     "zensight/netring/host01/@media/cam0/video/h264/main"
/// );
/// ```
pub fn media_video_key(
    protocol: Protocol,
    source: &str,
    stream: &str,
    codec: &str,
    profile: &str,
) -> String {
    format!(
        "{}/{}/{}/@media/{}/video/{}/{}",
        KEY_PREFIX,
        protocol.as_str(),
        source,
        stream,
        codec,
        profile
    )
}

/// Build the media-plane key for one stream's JPEG preview (#359):
/// `zensight/<protocol>/<source>/@media/<stream>/preview/jpeg`.
///
/// Same opaque, `@`-verbatim plane as [`media_video_key`] (no serialization
/// envelope, `QosClass::LiveVideo`); control rides the `@/` channels with
/// topics `stream`/`streams` — see [`media_video_key`] for the contract.
///
/// # Example
/// ```
/// use zensight_common::keyexpr::media_preview_key;
/// use zensight_common::telemetry::Protocol;
///
/// assert_eq!(
///     media_preview_key(Protocol::Netring, "host01", "cam0"),
///     "zensight/netring/host01/@media/cam0/preview/jpeg"
/// );
/// ```
pub fn media_preview_key(protocol: Protocol, source: &str, stream: &str) -> String {
    format!(
        "{}/{}/{}/@media/{}/preview/jpeg",
        KEY_PREFIX,
        protocol.as_str(),
        source,
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

/// Parse a key expression to extract protocol, source, and metric path.
///
/// Returns a descriptive error if the key expression doesn't match the expected pattern.
pub fn parse_key_expr(key: &str) -> Result<ParsedKeyExpr<'_>, ParseError> {
    let parts: Vec<&str> = key.split('/').collect();

    if parts.len() < 4 {
        return Err(ParseError::TooFewSegments(parts.len()));
    }

    if parts[0] != KEY_PREFIX {
        return Err(ParseError::InvalidPrefix {
            expected: KEY_PREFIX,
            actual: parts[0].to_string(),
        });
    }

    let protocol = match parts[1] {
        "snmp" => Protocol::Snmp,
        "logs" => Protocol::Logs,
        "gnmi" => Protocol::Gnmi,
        "netflow" => Protocol::Netflow,
        "opcua" => Protocol::Opcua,
        "modbus" => Protocol::Modbus,
        "sysinfo" => Protocol::Sysinfo,
        "netlink" => Protocol::Netlink,
        "netring" => Protocol::Netring,
        "systemd" => Protocol::Systemd,
        "parallax" => Protocol::Parallax,
        other => return Err(ParseError::UnknownProtocol(other.to_string())),
    };

    let source = parts[2];
    if source.is_empty() {
        return Err(ParseError::EmptySource);
    }

    let metric = parts[3..].join("/");

    Ok(ParsedKeyExpr {
        protocol,
        source,
        metric,
    })
}

/// Parsed components of a ZenSight key expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedKeyExpr<'a> {
    pub protocol: Protocol,
    pub source: &'a str,
    pub metric: String,
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

    /// #359 acceptance pin: the media plane rides `@media/…` — an `@`-verbatim
    /// chunk like `@/`, but a *different* chunk — so a concrete media key is
    /// invisible to BOTH the telemetry firehose (`zensight/**`) and the
    /// control-plane wildcard (`zensight/*/@/**`). The video firehose can
    /// never leak into telemetry/exporter/GUI consumers.
    #[test]
    fn media_plane_is_off_the_telemetry_and_control_buses() {
        use zenoh::key_expr::KeyExpr;
        let telemetry = KeyExpr::try_from(all_telemetry_wildcard()).unwrap();
        let control = KeyExpr::try_from("zensight/*/@/**").unwrap();
        for media_key in [
            media_video_key(Protocol::Netring, "host01", "cam0", "h264", "main"),
            media_preview_key(Protocol::Netring, "host01", "cam0"),
        ] {
            let media = KeyExpr::try_from(media_key.clone()).unwrap();
            assert!(
                !telemetry.intersects(&media),
                "zensight/** must not match {media_key}"
            );
            assert!(
                !control.intersects(&media),
                "zensight/*/@/** must not match {media_key} — @/ and @media are distinct verbatim chunks"
            );
        }
        // A subscriber declared on the exact concrete key does receive it.
        let exact =
            KeyExpr::try_from(media_preview_key(Protocol::Netring, "host01", "cam0")).unwrap();
        let same =
            KeyExpr::try_from(media_preview_key(Protocol::Netring, "host01", "cam0")).unwrap();
        assert!(exact.intersects(&same));
    }

    /// #310 acceptance pin: the historical passive-DNS tier rides `@pdns/<ip>` —
    /// an `@`-verbatim chunk like `@/` and `@media`, but a *different* chunk — so
    /// a concrete `@pdns` key is invisible to BOTH the telemetry firehose
    /// (`zensight/**`) and the per-sensor control-plane wildcard
    /// (`zensight/*/@/**`). The durable IP↔name records can never leak into
    /// telemetry/exporter/GUI consumers; only a storage backend subscribed on the
    /// explicit `zensight/@pdns/**` tier captures them.
    #[test]
    fn pdns_tier_is_off_the_telemetry_and_control_buses() {
        use zenoh::key_expr::KeyExpr;
        let telemetry = KeyExpr::try_from(all_telemetry_wildcard()).unwrap();
        let control = KeyExpr::try_from("zensight/*/@/**").unwrap();
        for ip in ["10.0.0.9", "2001:db8::1"] {
            let k = pdns_key(ip);
            let pdns = KeyExpr::try_from(k.clone()).unwrap();
            assert!(
                !telemetry.intersects(&pdns),
                "zensight/** must not match {k}"
            );
            assert!(
                !control.intersects(&pdns),
                "zensight/*/@/** must not match {k} — @/ and @pdns are distinct verbatim chunks"
            );
        }
        // The dedicated historical-tier subscriber DOES match a concrete record.
        let tier = KeyExpr::try_from(all_pdns_wildcard()).unwrap();
        let one = KeyExpr::try_from(pdns_key("10.0.0.9")).unwrap();
        assert!(
            tier.intersects(&one),
            "zensight/@pdns/** must match a concrete @pdns record"
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

    /// Multi-host acceptance pin: the host-scoped control plane
    /// (`zensight/<proto>/<source>/@/…`) must be (a) invisible to the telemetry
    /// firehose, (b) matched by the scoped control wildcard, and (c) NOT
    /// matched by the legacy protocol-scoped control wildcard — the GUI keeps a
    /// subscriber on each shape (legacy for `@/alerts/*` + old sensors, scoped
    /// for per-instance state), and this non-intersection is what guarantees a
    /// concrete key is never double-delivered.
    #[test]
    fn scoped_control_plane_is_disjoint_from_telemetry_and_legacy_control() {
        use zenoh::key_expr::KeyExpr;
        let telemetry = KeyExpr::try_from(all_telemetry_wildcard()).unwrap();
        let legacy_control = KeyExpr::try_from("zensight/*/@/**").unwrap();
        let scoped_control = KeyExpr::try_from(all_control_wildcard()).unwrap();

        for key in [
            "zensight/sysinfo/host1/@/health",
            "zensight/sysinfo/host1/@/errors",
            "zensight/sysinfo/host1/@/status",
            "zensight/sysinfo/host1/@/alive",
            "zensight/snmp/poller01/@/devices/router01/liveness",
            "zensight/snmp/poller01/@/devices/router01/alive",
        ] {
            let k = KeyExpr::try_from(key).unwrap();
            assert!(
                !telemetry.intersects(&k),
                "zensight/** must not match {key}"
            );
            assert!(
                scoped_control.intersects(&k),
                "zensight/*/*/@/** must match {key}"
            );
            assert!(
                !legacy_control.intersects(&k),
                "legacy zensight/*/@/** must not match {key} — dual subscribers must never double-deliver"
            );
        }

        // The scoped control wildcard must not stray onto the other planes.
        for key in [
            "zensight/sysinfo/host1/cpu/usage",                 // telemetry
            "zensight/netlink/@/alerts/abcd1234",               // protocol-scoped alerts (deferred)
            "zensight/netring/host01/@media/cam0/preview/jpeg", // media plane
            "zensight/@pdns/10-0-0-9",                          // pdns plane
        ] {
            let k = KeyExpr::try_from(key).unwrap();
            assert!(
                !scoped_control.intersects(&k),
                "zensight/*/*/@/** must not match {key}"
            );
        }
    }

    #[test]
    fn test_key_builder() {
        let builder = KeyExprBuilder::new(Protocol::Snmp);

        assert_eq!(
            builder.build("router01", "system/sysUpTime"),
            "zensight/snmp/router01/system/sysUpTime"
        );

        assert_eq!(
            builder.source_wildcard("router01"),
            "zensight/snmp/router01/**"
        );

        assert_eq!(builder.protocol_wildcard(), "zensight/snmp/**");

        assert_eq!(
            builder.status_key("poller01"),
            "zensight/snmp/poller01/@/status"
        );
    }

    #[test]
    fn test_sensor_control_prefix() {
        assert_eq!(
            sensor_control_prefix("sysinfo", "host1"),
            "zensight/sysinfo/host1"
        );
    }

    #[test]
    fn test_parse_key_expr() {
        let parsed = parse_key_expr("zensight/snmp/router01/system/sysUpTime").unwrap();

        assert_eq!(parsed.protocol, Protocol::Snmp);
        assert_eq!(parsed.source, "router01");
        assert_eq!(parsed.metric, "system/sysUpTime");
    }

    #[test]
    fn test_parse_sysinfo_key_expr() {
        let parsed = parse_key_expr("zensight/sysinfo/server01/cpu/usage").unwrap();
        assert_eq!(parsed.protocol, Protocol::Sysinfo);
        assert_eq!(parsed.source, "server01");
        assert_eq!(parsed.metric, "cpu/usage");
    }

    #[test]
    fn test_parse_invalid_key() {
        assert!(matches!(
            parse_key_expr("invalid/key"),
            Err(ParseError::TooFewSegments(2))
        ));
        assert!(matches!(
            parse_key_expr("zensight/unknown/device/metric"),
            Err(ParseError::UnknownProtocol(_))
        ));
        assert!(matches!(
            parse_key_expr("other/snmp/device/metric"),
            Err(ParseError::InvalidPrefix { .. })
        ));
    }

    #[test]
    fn test_all_telemetry_wildcard() {
        assert_eq!(all_telemetry_wildcard(), "zensight/@v1/*/telemetry/**");
    }
}
