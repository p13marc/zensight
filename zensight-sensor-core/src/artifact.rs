//! Unified artifact production + serving (the `@rpc/<producer>/artifact/*`
//! procedures + `@blob/{artifact,store,tree}` delivery).
//!
//! One channel subsumes the former `@/report` (Tier-1 whole-file) and
//! `@/snapshot` (Tier-2 directory-tree) channels and hosts new artifact kinds
//! (e.g. on-demand packet capture) as pluggable [`ArtifactProducer`]s instead of
//! a third copy of the same request/status/serve/TTL machinery.
//!
//! The channel owns the control plane — a **request** procedure
//! (`@rpc/<producer>/artifact/request`, request/reply), a **status** read
//! procedure (`…/artifact/status`, one entry
//! per registered kind), a **cancel** write procedure (`…/artifact/cancel`) and a TTL
//! reaper — plus the `zenoh-blob` delivery servers (a [`BlobServer`] for Tier-1
//! producers, a [`TreeServer`] + in-memory chunk store for Tier-2 producers),
//! spun up only when a producer of that delivery kind is registered.
//!
//! Producers never touch zenoh: a producer validates + authorizes a request
//! ([`ArtifactProducer::accepts`]) and produces a file or directory
//! ([`ArtifactProducer::produce`]); the channel turns that into a `Delivery` and
//! publishes the lifecycle. Busy/cooldown are **per kind**, so a long capture
//! never blocks a quick debug bundle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{Mutex, watch};
use ulid::Ulid;
use zblob::{
    BlobServer, BlobSpec, CancelToken, CdcParams, ContentStore, DirStore, Hash, MemoryBlobSource,
    MemoryStore, TreeIndex, TreeServer, build_tree_from, gc,
};
use zensight_common::artifact::{
    ArtifactKind, ArtifactRequest, ArtifactState, ArtifactStatus, Delivery, KindAdvert, KindStatus,
    TreeSummary,
};
use zensight_common::{
    CommonArtifactLimits, artifact_blob_prefix, artifact_cancel_key, artifact_request_key,
    artifact_status_key, artifact_store_prefix, artifact_tree_prefix,
};

mod producers;
pub use producers::{ReportProducer, SnapshotProducer};

/// Which transfer tier an artifact is delivered over. Declared up front (not
/// inferred from a produced artifact) so the channel can spin up the matching
/// `zenoh-blob` server before any request arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    /// Tier-1 whole-blob (a single file), served by [`BlobServer`].
    Blob,
    /// Tier-2 content-addressed tree (a directory), served by [`TreeServer`].
    Tree,
}

/// A produced artifact handed back to the channel for delivery. The variant must
/// match the producer's declared [`ArtifactProducer::delivery_kind`].
pub enum Produced {
    /// A single file → [`Delivery::Blob`]. `filename` is the suggested save name.
    File {
        /// Path to the produced file (owned by the channel afterwards; reaped on TTL).
        path: PathBuf,
        /// Suggested download filename.
        filename: String,
    },
    /// A directory → [`Delivery::Tree`]. The channel walks + chunks it.
    Dir {
        /// Absolute path of the directory to snapshot.
        path: PathBuf,
        /// Durable lineage tag (e.g. `snapshot-pcaps`). `Some` opts this
        /// build into incremental re-snapshotting: the channel keeps the
        /// latest index per lineage as the parent for `build_tree_from`
        /// (unchanged `(size, mtime)` files reuse their chunk references) and
        /// tags it so its chunks survive sweeps — and, with a durable store,
        /// restarts. `None` = a one-shot build, reclaimed once released.
        lineage: Option<String>,
    },
    /// An in-memory payload → [`Delivery::Blob`], served straight from memory
    /// (`BlobServer::register_source` over a `MemoryBlobSource`) — nothing
    /// lands on disk, so cleanup is unregister-only. For artifacts that are
    /// already fully built in memory (the debug report, the log bundle);
    /// anything genuinely file-backed stays [`Produced::File`].
    Bytes {
        /// The artifact payload.
        data: Vec<u8>,
        /// Suggested download filename.
        filename: String,
    },
}

/// A progress update a producer streams while generating (surfaced in
/// [`ArtifactState::Generating`]).
#[derive(Debug, Clone, Default)]
pub struct ProgressUpdate {
    /// Human-readable line (e.g. `"capturing 12s/30s · 3.1 MiB"`).
    pub detail: Option<String>,
    /// Fractional progress in `0.0..=1.0`.
    pub progress: Option<f32>,
}

/// Context handed to [`ArtifactProducer::produce`].
pub struct ProduceCtx {
    /// A private working directory for temp files (cleaned by the channel).
    pub workdir: PathBuf,
    /// Fires when the request is cancelled — a long-running producer must poll
    /// this and stop promptly.
    pub cancel: CancelToken,
    /// Send incremental progress here; the channel republishes it as `Generating`.
    pub progress: watch::Sender<ProgressUpdate>,
}

/// A pluggable artifact producer. One instance is registered per kind on an
/// [`ArtifactChannel`]; the channel drives its lifecycle.
#[async_trait]
pub trait ArtifactProducer: Send + Sync + 'static {
    /// The kind slug this producer serves (`"report"` / `"snapshot"` / `"capture"`).
    /// Must match the corresponding [`ArtifactKind::slug`].
    fn kind(&self) -> &'static str;

    /// The shared bounds the channel enforces (cooldown/TTL) and advertises.
    fn common(&self) -> &CommonArtifactLimits;

    /// The transfer tier this producer delivers over.
    fn delivery_kind(&self) -> DeliveryKind;

    /// What the GUI needs to render this kind's request affordance.
    fn advert(&self) -> KindAdvert;

    /// Validate + authorize a request's parameters against this producer's policy
    /// (e.g. snapshot allowlist resolution, capture filter/duration clamps). An
    /// `Err(reason)` is published as `Failed`.
    fn accepts(&self, kind: &ArtifactKind) -> Result<(), String>;

    /// For a `Tree` producer, an optional cap on the file count (enforced by the
    /// channel after the walk, since it depends on the walk result). Ignored for
    /// `Blob` producers.
    fn tree_max_files(&self) -> Option<u64> {
        None
    }

    /// For a `Tree` producer, the durable chunk-store directory
    /// (`artifacts.snapshot.state_dir`). `None` (the default) keeps chunks in
    /// memory, exactly the pre-durable behavior. The channel opens the store
    /// from the first `Some` among its enabled Tree producers.
    fn store_state_dir(&self) -> Option<PathBuf> {
        None
    }

    /// Produce the artifact. Long-running producers must honor `ctx.cancel` and
    /// stream `ctx.progress`.
    async fn produce(&self, kind: ArtifactKind, ctx: ProduceCtx) -> anyhow::Result<Produced>;
}

/// The live artifact for one kind (for TTL reaping + cancel).
struct Active {
    id: Ulid,
    cleanup: Cleanup,
    expires: Instant,
}

/// How to release a delivered artifact.
enum Cleanup {
    /// Unregister the blob (by artifact id) and remove the temp file.
    TempFile { id: Ulid, path: PathBuf },
    /// Unregister the blob (by artifact id); nothing on disk to remove
    /// (an in-memory [`Produced::Bytes`] artifact).
    Blob { id: Ulid },
    /// Unregister the tree index and clear the chunk store.
    Tree(String),
}

/// Per-kind runtime state.
#[derive(Default)]
struct KindRuntime {
    current: Option<ArtifactState>,
    busy: bool,
    last_gen: Option<Instant>,
    active: Option<Active>,
    /// Cancellation handle for an in-flight production (fired on `artifact/cancel`).
    in_flight: Option<(Ulid, CancelToken)>,
}

/// Serves the artifact channel for one sensor.
pub struct ArtifactChannel {
    session: Arc<zenoh::Session>,
    producer: String,
    source_id: String,
    producers: HashMap<&'static str, Arc<dyn ArtifactProducer>>,
    /// Tier-1 blob server (present iff a `Blob` producer is registered).
    blob: Option<BlobServer>,
    /// Tier-2 chunk store (present iff a `Tree` producer is registered): a
    /// durable `DirStore` when `artifacts.snapshot.state_dir` is set, else a
    /// `MemoryStore`. Either way it is shared by every snapshot and reclaimed
    /// by mark-and-sweep, never cleared wholesale.
    store: Option<Arc<dyn ContentStore>>,
    /// Snapshot tags — the liveness marks [`gc::sweep`] keeps chunks for.
    /// Durable beside the store, or in a channel-owned temp dir in volatile
    /// mode so both modes run the same sweep path.
    tags: Option<Arc<gc::SnapshotTags>>,
    /// In-flight-download protection for the sweep (zblob's temp tags). The
    /// serving side never takes one, but `sweep` requires the registry.
    temps: Arc<gc::TempTags>,
    /// Latest index per lineage tag — the `build_tree_from` parent. Held in
    /// memory because a `TagRecord` keeps only root+chunks, not the entries
    /// parent reuse matches against; after a restart the first build of each
    /// lineage is full (but still dedups against a warm durable store by
    /// chunk presence).
    last_index: Arc<Mutex<HashMap<String, TreeIndex>>>,
    /// Keeps the volatile-mode tags directory alive (deleted on drop).
    _tags_dir: Option<Arc<tempfile::TempDir>>,
    tree_server: Option<TreeServer>,
    state: Arc<Mutex<HashMap<&'static str, KindRuntime>>>,
}

impl ArtifactChannel {
    /// Build a channel for `producer` (e.g. `"netlink"`) serving the
    /// given producers. `source_id` is this host's id (for `target_source`
    /// matching). Returns `None` if no producer is enabled (so `main` can skip
    /// spawning it entirely).
    pub fn new(
        session: Arc<zenoh::Session>,
        producer: impl Into<String>,
        source_id: impl Into<String>,
        producers: Vec<Arc<dyn ArtifactProducer>>,
    ) -> Option<Self> {
        let enabled: Vec<_> = producers
            .into_iter()
            .filter(|p| p.common().enabled)
            .collect();
        if enabled.is_empty() {
            return None;
        }
        let producer = producer.into();

        let wants_blob = enabled
            .iter()
            .any(|p| p.delivery_kind() == DeliveryKind::Blob);
        let wants_tree = enabled
            .iter()
            .any(|p| p.delivery_kind() == DeliveryKind::Tree);

        // Own-origin prefixes are concrete by construction, which is exactly
        // what 0.3's `ServePrefix` demands — the expect documents the
        // invariant rather than handling a case that cannot occur.
        let serve = |prefix: String| {
            zblob::ServePrefix::new(prefix).expect("own-origin prefix is concrete")
        };
        let blob = wants_blob.then(|| BlobServer::new(&session, serve(artifact_blob_prefix())));
        let (store, tags, tags_dir, tree_server) = if wants_tree {
            // A durable store when a Tree producer names a state dir; memory
            // otherwise (the pre-durable behavior). A configured dir that
            // fails to open falls back loudly — the sensor keeps serving, but
            // the operator asked for durability and must hear it is missing.
            let state_dir = enabled.iter().find_map(|p| p.store_state_dir());
            let durable = state_dir.as_ref().and_then(|dir| {
                let open = || -> std::io::Result<(Arc<dyn ContentStore>, gc::SnapshotTags)> {
                    let store: Arc<dyn ContentStore> =
                        Arc::new(DirStore::open(dir.join("chunks"))?);
                    let tags = gc::SnapshotTags::open(dir.join("tags"))?;
                    Ok((store, tags))
                };
                match open() {
                    Ok(v) => Some(v),
                    Err(e) => {
                        tracing::warn!(
                            dir = %dir.display(), error = %e,
                            "artifacts.snapshot.state_dir unusable — falling back to an \
                             in-memory chunk store (snapshots will not survive restart)"
                        );
                        None
                    }
                }
            });
            let (store, tags, tags_dir) = match durable {
                Some((store, tags)) => (store, Arc::new(tags), None),
                None => {
                    // Volatile mode runs the same tag/sweep machinery against
                    // a private temp dir that dies with the channel. A host
                    // whose temp dir is unwritable cannot produce artifacts
                    // at all (the workdir is the same temp dir), so this
                    // expect cannot newly fail anything that worked before.
                    let dir = Arc::new(
                        tempfile::tempdir().expect("a writable temp dir for snapshot tags"),
                    );
                    let tags = gc::SnapshotTags::open(dir.path().join("tags"))
                        .expect("snapshot tags in a fresh private temp dir");
                    let store: Arc<dyn ContentStore> = Arc::new(MemoryStore::new());
                    (store, Arc::new(tags), Some(dir))
                }
            };
            let tree_server = TreeServer::new(
                &session,
                serve(artifact_store_prefix()),
                serve(artifact_tree_prefix()),
                store.clone(),
            );
            (Some(store), Some(tags), tags_dir, Some(tree_server))
        } else {
            (None, None, None, None)
        };

        let mut map = HashMap::new();
        for p in enabled {
            map.insert(p.kind(), p);
        }

        Some(ArtifactChannel {
            session,
            producer,
            source_id: source_id.into(),
            producers: map,
            blob,
            store,
            tags,
            temps: Arc::new(gc::TempTags::new()),
            last_index: Arc::new(Mutex::new(HashMap::new())),
            _tags_dir: tags_dir,
            tree_server,
            state: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Serve forever. Spawned as a worker by `SensorRunner::with_artifacts`.
    pub async fn run(self) {
        if let Err(e) = self.run_inner().await {
            tracing::error!(error = %e, "artifact channel exited");
        }
    }

    async fn run_inner(self: &ArtifactChannel) -> anyhow::Result<()> {
        let request_key = artifact_request_key(&self.producer);
        let req_q = self
            .session
            .declare_queryable(request_key.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("declare artifact request queryable: {e}"))?;
        let status_key = artifact_status_key(&self.producer);
        let status_q = self
            .session
            .declare_queryable(status_key.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("declare artifact status queryable: {e}"))?;
        let cancel_key = artifact_cancel_key(&self.producer);
        let cancel_q = self
            .session
            .declare_queryable(cancel_key.as_str())
            .await
            .map_err(|e| anyhow::anyhow!("declare artifact cancel queryable: {e}"))?;

        // `spawn()` declares the queryable *before* it returns, so by the time
        // this channel starts answering `artifact/request` its blob endpoints
        // are already live — a request cannot race ahead of the server that
        // must serve its bytes. (0.1's `run()` declared inside the spawned
        // task, which left that window open.) The handles are detached
        // deliberately: the servers live as long as the session, and dropping
        // a `ServerHandle` does not stop the loop.
        if let Some(blob) = &self.blob {
            let _ = blob
                .clone()
                .spawn()
                .await
                .map_err(|e| anyhow::anyhow!("spawn blob server: {e}"))?;
        }
        if let Some(tree_server) = &self.tree_server {
            let _ = tree_server
                .clone()
                .spawn()
                .await
                .map_err(|e| anyhow::anyhow!("spawn tree server: {e}"))?;
        }

        // Reclaim crash leftovers: a durable store reopened after an unclean
        // exit may hold chunks of a build that never registered or tagged —
        // garbage by definition, swept before the first request.
        self.sweep_store().await;

        let mut ttl_tick = tokio::time::interval(Duration::from_secs(5));
        tracing::info!(
            producer = %self.producer,
            kinds = ?self.producers.keys().collect::<Vec<_>>(),
            "artifact channel ready"
        );

        loop {
            tokio::select! {
                Ok(query) = req_q.recv_async() => {
                    // Write procedure (RFC 05 §3): value reply = accepted
                    // ({ id }); failures ride reply_err with a namespaced name.
                    let payload = query
                        .payload()
                        .map(|p| p.to_bytes().to_vec())
                        .unwrap_or_default();
                    match self.handle_request(&payload).await {
                        Ok(id) => {
                            let body = serde_json::to_vec(&serde_json::json!({ "id": id.to_string() }))
                                .unwrap_or_default();
                            if let Err(e) = query.reply(request_key.as_str(), body).await {
                                tracing::warn!(error = %e, "artifact request reply failed");
                            }
                        }
                        Err(err) => {
                            let body = serde_json::to_vec(&err).unwrap_or_default();
                            if let Err(e) = query.reply_err(body).await {
                                tracing::warn!(error = %e, "artifact request reply_err failed");
                            }
                        }
                    }
                }
                Ok(query) = status_q.recv_async() => {
                    let status = self.status().await;
                    let payload = serde_json::to_vec(&status).unwrap_or_default();
                    // Concrete reply key (RFC 05 §2.1).
                    if let Err(e) = query.reply(status_key.as_str(), payload).await {
                        tracing::warn!(error = %e, "artifact status reply failed");
                    }
                }
                Ok(query) = cancel_q.recv_async() => {
                    // `?id=<ulid>` selector param, with the legacy body form
                    // as fallback.
                    let param_id = query
                        .parameters()
                        .as_str()
                        .split(';')
                        .find_map(|kv| kv.strip_prefix("id="))
                        .map(str::to_string);
                    let body_id = query
                        .payload()
                        .map(|p| String::from_utf8_lossy(&p.to_bytes()).trim().to_string());
                    match param_id.or(body_id).and_then(|s| s.parse::<Ulid>().ok()) {
                        Some(id) => {
                            self.cancel(id).await;
                            if let Err(e) = query.reply(cancel_key.as_str(), Vec::new()).await {
                                tracing::warn!(error = %e, "artifact cancel reply failed");
                            }
                        }
                        None => {
                            let err = crate::rpc::RpcError::invalid_args(
                                "cancel needs ?id=<ulid> (or a ULID body)",
                            );
                            let body = serde_json::to_vec(&err).unwrap_or_default();
                            let _ = query.reply_err(body).await;
                        }
                    }
                }
                _ = ttl_tick.tick() => {
                    self.reap_expired().await;
                }
            }
        }
    }

    async fn status(&self) -> ArtifactStatus {
        let rt = self.state.lock().await;
        let mut kinds: Vec<KindStatus> = self
            .producers
            .values()
            .map(|p| {
                let k = p.kind();
                let common = p.common();
                let kr = rt.get(k);
                KindStatus {
                    kind: k.to_string(),
                    busy: kr.map(|r| r.busy).unwrap_or(false),
                    current: kr.and_then(|r| r.current.clone()),
                    max_bytes: common.max_bytes,
                    cooldown_secs: common.cooldown_secs,
                    advert: p.advert(),
                }
            })
            .collect();
        kinds.sort_by(|a, b| a.kind.cmp(&b.kind));
        ArtifactStatus { kinds }
    }

    async fn handle_request(&self, payload: &[u8]) -> Result<Ulid, crate::rpc::RpcError> {
        use crate::rpc::RpcError;
        let req: ArtifactRequest = serde_json::from_slice(payload)
            .map_err(|e| RpcError::invalid_args(format!("bad artifact request: {e}")))?;

        // Legacy payload targeting; v1 targeting is the origin chunk (RFC 05
        // §2), so a mismatch is a caller error, not a silent skip.
        if let Some(target) = &req.opts.target_source
            && target != &self.source_id
        {
            return Err(RpcError::not_found(format!(
                "target_source {target:?} is not this host (v1: target by origin key)"
            )));
        }

        let slug = req.kind.slug();
        let Some(producer) = self.producers.get(slug).cloned() else {
            self.set_failed(slug, req.id, &format!("unsupported artifact kind: {slug}"))
                .await;
            return Err(RpcError::unsupported(format!(
                "unsupported artifact kind: {slug}"
            )));
        };

        // Producer-specific validation / authorization.
        if let Err(reason) = producer.accepts(&req.kind) {
            self.set_failed(slug, req.id, &reason).await;
            return Err(RpcError::gated(reason));
        }

        // Per-kind busy + cooldown gate.
        let cancel = CancelToken::new();
        {
            let mut rt = self.state.lock().await;
            let kr = rt.entry(slug).or_default();
            if kr.busy {
                drop(rt);
                self.set_failed(slug, req.id, "already producing this artifact kind")
                    .await;
                return Err(crate::rpc::RpcError::new(
                    crate::rpc::ERR_BUSY,
                    "already producing this artifact kind",
                ));
            }
            if let Some(last) = kr.last_gen
                && last.elapsed() < Duration::from_secs(producer.common().cooldown_secs)
            {
                drop(rt);
                self.set_failed(slug, req.id, "cooling down; try again shortly")
                    .await;
                return Err(crate::rpc::RpcError::new(
                    crate::rpc::ERR_BUSY,
                    "cooling down; try again shortly",
                ));
            }
            kr.busy = true;
            kr.in_flight = Some((req.id, cancel.clone()));
            kr.current = Some(ArtifactState::Generating {
                id: req.id,
                kind: slug.to_string(),
                detail: None,
                progress: None,
            });
        }

        // Produce off the loop so status stays responsive.
        let this = self.clone_handle();
        let id = req.id;
        let kind = req.kind;
        tokio::spawn(async move {
            this.drive(slug, id, kind, producer, cancel).await;
        });
        Ok(id)
    }

    /// Run one production to completion and record the outcome.
    async fn drive(
        &self,
        slug: &'static str,
        id: Ulid,
        kind: ArtifactKind,
        producer: Arc<dyn ArtifactProducer>,
        cancel: CancelToken,
    ) {
        let (progress_tx, mut progress_rx) = watch::channel(ProgressUpdate::default());

        // Relay progress updates into the published Generating state.
        let progress_relay = {
            let state = self.state.clone();
            tokio::spawn(async move {
                while progress_rx.changed().await.is_ok() {
                    let upd = progress_rx.borrow().clone();
                    let mut rt = state.lock().await;
                    if let Some(kr) = rt.get_mut(slug)
                        && matches!(&kr.current, Some(ArtifactState::Generating { id: cur, .. }) if *cur == id)
                    {
                        kr.current = Some(ArtifactState::Generating {
                            id,
                            kind: slug.to_string(),
                            detail: upd.detail,
                            progress: upd.progress,
                        });
                    }
                }
            })
        };

        let workdir = std::env::temp_dir();
        let ctx = ProduceCtx {
            workdir,
            cancel: cancel.clone(),
            progress: progress_tx,
        };
        let outcome = producer.produce(kind, ctx).await;
        progress_relay.abort();

        // 0.3 refuses to re-register an id whose content changed ("use
        // unregister then register to replace"), and artifact ids are
        // client-chosen — a re-request under a live id regenerates the bytes,
        // so the prior registration must go *before* finalize registers the
        // new one. Scoped to an id match on purpose: for the ordinary
        // fresh-id case the previous artifact keeps serving until the new one
        // registered cleanly, so a failed regeneration costs nothing. (This
        // also retires the 0.2 hazard where the post-replace release below
        // unregistered the *fresh* same-id blob.)
        if outcome.is_ok() && !cancel.is_cancelled() {
            let prev_same_id = {
                let mut rt = self.state.lock().await;
                rt.get_mut(slug)
                    .filter(|kr| kr.active.as_ref().is_some_and(|a| a.id == id))
                    .and_then(|kr| kr.active.take())
            };
            if let Some(prev) = prev_same_id {
                self.release(prev.cleanup).await;
            }
        }

        // Finalize: turn the produced artifact into a Delivery + Active record.
        let finalized = match outcome {
            Ok(_) if cancel.is_cancelled() => Err(anyhow::anyhow!("cancelled")),
            Ok(produced) => self.finalize(id, slug, &producer, produced).await,
            Err(e) => Err(e),
        };

        let mut rt = self.state.lock().await;
        let kr = rt.entry(slug).or_default();
        kr.busy = false;
        kr.last_gen = Some(Instant::now());
        kr.in_flight = None;
        match finalized {
            Ok((state, active)) => {
                // Replace any prior artifact of this kind.
                if let Some(prev) = kr.active.take() {
                    self.release(prev.cleanup).await;
                }
                kr.active = Some(active);
                kr.current = Some(state);
            }
            Err(e) => {
                kr.current = Some(ArtifactState::Failed {
                    id,
                    kind: slug.to_string(),
                    reason: e.to_string(),
                });
                // A failed tree build may have streamed chunks into the
                // shared store before erroring; sweep reclaims them (a no-op
                // for blob kinds and for a store with no garbage).
                self.sweep_store().await;
            }
        }
    }

    /// Turn a produced file/dir into a published `Ready` state + a live-artifact
    /// record. Delivery is decided by the produced variant (which must match the
    /// producer's declared delivery kind).
    async fn finalize(
        &self,
        id: Ulid,
        slug: &'static str,
        producer: &Arc<dyn ArtifactProducer>,
        produced: Produced,
    ) -> anyhow::Result<(ArtifactState, Active)> {
        let ttl_secs = producer.common().ttl_secs;
        let chunk_size = producer.common().chunk_size;
        let max_bytes = producer.common().max_bytes;
        let created_ms = chrono::Utc::now().timestamp_millis();
        let expires_ms = created_ms + (ttl_secs as i64) * 1000;
        let expires = Instant::now() + Duration::from_secs(ttl_secs);

        match produced {
            Produced::File { path, filename } => {
                let blob = self
                    .blob
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no blob server for a File artifact"))?;
                // Registration streams the file once to build its bao
                // outboard and derives the manifest from it, so a served
                // manifest cannot disagree with the bytes — the manifest is
                // the *result* of registering, not an input to it.
                let manifest = blob
                    .register_file(
                        BlobSpec::new(id.to_string())
                            .filename(filename)
                            .chunk_size(chunk_size)
                            .created_ms(created_ms),
                        &path,
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("register blob: {e}"))?;
                let state = ArtifactState::Ready {
                    id,
                    kind: slug.to_string(),
                    delivery: Delivery::Blob {
                        manifest,
                        blob_prefix: artifact_blob_prefix(),
                    },
                    expires_ms,
                };
                Ok((
                    state,
                    Active {
                        id,
                        cleanup: Cleanup::TempFile { id, path },
                        expires,
                    },
                ))
            }
            Produced::Bytes { data, filename } => {
                let blob = self
                    .blob
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no blob server for a Bytes artifact"))?;
                // Same manifest-is-the-result contract as the File arm; the
                // bao outboard is computed over the in-memory source, which is
                // right-sized here — Bytes artifacts are bounded by their
                // producer's max_bytes long before this point.
                let manifest = blob
                    .register_source(
                        BlobSpec::new(id.to_string())
                            .filename(filename)
                            .chunk_size(chunk_size)
                            .created_ms(created_ms),
                        Arc::new(MemoryBlobSource::new(data)),
                    )
                    .await
                    .map_err(|e| anyhow::anyhow!("register blob: {e}"))?;
                let state = ArtifactState::Ready {
                    id,
                    kind: slug.to_string(),
                    delivery: Delivery::Blob {
                        manifest,
                        blob_prefix: artifact_blob_prefix(),
                    },
                    expires_ms,
                };
                Ok((
                    state,
                    Active {
                        id,
                        cleanup: Cleanup::Blob { id },
                        expires,
                    },
                ))
            }
            Produced::Dir { path, lineage } => {
                let store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no chunk store for a Dir artifact"))?;
                let tree_server = self
                    .tree_server
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no tree server for a Dir artifact"))?;
                // The store is shared by every snapshot and reclaimed by
                // mark-and-sweep on release — never cleared wholesale, which
                // is what lets chunks stay warm across builds (and across
                // restarts, with a durable store) for cross-build dedup.
                //
                // Bounds are pre-checked on a metadata-only walk BEFORE
                // chunking: the authoritative post-build checks below fire
                // only after the whole tree has already streamed into the
                // store, which for an over-limit directory means unbounded
                // memory (volatile) or disk (durable) growth first.
                {
                    let walk_path = path.clone();
                    let max_files = producer.tree_max_files();
                    tokio::task::spawn_blocking(move || {
                        preflight_tree_bounds(&walk_path, max_bytes, max_files)
                    })
                    .await??;
                }

                let build_store = store.clone();
                let build_id = id.to_string();
                let cdc = snapshot_cdc_params(chunk_size);
                cdc.validate().map_err(|e| {
                    anyhow::anyhow!(
                        "artifacts.snapshot.chunk_size = {chunk_size} yields invalid CDC \
                         parameters: {e}"
                    )
                })?;
                // Incremental build: the lineage's previous index is the
                // parent — unchanged `(size, mtime)` files reuse their chunk
                // references instead of being re-read and re-hashed. The CDC
                // guard is load-bearing, not defensive: `build_tree_from`
                // *errors* on a parent cut with different parameters (a
                // `chunk_size` config change between builds), and the right
                // response is a full rebuild, not a failed job.
                let parent = match &lineage {
                    Some(tag) => self
                        .last_index
                        .lock()
                        .await
                        .get(tag)
                        .filter(|p| p.cdc == cdc)
                        .cloned(),
                    None => None,
                };
                let index: TreeIndex = tokio::task::spawn_blocking(move || {
                    build_tree_from(&path, build_id, &cdc, build_store.as_ref(), parent.as_ref())
                })
                .await?
                .map_err(|e| anyhow::anyhow!("build tree: {e}"))?;

                let total_bytes = index.total_size();
                let file_count = index.file_count() as u64;
                if total_bytes > max_bytes {
                    anyhow::bail!("snapshot ({total_bytes} bytes) exceeds max_bytes ({max_bytes})");
                }
                if let Some(max_files) = producer.tree_max_files()
                    && file_count > max_files
                {
                    anyhow::bail!("snapshot ({file_count} files) exceeds max_files ({max_files})");
                }

                let summary = TreeSummary {
                    file_count,
                    total_bytes,
                };

                // RFC 07 §2.3: re-key the index by its own root before
                // serving it. The consumer then GETs the identity it demands
                // rather than a name it has to trust, so the fetch is
                // self-anchoring and the "immutable ⇒ cacheable" argument the
                // storage exemption rests on actually holds. The ULID stays
                // the *artifact* id (the RPC's handle); it is no longer a
                // `@blob` key.
                let index = index.keyed_by_root();
                let root: Hash = index.root_hash;
                // RFC 07 §2.3, checked in release builds too (was a
                // debug_assert): a snapshot whose key is not its own content
                // root must fail the job, not get served — zenkey's
                // ContentHash is the arbiter of what spells a legal tree key.
                if !index.is_content_addressed()
                    || zenkey::ContentHash::parse(&root.to_string()).is_err()
                {
                    anyhow::bail!(
                        "tree index is not content-addressed — refusing to serve a name \
                         as a @blob/tree key (root {root})"
                    );
                }
                // `register` returns `Result` since 0.3: a large index is
                // sharded into the store (RFC 07 §2.3 v1.17), and a failed
                // shard write must fail the job rather than serve a snapshot
                // whose index chunks are missing.
                tree_server
                    .register(index.clone())
                    .await
                    .map_err(|e| anyhow::anyhow!("registering the snapshot index: {e}"))?;

                // Lineage bookkeeping: replacing the tag atomically drops
                // liveness of the previous snapshot's *unshared* chunks (the
                // next sweep reclaims them) while keeping this one warm for
                // the next incremental build. A tag failure is a warning, not
                // a failed job — the snapshot is registered and downloadable
                // either way, it just won't survive release/restart warm.
                if let Some(tag) = &lineage {
                    if let Some(tags) = &self.tags {
                        let tags = tags.clone();
                        let tag_name = tag.clone();
                        let tag_index = index.clone();
                        let tagged =
                            tokio::task::spawn_blocking(move || tags.set(&tag_name, &tag_index))
                                .await;
                        match tagged {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => tracing::warn!(
                                lineage = %tag, error = %e,
                                "snapshot lineage tag failed; chunks will not stay warm"
                            ),
                            Err(e) => tracing::warn!(
                                lineage = %tag, error = %e,
                                "snapshot lineage tag task failed"
                            ),
                        }
                    }
                    self.last_index
                        .lock()
                        .await
                        .insert(tag.clone(), index.clone());
                }

                let state = ArtifactState::Ready {
                    id,
                    kind: slug.to_string(),
                    delivery: Delivery::Tree {
                        root,
                        store_prefix: artifact_store_prefix(),
                        tree_prefix: artifact_tree_prefix(),
                        summary,
                    },
                    expires_ms,
                };
                Ok((
                    state,
                    Active {
                        id,
                        cleanup: Cleanup::Tree(root.to_string()),
                        expires,
                    },
                ))
            }
        }
    }

    async fn set_failed(&self, slug: &'static str, id: Ulid, reason: &str) {
        let mut rt = self.state.lock().await;
        let kr = rt.entry(slug).or_default();
        kr.current = Some(ArtifactState::Failed {
            id,
            kind: slug.to_string(),
            reason: reason.to_string(),
        });
    }

    /// Cancel by id: abort an in-flight production, or expire a ready artifact.
    async fn cancel(&self, id: Ulid) {
        let mut rt = self.state.lock().await;
        for (slug, kr) in rt.iter_mut() {
            // In-flight: signal the producer to stop; `drive` records Failed.
            if let Some((in_id, token)) = &kr.in_flight
                && *in_id == id
            {
                token.cancel();
                return;
            }
            // Ready: expire the delivered artifact now.
            if let Some(active) = &kr.active
                && active.id == id
            {
                let cleanup = kr.active.take().map(|a| a.cleanup);
                kr.current = Some(ArtifactState::Expired {
                    id,
                    kind: slug.to_string(),
                });
                if let Some(cleanup) = cleanup {
                    drop(rt);
                    self.release(cleanup).await;
                }
                return;
            }
        }
    }

    /// Reap any artifact past its TTL.
    async fn reap_expired(&self) {
        let now = Instant::now();
        let mut to_release = Vec::new();
        {
            let mut rt = self.state.lock().await;
            for (slug, kr) in rt.iter_mut() {
                if kr.active.as_ref().is_some_and(|a| a.expires <= now) {
                    let active = kr.active.take().unwrap();
                    kr.current = Some(ArtifactState::Expired {
                        id: active.id,
                        kind: slug.to_string(),
                    });
                    to_release.push(active.cleanup);
                }
            }
        }
        for cleanup in to_release {
            self.release(cleanup).await;
        }
    }

    /// Release a delivered artifact's resources.
    async fn release(&self, cleanup: Cleanup) {
        match cleanup {
            Cleanup::TempFile { id, path } => {
                if let Some(blob) = &self.blob {
                    blob.unregister(&id.to_string()).await;
                }
                let _ = tokio::fs::remove_file(&path).await;
            }
            Cleanup::Blob { id } => {
                if let Some(blob) = &self.blob {
                    blob.unregister(&id.to_string()).await;
                }
            }
            Cleanup::Tree(tree_id) => {
                if let Some(tree_server) = &self.tree_server {
                    tree_server.unregister(&tree_id).await;
                }
                // Mark-and-sweep instead of the old wholesale clear: only the
                // released snapshot's *unshared* chunks go; chunks still
                // referenced by another registered snapshot (or a lineage
                // tag) stay warm.
                self.sweep_store().await;
            }
        }
    }

    /// Sweep the shared chunk store: keep every chunk referenced by a
    /// currently registered snapshot index (`extra_roots` — registration and
    /// tagging are not atomic, gc's documented rule), a snapshot tag, or an
    /// in-flight temp tag; remove the rest. Blocking work runs off the
    /// reactor. A channel with no tree store is a no-op.
    async fn sweep_store(&self) {
        let (Some(store), Some(tags), Some(tree_server)) =
            (&self.store, &self.tags, &self.tree_server)
        else {
            return;
        };
        let mut roots = Vec::new();
        for id in tree_server.registered().await {
            if let Some(index) = tree_server.index(id.as_str()).await {
                roots.push(index);
            }
        }
        // Lineage parents ride along too: registration, tagging and this
        // sweep are not atomic, so anything the channel still intends to
        // build from must be marked live independently of the tag file.
        roots.extend(self.last_index.lock().await.values().cloned());
        let store = store.clone();
        let tags = tags.clone();
        let temps = self.temps.clone();
        let swept = tokio::task::spawn_blocking(move || {
            gc::sweep(store.as_ref(), &tags, &temps, roots.iter())
        })
        .await;
        match swept {
            Ok(Ok(stats)) => {
                tracing::debug!(
                    removed = stats.removed,
                    kept = stats.kept,
                    "chunk-store sweep"
                )
            }
            Ok(Err(e)) => tracing::warn!(error = %e, "chunk-store sweep failed"),
            Err(e) => tracing::warn!(error = %e, "chunk-store sweep task failed"),
        }
    }

    /// A cheap handle sharing the channel's Arcs for the spawned `drive` task.
    fn clone_handle(&self) -> ArtifactChannel {
        ArtifactChannel {
            session: self.session.clone(),
            producer: self.producer.clone(),
            source_id: self.source_id.clone(),
            producers: self.producers.clone(),
            blob: self.blob.clone(),
            store: self.store.clone(),
            tags: self.tags.clone(),
            temps: self.temps.clone(),
            last_index: self.last_index.clone(),
            _tags_dir: self._tags_dir.clone(),
            tree_server: self.tree_server.clone(),
            state: self.state.clone(),
        }
    }
}

/// Metadata-only bounds check of a snapshot source tree, run BEFORE chunking
/// so an over-limit directory fails early instead of first streaming itself
/// into the chunk store. Advisory (files can change between walk and build);
/// the post-build checks on the index remain authoritative.
///
/// Symlinks are not followed (matching `build_tree`, which records them as
/// entries), so a link cannot cycle the walk or drag in an outside tree.
fn preflight_tree_bounds(
    root: &std::path::Path,
    max_bytes: u64,
    max_files: Option<u64>,
) -> anyhow::Result<()> {
    let mut total: u64 = 0;
    let mut files: u64 = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.path().symlink_metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                files += 1;
                total = total.saturating_add(meta.len());
            }
            if total > max_bytes {
                anyhow::bail!("snapshot source (>{total} bytes) exceeds max_bytes ({max_bytes})");
            }
            if let Some(max) = max_files
                && files > max
            {
                anyhow::bail!("snapshot source (>{files} files) exceeds max_files ({max})");
            }
        }
    }
    Ok(())
}

/// CDC parameters for a snapshot build, derived from the configured average
/// chunk size.
///
/// `min`/`max` scale with the average, keeping zblob's default 1:4:16 geometry
/// (16 KiB / 64 KiB / 256 KiB). Setting only `avg` and inheriting the default
/// `max` — what this code did before — made `avg == max` at the shipped
/// 256 KiB default, so FastCDC cut at `max` on nearly every chunk: fixed-size
/// chunking in disguise, with none of the shift resistance content-defined
/// chunking exists for. It also rejected any configured `chunk_size` above
/// 256 KiB outright (`CdcParams::validate` requires `avg <= max`).
fn snapshot_cdc_params(chunk_size: u32) -> CdcParams {
    CdcParams {
        min: (chunk_size / 4).max(64),
        avg: chunk_size.max(256),
        max: chunk_size.saturating_mul(4).min(16 * 1024 * 1024),
        ..CdcParams::default()
    }
}

#[cfg(test)]
mod tests {
    use super::snapshot_cdc_params;

    #[test]
    fn snapshot_cdc_params_validate_across_the_knob_range() {
        // Shipped default, the old report default, and a small-average edge.
        for chunk_size in [262_144, 524_288, 65_536, 256, 4 * 1024 * 1024] {
            let cdc = snapshot_cdc_params(chunk_size);
            cdc.validate()
                .unwrap_or_else(|e| panic!("chunk_size {chunk_size}: {e}"));
            assert!(
                cdc.avg < cdc.max,
                "chunk_size {chunk_size}: avg == max degenerates FastCDC into \
                 fixed-size chunking"
            );
        }
    }

    #[test]
    fn snapshot_cdc_params_keep_the_default_geometry() {
        // The shipped 256 KiB average maps onto 64 KiB / 256 KiB / 1 MiB —
        // zblob's default 1:4:16 ratio shifted up by the config knob.
        let cdc = snapshot_cdc_params(262_144);
        assert_eq!((cdc.min, cdc.avg, cdc.max), (65_536, 262_144, 1_048_576));
    }
}
