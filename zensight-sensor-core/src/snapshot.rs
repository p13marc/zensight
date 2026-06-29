//! Tier-2 directory-snapshot generation + serving (`@/snapshot` + `@/store` +
//! `@/tree`).
//!
//! Wires `zenoh-blob`'s content-addressed tree transfer onto the ZenSight keyspace
//! so an opting-in sensor can serve an operator-requested directory snapshot:
//!
//! - a **request** subscriber (`@/snapshot/request`) — the single authorization
//!   trigger; the operator names an *allowlisted* directory (never a raw path);
//! - a **status** queryable (`@/snapshot/status`) — the snapshot lifecycle + the
//!   advertised directory names;
//! - a **cancel** subscriber (`@/snapshot/cancel`) — free the temp chunk store
//!   early;
//! - a [`TreeServer`] serving the chunks (`@/store/<algo>/<hash>`) + index
//!   (`@/tree/<id>`).
//!
//! The directory walk + chunking runs off the capture/poll path
//! (`spawn_blocking`), is bounded (`max_bytes` / `max_files`) and rate-limited, and
//! only one snapshot is live at a time (a TTL'd in-memory chunk store).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use ulid::Ulid;
use zenoh_blob::{
    ContentStore, FastCdcChunker, Format, Hash, MemoryStore, TreeIndex, TreeServer, build_tree,
};
use zensight_common::snapshot::{
    SnapshotDirInfo, SnapshotRequest, SnapshotState, SnapshotStatus, SnapshotSummary,
};
use zensight_common::{
    SnapshotLimits, snapshot_cancel_key, snapshot_request_key, snapshot_status_key,
    snapshot_store_prefix, snapshot_tree_prefix,
};

/// Mutable runtime state shared between the channel loop and build tasks.
#[derive(Default)]
struct Runtime {
    current: Option<SnapshotState>,
    busy: bool,
    last_gen: Option<Instant>,
    /// The live snapshot: id, tree id, and TTL deadline.
    active: Option<Active>,
}

struct Active {
    id: Ulid,
    tree_id: String,
    expires: Instant,
}

/// Serves the `@/snapshot` channel for one sensor.
pub struct SnapshotChannel {
    session: Arc<zenoh::Session>,
    key_prefix: String,
    source_id: String,
    limits: SnapshotLimits,
    store: Arc<MemoryStore>,
    tree_server: TreeServer,
    state: Arc<Mutex<Runtime>>,
}

impl SnapshotChannel {
    /// Build a channel for `key_prefix` (e.g. `"zensight/sysinfo"`). `source_id`
    /// is this host's id, used to answer a request's `target_source` filter.
    pub fn new(
        session: Arc<zenoh::Session>,
        key_prefix: impl Into<String>,
        source_id: impl Into<String>,
        limits: SnapshotLimits,
    ) -> Self {
        let key_prefix = key_prefix.into();
        let store = Arc::new(MemoryStore::new());
        let tree_server = TreeServer::new(
            session.clone(),
            snapshot_store_prefix(&key_prefix),
            snapshot_tree_prefix(&key_prefix),
            Format::Json,
            store.clone() as Arc<dyn ContentStore>,
        );
        SnapshotChannel {
            session,
            key_prefix,
            source_id: source_id.into(),
            limits,
            store,
            tree_server,
            state: Arc::new(Mutex::new(Runtime::default())),
        }
    }

    /// Serve forever. Spawned as a worker by `SensorRunner::with_snapshot`.
    pub async fn run(self) {
        if let Err(e) = self.run_inner().await {
            tracing::error!(error = %e, "snapshot channel exited");
        }
    }

    async fn run_inner(&self) -> anyhow::Result<()> {
        let req_sub = self
            .session
            .declare_subscriber(snapshot_request_key(&self.key_prefix))
            .await
            .map_err(|e| anyhow::anyhow!("declare snapshot request sub: {e}"))?;
        let status_q = self
            .session
            .declare_queryable(snapshot_status_key(&self.key_prefix))
            .await
            .map_err(|e| anyhow::anyhow!("declare snapshot status queryable: {e}"))?;
        let cancel_sub = self
            .session
            .declare_subscriber(snapshot_cancel_key(&self.key_prefix))
            .await
            .map_err(|e| anyhow::anyhow!("declare snapshot cancel sub: {e}"))?;

        // Serve the chunk + index queryables on their own task.
        tokio::spawn(self.tree_server.clone().run());

        let mut ttl_tick = tokio::time::interval(Duration::from_secs(5));
        tracing::info!(prefix = %self.key_prefix, "snapshot channel ready");

        loop {
            tokio::select! {
                Ok(sample) = req_sub.recv_async() => {
                    self.handle_request(&sample.payload().to_bytes()).await;
                }
                Ok(query) = status_q.recv_async() => {
                    let status = self.status().await;
                    let payload = serde_json::to_vec(&status).unwrap_or_default();
                    if let Err(e) = query.reply(query.key_expr().clone(), payload).await {
                        tracing::warn!(error = %e, "snapshot status reply failed");
                    }
                }
                Ok(sample) = cancel_sub.recv_async() => {
                    let body = sample.payload().to_bytes();
                    if let Ok(id) = std::str::from_utf8(&body).unwrap_or("").trim().parse::<Ulid>() {
                        self.expire(id).await;
                    }
                }
                _ = ttl_tick.tick() => {
                    self.reap_expired().await;
                }
            }
        }
    }

    async fn status(&self) -> SnapshotStatus {
        let rt = self.state.lock().await;
        SnapshotStatus {
            current: rt.current.clone(),
            busy: rt.busy,
            dirs: self
                .limits
                .dir_names()
                .into_iter()
                .map(|name| SnapshotDirInfo { name })
                .collect(),
            max_bytes: self.limits.max_bytes,
            cooldown_secs: self.limits.cooldown_secs,
        }
    }

    async fn handle_request(&self, payload: &[u8]) {
        let req: SnapshotRequest = match serde_json::from_slice(payload) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "bad snapshot request");
                return;
            }
        };

        // Authorization / validation: target host, then allowlist resolution.
        if let Some(target) = &req.opts.target_source
            && target != &self.source_id
        {
            // Not for us — another host running this protocol will answer.
            return;
        }
        let Some(path) = self.limits.resolve(&req.dir).map(|p| p.to_string()) else {
            self.set_failed(req.id, &format!("unknown directory: {}", req.dir))
                .await;
            return;
        };

        {
            let mut rt = self.state.lock().await;
            if rt.busy {
                drop(rt);
                self.set_failed(req.id, "already building a snapshot").await;
                return;
            }
            if let Some(last) = rt.last_gen
                && last.elapsed() < Duration::from_secs(self.limits.cooldown_secs)
            {
                drop(rt);
                self.set_failed(req.id, "cooling down; try again shortly")
                    .await;
                return;
            }
            rt.busy = true;
            rt.current = Some(SnapshotState::Generating { id: req.id });
        }

        let limits = self.limits.clone();
        let store = self.store.clone();
        let tree_server = self.tree_server.clone();
        let state = self.state.clone();
        let key_prefix = self.key_prefix.clone();
        let id = req.id;
        tokio::spawn(async move {
            let outcome = build(id, path, &limits, &store, &tree_server, &key_prefix).await;
            let mut rt = state.lock().await;
            rt.busy = false;
            rt.last_gen = Some(Instant::now());
            match outcome {
                Ok(output) => {
                    // Replace any prior snapshot: its chunks were already dropped in
                    // `build` (single live snapshot), so just unregister its index.
                    if let Some(prev) = rt.active.take() {
                        tree_server.unregister(&prev.tree_id).await;
                    }
                    rt.active = Some(output.active);
                    rt.current = Some(output.state);
                }
                Err(e) => {
                    rt.current = Some(SnapshotState::Failed {
                        id,
                        reason: e.to_string(),
                    });
                }
            }
        });
    }

    async fn set_failed(&self, id: Ulid, reason: &str) {
        let mut rt = self.state.lock().await;
        rt.current = Some(SnapshotState::Failed {
            id,
            reason: reason.to_string(),
        });
    }

    /// Expire a specific snapshot now (cancel): drop its chunks + index.
    async fn expire(&self, id: Ulid) {
        let mut rt = self.state.lock().await;
        if let Some(active) = &rt.active
            && active.id == id
        {
            let tree_id = active.tree_id.clone();
            rt.active = None;
            self.store.clear();
            self.tree_server.unregister(&tree_id).await;
            rt.current = Some(SnapshotState::Expired { id });
        }
    }

    /// Reap the snapshot if it is past its TTL.
    async fn reap_expired(&self) {
        let expired = {
            let rt = self.state.lock().await;
            rt.active
                .as_ref()
                .filter(|a| a.expires <= Instant::now())
                .map(|a| (a.id, a.tree_id.clone()))
        };
        if let Some((id, tree_id)) = expired {
            let mut rt = self.state.lock().await;
            rt.active = None;
            self.store.clear();
            self.tree_server.unregister(&tree_id).await;
            rt.current = Some(SnapshotState::Expired { id });
        }
    }
}

/// Output of a successful build: the `Ready` state to publish + the live snapshot
/// record for TTL reaping.
struct BuildOutput {
    state: SnapshotState,
    active: Active,
}

/// Walk + chunk the directory (off-thread), bound it, populate the store, and
/// register the index. One snapshot is live at a time, so the store is cleared
/// first.
async fn build(
    id: Ulid,
    path: String,
    limits: &SnapshotLimits,
    store: &Arc<MemoryStore>,
    tree_server: &TreeServer,
    key_prefix: &str,
) -> anyhow::Result<BuildOutput> {
    let tree_id = id.to_string();
    let chunk_size = limits.chunk_size;
    let max_bytes = limits.max_bytes;
    let max_files = limits.max_files;
    let build_id = tree_id.clone();

    let (index, chunks): (TreeIndex, Vec<(Hash, Vec<u8>)>) =
        tokio::task::spawn_blocking(move || {
            let chunker = FastCdcChunker::new(chunk_size);
            build_tree(std::path::Path::new(&path), build_id, &chunker)
        })
        .await??;

    // Enforce bounds before publishing anything.
    let total_bytes = index.total_size();
    let file_count = index.file_count() as u64;
    if total_bytes > max_bytes {
        anyhow::bail!("snapshot ({total_bytes} bytes) exceeds max_bytes ({max_bytes})");
    }
    if file_count > max_files {
        anyhow::bail!("snapshot ({file_count} files) exceeds max_files ({max_files})");
    }

    // Single live snapshot: drop the prior chunks, then publish this one's.
    store.clear();
    for (h, bytes) in &chunks {
        store.put(h, bytes)?;
    }
    let summary = SnapshotSummary {
        file_count,
        total_bytes,
        root_hash_hex: index.root_hash.to_string(),
    };
    tree_server.register(index).await;

    let created_ms = chrono::Utc::now().timestamp_millis();
    let expires_ms = created_ms + (limits.ttl_secs as i64) * 1000;
    Ok(BuildOutput {
        state: SnapshotState::Ready {
            id,
            tree_id: tree_id.clone(),
            store_prefix: snapshot_store_prefix(key_prefix),
            tree_prefix: snapshot_tree_prefix(key_prefix),
            summary,
            expires_ms,
        },
        active: Active {
            id,
            tree_id,
            expires: Instant::now() + Duration::from_secs(limits.ttl_secs),
        },
    })
}
