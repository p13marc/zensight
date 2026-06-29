//! Operator Tier-2 **directory** download (`@/snapshot` + `@/store` + `@/tree`) —
//! client state machine, the request/poll/stream helpers that drive
//! `zenoh-blob`'s `TreeClient`, and the per-sensor UI.
//!
//! Mirrors the Tier-1 [`blob_fetch`](crate::view::blob_fetch) module, but the
//! payload is a whole directory tree reconstructed into a chosen folder, with
//! progress = "which content-addressed chunks are already on disk" (so a pause or
//! restart resumes for free). See `docs/LARGE-DATA-TRANSFER.md` §5.7.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::futures::Stream;
use iced::widget::{Row, button, column, row, text};
use iced::{Alignment, Element};
use ulid::Ulid;
use zenoh::Session;
use zenoh_blob::{CancelToken, ContentStore, Format, Progress, ProgressSink, TreeClient};
use zensight_common::snapshot::{SnapshotRequest, SnapshotState, SnapshotStatus};
use zensight_common::{snapshot_request_key, snapshot_status_key};

use crate::message::Message;
use crate::view::tokens::{font, space};

/// Client-side lifecycle of one directory download.
#[derive(Debug, Clone, Default)]
pub enum DirFetch {
    /// Nothing in flight.
    #[default]
    Idle,
    /// The request was PUT; awaiting the sensor's status.
    Requesting,
    /// The sensor is walking + chunking the directory.
    Generating,
    /// Fetching chunks (`got`/`total` distinct chunks).
    Fetching {
        /// Chunks resolved so far (already-on-disk + fetched).
        got: u64,
        /// Total distinct chunks needed.
        total: u64,
    },
    /// Paused by the operator; chunks fetched so far are kept and can be resumed.
    Paused {
        /// Chunks resolved so far.
        got: u64,
        /// Total distinct chunks needed.
        total: u64,
    },
    /// Reconstructing + verifying the tree root.
    Verifying,
    /// Reconstructed into `path`.
    Saved(String),
    /// Failed with a reason.
    Failed(String),
}

impl DirFetch {
    /// Whether a download is actively running (so the download buttons are hidden).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            DirFetch::Requesting
                | DirFetch::Generating
                | DirFetch::Fetching { .. }
                | DirFetch::Verifying
        )
    }

    /// Whether this state occupies the card (active or paused).
    pub fn is_busy(&self) -> bool {
        self.is_active() || matches!(self, DirFetch::Paused { .. })
    }

    /// Download fraction `[0,1]`, if known.
    pub fn progress_frac(&self) -> Option<f32> {
        match self {
            DirFetch::Fetching { got, total } | DirFetch::Paused { got, total } if *total > 0 => {
                Some(*got as f32 / *total as f32)
            }
            _ => None,
        }
    }

    /// A short status label for the UI.
    pub fn label(&self) -> String {
        match self {
            DirFetch::Idle => "Idle".into(),
            DirFetch::Requesting => "Requesting snapshot…".into(),
            DirFetch::Generating => "Building snapshot…".into(),
            DirFetch::Fetching { got, total } => {
                let pct = self
                    .progress_frac()
                    .map(|f| (f * 100.0) as u32)
                    .unwrap_or(0);
                format!("Downloading {got}/{total} chunks ({pct}%)")
            }
            DirFetch::Paused { got, total } => format!("Paused {got}/{total}"),
            DirFetch::Verifying => "Reconstructing…".into(),
            DirFetch::Saved(p) => format!("Saved to {p}"),
            DirFetch::Failed(e) => format!("Failed: {e}"),
        }
    }
}

/// The in-flight directory download's identity + controls, carried between handlers.
#[derive(Clone)]
pub struct DirJob {
    /// Sensor key prefix, e.g. `zensight/sysinfo`.
    pub key_prefix: String,
    /// The requested directory's logical name.
    pub dir_name: String,
    /// Snapshot id.
    pub id: Ulid,
    /// The `TreeIndex` id to fetch (set once `Ready`).
    pub tree_id: Option<String>,
    /// `@/store` prefix to fetch chunks from (set once `Ready`).
    pub store_prefix: Option<String>,
    /// `@/tree` prefix to fetch the index from (set once `Ready`).
    pub tree_prefix: Option<String>,
    /// Cancellation flag for the in-flight stream (pause/cancel).
    pub cancel: CancelToken,
    /// Destination directory the tree is reconstructed into.
    pub dest_root: PathBuf,
}

impl DirJob {
    /// Start a job for `key_prefix`/`dir_name` reconstructing into `dest_root`.
    pub fn new(key_prefix: String, dir_name: String, dest_root: PathBuf) -> Self {
        DirJob {
            key_prefix,
            dir_name,
            id: Ulid::new(),
            tree_id: None,
            store_prefix: None,
            tree_prefix: None,
            cancel: CancelToken::new(),
            dest_root,
        }
    }

    /// Replace the cancel token with a fresh one (on resume).
    pub fn reset_cancel(&mut self) -> CancelToken {
        self.cancel = CancelToken::new();
        self.cancel.clone()
    }
}

/// GET the snapshot status queryable and return the advertised directory names.
pub async fn load_snapshot_dirs(session: Arc<Session>, key_prefix: String) -> Vec<String> {
    let status_key = snapshot_status_key(&key_prefix);
    let Ok(replies) = session.get(&status_key).await else {
        return Vec::new();
    };
    if let Ok(reply) = replies.recv_async().await
        && let Ok(sample) = reply.result()
        && let Ok(status) = serde_json::from_slice::<SnapshotStatus>(&sample.payload().to_bytes())
    {
        return status.dirs.into_iter().map(|d| d.name).collect();
    }
    Vec::new()
}

/// PUT a `SnapshotRequest` for `dir_name`/`id`, then poll the status queryable
/// until the snapshot is `Ready` (returns that state) or `Failed`/`Expired`/timeout.
pub async fn request_and_await_ready(
    session: Arc<Session>,
    key_prefix: String,
    dir_name: String,
    id: Ulid,
) -> Result<SnapshotState, String> {
    let req = SnapshotRequest {
        id,
        dir: dir_name,
        opts: Default::default(),
    };
    let payload = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    session
        .put(snapshot_request_key(&key_prefix), payload)
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status_key = snapshot_status_key(&key_prefix);
    // Poll for up to ~60s (120 × 500ms).
    for _ in 0..120 {
        if let Some(state) = poll_status(&session, &status_key, id).await {
            match state {
                SnapshotState::Ready { .. } => return Ok(state),
                SnapshotState::Failed { reason, .. } => return Err(reason),
                SnapshotState::Expired { .. } => return Err("snapshot expired".into()),
                SnapshotState::Generating { .. } => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("timed out waiting for snapshot".into())
}

/// GET the status queryable and return the current state iff it is for `id`.
async fn poll_status(session: &Session, status_key: &str, id: Ulid) -> Option<SnapshotState> {
    let replies = session.get(status_key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    let status: SnapshotStatus = serde_json::from_slice(&sample.payload().to_bytes()).ok()?;
    status.current.filter(|s| s.id() == id)
}

/// Drive `TreeClient::download_tree_cancellable` into `dest_root`, yielding
/// [`Message::SnapshotProgress`] as chunks arrive and a final
/// [`Message::SnapshotDownloaded`]. `store` is the local content store (the redb
/// `chunks` table), so already-present chunks are skipped and a resume is free.
#[allow(clippy::too_many_arguments)]
pub fn download_stream(
    session: Arc<Session>,
    store_prefix: String,
    tree_prefix: String,
    tree_id: String,
    dest_root: PathBuf,
    store: Arc<dyn ContentStore>,
    cancel: CancelToken,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
        let client = TreeClient::new(session, store_prefix, tree_prefix, Format::Json);
        let ret = dest_root.clone();
        let dl = tokio::spawn(async move {
            struct Sink(tokio::sync::mpsc::UnboundedSender<Progress>);
            impl ProgressSink for Sink {
                fn emit(&self, p: Progress) {
                    let _ = self.0.send(p);
                }
            }
            let sink = Sink(tx);
            client
                .download_tree_cancellable(&tree_id, &dest_root, store.as_ref(), &sink, &cancel)
                .await
                .map(|_| ret)
        });
        while let Some(p) = rx.recv().await {
            if let Progress::Chunk { received, total, .. } = p {
                yield Message::SnapshotProgress { got: received as u64, total: total as u64 };
            }
        }
        match dl.await {
            Ok(Ok(path)) => yield Message::SnapshotDownloaded(Ok(path)),
            Ok(Err(e)) => yield Message::SnapshotDownloaded(Err(e.to_string())),
            Err(e) => yield Message::SnapshotDownloaded(Err(format!("download task failed: {e}"))),
        }
    }
}

/// Render the per-sensor directory-snapshot controls. `dirs` are the advertised
/// directory names for this sensor (empty ⇒ nothing rendered). `active_prefix` is
/// the key prefix of the one in-flight job (if any), so only the matching card
/// shows progress.
pub fn dir_section<'a>(
    dir_fetch: &DirFetch,
    this_prefix: &str,
    dirs: &[String],
    active_prefix: Option<&str>,
) -> Element<'a, Message> {
    if dirs.is_empty() {
        return column![].into();
    }
    let is_this = active_prefix == Some(this_prefix);
    let header = text("Directory snapshots").size(font::CAPTION);

    // Active or paused: show the in-flight job's status + controls.
    if is_this && dir_fetch.is_busy() {
        let mut controls = row![text(dir_fetch.label()).size(font::CAPTION)]
            .spacing(space::MD)
            .align_y(Alignment::Center);
        match dir_fetch {
            DirFetch::Fetching { .. } => {
                controls = controls.push(
                    button(text("Pause").size(font::CAPTION)).on_press(Message::PauseSnapshot),
                );
            }
            DirFetch::Paused { .. } => {
                controls = controls.push(
                    button(text("Resume").size(font::CAPTION)).on_press(Message::ResumeSnapshot),
                );
            }
            _ => {}
        }
        controls = controls
            .push(button(text("Cancel").size(font::CAPTION)).on_press(Message::CancelSnapshot));
        return column![header, controls].spacing(space::XS).into();
    }

    // Idle / finished: a download button per advertised directory, disabled while
    // another card's download is in flight.
    let other_busy = dir_fetch.is_busy() && !is_this;
    let mut btns = Row::new().spacing(space::SM).align_y(Alignment::Center);
    for d in dirs {
        let mut b = button(text(format!("Download {d}")).size(font::CAPTION));
        if !other_busy {
            b = b.on_press(Message::DownloadSnapshot {
                key_prefix: this_prefix.to_string(),
                dir: d.clone(),
            });
        }
        btns = btns.push(b);
    }

    let mut col = column![header, btns].spacing(space::XS);
    if is_this && matches!(dir_fetch, DirFetch::Saved(_) | DirFetch::Failed(_)) {
        col = col.push(text(dir_fetch.label()).size(font::CAPTION));
    }
    col.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_states() {
        assert!(!DirFetch::Idle.is_active());
        assert!(DirFetch::Requesting.is_active());
        assert!(DirFetch::Fetching { got: 1, total: 4 }.is_active());
        assert!(!DirFetch::Saved("x".into()).is_active());
        let paused = DirFetch::Paused { got: 1, total: 4 };
        assert!(!paused.is_active());
        assert!(paused.is_busy());
        assert!(DirFetch::Fetching { got: 1, total: 4 }.is_busy());
    }

    #[test]
    fn progress_fraction() {
        assert_eq!(
            DirFetch::Fetching { got: 2, total: 4 }.progress_frac(),
            Some(0.5)
        );
        assert_eq!(
            DirFetch::Paused { got: 1, total: 4 }.progress_frac(),
            Some(0.25)
        );
        assert_eq!(
            DirFetch::Fetching { got: 0, total: 0 }.progress_frac(),
            None
        );
        assert_eq!(DirFetch::Idle.progress_frac(), None);
    }

    #[test]
    fn labels() {
        assert!(
            DirFetch::Fetching { got: 3, total: 6 }
                .label()
                .contains("3/6")
        );
        assert!(DirFetch::Failed("boom".into()).label().contains("boom"));
    }
}
