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
    BlobServer, BlobSpec, CancelToken, CdcParams, ContentStore, Hash, MemoryStore, TreeIndex,
    TreeServer, build_tree,
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
    /// Tier-2 chunk store + tree server (present iff a `Tree` producer is registered).
    store: Option<Arc<MemoryStore>>,
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
        let blob =
            wants_blob.then(|| BlobServer::new(&session, serve(artifact_blob_prefix(&producer))));
        let (store, tree_server) = if wants_tree {
            let store = Arc::new(MemoryStore::new());
            let tree_server = TreeServer::new(
                &session,
                serve(artifact_store_prefix(&producer)),
                serve(artifact_tree_prefix(&producer)),
                store.clone() as Arc<dyn ContentStore>,
            );
            (Some(store), Some(tree_server))
        } else {
            (None, None)
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
                        blob_prefix: artifact_blob_prefix(&self.producer),
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
            Produced::Dir { path } => {
                let store = self
                    .store
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no chunk store for a Dir artifact"))?;
                let tree_server = self
                    .tree_server
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("no tree server for a Dir artifact"))?;
                // Single live tree: drop the prior snapshot's chunks *before*
                // building, because `build_tree` streams each new chunk into
                // the store as it walks. (In 0.1 it returned the chunks and
                // the caller put them, so clearing afterwards was correct;
                // doing that here would erase the snapshot just built.)
                store.clear();

                let build_store = store.clone() as Arc<dyn ContentStore>;
                let build_id = id.to_string();
                let cdc = CdcParams {
                    avg: chunk_size,
                    ..CdcParams::default()
                };
                let index: TreeIndex = tokio::task::spawn_blocking(move || {
                    build_tree(&path, build_id, &cdc, build_store.as_ref())
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
                    .register(index)
                    .await
                    .map_err(|e| anyhow::anyhow!("registering the snapshot index: {e}"))?;

                let state = ArtifactState::Ready {
                    id,
                    kind: slug.to_string(),
                    delivery: Delivery::Tree {
                        root,
                        store_prefix: artifact_store_prefix(&self.producer),
                        tree_prefix: artifact_tree_prefix(&self.producer),
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
            Cleanup::Tree(tree_id) => {
                if let Some(store) = &self.store {
                    store.clear();
                }
                if let Some(tree_server) = &self.tree_server {
                    tree_server.unregister(&tree_id).await;
                }
            }
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
            tree_server: self.tree_server.clone(),
            state: self.state.clone(),
        }
    }
}
