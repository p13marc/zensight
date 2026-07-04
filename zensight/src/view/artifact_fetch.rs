//! Operator artifact download over the unified `@/artifact` channel — the client
//! state machine, the request/poll/stream helpers that drive `zenoh-blob`, and the
//! per-sensor UI. Subsumes the old Tier-1 debug-report (`blob_fetch`) and Tier-2
//! directory-snapshot (`dir_fetch`) modules: they were the same lifecycle with
//! different labels, so this unifies them and keys the wording off the artifact
//! kind slug. See `docs/LARGE-DATA-TRANSFER.md` and `docs/KEYSPACE.md` §3.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::futures::Stream;
use iced::widget::{Row, button, column, row, text};
use iced::{Alignment, Element};
use ulid::Ulid;
use zenoh::Session;
use zenoh_blob::{
    BlobClient, CancelToken, ContentStore, Format, Progress, ProgressSink, TreeClient,
};
use zensight_common::{
    ArtifactKind, ArtifactRequest, ArtifactState, ArtifactStatus, Delivery, KindAdvert, KindStatus,
    artifact_request_key, artifact_status_key,
};

use crate::message::Message;
use crate::view::tokens::{font, space};

/// Client-side lifecycle of one artifact download. Kind-agnostic: the wording of
/// [`ArtifactFetch::label`] varies by the artifact kind slug, not the state.
#[derive(Debug, Clone, Default)]
pub enum ArtifactFetch {
    /// Nothing in flight.
    #[default]
    Idle,
    /// The request was PUT; awaiting the sensor's status.
    Requesting,
    /// The sensor is producing the artifact. `detail` carries an optional
    /// producer-reported progress line (e.g. `"capturing 12s/30s"`).
    Generating {
        /// Optional human-readable progress line from the sensor.
        detail: Option<String>,
    },
    /// Streaming units (`got`/`total`: chunks for a blob, distinct chunks for a tree).
    Downloading {
        /// Units received so far.
        got: u64,
        /// Total units.
        total: u64,
    },
    /// Paused by the operator; the partial is kept and can be resumed.
    Paused {
        /// Units received so far.
        got: u64,
        /// Total units.
        total: u64,
    },
    /// Verifying / reconstructing (done inside `zenoh-blob`) before save.
    Verifying,
    /// Saved to `path`.
    Saved(String),
    /// Failed with a reason.
    Failed(String),
}

impl ArtifactFetch {
    /// Whether a download is actively running (so the (re)download button is
    /// hidden). `Paused` is *not* active — it offers Resume.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            ArtifactFetch::Requesting
                | ArtifactFetch::Generating { .. }
                | ArtifactFetch::Downloading { .. }
                | ArtifactFetch::Verifying
        )
    }

    /// Whether this state occupies the sensor card (active or paused) — used to
    /// decide whether to show the job controls vs the start button.
    pub fn is_busy(&self) -> bool {
        self.is_active() || matches!(self, ArtifactFetch::Paused { .. })
    }

    /// Download fraction `[0,1]`, if known.
    pub fn progress_frac(&self) -> Option<f32> {
        match self {
            ArtifactFetch::Downloading { got, total } | ArtifactFetch::Paused { got, total }
                if *total > 0 =>
            {
                Some(*got as f32 / *total as f32)
            }
            _ => None,
        }
    }

    /// A short status label for the UI, worded for the artifact `kind` slug
    /// (`report` / `snapshot` / `capture`). A blob's progress counts file chunks,
    /// a tree's counts content-addressed chunks — hence the `snapshot` wording.
    pub fn label(&self, kind: &str) -> String {
        match self {
            ArtifactFetch::Idle => "Idle".into(),
            ArtifactFetch::Requesting => match kind {
                "snapshot" => "Requesting snapshot…".into(),
                "capture" => "Requesting capture…".into(),
                _ => "Requesting report…".into(),
            },
            ArtifactFetch::Generating { detail } => {
                if let Some(d) = detail {
                    return d.clone();
                }
                match kind {
                    "snapshot" => "Building snapshot…".into(),
                    "capture" => "Capturing…".into(),
                    _ => "Generating bundle…".into(),
                }
            }
            ArtifactFetch::Downloading { got, total } => {
                let pct = self
                    .progress_frac()
                    .map(|f| (f * 100.0) as u32)
                    .unwrap_or(0);
                if kind == "snapshot" {
                    format!("Downloading {got}/{total} chunks ({pct}%)")
                } else {
                    format!("Downloading {got}/{total} ({pct}%)")
                }
            }
            ArtifactFetch::Paused { got, total } => format!("Paused {got}/{total}"),
            ArtifactFetch::Verifying => match kind {
                "snapshot" => "Reconstructing…".into(),
                _ => "Verifying…".into(),
            },
            ArtifactFetch::Saved(p) => format!("Saved to {p}"),
            ArtifactFetch::Failed(e) => format!("Failed: {e}"),
        }
    }
}

/// The in-flight download's identity + controls, carried between handlers.
#[derive(Clone)]
pub struct ArtifactJob {
    /// Sensor key prefix, e.g. `zensight/netlink`.
    pub key_prefix: String,
    /// What is being produced (its slug drives status matching + label wording).
    pub kind: ArtifactKind,
    /// Artifact id.
    pub id: Ulid,
    /// How the ready artifact is delivered (set once `Ready`).
    pub delivery: Option<Delivery>,
    /// Suggested save filename (set once `Ready`, blob deliveries only).
    pub filename: Option<String>,
    /// Cancellation flag for the in-flight stream (pause/cancel).
    pub cancel: CancelToken,
    /// Where the bytes land: a temp dir for a blob, the chosen folder for a tree.
    pub dest: PathBuf,
}

impl ArtifactJob {
    /// Start a job for `key_prefix`/`kind` with a fresh id + cancel token landing
    /// in `dest`.
    pub fn new(key_prefix: String, kind: ArtifactKind, dest: PathBuf) -> Self {
        ArtifactJob {
            key_prefix,
            kind,
            id: Ulid::new(),
            delivery: None,
            filename: None,
            cancel: CancelToken::new(),
            dest,
        }
    }

    /// Replace the cancel token with a fresh one (on resume).
    pub fn reset_cancel(&mut self) -> CancelToken {
        self.cancel = CancelToken::new();
        self.cancel.clone()
    }
}

/// GET the artifact status queryable and return every kind the sensor produces
/// (with its bounds/advert), so the GUI knows which affordances to render.
pub async fn load_artifact_kinds(session: Arc<Session>, key_prefix: String) -> Vec<KindStatus> {
    let status_key = artifact_status_key(&key_prefix);
    let Ok(replies) = session.get(&status_key).await else {
        return Vec::new();
    };
    if let Ok(reply) = replies.recv_async().await
        && let Ok(sample) = reply.result()
        && let Ok(status) = serde_json::from_slice::<ArtifactStatus>(&sample.payload().to_bytes())
    {
        return status.kinds;
    }
    Vec::new()
}

/// PUT an `ArtifactRequest` for `kind`/`id`, then poll the status queryable until
/// the artifact is `Ready` (returns that state) or `Failed`/`Expired`/timeout.
/// The timeout scales with the request: a 60s baseline plus a Capture's
/// `duration_secs`, so a long capture does not time out mid-flight.
pub async fn request_and_await_ready(
    session: Arc<Session>,
    key_prefix: String,
    kind: ArtifactKind,
    id: Ulid,
) -> Result<ArtifactState, String> {
    let slug = kind.slug().to_string();
    let extra_secs = match &kind {
        ArtifactKind::Capture { duration_secs, .. } => *duration_secs as u64,
        _ => 0,
    };
    let req = ArtifactRequest {
        id,
        kind,
        opts: Default::default(),
    };
    let payload = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    session
        .put(artifact_request_key(&key_prefix), payload)
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status_key = artifact_status_key(&key_prefix);
    // Poll every 500ms for a scaled window (2 iters/sec).
    let iters = (60 + extra_secs) * 2;
    for _ in 0..iters {
        if let Some(state) = poll_status(&session, &status_key, &slug, id).await {
            match state {
                ArtifactState::Ready { .. } => return Ok(state),
                ArtifactState::Failed { reason, .. } => return Err(reason),
                ArtifactState::Expired { .. } => return Err("artifact expired".into()),
                ArtifactState::Generating { .. } => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("timed out waiting for artifact".into())
}

/// GET the status queryable and return the current state iff it is for this
/// `slug` + `id` (a sensor lists one entry per kind it produces).
async fn poll_status(
    session: &Session,
    status_key: &str,
    slug: &str,
    id: Ulid,
) -> Option<ArtifactState> {
    let replies = session.get(status_key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    let status: ArtifactStatus = serde_json::from_slice(&sample.payload().to_bytes()).ok()?;
    status
        .kinds
        .into_iter()
        .find(|k| k.kind == slug)
        .and_then(|k| k.current)
        .filter(|s| s.id() == id)
}

/// Drive the right `zenoh-blob` client for `delivery` into `dest`, yielding
/// [`Message::ArtifactProgress`] as chunks arrive and a final
/// [`Message::ArtifactDownloaded`]. `store` (the local content store) is only used
/// by the tree arm, so already-present chunks are skipped and a resume is free.
pub fn download_stream(
    session: Arc<Session>,
    delivery: Delivery,
    id: String,
    dest: PathBuf,
    store: Arc<dyn ContentStore>,
    cancel: CancelToken,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
        let ret = dest.clone();
        let dl = tokio::spawn(async move {
            struct Sink(tokio::sync::mpsc::UnboundedSender<Progress>);
            impl ProgressSink for Sink {
                fn emit(&self, p: Progress) {
                    let _ = self.0.send(p);
                }
            }
            let sink = Sink(tx);
            match delivery {
                Delivery::Blob { blob_prefix, .. } => {
                    let client = BlobClient::new(session, blob_prefix, Format::Json);
                    client.download_cancellable(&id, &dest, &sink, &cancel).await
                }
                Delivery::Tree {
                    tree_id,
                    store_prefix,
                    tree_prefix,
                    ..
                } => {
                    let client = TreeClient::new(session, store_prefix, tree_prefix, Format::Json);
                    client
                        .download_tree_cancellable(&tree_id, &dest, store.as_ref(), &sink, &cancel)
                        .await
                        .map(|_| ret)
                }
            }
        });
        while let Some(p) = rx.recv().await {
            if let Progress::Chunk { received, total, .. } = p {
                yield Message::ArtifactProgress { got: received as u64, total: total as u64 };
            }
        }
        match dl.await {
            Ok(Ok(path)) => yield Message::ArtifactDownloaded(Ok(path)),
            Ok(Err(e)) => yield Message::ArtifactDownloaded(Err(e.to_string())),
            Err(e) => yield Message::ArtifactDownloaded(Err(format!("download task failed: {e}"))),
        }
    }
}

/// Render the per-sensor artifact controls from the sensor's advertised `kinds`.
/// `active_prefix`/`active_kind` identify the one in-flight job (if any), so only
/// the matching card shows progress + the job controls.
pub fn artifact_section<'a>(
    fetch: &ArtifactFetch,
    this_prefix: &str,
    kinds: &[KindStatus],
    active_prefix: Option<&str>,
    active_kind: Option<&str>,
) -> Element<'a, Message> {
    let is_this = active_prefix == Some(this_prefix);

    // Active or paused: show the in-flight job's status + controls, worded for the
    // job's kind.
    if is_this && fetch.is_busy() {
        let kind = active_kind.unwrap_or("report");
        let mut controls = row![text(fetch.label(kind)).size(font::CAPTION)]
            .spacing(space::MD)
            .align_y(Alignment::Center);
        match fetch {
            ArtifactFetch::Downloading { .. } => {
                controls = controls.push(
                    button(text("Pause").size(font::CAPTION)).on_press(Message::PauseArtifact),
                );
            }
            ArtifactFetch::Paused { .. } => {
                controls = controls.push(
                    button(text("Resume").size(font::CAPTION)).on_press(Message::ResumeArtifact),
                );
            }
            _ => {}
        }
        controls = controls
            .push(button(text("Cancel").size(font::CAPTION)).on_press(Message::CancelArtifact));
        return controls.into();
    }

    // Idle / finished: a request affordance per advertised kind, disabled while
    // another card's job is in flight.
    let other_busy = fetch.is_busy() && !is_this;
    let mut col = column![].spacing(space::SM);
    let mut any = false;
    for ks in kinds {
        match &ks.advert {
            KindAdvert::Report {} => {
                any = true;
                let mut btn = button(text("Download debug report").size(font::CAPTION));
                if !other_busy {
                    btn = btn.on_press(Message::StartArtifact {
                        key_prefix: this_prefix.to_string(),
                        kind: ArtifactKind::Report {},
                    });
                }
                col = col.push(btn);
            }
            KindAdvert::Snapshot { dirs } if !dirs.is_empty() => {
                any = true;
                let header = text("Directory snapshots").size(font::CAPTION);
                let mut btns = Row::new().spacing(space::SM).align_y(Alignment::Center);
                for d in dirs {
                    let mut b = button(text(format!("Download {d}")).size(font::CAPTION));
                    if !other_busy {
                        b = b.on_press(Message::StartArtifact {
                            key_prefix: this_prefix.to_string(),
                            kind: ArtifactKind::Snapshot { dir: d.clone() },
                        });
                    }
                    btns = btns.push(b);
                }
                col = col.push(column![header, btns].spacing(space::XS));
            }
            // Capture form is issue #333, not this PR; unknown kinds are hidden.
            KindAdvert::Snapshot { .. } | KindAdvert::Capture { .. } | KindAdvert::Unknown => {}
        }
    }
    if !any {
        return column![].into();
    }

    // A finished-job status line on this card.
    if is_this && matches!(fetch, ArtifactFetch::Saved(_) | ArtifactFetch::Failed(_)) {
        let kind = active_kind.unwrap_or("report");
        col = col.push(text(fetch.label(kind)).size(font::CAPTION));
    }
    col.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_states() {
        assert!(!ArtifactFetch::Idle.is_active());
        assert!(ArtifactFetch::Requesting.is_active());
        assert!(ArtifactFetch::Generating { detail: None }.is_active());
        assert!(ArtifactFetch::Downloading { got: 1, total: 4 }.is_active());
        assert!(!ArtifactFetch::Saved("x".into()).is_active());
        assert!(!ArtifactFetch::Failed("x".into()).is_active());
        // Paused is not "active" (it offers Resume) but is "busy" (occupies card).
        let paused = ArtifactFetch::Paused { got: 1, total: 4 };
        assert!(!paused.is_active());
        assert!(paused.is_busy());
        assert!(ArtifactFetch::Downloading { got: 1, total: 4 }.is_busy());
    }

    #[test]
    fn progress_fraction() {
        assert_eq!(
            ArtifactFetch::Downloading { got: 2, total: 4 }.progress_frac(),
            Some(0.5)
        );
        assert_eq!(
            ArtifactFetch::Paused { got: 1, total: 4 }.progress_frac(),
            Some(0.25)
        );
        assert_eq!(
            ArtifactFetch::Downloading { got: 0, total: 0 }.progress_frac(),
            None
        );
        assert_eq!(ArtifactFetch::Idle.progress_frac(), None);
    }

    #[test]
    fn labels_key_off_kind() {
        // The download counter is shared; only the wording keys off the kind.
        let dl = ArtifactFetch::Downloading { got: 3, total: 6 };
        assert!(dl.label("report").contains("3/6"));
        assert!(!dl.label("report").contains("chunks"));
        assert!(dl.label("snapshot").contains("3/6"));
        assert!(dl.label("snapshot").contains("chunks"));

        // The producing/requesting phases are worded per kind.
        assert_eq!(
            ArtifactFetch::Requesting.label("report"),
            "Requesting report…"
        );
        assert_eq!(
            ArtifactFetch::Requesting.label("snapshot"),
            "Requesting snapshot…"
        );
        assert_eq!(
            ArtifactFetch::Generating { detail: None }.label("snapshot"),
            "Building snapshot…"
        );
        // A producer-supplied detail line overrides the default wording.
        assert_eq!(
            ArtifactFetch::Generating {
                detail: Some("capturing 12s/30s".into())
            }
            .label("capture"),
            "capturing 12s/30s"
        );

        assert!(
            ArtifactFetch::Failed("boom".into())
                .label("report")
                .contains("boom")
        );
    }
}
