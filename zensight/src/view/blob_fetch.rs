//! Operator debug-report download (`@/report`) — client state machine, the
//! request/poll/stream helpers that drive `zenoh-blob`, and the per-sensor UI.
//!
//! Mirrors the `Fetch<T>` pattern but adds the multi-phase lifecycle + a progress
//! numerator the bulk transfer needs. See `docs/LARGE-DATA-TRANSFER.md` §5.5.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::futures::Stream;
use iced::widget::{button, row, text};
use iced::{Alignment, Element};
use ulid::Ulid;
use zenoh::Session;
use zenoh_blob::{BlobClient, CancelToken, Progress, ProgressSink};
use zensight_common::report::{ReportKind, ReportRequest, ReportState, ReportStatus};
use zensight_common::{report_request_key, report_status_key};

use crate::message::Message;
use crate::view::tokens::{font, space};

/// Client-side lifecycle of one report download.
#[derive(Debug, Clone, Default)]
pub enum BlobFetch {
    /// Nothing in flight.
    #[default]
    Idle,
    /// The request was PUT; awaiting the sensor's status.
    Requesting,
    /// The sensor is building the bundle.
    Generating,
    /// Streaming chunks (`got`/`total` chunks).
    Downloading {
        /// Chunks received so far.
        got: u64,
        /// Total chunks.
        total: u64,
    },
    /// Paused by the operator; the partial is kept and can be resumed.
    Paused {
        /// Chunks received so far.
        got: u64,
        /// Total chunks.
        total: u64,
    },
    /// Verifying the whole-blob hash (done inside `zenoh-blob`) / saving.
    Verifying,
    /// Saved to `path`.
    Saved(String),
    /// Failed with a reason.
    Failed(String),
}

impl BlobFetch {
    /// Whether a download is actively running (so the (re)download button is
    /// hidden). `Paused` is *not* active — it offers Resume.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            BlobFetch::Requesting
                | BlobFetch::Generating
                | BlobFetch::Downloading { .. }
                | BlobFetch::Verifying
        )
    }

    /// Whether this state occupies the sensor card (active or paused) — used to
    /// decide whether to show the job controls vs the start button.
    pub fn is_busy(&self) -> bool {
        self.is_active() || matches!(self, BlobFetch::Paused { .. })
    }

    /// Download fraction `[0,1]`, if known.
    pub fn progress_frac(&self) -> Option<f32> {
        match self {
            BlobFetch::Downloading { got, total } | BlobFetch::Paused { got, total }
                if *total > 0 =>
            {
                Some(*got as f32 / *total as f32)
            }
            _ => None,
        }
    }

    /// A short status label for the UI.
    pub fn label(&self) -> String {
        match self {
            BlobFetch::Idle => "Idle".into(),
            BlobFetch::Requesting => "Requesting report…".into(),
            BlobFetch::Generating => "Generating bundle…".into(),
            BlobFetch::Downloading { got, total } => {
                let pct = self
                    .progress_frac()
                    .map(|f| (f * 100.0) as u32)
                    .unwrap_or(0);
                format!("Downloading {got}/{total} ({pct}%)")
            }
            BlobFetch::Paused { got, total } => format!("Paused {got}/{total}"),
            BlobFetch::Verifying => "Verifying…".into(),
            BlobFetch::Saved(p) => format!("Saved to {p}"),
            BlobFetch::Failed(e) => format!("Failed: {e}"),
        }
    }
}

/// The in-flight download's identity + controls, carried between handlers.
#[derive(Clone)]
pub struct BlobJob {
    /// Sensor key prefix, e.g. `zensight/netlink`.
    pub key_prefix: String,
    /// Report id.
    pub id: Ulid,
    /// `zenoh-blob` server prefix to download from (set once `Ready`).
    pub blob_prefix: Option<String>,
    /// Suggested save filename (set once `Ready`).
    pub filename: Option<String>,
    /// Cancellation flag for the in-flight stream (pause/cancel).
    pub cancel: CancelToken,
    /// Where the `.part` + sidecar live.
    pub dest_dir: PathBuf,
}

impl BlobJob {
    /// Start a job for `key_prefix` with a fresh report id + cancel token.
    pub fn new(key_prefix: String) -> Self {
        BlobJob {
            key_prefix,
            id: Ulid::new(),
            blob_prefix: None,
            filename: None,
            cancel: CancelToken::new(),
            dest_dir: std::env::temp_dir().join("zensight-downloads"),
        }
    }

    /// Replace the cancel token with a fresh one (on resume).
    pub fn reset_cancel(&mut self) -> CancelToken {
        self.cancel = CancelToken::new();
        self.cancel.clone()
    }
}

/// PUT a `DebugBundle` request for `id`, then poll the status queryable until the
/// report is `Ready` (returns that state) or `Failed`/`Expired`/timeout (Err).
pub async fn request_and_await_ready(
    session: Arc<Session>,
    key_prefix: String,
    id: Ulid,
) -> Result<ReportState, String> {
    let req = ReportRequest {
        id,
        kind: ReportKind::DebugBundle,
        opts: Default::default(),
    };
    let payload = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    session
        .put(report_request_key(&key_prefix), payload)
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status_key = report_status_key(&key_prefix);
    // Poll for up to ~60s (120 × 500ms).
    for _ in 0..120 {
        if let Some(state) = poll_status(&session, &status_key, id).await {
            match state {
                ReportState::Ready { .. } => return Ok(state),
                ReportState::Failed { reason, .. } => return Err(reason),
                ReportState::Expired { .. } => return Err("report expired".into()),
                ReportState::Generating { .. } => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err("timed out waiting for report".into())
}

/// GET the status queryable and return the current state iff it is for `id`.
async fn poll_status(session: &Session, status_key: &str, id: Ulid) -> Option<ReportState> {
    let replies = session.get(status_key).await.ok()?;
    let reply = replies.recv_async().await.ok()?;
    let sample = reply.result().ok()?;
    let status: ReportStatus = serde_json::from_slice(&sample.payload().to_bytes()).ok()?;
    status.current.filter(|s| s.id() == id)
}

/// Drive `BlobClient::download` to `dest_dir`, yielding [`Message::ReportProgress`]
/// as chunks arrive and a final [`Message::ReportDownloaded`].
pub fn download_stream(
    session: Arc<Session>,
    blob_prefix: String,
    id: String,
    dest_dir: PathBuf,
    cancel: CancelToken,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
        let client = BlobClient::new(session, blob_prefix, zenoh_blob::Format::Json);
        let dl = tokio::spawn(async move {
            struct Sink(tokio::sync::mpsc::UnboundedSender<Progress>);
            impl ProgressSink for Sink {
                fn emit(&self, p: Progress) {
                    let _ = self.0.send(p);
                }
            }
            let sink = Sink(tx);
            client.download_cancellable(&id, &dest_dir, &sink, &cancel).await
        });
        while let Some(p) = rx.recv().await {
            if let Progress::Chunk { received, total, .. } = p {
                yield Message::ReportProgress { got: received as u64, total: total as u64 };
            }
        }
        match dl.await {
            Ok(Ok(path)) => yield Message::ReportDownloaded(Ok(path)),
            Ok(Err(e)) => yield Message::ReportDownloaded(Err(e.to_string())),
            Err(e) => yield Message::ReportDownloaded(Err(format!("download task failed: {e}"))),
        }
    }
}

/// Render the per-sensor download control. `active_prefix` is the key prefix of
/// the one in-flight job (if any), so only the matching card shows progress.
pub fn download_section<'a>(
    blob_fetch: &BlobFetch,
    this_prefix: &str,
    active_prefix: Option<&str>,
) -> Element<'a, Message> {
    let is_this = active_prefix == Some(this_prefix);

    if is_this && blob_fetch.is_busy() {
        let mut controls = row![text(blob_fetch.label()).size(font::CAPTION)]
            .spacing(space::MD)
            .align_y(Alignment::Center);
        match blob_fetch {
            BlobFetch::Downloading { .. } => {
                controls = controls.push(
                    button(text("Pause").size(font::CAPTION)).on_press(Message::PauseDownload),
                );
            }
            BlobFetch::Paused { .. } => {
                controls = controls.push(
                    button(text("Resume").size(font::CAPTION)).on_press(Message::ResumeDownload),
                );
            }
            _ => {}
        }
        controls = controls
            .push(button(text("Cancel").size(font::CAPTION)).on_press(Message::CancelDownload));
        return controls.into();
    }

    // Idle / finished: offer a (re)download button, disabled while another card's
    // download is in flight.
    let other_busy = blob_fetch.is_busy() && !is_this;
    let mut btn = button(text("Download debug report").size(font::CAPTION));
    if !other_busy {
        btn = btn.on_press(Message::DownloadDebugReport(this_prefix.to_string()));
    }

    let mut r = row![btn].spacing(space::MD).align_y(Alignment::Center);
    if is_this && matches!(blob_fetch, BlobFetch::Saved(_) | BlobFetch::Failed(_)) {
        r = r.push(text(blob_fetch.label()).size(font::CAPTION));
    }
    r.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_states() {
        assert!(!BlobFetch::Idle.is_active());
        assert!(BlobFetch::Requesting.is_active());
        assert!(BlobFetch::Downloading { got: 1, total: 4 }.is_active());
        assert!(!BlobFetch::Saved("x".into()).is_active());
        assert!(!BlobFetch::Failed("x".into()).is_active());
        // Paused is not "active" (it offers Resume) but is "busy" (occupies card).
        let paused = BlobFetch::Paused { got: 1, total: 4 };
        assert!(!paused.is_active());
        assert!(paused.is_busy());
        assert!(BlobFetch::Downloading { got: 1, total: 4 }.is_busy());
    }

    #[test]
    fn progress_fraction() {
        assert_eq!(
            BlobFetch::Downloading { got: 2, total: 4 }.progress_frac(),
            Some(0.5)
        );
        assert_eq!(
            BlobFetch::Paused { got: 1, total: 4 }.progress_frac(),
            Some(0.25)
        );
        assert_eq!(
            BlobFetch::Downloading { got: 0, total: 0 }.progress_frac(),
            None
        );
        assert_eq!(BlobFetch::Idle.progress_frac(), None);
    }

    #[test]
    fn labels() {
        assert!(
            BlobFetch::Downloading { got: 3, total: 6 }
                .label()
                .contains("3/6")
        );
        assert!(BlobFetch::Failed("boom".into()).label().contains("boom"));
    }
}
