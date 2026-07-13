//! The v1 keyspace context: base + origin + producer in one value
//! (epic #453, RFC `docs/rfcs/keyspace-v2/`).
//!
//! One value carries everything a producer needs to build conforming keys:
//! the deployment base, the host origin (`h-<12hex>`, minted once per
//! process), and the producer chunk. All framework keys flow through here —
//! sensors never spell `@v1` by hand.
//!
//! Contexts are built [`V1Context::for_producer`] from the producer name
//! alone — the legacy config `key_prefix` is retired (#465). Keys are
//! emitted with the base spelled out; when sessions switch to the Zenoh
//! namespace, the base becomes empty here and nothing else changes.

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::grammar::{self, Class, Origin, Producer};
use crate::origin::HostId;
use crate::slug::chunk_slug;

/// The process-wide host origin (RFC 06 §1): `/etc/machine-id` + the
/// application salt, with the persisted-random fallback. Minted once.
pub fn host_id() -> &'static HostId {
    static HOST_ID: OnceLock<HostId> = OnceLock::new();
    HOST_ID.get_or_init(|| {
        let fallback = dirs::state_dir()
            .map(|d| d.join("zensight/host-id"))
            .unwrap_or_else(|| PathBuf::from("/var/lib/zensight/host-id"));
        let id = HostId::mint_system(&fallback);
        tracing::info!(origin = %id, "host origin minted");
        id
    })
}

/// Everything needed to build this producer's v1 keys.
#[derive(Debug, Clone)]
pub struct V1Context {
    base: String,
    origin: Origin,
    producer: Producer,
}

impl V1Context {
    /// Build the context for one producer on this host: base `zensight`,
    /// origin = the local minted host id, producer = `name` (slugged to a
    /// valid chunk when necessary; a degenerate name falls back to
    /// `sensor`).
    pub fn for_producer(name: &str) -> Self {
        let producer = Producer::new(name).unwrap_or_else(|_| {
            let slug = chunk_slug(name);
            Producer::parse_chunk(&slug)
                .or_else(|_| Producer::new("sensor"))
                .expect("fallback producer name is valid")
        });
        Self {
            base: "zensight".to_string(),
            origin: Origin::Host(host_id().clone()),
            producer,
        }
    }

    /// Override the deployment base (RFC 03 §1.1 multi-chunk bases are
    /// legal; the default is `zensight`).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// As [`for_producer`](Self::for_producer) with an explicit producer
    /// instance (RFC 03 §1.5).
    pub fn with_instance(mut self, instance: u32) -> Self {
        if let Ok(p) = Producer::with_instance(self.producer.name(), instance) {
            self.producer = p;
        }
        self
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    pub fn producer(&self) -> &Producer {
        &self.producer
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn full(&self, rel: &str) -> String {
        format!("{}/{}", self.base, rel)
    }

    /// The telemetry prefix: `<base>/@v1/<origin>/telemetry/<producer>`.
    /// Metric suffixes append below it ({metric...} / {device}/{metric...}
    /// registry families).
    pub fn telemetry_prefix(&self) -> String {
        self.full(&format!(
            "{}/{}/{}/{}",
            grammar::VERSION_CHUNK,
            self.origin.chunk(),
            grammar::CLASS_TELEMETRY,
            self.producer.chunk()
        ))
    }

    /// A `state/<producer>/<subject...>` key. Subject chunks are slugged
    /// where not already legal.
    pub fn state_key(&self, subject: &[&str]) -> String {
        let slugged: Vec<String> = subject.iter().map(|c| chunk_slug(c)).collect();
        let refs: Vec<&str> = slugged.iter().map(String::as_str).collect();
        self.full(
            &grammar::data_key(&self.origin, Class::State, Some(&self.producer), &refs)
                .expect("slugged subject chunks are valid"),
        )
    }

    pub fn health_key(&self) -> String {
        self.state_key(&["health"])
    }

    pub fn errors_key(&self) -> String {
        self.state_key(&["errors"])
    }

    /// The registration document (RFC: `state/<producer>/sensor`).
    pub fn sensor_info_key(&self) -> String {
        self.state_key(&["sensor"])
    }

    pub fn evidence_self_key(&self) -> String {
        self.state_key(&["evidence", "self"])
    }

    pub fn evidence_device_key(&self, device: &str) -> String {
        self.state_key(&["evidence", "device", device])
    }

    pub fn artifact_status_key(&self, kind: &str) -> String {
        self.state_key(&["artifact", kind])
    }

    /// Liveliness token key (RFC 04 §5) — machinery, not a data subject.
    pub fn alive_key(&self) -> String {
        self.full(
            &grammar::alive_key(&self.origin, Some(&self.producer))
                .expect("producer context is valid"),
        )
    }

    /// Device liveliness token key (RFC 04 §5).
    pub fn device_alive_key(&self, device: &str) -> String {
        let device = chunk_slug(device);
        self.full(
            &grammar::device_alive_key(&self.origin, &self.producer, &device)
                .expect("slugged device chunk is valid"),
        )
    }

    /// An `@rpc/<producer>/<procedure...>` key (RFC 05).
    pub fn rpc_key(&self, procedure: &[&str]) -> String {
        self.full(
            &grammar::rpc_key(&self.origin, Some(&self.producer), procedure)
                .expect("registry procedure chunks are valid"),
        )
    }

    /// Media plane keys (RFC 07 §1).
    pub fn media_video_key(&self, stream: &str, codec: &str, profile: &str) -> String {
        let chunks = [
            chunk_slug(stream),
            "video".into(),
            chunk_slug(codec),
            chunk_slug(profile),
        ];
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        self.full(
            &grammar::media_key(&self.origin, &self.producer, &refs)
                .expect("slugged stream chunks are valid"),
        )
    }

    /// The `@blob` tier prefix (RFC 07 §2):
    /// `<base>/@v1/<origin>/@blob/<tier>` — Tier-1 `artifact`, Tier-2
    /// `tree`/`store`.
    pub fn blob_prefix(&self, tier: grammar::BlobTier) -> String {
        self.full(&format!(
            "{}/{}/{}/{}",
            grammar::VERSION_CHUNK,
            self.origin.chunk(),
            grammar::PLANE_BLOB,
            tier.chunk()
        ))
    }

    pub fn media_preview_key(&self, stream: &str) -> String {
        let chunks = [chunk_slug(stream), "preview".into(), "jpeg".into()];
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        self.full(
            &grammar::media_key(&self.origin, &self.producer, &refs)
                .expect("slugged stream chunks are valid"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> V1Context {
        let mut c = V1Context::for_producer("sysinfo");
        c.origin = Origin::Host(HostId::parse("h-3fa9c2d41b7e").unwrap());
        c
    }

    #[test]
    fn key_shapes() {
        let c = ctx();
        assert_eq!(
            c.telemetry_prefix(),
            "zensight/@v1/h-3fa9c2d41b7e/telemetry/sysinfo"
        );
        assert_eq!(
            c.health_key(),
            "zensight/@v1/h-3fa9c2d41b7e/state/sysinfo/health"
        );
        assert_eq!(
            c.evidence_self_key(),
            "zensight/@v1/h-3fa9c2d41b7e/state/sysinfo/evidence/self"
        );
        assert_eq!(
            c.alive_key(),
            "zensight/@v1/h-3fa9c2d41b7e/state/sysinfo/alive"
        );
        assert_eq!(
            c.device_alive_key("router01"),
            "zensight/@v1/h-3fa9c2d41b7e/state/sysinfo/device/router01/alive"
        );
        // Foreign device names slug injectively (RFC 03 §2) — never lossy
        // lowercasing ("Router01" and "router01" must not share a key).
        assert_ne!(
            c.device_alive_key("Router01"),
            c.device_alive_key("router01")
        );
        assert_eq!(
            c.rpc_key(&["introspect"]),
            "zensight/@v1/h-3fa9c2d41b7e/@rpc/sysinfo/introspect"
        );
        assert_eq!(
            c.media_video_key("cam0", "h264", "main"),
            "zensight/@v1/h-3fa9c2d41b7e/@media/sysinfo/cam0/video/h264/main"
        );
    }

    #[test]
    fn multi_chunk_base() {
        let mut c = V1Context::for_producer("netring").with_base("acme/fleet-a");
        c.origin = Origin::Host(HostId::parse("h-3fa9c2d41b7e").unwrap());
        assert_eq!(
            c.telemetry_prefix(),
            "acme/fleet-a/@v1/h-3fa9c2d41b7e/telemetry/netring"
        );
    }
}
