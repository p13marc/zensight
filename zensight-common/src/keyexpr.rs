use crate::telemetry::Protocol;

use crate::registry::{self, AnySubject};
use zenkey::grammar::{self, Class, ClassOrPlane, Origin, StructuralKey};
use zenkey::origin::{RemoteOrigin, ServiceOrigin};
use zenkey::selector::{self, Scope};

/// Split a caller-side procedure path (`"artifact/cancel"`) into the chunk
/// slice zenkey's selector builders take. The chunks are registry constants,
/// so an illegal one is a programmer error — zenkey asserts it eagerly.
fn proc_chunks(procedure: &str) -> Vec<&str> {
    procedure.split('/').collect()
}

// ---------------------------------------------------------------------------
// Parsing: base-relative for applications, full-key for un-namespaced tools.
//
// Every ZenSight session sets the deployment base as its Zenoh `namespace`
// (#466, RFC 09 §0), so the session has already stripped it by the time a
// sample reaches application code: what a subscriber callback sees is `v1/…`.
// Applications therefore parse with [`parse_key`].
//
// Two kinds of party legitimately see the FULL key and must say so explicitly:
// router-side artifacts (storages, ACL) and un-namespaced debug tools — zenctl
// and the `v1_probe` example, which are un-namespaced *on purpose*, because the
// honest view of the wire is the whole point of a debug tool (RFC 09 §5). They
// parse with [`parse_full_key`], passing the base they were configured with.
//
// The two are deliberately different function names rather than one tolerant
// parser. A parser that accepts both would silently accept a full key from a
// namespaced session — which is exactly the key that will never match anything.
// ---------------------------------------------------------------------------

/// Structurally parse a **base-relative** key (RFC 03) — the form every
/// namespaced session sees.
///
/// Consumers should reach for this instead of hand-rolling `split('/')`:
/// positional re-parsing of keys is exactly what the registry was built to
/// delete (RFC 08 §1, issue #475).
pub fn parse_key(key: &str) -> Option<StructuralKey<'_>> {
    grammar::parse(key).ok()
}

/// Parse a base-relative key and refine its subject tail through the registry
/// (RFC 08 §1's *parse* direction). Returns the producer (or service) base name
/// alongside the refined subject.
///
/// `None` when the key is not a v1 data key, or when the subject is not
/// registered — "a subject that is not registered does not exist".
pub fn refine_key(key: &str) -> Option<(StructuralKey<'_>, String, AnySubject)> {
    let parsed = parse_key(key)?;
    let ClassOrPlane::Class(class) = parsed.class else {
        return None;
    };
    let name = match parsed.producer.as_ref() {
        // The instance suffix (`netring-2`) is already stripped, so the
        // registry lookup sees the base name.
        Some(p) => p.name().to_string(),
        // Service origins (`@catalog`) carry no producer chunk.
        None => match &parsed.origin {
            Origin::Service(s) => s.trim_start_matches('@').to_string(),
            Origin::Host(_) => return None,
        },
    };
    let subject = registry::parse_subject(&name, class, &parsed.subject)?;
    Some((parsed, name, subject))
}

/// Structurally parse a **full** key as it appears on the wire, given the
/// deployment base.
///
/// For un-namespaced observers only (`zenctl`, `v1_probe`). A namespaced
/// session must use [`parse_key`] — it never sees the base.
///
/// `None` when the key belongs to another deployment (a different base), which
/// for an observer is the meaningful answer rather than an error.
pub fn parse_full_key<'k>(base: &str, key: &'k str) -> Option<StructuralKey<'k>> {
    parse_key(grammar::strip_base(base, key)?)
}

/// [`refine_key`] over a full wire key. See [`parse_full_key`].
pub fn refine_full_key<'k>(
    base: &str,
    key: &'k str,
) -> Option<(StructuralKey<'k>, String, AnySubject)> {
    refine_key(grammar::strip_base(base, key)?)
}

/// Whether a base-relative key carries a [`crate::TelemetryPoint`] — i.e. is a
/// v1 telemetry-class key on a host origin.
///
/// This replaces a positional 4-chunk gate that had been copy-pasted verbatim
/// into both exporters.
pub fn is_telemetry_key(key: &str) -> bool {
    parse_key(key).is_some_and(|k| {
        matches!(k.class, ClassOrPlane::Class(Class::Telemetry))
            && matches!(k.origin, Origin::Host(_))
    })
}

/// Validate a **config-supplied** key expression.
///
/// Config selectors are base-relative like everything else (#466): the session
/// sets the base as its namespace, so a config carrying a full key
/// (`zensight/v1/*/telemetry/**`) would be re-prefixed on egress to
/// `zensight/zensight/v1/…` and match nothing at all — silently, with a
/// perfectly healthy-looking session and an empty dashboard.
///
/// That is the failure this rejects at startup, where it is still legible.
/// The check is heuristic: it can only catch the *conventional* base spelling
/// (it has no access to the deployment's actual namespace).
pub fn validate_relative_selector(ke: &str) -> std::result::Result<(), String> {
    let base = crate::CONVENTIONAL_BASE;
    if let Some(rel) = grammar::strip_base(base, ke) {
        return Err(format!(
            "key expression {ke:?} spells the deployment base. Since #466 the session sets \
             the configured base as its Zenoh namespace and every key is base-relative — \
             write {rel:?}."
        ));
    }
    zenoh::key_expr::KeyExpr::try_from(ke)
        .map(|_| ())
        .map_err(|e| format!("invalid key expression {ke:?}: {e}"))
}

/// Build the v1 telemetry class selector (all producers, all origins).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_telemetry_wildcard;
///
/// assert_eq!(all_telemetry_wildcard(), "v1/*/telemetry/**");
/// ```
pub fn all_telemetry_wildcard() -> String {
    // v1 (RFC 04): the telemetry class selector — nothing to discard
    // client-side (incumbent pain P6 retired).
    selector::all_telemetry(Scope::fleet()).into()
}

/// Caller-side fleet procedure selector (RFC 05 §2): GET
/// `<base>/v1/*/@rpc/<producer>/<procedure...>` reaches every host serving
/// the producer. Callers MUST use query target `All` (RFC 05 §2.1) —
/// `BestMatching` can short-circuit the fan-in.
pub fn fleet_rpc_key(producer: &str, procedure: &str) -> String {
    selector::fleet_rpc(producer, &proc_chunks(procedure)).into()
}

/// Caller-side fleet write selector: the `<topic>/set` procedure fleet-wide.
pub fn fleet_command_key(producer: &str, topic: &str) -> String {
    fleet_rpc_key(producer, &format!("{topic}/set"))
}

// A `fleet_blob_prefix()` (`v1/*/@blob/artifact`) lived here through 0.10 and
// is deliberately gone. It existed for one call site, which used it for a
// *bulk fetch* — the thing RFC 07 §3 forbids, because every matching holder
// ships the full payload and Zenoh cannot cancel remote replies in flight. Its
// cost was bounded only by artifact ids happening to be unique ULIDs, i.e. by
// id collisions rather than by the protocol; applied to `store/<algo>/<hash>`,
// which many hosts legitimately hold, the same shape amplifies badly.
//
// Consumers now carry the concrete origin instead (`CaptureRecord::
// artifact_prefix`, `Delivery::Blob::blob_prefix`). §2.5 does sanction a
// `*`-origin *probe* with tiny replies (`have`/`manifest`) — but a probe
// prefix and a fetch prefix are interchangeable as strings, which is exactly
// how the first becomes the second, so a probe helper belongs behind a
// distinct type rather than as another `String` here. Since zenkey 0.4 that
// type exists: `zenkey::BlobProbePrefix` (deliberately not convertible to a
// `Key`), reachable as `registry::blob::Tier::probe()`. Nothing probes yet;
// when something does, its helper goes next to the tier builders in
// `command.rs`, not in this file (the guard test below scans this file's
// source and rightly refuses any `@blob` + `*` spelling).

/// Caller-side single-host procedure key (RFC 05 §2): GET
/// `<base>/v1/<origin>/@rpc/<producer>/<procedure...>` reaches exactly one
/// host's producer — use when the origin is already known (e.g. a drill-down
/// view), [`fleet_rpc_key`] otherwise.
pub fn origin_rpc_key(origin: &str, producer: &str, procedure: &str) -> String {
    match RemoteOrigin::parse(origin) {
        Ok(o) => selector::rpc_at(&o, producer, &proc_chunks(procedure)).into(),
        // A malformed origin never parsed before either — the legacy spelling
        // simply matched nothing, which is exactly the behavior to keep.
        Err(_) => format!("v1/{origin}/@rpc/{producer}/{procedure}"),
    }
}

/// Build a wildcard key expression for the whole fleet state plane.
pub fn all_state_wildcard() -> String {
    // v1 (RFC 04): the whole fleet state plane, one selector.
    selector::all_state(Scope::fleet()).into()
}

/// Build a wildcard key expression for the whole fleet events plane (#536).
pub fn all_events_wildcard() -> String {
    selector::all_events(Scope::fleet()).into()
}

/// Build a wildcard key expression for all sensor health data.
///
/// Matches: `<base>/v1/<origin>/state/<producer>/health`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_health_wildcard;
///
/// assert_eq!(all_health_wildcard(), "v1/*/state/*/health");
/// ```
pub fn all_health_wildcard() -> String {
    // Hand-spelled: zenkey 0.6 has no cross-producer common-family selector —
    // the generated `Family::Health.selector(scope)` interpolates one
    // producer's name literal, not `*`. (Upstream candidate: a
    // `selector::common_family(scope, family)`.)
    "v1/*/state/*/health".to_string()
}

// `host_evidence_key(sensor, device)` and `name_observation_key(sensor, ip)`
// lived here through 0.10 and are gone: they string-dispatched on the sensor
// name, where every publisher knows its producer statically. Evidence
// publishers now build their keys with the per-producer generated builders —
// `registry::<producer>::key(&PROFILE.local_origin(),
// &Subject::evidence_device(..))` — which slug identically (`Chunk::slug` is
// `chunk_slug`) and additionally guarantee the subject is registered.

/// Build a wildcard key expression for the whole evidence keyspace
/// (`host/**` claims and `names/**` observation batches).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_evidence_wildcard;
///
/// assert_eq!(all_evidence_wildcard(), "v1/*/state/*/evidence/**");
/// ```
pub fn all_evidence_wildcard() -> String {
    // v1 (RFC 06 §4): evidence is ordinary per-origin state.
    "v1/*/state/*/evidence/**".to_string()
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
///     "v1/*/state/*/evidence/names/*"
/// );
/// ```
pub fn all_name_evidence_wildcard() -> String {
    "v1/*/state/*/evidence/names/*".to_string()
}

/// Build the entity key for one resolved host, published by the correlator on
/// `<base>/v1/@catalog/state/entity/<entity_id>` (RFC 06 §5).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::entity_key;
///
/// assert_eq!(
///     entity_key("h-0123456789ab"),
///     "v1/@catalog/state/entity/h-0123456789ab"
/// );
/// ```
pub fn entity_key(entity_id: &str) -> String {
    // v1 (RFC 06 §5): a catalog conclusion under the verbatim service origin.
    // Entity ids are `h-<12hex>` — already chunk-legal, so the generated
    // constructor's slug is a no-op.
    registry::catalog::key(&registry::catalog::Subject::entity(entity_id)).into()
}

/// Build the alias-record key (RFC 06 §5): old-id → entity-id re-pointing on
/// merges/upgrades, published by the catalog as its own key family.
pub fn alias_key(old_id: &str) -> String {
    registry::catalog::key(&registry::catalog::Subject::alias(old_id)).into()
}

/// Wildcard over the alias family — what a UI subscribes to so an operator's
/// merge is *visible* (RFC 06 §5.1 step 1). Without this a link is a no-op as
/// far as the product is concerned.
pub fn all_alias_wildcard() -> String {
    registry::catalog::Family::Alias.selector().into()
}

/// Build the operator-assertion key (#473): an explicit operator statement about
/// identity, held on the bus as ordinary catalog state.
///
/// This is what keeps the catalog a **pure function of live bus state** (RFC 06
/// §5: "no private database, no migration state"). An operator override is state
/// that is not evidence, so it would otherwise have to live in a side table that
/// no restart, replica, or storage could see. Publishing it as a registered
/// state subject means a restarted correlator re-seeds it through exactly the
/// same path as everything else.
pub fn assertion_key(id: &str) -> String {
    // Ids are `link-<old>-<new>` / `unlink-<old>-<new>` over host ids —
    // chunk-legal, so the constructor's slug is a no-op.
    registry::catalog::key(&registry::catalog::Subject::assertion(id)).into()
}

/// Wildcard over the assertion family — the correlator's own re-seed selector.
pub fn all_assertion_wildcard() -> String {
    registry::catalog::Family::Assertion.selector().into()
}

/// Build a wildcard key expression for the whole entity keyspace — the
/// correlator's single-writer materialized view (#305).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_entity_wildcard;
///
/// assert_eq!(all_entity_wildcard(), "v1/@catalog/state/entity/*");
/// ```
pub fn all_entity_wildcard() -> String {
    // v1 (RFC 06 §5): the catalog's entity documents.
    registry::catalog::Family::Entity.selector().into()
}

/// Build the queryable key a late joiner GETs to seed the full current entity
/// set from the correlator (#305).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::entities_query_key;
///
/// assert_eq!(entities_query_key(), "v1/@catalog/state/entity/*");
/// ```
pub fn entities_query_key() -> String {
    // v1 (RFC 05 §4): the seed IS the state selector — the catalog answers
    // it storage-shaped (one reply per entity on its concrete key).
    registry::catalog::Family::Entity.selector().into()
}

/// Build a catalog procedure key (RFC 06 §5): the catalog is a service
/// origin, so its procedures ride `<base>/v1/@catalog/@rpc/<procedure>`
/// with no producer chunk.
pub fn catalog_rpc_key(procedure: &str) -> String {
    selector::service_rpc(&ServiceOrigin::catalog(), &proc_chunks(procedure)).into()
}

/// Build the queryable key for on-demand IP→name resolution (selector
/// `?ip=<ip>`), served by the catalog so arbitrary/external IPs don't flood
/// the bus (#305).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::names_query_key;
///
/// assert_eq!(names_query_key(), "v1/@catalog/@rpc/names");
/// ```
pub fn names_query_key() -> String {
    // v1 (RFC 06 §5): on-demand name resolution is a catalog procedure —
    // this is the registry's own generated builder for it.
    registry::catalog::names_key().into()
}

/// Build the correlator's liveliness-token key. A second correlator instance
/// GETs this to detect the first (single-writer guard) before declaring its own
/// token (#305).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::correlator_alive_key;
///
/// assert_eq!(correlator_alive_key(), "v1/@catalog/state/alive");
/// ```
pub fn correlator_alive_key() -> String {
    // v1 (RFC 04 §5): declared by the elected catalog owner only.
    selector::service_alive(&ServiceOrigin::catalog()).into()
}

/// Build a catalog ownership-claim token key (RFC 06 §5.3). Every candidate
/// declares one; the lexically-lowest claim chunk wins the election.
///
/// Hand-spelled: `claim/{zid}` is *deliberately not registered* (see the
/// header comment of `registry/catalog.toml`) — it is a liveliness token, not
/// a data surface, and zenkey-build has no token/liveliness section yet, so
/// there is no generated builder for it. The zid is lowercased here because
/// `Chunk::slug` escapes rather than folds case, and the wire form must stay
/// the canonical lowercase.
pub fn catalog_claim_key(zid: &str) -> String {
    format!("v1/@catalog/state/claim/{}", zid.to_ascii_lowercase())
}

/// The claim-set selector (liveliness) the election and standbys watch.
/// Hand-spelled for the same reason as [`catalog_claim_key`].
pub fn catalog_claims_wildcard() -> String {
    "v1/@catalog/state/claim/*".to_string()
}

/// Build a wildcard key expression for all sensor-emitted alerts.
///
/// Matches: `<base>/v1/<origin>/state/<producer>/alert/<alert_key>`
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_alerts_wildcard;
///
/// assert_eq!(all_alerts_wildcard(), "v1/*/state/*/alert/*");
/// ```
pub fn all_alerts_wildcard() -> String {
    // Hand-spelled: no cross-producer common-family selector in zenkey 0.6
    // (see [`all_health_wildcard`]).
    "v1/*/state/*/alert/*".to_string()
}

// ---------------------------------------------------------------------------
// Focus mode (issue #476): one origin instead of the fleet firehose.
//
// The origin is a single chunk at a fixed position, so "everything one host
// publishes, and nothing else" is one selector — a subscription that was not
// expressible on the incumbent keyspace, where the host was buried in a
// per-protocol position. On a constrained link (the deployment this convention
// was shaped around, RFC 09 §4) a technician debugging one host otherwise pays
// for the whole fleet's telemetry to reach their laptop.
//
// The `@`-verbatim planes are structurally excluded: a wildcard never matches an
// `@`-chunk, so none of these can pull `@rpc`/`@media`/`@blob` (design property
// D2). RFC 09 §1's cookbook writes this as a single `v1/<origin>/**`; we keep
// the per-class selectors instead, because telemetry and state ride *different
// delivery tiers* in this consumer (advanced/history vs plain) and one selector
// would collapse them onto one subscriber.
// ---------------------------------------------------------------------------

/// Resolve a wire-received origin into a selector scope. A malformed origin
/// falls back to the legacy literal spelling via the caller's `format!` arm —
/// such a selector never parsed before either and simply matched nothing,
/// which is the behavior to keep for a focus target read off the wire.
fn origin_scope(origin: &str) -> Option<Scope> {
    RemoteOrigin::parse(origin).ok().map(|o| Scope::origin(&o))
}

/// Every telemetry key published by one host.
pub fn origin_telemetry_wildcard(origin: &str) -> String {
    match origin_scope(origin) {
        Some(scope) => selector::all_telemetry(scope).into(),
        None => format!("v1/{origin}/telemetry/**"),
    }
}

/// Every state key published by one host.
pub fn origin_state_wildcard(origin: &str) -> String {
    match origin_scope(origin) {
        Some(scope) => selector::all_state(scope).into(),
        None => format!("v1/{origin}/state/**"),
    }
}

/// One host's firing alerts (the late-joiner seed GET). Hand-spelled tail: no
/// cross-producer common-family selector in zenkey 0.6 (see
/// [`all_health_wildcard`]).
pub fn origin_alerts_wildcard(origin: &str) -> String {
    format!("v1/{origin}/state/*/alert/*")
}

/// One host's events plane (#536).
pub fn origin_events_wildcard(origin: &str) -> String {
    match origin_scope(origin) {
        Some(scope) => selector::all_events(scope).into(),
        None => format!("v1/{origin}/events/**"),
    }
}

/// One host's sensor liveliness tokens.
pub fn origin_liveliness_expr(origin: &str) -> String {
    match origin_scope(origin) {
        Some(scope) => selector::all_liveliness(scope).into(),
        None => format!("v1/{origin}/state/*/alive"),
    }
}

/// One host's device liveliness tokens.
pub fn origin_device_liveliness_expr(origin: &str) -> String {
    format!("v1/{origin}/state/*/device/*/alive")
}

/// The whole fleet's producer liveliness tokens — RFC 04 §5's "entire
/// fleet-presence protocol, zero payload bytes".
///
/// The token *key* is the record: `…/<origin>/state/<producer>/alive` says who
/// is up and what they run. Note `*` in the origin position can never match a
/// verbatim service origin (design property D4), so `@catalog`'s own token
/// ([`correlator_alive_key`]) is **not** in this set and must be asked for by
/// name.
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_liveliness_wildcard;
///
/// assert_eq!(all_liveliness_wildcard(), "v1/*/state/*/alive");
/// ```
pub fn all_liveliness_wildcard() -> String {
    selector::all_liveliness(Scope::fleet()).into()
}

/// The whole fleet's device liveliness tokens (producers that track downstream
/// devices — RFC 04 §5). Hand-spelled: `selector::all_liveliness` covers only
/// the producer-token shape (`state/*/alive`), not the device rung.
pub fn all_device_liveliness_wildcard() -> String {
    "v1/*/state/*/device/*/alive".to_string()
}

/// Build the v1 media-plane key for one video stream tier (RFC 07 §1):
/// `<base>/v1/<origin>/@media/<producer>/<stream>/video/<codec>/<tier>`.
///
/// The last chunk is a viewer-chosen **tier** (`low`/`medium`/`high`), a named
/// bandwidth rung published concurrently on its own key — subscribed to
/// *exactly*, never with a wildcard (keyspace v1.3, RFC 07 §1). It is not an
/// H.264 coding profile.
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
///     media_video_key(Protocol::Parallax, "h-3fa9c2d41b7e", "cam0", "h264", "high"),
///     "v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/high"
/// );
/// ```
pub fn media_video_key(
    protocol: Protocol,
    origin: &str,
    stream: &str,
    codec: &str,
    tier: &str,
) -> String {
    // Parallax is the registry's only `[[media]]` declarer, so a concrete
    // origin rides the generated builder — the same one the sensor publishes
    // with, which is what makes the two sides agree by construction. The
    // fallback arm keeps two deliberate behaviors: a `*` origin (a viewer
    // subscribing before the origin map fills — exact on every other chunk)
    // and non-parallax protocols (sensor-core's generic media machinery).
    if matches!(protocol, Protocol::Parallax)
        && let Ok(o) = RemoteOrigin::parse(origin)
    {
        return registry::parallax::media_key_at(
            &o,
            &registry::parallax::Media::video(stream, codec, tier),
        )
        .into();
    }
    format!(
        "v1/{}/@media/{}/{}/video/{}/{}",
        origin,
        protocol.as_str(),
        stream,
        codec,
        tier
    )
}

/// Build the v1 media-plane key for one stream's JPEG preview (RFC 07 §1):
/// `<base>/v1/<origin>/@media/<producer>/<stream>/preview/jpeg`.
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
///     "v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg"
/// );
/// ```
pub fn media_preview_key(protocol: Protocol, origin: &str, stream: &str) -> String {
    // Same delegation split as [`media_video_key`].
    if matches!(protocol, Protocol::Parallax)
        && let Ok(o) = RemoteOrigin::parse(origin)
    {
        return registry::parallax::media_key_at(
            &o,
            &registry::parallax::Media::preview_jpeg(stream),
        )
        .into();
    }
    format!(
        "v1/{}/@media/{}/{}/preview/jpeg",
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
/// `<base>/v1/@catalog/state/pdns/<ip-slug>`.
///
/// `@catalog` is a verbatim origin — the `*` selectors never match it (D4) —
/// so a durable IP↔name record is invisible to BOTH the telemetry class
/// selector (`zensight/v1/*/telemetry/**`) and the `*`-origin state
/// selectors. These records
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
/// assert_eq!(pdns_key("10.0.0.9"), "v1/@catalog/state/pdns/10-0-0-9");
/// assert_eq!(pdns_key("2001:db8::1"), "v1/@catalog/state/pdns/2001-db8--1");
/// ```
pub fn pdns_key(ip: &str) -> String {
    // v1 (RFC 06 §5.2): catalog state; the historical tier is a storage
    // choice. The value handed to the generated constructor is the
    // *pre-slugged* form: `Chunk::slug` treats `.` as a legal chunk char, so
    // a dotted IPv4 passed raw would land on the wire dotted — a different
    // key than every existing pdns record.
    registry::catalog::key(&registry::catalog::Subject::pdns(ip_slug(ip))).into()
}

/// Build the wildcard key for the whole historical passive-DNS tier
/// (`zensight/v1/@catalog/state/pdns/**`) — what a router-hosted storage
/// backend subscribes to to capture every IP↔name record (#310).
///
/// # Example
/// ```
/// use zensight_common::keyexpr::all_pdns_wildcard;
///
/// assert_eq!(all_pdns_wildcard(), "v1/@catalog/state/pdns/**");
/// ```
pub fn all_pdns_wildcard() -> String {
    // Hand-spelled: the generated `Family::Pdns.selector()` is the narrower
    // single-chunk `…/pdns/*`. Semantically equivalent for the registered
    // family, but byte-different — and this string configures router-side
    // storage selectors, so narrowing it is a deliberate change, not a
    // refactor.
    "v1/@catalog/state/pdns/**".to_string()
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
        let query_key = KeyExpr::try_from(query_key("logs", "events")).unwrap();
        assert!(
            !telemetry.intersects(&query_key),
            "the events procedure must be invisible to the telemetry firehose"
        );
        // ...and a telemetry rollup key IS on the bus (logs metrics keep
        // their legacy `logs/` head chunk, hence the doubled chunk).
        let rollup =
            KeyExpr::try_from("v1/h-3fa9c2d41b7e/telemetry/logs/logs/by_severity/error").unwrap();
        assert!(telemetry.intersects(&rollup));
    }

    // The `no_key_builder_hands_out_a_wildcard_origin_blob_prefix` source
    // grep that lived here (RFC 07 §3, written after `fleet_blob_prefix()`'s
    // bulk-fetch defect) is deleted with the zblob 0.3 port: the property is
    // enforced by the type system now — a `ServePrefix` cannot be built from
    // a wildcard, a fetch client takes a typed prefix, and the probe form is
    // `zenkey::BlobProbePrefix`, which is not convertible to a key.

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
        let producer = "netring";
        let cmd = command_key(producer, "stream");
        assert!(cmd.starts_with("v1/h-"), "{cmd}");
        assert!(cmd.ends_with("/@rpc/netring/stream/set"), "{cmd}");
        assert!(query_key(producer, "streams").ends_with("/@rpc/netring/streams"));
        assert_eq!(
            query_key(producer, "streams"),
            status_key(producer, "streams")
        );
    }

    #[test]
    fn test_all_telemetry_wildcard() {
        assert_eq!(all_telemetry_wildcard(), "v1/*/telemetry/**");
    }

    /// Golden pins for every helper in this module, asserted against the
    /// literal v1 spelling. These predate the delegation of the helper
    /// bodies to zenkey 0.6's typed selectors / generated registry builders,
    /// and are the acceptance criteria for that refactor: the wire bytes
    /// must not move. Origin-parameterized helpers are pinned on a fixed
    /// remote origin; PROFILE-derived keys (machine-minted origin) are
    /// pinned against their explicit literal construction.
    mod golden_v1_spellings {
        use super::super::*;

        const ORIGIN: &str = "h-3fa9c2d41b7e";

        #[test]
        fn class_and_family_wildcards() {
            assert_eq!(all_telemetry_wildcard(), "v1/*/telemetry/**");
            assert_eq!(all_state_wildcard(), "v1/*/state/**");
            assert_eq!(all_events_wildcard(), "v1/*/events/**");
            assert_eq!(all_health_wildcard(), "v1/*/state/*/health");
            assert_eq!(all_alerts_wildcard(), "v1/*/state/*/alert/*");
            assert_eq!(all_liveliness_wildcard(), "v1/*/state/*/alive");
            assert_eq!(
                all_device_liveliness_wildcard(),
                "v1/*/state/*/device/*/alive"
            );
            assert_eq!(all_evidence_wildcard(), "v1/*/state/*/evidence/**");
            assert_eq!(
                all_name_evidence_wildcard(),
                "v1/*/state/*/evidence/names/*"
            );
        }

        #[test]
        fn origin_scoped_selectors() {
            assert_eq!(
                origin_telemetry_wildcard(ORIGIN),
                "v1/h-3fa9c2d41b7e/telemetry/**"
            );
            assert_eq!(origin_state_wildcard(ORIGIN), "v1/h-3fa9c2d41b7e/state/**");
            assert_eq!(
                origin_events_wildcard(ORIGIN),
                "v1/h-3fa9c2d41b7e/events/**"
            );
            assert_eq!(
                origin_alerts_wildcard(ORIGIN),
                "v1/h-3fa9c2d41b7e/state/*/alert/*"
            );
            assert_eq!(
                origin_liveliness_expr(ORIGIN),
                "v1/h-3fa9c2d41b7e/state/*/alive"
            );
            assert_eq!(
                origin_device_liveliness_expr(ORIGIN),
                "v1/h-3fa9c2d41b7e/state/*/device/*/alive"
            );
        }

        #[test]
        fn rpc_keys() {
            // A multi-chunk procedure, exactly as the GUI spells one.
            assert_eq!(
                fleet_rpc_key("netring", "artifact/cancel"),
                "v1/*/@rpc/netring/artifact/cancel"
            );
            assert_eq!(
                fleet_command_key("netring", "stream"),
                "v1/*/@rpc/netring/stream/set"
            );
            assert_eq!(
                origin_rpc_key(ORIGIN, "netring", "artifact/status"),
                "v1/h-3fa9c2d41b7e/@rpc/netring/artifact/status"
            );
            assert_eq!(catalog_rpc_key("names"), "v1/@catalog/@rpc/names");
            assert_eq!(catalog_rpc_key("link"), "v1/@catalog/@rpc/link");
            assert_eq!(names_query_key(), "v1/@catalog/@rpc/names");
        }

        #[test]
        fn catalog_subjects_and_wildcards() {
            assert_eq!(
                entity_key("h-0123456789ab"),
                "v1/@catalog/state/entity/h-0123456789ab"
            );
            assert_eq!(
                alias_key("h-0123456789ab"),
                "v1/@catalog/state/alias/h-0123456789ab"
            );
            // The id shape `OperatorAssertion::id` actually produces.
            assert_eq!(
                assertion_key("link-h-0123456789ab-h-3fa9c2d41b7e"),
                "v1/@catalog/state/assertion/link-h-0123456789ab-h-3fa9c2d41b7e"
            );
            assert_eq!(all_entity_wildcard(), "v1/@catalog/state/entity/*");
            assert_eq!(all_alias_wildcard(), "v1/@catalog/state/alias/*");
            assert_eq!(all_assertion_wildcard(), "v1/@catalog/state/assertion/*");
            assert_eq!(entities_query_key(), "v1/@catalog/state/entity/*");
            assert_eq!(correlator_alive_key(), "v1/@catalog/state/alive");
        }

        #[test]
        fn catalog_claims() {
            // The zid is lowercased at this boundary (zenoh zids are hex, but
            // the wire form must be canonical either way).
            assert_eq!(catalog_claim_key("A3F0"), "v1/@catalog/state/claim/a3f0");
            assert_eq!(catalog_claims_wildcard(), "v1/@catalog/state/claim/*");
        }

        #[test]
        fn pdns_keys() {
            assert_eq!(pdns_key("10.0.0.9"), "v1/@catalog/state/pdns/10-0-0-9");
            assert_eq!(
                pdns_key("2001:db8::1"),
                "v1/@catalog/state/pdns/2001-db8--1"
            );
            assert_eq!(all_pdns_wildcard(), "v1/@catalog/state/pdns/**");
        }

        #[test]
        fn media_keys() {
            assert_eq!(
                media_video_key(Protocol::Parallax, ORIGIN, "cam0", "h264", "high"),
                "v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/high"
            );
            assert_eq!(
                media_preview_key(Protocol::Parallax, ORIGIN, "cam0"),
                "v1/h-3fa9c2d41b7e/@media/parallax/cam0/preview/jpeg"
            );
        }

        #[test]
        fn profile_derived_evidence_keys() {
            // The origin is machine-minted, so pin the exact construction:
            // origin + literal spelling + the slugged device chunk. These pin
            // the generated per-producer builders the evidence publishers use
            // (the string-dispatching keyexpr helpers they replaced are gone).
            let origin = crate::PROFILE.host_id().as_str().to_string();
            let local = crate::PROFILE.local_origin();
            let device_key: String = registry::netring::key(
                &local,
                &registry::netring::Subject::evidence_device("AA:BB:CC:00:11:22"),
            )
            .into();
            assert_eq!(
                device_key,
                format!(
                    "v1/{origin}/state/netring/evidence/device/{}",
                    zenkey::slug::chunk_slug("AA:BB:CC:00:11:22")
                )
            );
            let names_key: String = registry::netring::key(
                &local,
                &registry::netring::Subject::evidence_names("10-0-0-9"),
            )
            .into();
            assert_eq!(
                names_key,
                format!("v1/{origin}/state/netring/evidence/names/10-0-0-9")
            );
        }

        #[test]
        fn blob_prefixes() {
            // PROFILE-derived; pin the literal tier spellings.
            let origin = crate::PROFILE.host_id().as_str().to_string();
            assert_eq!(
                crate::artifact_blob_prefix(),
                format!("v1/{origin}/@blob/artifact")
            );
            assert_eq!(
                crate::artifact_store_prefix(),
                format!("v1/{origin}/@blob/store")
            );
            assert_eq!(
                crate::artifact_tree_prefix(),
                format!("v1/{origin}/@blob/tree")
            );
        }
    }
}
