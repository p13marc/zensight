//! Chunk lexical rules, reserved tokens, and structural key assembly/parsing.
//!
//! Normative source: RFC 03 (`docs/rfcs/keyspace-v2/03-grammar.md`). Every
//! rule here cites its section. Keys are **base-relative**: they start at the
//! `@v1` version chunk; the deployment base rides the session namespace
//! (RFC 03 §1.1) and never appears in application-built keys.

use std::fmt;

/// The convention major this crate implements (RFC 03 §1.2).
pub const VERSION_CHUNK: &str = "@v1";

/// Data classes (RFC 03 §1.4). Plain chunks — they participate in wildcards.
pub const CLASS_TELEMETRY: &str = "telemetry";
pub const CLASS_STATE: &str = "state";
pub const CLASS_EVENTS: &str = "events";

/// Verbatim planes (RFC 03 §1.4). Hermetic — no `*`/`**` reaches them.
pub const PLANE_RPC: &str = "@rpc";
pub const PLANE_MEDIA: &str = "@media";
pub const PLANE_BLOB: &str = "@blob";

/// The reserved service origin (RFC 03 §3).
pub const SERVICE_CATALOG: &str = "@catalog";

/// Blob tier tokens — position 5 under `@blob` (RFC 03 §1.5, 07 §2).
pub const BLOB_TIER_ARTIFACT: &str = "artifact";
pub const BLOB_TIER_TREE: &str = "tree";
pub const BLOB_TIER_STORE: &str = "store";

/// Reserved liveliness subject leaf (RFC 03 §3, 04 §5). Never a data subject.
pub const SUBJECT_ALIVE: &str = "alive";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("invalid plain chunk {0:?}: must match [a-z0-9]([a-z0-9._-]*[a-z0-9])? (RFC 03 §2)")]
    InvalidPlainChunk(String),
    #[error("invalid verbatim chunk {0:?}: must match @[a-z0-9][a-z0-9_-]* (RFC 03 §2)")]
    InvalidVerbatimChunk(String),
    #[error("invalid host origin {0:?}: must match h-[0-9a-f]{{12}} (RFC 03 §1.3)")]
    InvalidHostOrigin(String),
    #[error("invalid producer {0:?}: {1} (RFC 03 §1.5)")]
    InvalidProducer(String, &'static str),
    #[error("empty subject: keys need >= 1 subject chunk (RFC 03 §1.6)")]
    EmptySubject,
    #[error("blob tier token expected (artifact|tree|store), got {0:?} (RFC 03 §1.5)")]
    InvalidBlobTier(String),
    #[error("reserved token {0:?} may not be used as a {1} (RFC 03 §3)")]
    ReservedToken(String, &'static str),
    #[error("not a v1 key: {0}")]
    Parse(String),
}

include!("chunk_rules.rs");

/// The publishing identity in position 3 (RFC 03 §1.3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Origin {
    /// `h-<12hex>` — a machine (see [`crate::origin`] for minting).
    Host(crate::origin::HostId),
    /// A verbatim service origin, e.g. `@catalog`. Producer chunk omitted.
    Service(String),
}

impl Origin {
    pub fn catalog() -> Self {
        Origin::Service(SERVICE_CATALOG.to_string())
    }

    pub fn service(name: &str) -> Result<Self, KeyError> {
        if !is_valid_verbatim_chunk(name) {
            return Err(KeyError::InvalidVerbatimChunk(name.to_string()));
        }
        Ok(Origin::Service(name.to_string()))
    }

    pub fn chunk(&self) -> &str {
        match self {
            Origin::Host(id) => id.as_str(),
            Origin::Service(s) => s,
        }
    }

    /// Service origins omit the producer position (RFC 03 §1.5).
    pub fn has_producer_chunk(&self) -> bool {
        matches!(self, Origin::Host(_))
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.chunk())
    }
}

/// The producing component: `<name>` or `<name>-<instance>` (RFC 03 §1.5).
///
/// The instance suffix is `-<positive int>` and base names MUST NOT end in
/// `-<int>`, so the chunk parses back unambiguously.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Producer {
    name: String,
    instance: Option<u32>,
}

impl Producer {
    pub fn new(name: &str) -> Result<Self, KeyError> {
        Self::validate_name(name)?;
        Ok(Producer {
            name: name.to_string(),
            instance: None,
        })
    }

    pub fn with_instance(name: &str, instance: u32) -> Result<Self, KeyError> {
        Self::validate_name(name)?;
        if instance == 0 {
            return Err(KeyError::InvalidProducer(
                name.to_string(),
                "instance numbers start at 1 (the first instance uses the bare name)",
            ));
        }
        Ok(Producer {
            name: name.to_string(),
            instance: Some(instance),
        })
    }

    fn validate_name(name: &str) -> Result<(), KeyError> {
        if !is_valid_plain_chunk(name) {
            return Err(KeyError::InvalidProducer(
                name.to_string(),
                "not a valid plain chunk",
            ));
        }
        if Self::split_trailing_int(name).is_some() {
            return Err(KeyError::InvalidProducer(
                name.to_string(),
                "base names must not end in -<int> (reserved for instance suffixes)",
            ));
        }
        if name == BLOB_TIER_ARTIFACT || name == BLOB_TIER_TREE || name == BLOB_TIER_STORE {
            return Err(KeyError::ReservedToken(name.to_string(), "producer name"));
        }
        Ok(())
    }

    fn split_trailing_int(chunk: &str) -> Option<(&str, u32)> {
        let (base, tail) = chunk.rsplit_once('-')?;
        if base.is_empty() || tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        tail.parse().ok().map(|n| (base, n))
    }

    /// Parse a producer chunk back into (name, instance) — RFC 03 §1.5.
    pub fn parse_chunk(chunk: &str) -> Result<Self, KeyError> {
        if !is_valid_plain_chunk(chunk) {
            return Err(KeyError::InvalidProducer(
                chunk.to_string(),
                "not a valid plain chunk",
            ));
        }
        match Self::split_trailing_int(chunk) {
            Some((base, n)) if n >= 1 => Ok(Producer {
                name: base.to_string(),
                instance: Some(n),
            }),
            _ => Ok(Producer {
                name: chunk.to_string(),
                instance: None,
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn instance(&self) -> Option<u32> {
        self.instance
    }

    pub fn chunk(&self) -> String {
        match self.instance {
            None => self.name.clone(),
            Some(n) => format!("{}-{n}", self.name),
        }
    }
}

/// Data classes (RFC 03 §1.4 / 04 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    Telemetry,
    State,
    Events,
}

impl Class {
    pub fn chunk(self) -> &'static str {
        match self {
            Class::Telemetry => CLASS_TELEMETRY,
            Class::State => CLASS_STATE,
            Class::Events => CLASS_EVENTS,
        }
    }

    pub fn from_chunk(chunk: &str) -> Option<Self> {
        match chunk {
            CLASS_TELEMETRY => Some(Class::Telemetry),
            CLASS_STATE => Some(Class::State),
            CLASS_EVENTS => Some(Class::Events),
            _ => None,
        }
    }
}

/// Verbatim planes (RFC 03 §1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Plane {
    Rpc,
    Media,
    Blob,
}

impl Plane {
    pub fn chunk(self) -> &'static str {
        match self {
            Plane::Rpc => PLANE_RPC,
            Plane::Media => PLANE_MEDIA,
            Plane::Blob => PLANE_BLOB,
        }
    }

    pub fn from_chunk(chunk: &str) -> Option<Self> {
        match chunk {
            PLANE_RPC => Some(Plane::Rpc),
            PLANE_MEDIA => Some(Plane::Media),
            PLANE_BLOB => Some(Plane::Blob),
            _ => None,
        }
    }
}

/// Position 4: a data class or a verbatim plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClassOrPlane {
    Class(Class),
    Plane(Plane),
}

/// Blob tier token (position 5 under `@blob`, RFC 03 §1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlobTier {
    Artifact,
    Tree,
    Store,
}

impl BlobTier {
    pub fn chunk(self) -> &'static str {
        match self {
            BlobTier::Artifact => BLOB_TIER_ARTIFACT,
            BlobTier::Tree => BLOB_TIER_TREE,
            BlobTier::Store => BLOB_TIER_STORE,
        }
    }

    pub fn from_chunk(chunk: &str) -> Option<Self> {
        match chunk {
            BLOB_TIER_ARTIFACT => Some(BlobTier::Artifact),
            BLOB_TIER_TREE => Some(BlobTier::Tree),
            BLOB_TIER_STORE => Some(BlobTier::Store),
            _ => None,
        }
    }
}

fn validate_subject(subject: &[&str]) -> Result<(), KeyError> {
    if subject.is_empty() {
        return Err(KeyError::EmptySubject);
    }
    for chunk in subject {
        if !is_valid_plain_chunk(chunk) {
            return Err(KeyError::InvalidPlainChunk((*chunk).to_string()));
        }
    }
    Ok(())
}

fn push_key(parts: &mut String, chunk: &str) {
    if !parts.is_empty() {
        parts.push('/');
    }
    parts.push_str(chunk);
}

/// Build a data-class key: `@v1/<origin>/<class>[/<producer>]/<subject...>`.
///
/// The producer chunk is omitted under service origins (RFC 03 §1.5).
pub fn data_key(
    origin: &Origin,
    class: Class,
    producer: Option<&Producer>,
    subject: &[&str],
) -> Result<String, KeyError> {
    validate_subject(subject)?;
    if origin.has_producer_chunk() != producer.is_some() {
        return Err(KeyError::Parse(
            "host origins require a producer chunk; service origins forbid one (RFC 03 §1.5)"
                .to_string(),
        ));
    }
    // `alive` is a reserved liveliness-only token under `state` (RFC 03 §3):
    // state keys carrying it come only from the dedicated builders below.
    if class == Class::State && subject.contains(&SUBJECT_ALIVE) {
        return Err(KeyError::ReservedToken(
            SUBJECT_ALIVE.to_string(),
            "data subject chunk",
        ));
    }
    let mut key = String::new();
    push_key(&mut key, VERSION_CHUNK);
    push_key(&mut key, origin.chunk());
    push_key(&mut key, class.chunk());
    if let Some(p) = producer {
        push_key(&mut key, &p.chunk());
    }
    for chunk in subject {
        push_key(&mut key, chunk);
    }
    Ok(key)
}

/// Build an `@rpc` procedure key: `@v1/<origin>/@rpc[/<producer>]/<procedure...>`.
pub fn rpc_key(
    origin: &Origin,
    producer: Option<&Producer>,
    procedure: &[&str],
) -> Result<String, KeyError> {
    validate_subject(procedure)?;
    if origin.has_producer_chunk() != producer.is_some() {
        return Err(KeyError::Parse(
            "host origins require a producer chunk; service origins forbid one (RFC 03 §1.5)"
                .to_string(),
        ));
    }
    let mut key = String::new();
    push_key(&mut key, VERSION_CHUNK);
    push_key(&mut key, origin.chunk());
    push_key(&mut key, PLANE_RPC);
    if let Some(p) = producer {
        push_key(&mut key, &p.chunk());
    }
    for chunk in procedure {
        push_key(&mut key, chunk);
    }
    Ok(key)
}

/// Build an `@media` key: `@v1/<origin>/@media/<producer>/<stream...>`.
pub fn media_key(
    origin: &Origin,
    producer: &Producer,
    stream: &[&str],
) -> Result<String, KeyError> {
    validate_subject(stream)?;
    let mut key = String::new();
    push_key(&mut key, VERSION_CHUNK);
    push_key(&mut key, origin.chunk());
    push_key(&mut key, PLANE_MEDIA);
    push_key(&mut key, &producer.chunk());
    for chunk in stream {
        push_key(&mut key, chunk);
    }
    Ok(key)
}

/// Build an `@blob` key: `@v1/<origin>/@blob/<tier>/<rest...>` (RFC 07 §2).
pub fn blob_key(origin: &Origin, tier: BlobTier, rest: &[&str]) -> Result<String, KeyError> {
    validate_subject(rest)?;
    let mut key = String::new();
    push_key(&mut key, VERSION_CHUNK);
    push_key(&mut key, origin.chunk());
    push_key(&mut key, PLANE_BLOB);
    push_key(&mut key, tier.chunk());
    for chunk in rest {
        push_key(&mut key, chunk);
    }
    Ok(key)
}

/// Liveliness token key for a producer: `@v1/<origin>/state/<producer>/alive`
/// (RFC 04 §5). Service origins: `@v1/@<service>/state/alive`.
pub fn alive_key(origin: &Origin, producer: Option<&Producer>) -> Result<String, KeyError> {
    if origin.has_producer_chunk() != producer.is_some() {
        return Err(KeyError::Parse(
            "host origins require a producer chunk; service origins forbid one (RFC 03 §1.5)"
                .to_string(),
        ));
    }
    let mut key = String::new();
    push_key(&mut key, VERSION_CHUNK);
    push_key(&mut key, origin.chunk());
    push_key(&mut key, CLASS_STATE);
    if let Some(p) = producer {
        push_key(&mut key, &p.chunk());
    }
    push_key(&mut key, SUBJECT_ALIVE);
    Ok(key)
}

/// Liveliness token key for a tracked downstream device (RFC 04 §5):
/// `@v1/<origin>/state/<producer>/device/<device>/alive`.
pub fn device_alive_key(
    origin: &Origin,
    producer: &Producer,
    device: &str,
) -> Result<String, KeyError> {
    if !is_valid_plain_chunk(device) {
        return Err(KeyError::InvalidPlainChunk(device.to_string()));
    }
    let mut key = String::new();
    push_key(&mut key, VERSION_CHUNK);
    push_key(&mut key, origin.chunk());
    push_key(&mut key, CLASS_STATE);
    push_key(&mut key, &producer.chunk());
    push_key(&mut key, "device");
    push_key(&mut key, device);
    push_key(&mut key, SUBJECT_ALIVE);
    Ok(key)
}

/// A structurally parsed v1 key (positions 2–5; the subject tail is opaque
/// here — registry-generated parsers refine it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralKey {
    pub origin: Origin,
    pub class: ClassOrPlane,
    /// `None` for service origins and for `@blob` (tier token instead).
    pub producer: Option<Producer>,
    /// Tier token, only under `@blob`.
    pub blob_tier: Option<BlobTier>,
    /// Everything after the producer/tier position.
    pub subject: Vec<String>,
}

/// Parse a base-relative v1 key (`@v1/...`). Structural only: subject tails
/// stay opaque (RFC 03 §1). Rejects anything that is not under `@v1`.
pub fn parse(key: &str) -> Result<StructuralKey, KeyError> {
    let mut chunks = key.split('/');
    let version = chunks
        .next()
        .ok_or_else(|| KeyError::Parse("empty key".into()))?;
    if version != VERSION_CHUNK {
        return Err(KeyError::Parse(format!(
            "expected {VERSION_CHUNK} first, got {version:?}"
        )));
    }
    let origin_chunk = chunks
        .next()
        .ok_or_else(|| KeyError::Parse("missing origin chunk".into()))?;
    let origin = if is_valid_host_origin(origin_chunk) {
        Origin::Host(crate::origin::HostId::parse(origin_chunk).expect("validated"))
    } else if is_valid_verbatim_chunk(origin_chunk) {
        Origin::Service(origin_chunk.to_string())
    } else {
        return Err(KeyError::InvalidHostOrigin(origin_chunk.to_string()));
    };
    let class_chunk = chunks
        .next()
        .ok_or_else(|| KeyError::Parse("missing class chunk".into()))?;
    let class = if let Some(c) = Class::from_chunk(class_chunk) {
        ClassOrPlane::Class(c)
    } else if let Some(p) = Plane::from_chunk(class_chunk) {
        ClassOrPlane::Plane(p)
    } else {
        return Err(KeyError::Parse(format!(
            "unknown class/plane chunk {class_chunk:?}"
        )));
    };

    let mut producer = None;
    let mut blob_tier = None;
    match (&origin, &class) {
        (_, ClassOrPlane::Plane(Plane::Blob)) => {
            let tier = chunks
                .next()
                .ok_or_else(|| KeyError::Parse("missing blob tier".into()))?;
            blob_tier = Some(
                BlobTier::from_chunk(tier)
                    .ok_or_else(|| KeyError::InvalidBlobTier(tier.to_string()))?,
            );
        }
        (Origin::Host(_), _) => {
            let chunk = chunks
                .next()
                .ok_or_else(|| KeyError::Parse("missing producer chunk".into()))?;
            producer = Some(Producer::parse_chunk(chunk)?);
        }
        (Origin::Service(_), _) => {}
    }

    let subject: Vec<String> = chunks.map(str::to_string).collect();
    if subject.is_empty() {
        return Err(KeyError::EmptySubject);
    }
    Ok(StructuralKey {
        origin,
        class,
        producer,
        blob_tier,
        subject,
    })
}

/// Prepend an explicit base — for router-side artifacts (storage selectors,
/// ACL rules) and tests. Application sessions use the namespace instead
/// (RFC 09 §0).
pub fn with_base(base: &str, key_or_selector: &str) -> String {
    format!("{base}/{key_or_selector}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::HostId;

    fn host() -> Origin {
        Origin::Host(HostId::parse("h-3fa9c2d41b7e").unwrap())
    }

    #[test]
    fn plain_chunk_rules() {
        for ok in [
            "a",
            "cpu",
            "sys_uptime",
            "10-0-0-7",
            "sshd.service",
            "p95_ms",
            "h-3fa9c2d41b7e",
        ] {
            assert!(is_valid_plain_chunk(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", ".a", "A", "Cpu", "a/b", "a*", "@v1", "é"] {
            assert!(!is_valid_plain_chunk(bad), "{bad}");
        }
    }

    #[test]
    fn verbatim_chunk_rules() {
        for ok in ["@v1", "@rpc", "@catalog", "@adv"] {
            assert!(is_valid_verbatim_chunk(ok), "{ok}");
        }
        for bad in ["@", "@-x", "v1", "@V1", "@a/b"] {
            assert!(!is_valid_verbatim_chunk(bad), "{bad}");
        }
    }

    #[test]
    fn producer_instance_split_is_unambiguous() {
        // RFC 03 §1.5: base names must not end in -<int>.
        assert!(Producer::new("snmp").is_ok());
        assert!(Producer::new("net-ring").is_ok());
        assert!(Producer::new("ipv6-2").is_err());
        assert!(Producer::with_instance("snmp", 0).is_err());
        let p = Producer::with_instance("snmp", 2).unwrap();
        assert_eq!(p.chunk(), "snmp-2");
        let back = Producer::parse_chunk("snmp-2").unwrap();
        assert_eq!(back.name(), "snmp");
        assert_eq!(back.instance(), Some(2));
        let bare = Producer::parse_chunk("net-ring").unwrap();
        assert_eq!(bare.name(), "net-ring");
        assert_eq!(bare.instance(), None);
    }

    #[test]
    fn blob_tiers_are_not_producers() {
        assert!(Producer::new("store").is_err());
        assert!(Producer::new("tree").is_err());
        assert!(Producer::new("artifact").is_err());
    }

    #[test]
    fn normative_examples_build_and_roundtrip() {
        // The RFC 03 §5 example set, base-relative.
        let p = |n| Producer::new(n).unwrap();
        let cases = [
            data_key(
                &host(),
                Class::Telemetry,
                Some(&p("sysinfo")),
                &["cpu", "usage"],
            )
            .unwrap(),
            data_key(
                &host(),
                Class::Telemetry,
                Some(&p("snmp")),
                &["router01", "system", "sys_uptime"],
            )
            .unwrap(),
            data_key(&host(), Class::State, Some(&p("netring")), &["health"]).unwrap(),
            data_key(
                &host(),
                Class::State,
                Some(&p("netlink")),
                &["alert", "9f2c81ab04d7e3f1"],
            )
            .unwrap(),
            data_key(
                &host(),
                Class::State,
                Some(&p("netring")),
                &["evidence", "names", "10-0-0-7"],
            )
            .unwrap(),
            data_key(
                &host(),
                Class::Events,
                Some(&p("netring")),
                &["capture", "01jgxqz4yqk8v6txw3m9f2a7cd"],
            )
            .unwrap(),
            rpc_key(&host(), Some(&p("netlink")), &["sockets"]).unwrap(),
            media_key(&host(), &p("parallax"), &["cam0", "video", "h264", "main"]).unwrap(),
            blob_key(&host(), BlobTier::Store, &["sha256", "ab12cd34ef56"]).unwrap(),
            data_key(
                &Origin::catalog(),
                Class::State,
                None,
                &["entity", "h-3fa9c2d41b7e"],
            )
            .unwrap(),
            data_key(
                &Origin::catalog(),
                Class::State,
                None,
                &["pdns", "93-184-216-34"],
            )
            .unwrap(),
        ];
        let expected = [
            "@v1/h-3fa9c2d41b7e/telemetry/sysinfo/cpu/usage",
            "@v1/h-3fa9c2d41b7e/telemetry/snmp/router01/system/sys_uptime",
            "@v1/h-3fa9c2d41b7e/state/netring/health",
            "@v1/h-3fa9c2d41b7e/state/netlink/alert/9f2c81ab04d7e3f1",
            "@v1/h-3fa9c2d41b7e/state/netring/evidence/names/10-0-0-7",
            "@v1/h-3fa9c2d41b7e/events/netring/capture/01jgxqz4yqk8v6txw3m9f2a7cd",
            "@v1/h-3fa9c2d41b7e/@rpc/netlink/sockets",
            "@v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main",
            "@v1/h-3fa9c2d41b7e/@blob/store/sha256/ab12cd34ef56",
            "@v1/@catalog/state/entity/h-3fa9c2d41b7e",
            "@v1/@catalog/state/pdns/93-184-216-34",
        ];
        for (built, want) in cases.iter().zip(expected) {
            assert_eq!(built, want);
            let parsed = parse(built).unwrap();
            // Rebuild from parts must reproduce the key (canon round-trip).
            let subject: Vec<&str> = parsed.subject.iter().map(String::as_str).collect();
            let rebuilt = match parsed.class {
                ClassOrPlane::Class(c) => {
                    data_key(&parsed.origin, c, parsed.producer.as_ref(), &subject).unwrap()
                }
                ClassOrPlane::Plane(Plane::Rpc) => {
                    rpc_key(&parsed.origin, parsed.producer.as_ref(), &subject).unwrap()
                }
                ClassOrPlane::Plane(Plane::Media) => {
                    media_key(&parsed.origin, parsed.producer.as_ref().unwrap(), &subject).unwrap()
                }
                ClassOrPlane::Plane(Plane::Blob) => {
                    blob_key(&parsed.origin, parsed.blob_tier.unwrap(), &subject).unwrap()
                }
            };
            assert_eq!(&rebuilt, want);
        }
    }

    #[test]
    fn alive_is_liveliness_only() {
        assert!(
            data_key(
                &host(),
                Class::State,
                Some(&Producer::new("netlink").unwrap()),
                &["alive"]
            )
            .is_err()
        );
        assert_eq!(
            alive_key(&host(), Some(&Producer::new("netlink").unwrap())).unwrap(),
            "@v1/h-3fa9c2d41b7e/state/netlink/alive"
        );
        assert_eq!(
            alive_key(&Origin::catalog(), None).unwrap(),
            "@v1/@catalog/state/alive"
        );
        assert_eq!(
            device_alive_key(&host(), &Producer::new("snmp").unwrap(), "router01").unwrap(),
            "@v1/h-3fa9c2d41b7e/state/snmp/device/router01/alive"
        );
    }

    #[test]
    fn service_origin_omits_producer() {
        assert!(
            data_key(
                &Origin::catalog(),
                Class::State,
                Some(&Producer::new("x").unwrap()),
                &["entity", "a"]
            )
            .is_err()
        );
        assert!(data_key(&host(), Class::State, None, &["health"]).is_err());
    }

    #[test]
    fn parse_rejects_foreign_keys() {
        assert!(parse("zensight/netlink/host/@/health").is_err());
        assert!(parse("@v2/h-3fa9c2d41b7e/state/x/health").is_err());
        assert!(parse("@v1/h-3fa9c2d41b7e/bogus/x/health").is_err());
        assert!(parse("@v1/h-3fa9c2d41b7e/@blob/bogus/x").is_err());
    }
}
