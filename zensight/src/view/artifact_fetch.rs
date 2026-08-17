//! Operator artifact download over the unified artifact channel — the client
//! state machine, the request/poll/stream helpers that drive `zenoh-blob`, and the
//! per-sensor UI. Subsumes the old Tier-1 debug-report (`blob_fetch`) and Tier-2
//! directory-snapshot (`dir_fetch`) modules: they were the same lifecycle with
//! different labels, so this unifies them and keys the wording off the artifact
//! kind slug. See `docs/design/large-data-transfer.md` and `docs/KEYSPACE.md` §3.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::futures::Stream;
use iced::widget::{Row, button, checkbox, column, row, text, text_input};
use iced::{Alignment, Element, Length};
use ulid::Ulid;
use zblob::{BlobClient, CancelToken, ContentStore, DownloadRequest, Progress, TreeClient};
use zenoh::Session;
use zensight_common::{
    ArtifactKind, ArtifactRequest, ArtifactState, ArtifactStatus, Delivery, KindAdvert, KindStatus,
    LogBundleFormat, fleet_rpc_key,
};

use crate::message::Message;
use crate::view::components::fraction_bar;
use crate::view::theme;
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
    /// The sensor is producing the artifact. `detail`/`progress` carry the
    /// producer-reported progress (e.g. `"capturing 12s/30s"`, `0.4`), streamed
    /// from the status queryable while the request poll runs.
    Generating {
        /// Optional human-readable progress line from the sensor.
        detail: Option<String>,
        /// Optional producer-reported fraction in `0.0..=1.0`.
        progress: Option<f32>,
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

    /// Progress fraction `[0,1]`, if known: the producer-reported fraction while
    /// generating (e.g. capture elapsed/duration), chunks received while
    /// downloading or paused.
    pub fn progress_frac(&self) -> Option<f32> {
        match self {
            ArtifactFetch::Generating { progress, .. } => progress.map(|f| f.clamp(0.0, 1.0)),
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
                "logbundle" => "Requesting log export…".into(),
                _ => "Requesting report…".into(),
            },
            ArtifactFetch::Generating { detail, .. } => {
                if let Some(d) = detail {
                    return d.clone();
                }
                match kind {
                    "snapshot" => "Building snapshot…".into(),
                    "capture" => "Capturing…".into(),
                    "logbundle" => "Bundling logs…".into(),
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

/// Which text field of a [`CaptureForm`] an edit targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureField {
    /// Capture duration, seconds.
    Duration,
    /// Capture filter expression.
    Filter,
    /// Byte cap, MiB.
    MaxMib,
}

/// Which boolean toggle of a [`CaptureForm`] a click targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureToggle {
    /// zstd-compress the capture before transfer.
    Compress,
    /// Decompress the saved `.pcap.zst` back to `.pcap` on save.
    DecompressOnSave,
}

/// A borrow-bundle of the app's artifact state (#351), threaded into the
/// device drill-down so contextual actions (the netring Capture tab's form)
/// render the same shared state as the Sensors-page cards — mirror, not move.
#[derive(Clone, Copy)]
pub struct ArtifactCtx<'a> {
    /// The single in-flight (or finished) transfer's state.
    pub fetch: &'a ArtifactFetch,
    /// Advertised artifact kinds per sensor key prefix (`zensight/<sensor>`).
    pub kinds: &'a std::collections::HashMap<String, Vec<KindStatus>>,
    /// The shared capture forms, one per sensor key prefix.
    pub capture_forms: &'a std::collections::HashMap<String, CaptureForm>,
    /// Key prefix of the active job, if any.
    pub active_prefix: Option<&'a str>,
    /// Kind slug of the active job, if any.
    pub active_kind: Option<&'a str>,
}

/// The operator-authored capture parameters, shared by the Sensors-page sensor
/// card and the netring Capture tab (#351) — one form per sensor key prefix,
/// held in `app.rs`, so edits mirror between the two surfaces. The numeric
/// fields are free-text so [`CaptureForm::validate`] is the single source of
/// truth — used both to render the inline error and to gate submit.
#[derive(Debug, Clone)]
pub struct CaptureForm {
    /// Capture duration, seconds (text input).
    pub duration_secs: String,
    /// Capture filter expression (text input; empty ⇒ no filter).
    pub filter: String,
    /// Byte cap in MiB (text input; empty ⇒ the sensor's configured max).
    pub max_mib: String,
    /// zstd-compress before transfer (default on).
    pub compress: bool,
    /// Decompress the saved `.pcap.zst` back to `.pcap` on save.
    pub decompress_on_save: bool,
}

impl Default for CaptureForm {
    fn default() -> Self {
        CaptureForm {
            duration_secs: "30".to_string(),
            filter: String::new(),
            max_mib: String::new(),
            compress: true,
            decompress_on_save: true,
        }
    }
}

impl CaptureForm {
    /// Validate the form against the sensor's advertised bounds + configured byte
    /// cap. `Ok(kind)` is the request to submit; `Err(reason)` disables submit and
    /// is shown inline. Clamping is defense-in-depth on the sensor; this keeps a
    /// stale advert from submitting an obviously-over-cap request.
    pub fn validate(&self, advert: &KindAdvert, ks: &KindStatus) -> Result<ArtifactKind, String> {
        let (max_duration_secs, filter_allowed) = match advert {
            KindAdvert::Capture {
                max_duration_secs,
                filter_allowed,
                ..
            } => (*max_duration_secs, *filter_allowed),
            _ => return Err("this sensor does not offer capture".into()),
        };

        let duration_secs: u32 = self
            .duration_secs
            .trim()
            .parse()
            .map_err(|_| "duration must be a whole number of seconds".to_string())?;
        if duration_secs == 0 {
            return Err("duration must be greater than 0".into());
        }
        if duration_secs > max_duration_secs {
            return Err(format!(
                "duration exceeds the sensor max ({max_duration_secs}s)"
            ));
        }

        let filter = match self.filter.trim() {
            "" => None,
            f => {
                if !filter_allowed {
                    return Err("this sensor does not allow capture filters".into());
                }
                Some(f.to_string())
            }
        };

        let max_bytes = match self.max_mib.trim() {
            "" => None,
            m => {
                let mib: u64 = m
                    .parse()
                    .map_err(|_| "size cap must be a whole number of MiB".to_string())?;
                let bytes = mib.saturating_mul(1024 * 1024);
                if bytes == 0 {
                    return Err("size cap must be greater than 0".into());
                }
                if bytes > ks.max_bytes {
                    return Err(format!(
                        "size cap exceeds the sensor max ({} MiB)",
                        ks.max_bytes / (1024 * 1024)
                    ));
                }
                Some(bytes)
            }
        };

        Ok(ArtifactKind::Capture {
            duration_secs,
            max_bytes,
            filter,
            snaplen: None,
            compress: self.compress,
        })
    }
}

/// The in-flight download's identity + controls, carried between handlers.
#[derive(Clone)]
pub struct ArtifactJob {
    /// Sensor key prefix, e.g. `zensight/netlink`.
    pub producer: String,
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
    /// Start a job for `producer`/`kind` with a fresh id + cancel token landing
    /// in `dest`.
    pub fn new(producer: String, kind: ArtifactKind, dest: PathBuf) -> Self {
        ArtifactJob {
            producer,
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
pub async fn load_artifact_kinds(session: Arc<Session>, producer: String) -> Vec<KindStatus> {
    let status_key = fleet_rpc_key(&producer, "artifact/status");
    // Fleet fan-in (RFC 05 §2.1): target All and take the first decodable
    // reply — every host advertising the producer serves the same kind set.
    let Ok(replies) = session
        .get(&status_key)
        .target(zenoh::query::QueryTarget::All)
        .await
    else {
        return Vec::new();
    };
    while let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result()
            && let Ok(status) =
                serde_json::from_slice::<ArtifactStatus>(&sample.payload().to_bytes())
        {
            return status.kinds;
        }
    }
    Vec::new()
}

/// PUT an `ArtifactRequest` for `kind`/`id`, then poll the status queryable until
/// the artifact is `Ready` or `Failed`/`Expired`/timeout — the outcome lands as a
/// final [`Message::ArtifactRequested`]. While the sensor is producing, each
/// observed `Generating` state is yielded as [`Message::ArtifactGenerating`] so
/// the UI shows the producer's own progress (a capture's `"capturing 12s/30s"`
/// line + elapsed/duration fraction) instead of sitting on "Requesting…".
/// The timeout scales with the request: a 60s baseline plus a Capture's
/// `duration_secs`, so a long capture does not time out mid-flight.
pub fn request_and_stream_ready(
    session: Arc<Session>,
    registry: Arc<zensight_common::PublisherRegistry>,
    producer: String,
    kind: ArtifactKind,
    id: Ulid,
    target_source: Option<String>,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        let slug = kind.slug().to_string();
        let extra_secs = match &kind {
            ArtifactKind::Capture { duration_secs, .. } => *duration_secs as u64,
            _ => 0,
        };
        // v1 (RFC 05 §3): the request is a write procedure — GET with a body;
        // the value reply is the ack, refusals arrive as reply errors. A
        // `*`-origin GET fans the request out to every host serving the
        // producer (`target_source` remains a legacy payload narrow).
        let req = ArtifactRequest {
            id,
            kind,
            opts: zensight_common::ArtifactOptions { target_source },
        };
        let payload = match serde_json::to_vec(&req) {
            Ok(p) => p,
            Err(e) => {
                yield Message::ArtifactRequested(Err(e.to_string()));
                return;
            }
        };
        let _ = &registry; // command registry no longer used on this path
        match session
            .get(&fleet_rpc_key(
                &producer,
                "artifact/request",
            ))
            .target(zenoh::query::QueryTarget::All)
            .payload(payload)
            .timeout(std::time::Duration::from_secs(5))
            .await
        {
            Ok(replies) => match replies.recv_async().await {
                Ok(reply) => {
                    if let Err(err) = reply.result() {
                        let msg = String::from_utf8_lossy(&err.payload().to_bytes()).to_string();
                        yield Message::ArtifactRequested(Err(format!("request refused: {msg}")));
                        return;
                    }
                }
                Err(_) => {
                    yield Message::ArtifactRequested(Err(
                        "request unanswered — sensor offline or artifacts disabled".to_string(),
                    ));
                    return;
                }
            },
            Err(e) => {
                yield Message::ArtifactRequested(Err(format!("request failed: {e}")));
                return;
            }
        }

        let status_key = fleet_rpc_key(
            &producer,
            "artifact/status",
        );
        // Poll every 500ms for a scaled window (2 iters/sec).
        let iters = (60 + extra_secs) * 2;
        for _ in 0..iters {
            if let Some(state) = poll_status(&session, &status_key, &slug, id).await {
                match state {
                    ArtifactState::Ready { .. } => {
                        yield Message::ArtifactRequested(Ok(state));
                        return;
                    }
                    ArtifactState::Failed { reason, .. } => {
                        yield Message::ArtifactRequested(Err(reason));
                        return;
                    }
                    ArtifactState::Expired { .. } => {
                        yield Message::ArtifactRequested(Err("artifact expired".into()));
                        return;
                    }
                    ArtifactState::Generating { detail, progress, .. } => {
                        yield Message::ArtifactGenerating { detail, progress };
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        yield Message::ArtifactRequested(Err("timed out waiting for artifact".into()));
    }
}

/// GET the status queryable and return the current state iff it is for this
/// `slug` + `id` (a sensor lists one entry per kind it produces).
async fn poll_status(
    session: &Session,
    status_key: &str,
    slug: &str,
    id: Ulid,
) -> Option<ArtifactState> {
    // Fleet fan-in (RFC 05 §2.1): every host serving the producer replies,
    // but only the one that accepted `id` carries its state — scan them all
    // instead of trusting whichever host answered first.
    let replies = session
        .get(status_key)
        .target(zenoh::query::QueryTarget::All)
        .await
        .ok()?;
    while let Ok(reply) = replies.recv_async().await {
        let Ok(sample) = reply.result() else { continue };
        let Ok(status) = serde_json::from_slice::<ArtifactStatus>(&sample.payload().to_bytes())
        else {
            continue;
        };
        if let Some(state) = status
            .kinds
            .into_iter()
            .find(|k| k.kind == slug)
            .and_then(|k| k.current)
            .filter(|s| s.id() == id)
        {
            return Some(state);
        }
    }
    None
}

/// Drive the right `zblob` client for `delivery` into `dest`, yielding
/// [`Message::ArtifactProgress`] as chunks arrive and a final
/// [`Message::ArtifactDownloaded`] carrying the produced path. `store` (the
/// local content store) is only used by the tree arm, so already-present
/// chunks are skipped and a resume is free.
///
/// `dest` is the **staging directory** for a blob (`download_staged` stages
/// under the blob id — the caller chooses where bytes land, and the manifest's
/// advisory filename is only offered later, in the Save-as dialog) and the
/// **destination root directory** for a tree.
///
/// Both arms are **anchored**: the blob is pinned to `manifest.root` and the
/// tree is fetched by root, so a wrong or tampered reply is rejected before it
/// reaches disk rather than assembled and detected afterwards (RFC 07 §2.1).
/// The id comes out of `delivery` rather than being passed alongside it, so an
/// id and an anchor that disagree are not expressible here.
pub fn download_stream(
    session: Arc<Session>,
    delivery: Delivery,
    dest: PathBuf,
    store: Arc<dyn ContentStore>,
    cancel: CancelToken,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        // The crate's own bounded, event-dropping progress adapter: a Progress
        // carries absolute counts, so a dropped event costs one repaint, never
        // a wrong total — and a slow repaint can no longer queue unboundedly
        // against a fast transfer.
        let (sink, mut rx) = zblob::progress_channel(64);
        let dl: tokio::task::JoinHandle<zblob::Result<PathBuf>> = tokio::spawn(async move {
            // A delivery's prefixes arrive **off the network** (the sensor's
            // `ArtifactStatus`), and `QueryPrefix` deliberately admits
            // single-segment wildcards because probing needs them — so
            // concreteness is checked here, where a fetch begins. A
            // wildcard-origin *fetch* is RFC 07 §3's amplification: every
            // matching holder ships the full payload and nothing cancels the
            // replies in flight.
            let concrete = |prefix: String| -> zblob::Result<zblob::QueryPrefix> {
                let p = zblob::QueryPrefix::new(prefix)?;
                if !p.is_concrete() {
                    return Err(zblob::BlobError::Usage(format!(
                        "refusing a wildcard-origin bulk fetch on {:?} (RFC 07 §3)",
                        p.as_str()
                    )));
                }
                Ok(p)
            };
            match delivery {
                Delivery::Blob { manifest, blob_prefix } => {
                    let client = BlobClient::new(&session, concrete(blob_prefix)?);
                    let req = DownloadRequest::pinned(manifest.id.to_string(), manifest.root);
                    // `Staged.suggested` is deliberately discarded: the job
                    // already holds the manifest's (sanitized) filename off
                    // the sensor's ArtifactStatus for the Save-as dialog.
                    let staged = client
                        .download_staged(&req, &dest)
                        .progress(&sink)
                        .cancel(&cancel)
                        .await?;
                    Ok(staged.path)
                }
                Delivery::Tree {
                    root,
                    store_prefix,
                    tree_prefix,
                    ..
                } => {
                    let client = TreeClient::new(
                        &session,
                        concrete(store_prefix)?,
                        concrete(tree_prefix)?,
                    );
                    let req = DownloadRequest::by_root(root);
                    client
                        .download_tree(&req, &dest, &store)
                        .progress(&sink)
                        .cancel(&cancel)
                        .await?;
                    Ok(dest)
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

/// Download an already-registered blob by id (#327: triggered captures listed on
/// `@rpc/netring/captures`). Unlike [`download_stream`] there is no request/produce
/// phase and no `Delivery` in hand. Pause/resume is not offered on this path (no
/// stored delivery); cancel works through the token.
///
/// `blob_prefix` MUST name a **concrete** origin. RFC 07 §3 forbids a
/// wildcard-origin bulk fetch: every matching holder ships the full payload
/// and Zenoh cannot cancel remote replies in flight, so N holders cost N× the
/// bytes on exactly the links the plane exists to spare. Through 0.10 this
/// function was handed `v1/*/@blob/artifact` and the amplification was held
/// down only by artifact ids happening to be unique ULIDs — a cost bound
/// resting on id collisions rather than on the protocol. The origin now rides
/// on [`CaptureRecord::artifact_prefix`](zensight_common::query_detail::CaptureRecord).
///
/// `root`, when known, pins the transfer (RFC 07 §2.1). It is optional only
/// because a record served by a pre-0.11 sensor carries no root; that case is
/// trust-on-first-use and is logged as such rather than silently accepted.
///
/// `dir` is the **staging directory** — `download_staged` stages under the
/// blob id, so the caller chooses where bytes land and the record's filename
/// stays advisory until the Save-as dialog.
pub fn download_blob_direct(
    session: Arc<Session>,
    blob_prefix: String,
    id: String,
    root: Option<zblob::Hash>,
    dir: PathBuf,
    cancel: CancelToken,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        // The typed prefix replaces the old `contains('*')` string hygiene:
        // `QueryPrefix` validates the shape, and `is_concrete()` is the RFC 07
        // §3 question — a fetch fans out to every matching holder, so it must
        // name exactly one.
        let prefix = match zblob::QueryPrefix::new(blob_prefix.clone()) {
            Ok(p) if p.is_concrete() => p,
            Ok(_) => {
                yield Message::ArtifactDownloaded(Err(format!(
                    "refusing a wildcard-origin bulk fetch on {blob_prefix:?} (RFC 07 §3)"
                )));
                return;
            }
            Err(e) => {
                yield Message::ArtifactDownloaded(Err(format!(
                    "`{blob_prefix}` is not a fetchable prefix: {e}"
                )));
                return;
            }
        };
        if root.is_none() {
            tracing::warn!(
                id = %id,
                "capture download is unanchored (sensor served no content root) — \
                 trust-on-first-use, see RFC 07 §2.1"
            );
        }
        // Same bounded, event-dropping adapter as `download_stream`.
        let (sink, mut rx) = zblob::progress_channel(64);
        let dl: tokio::task::JoinHandle<zblob::Result<PathBuf>> = tokio::spawn(async move {
            let client = BlobClient::new(&session, prefix);
            let req = match root {
                Some(r) => DownloadRequest::pinned(id, r),
                None => DownloadRequest::new(id),
            };
            // `Staged.suggested` is deliberately discarded: this path's
            // Save-as name comes off the CaptureRecord that listed the blob.
            let staged = client
                .download_staged(&req, &dir)
                .progress(&sink)
                .cancel(&cancel)
                .await?;
            Ok(staged.path)
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

fn caption_danger(theme: &iced::Theme) -> iced::widget::text::Style {
    iced::widget::text::Style {
        color: Some(theme::colors(theme).danger()),
    }
}

/// The on-demand capture request form (#333) — one per sensor key prefix,
/// shared since #351 by the Sensors-page sensor card and the netring Capture
/// tab (both render through [`artifact_section`]). `disabled` gates submit
/// while another card's job is in flight; `validate` gates it against the
/// advert.
pub fn capture_form_view<'a>(
    form: &CaptureForm,
    producer: &str,
    target_source: Option<&str>,
    advert: &KindAdvert,
    ks: &KindStatus,
    disabled: bool,
) -> Element<'a, Message> {
    let (max_dur, filter_allowed) = match advert {
        KindAdvert::Capture {
            max_duration_secs,
            filter_allowed,
            ..
        } => (*max_duration_secs, *filter_allowed),
        _ => (0, false),
    };
    let kp = producer.to_string();
    let max_mib = ks.max_bytes / (1024 * 1024);

    let edit = |field: CaptureField| {
        let kp = kp.clone();
        move |value: String| Message::CaptureFormEdited {
            producer: kp.clone(),
            field,
            value,
        }
    };

    let duration = text_input(&format!("≤ {max_dur}s"), &form.duration_secs)
        .on_input(edit(CaptureField::Duration))
        .size(font::CAPTION)
        .width(Length::Fixed(80.0));
    let size = text_input(&format!("≤ {max_mib} MiB"), &form.max_mib)
        .on_input(edit(CaptureField::MaxMib))
        .size(font::CAPTION)
        .width(Length::Fixed(90.0));

    let mut fields = row![
        text("Duration (s)").size(font::CAPTION),
        duration,
        text("Max (MiB)").size(font::CAPTION),
        size,
    ]
    .spacing(space::SM)
    .align_y(Alignment::Center);
    if filter_allowed {
        fields = fields.push(
            text_input("filter (e.g. udp and port 53)", &form.filter)
                .on_input(edit(CaptureField::Filter))
                .size(font::CAPTION)
                .width(Length::Fixed(220.0)),
        );
    }

    let kp_c = kp.clone();
    let compress = checkbox(form.compress)
        .label("Compress (zstd)")
        .text_size(font::CAPTION)
        .on_toggle(move |_| Message::CaptureFormToggled {
            producer: kp_c.clone(),
            field: CaptureToggle::Compress,
        });
    let mut toggles = row![compress].spacing(space::MD).align_y(Alignment::Center);
    if form.compress {
        let kp_d = kp.clone();
        toggles = toggles.push(
            checkbox(form.decompress_on_save)
                .label("Decompress on save")
                .text_size(font::CAPTION)
                .on_toggle(move |_| Message::CaptureFormToggled {
                    producer: kp_d.clone(),
                    field: CaptureToggle::DecompressOnSave,
                }),
        );
    }

    let validation = form.validate(advert, ks);
    let mut submit = button(text("Start capture").size(font::CAPTION));
    if !disabled && let Ok(kind) = &validation {
        submit = submit.on_press(Message::StartArtifact {
            producer: kp.clone(),
            kind: kind.clone(),
            target_source: target_source.map(str::to_string),
        });
    }

    let mut col = column![
        text("On-demand packet capture").size(font::CAPTION),
        fields,
        toggles,
        submit,
    ]
    .spacing(space::XS);
    if let Err(e) = &validation {
        col = col.push(text(e.clone()).size(font::CAPTION).style(caption_danger));
    }
    col.into()
}

/// Render the per-sensor artifact controls from the sensor's advertised `kinds`.
/// `active_prefix`/`active_kind` identify the one in-flight job (if any), so only
/// the matching card shows progress + the job controls. `capture_form` supplies
/// the shared capture form for the `Capture` kind (None hides it).
/// `target_source` restricts the request to one sensor instance
/// (`ArtifactRequest.opts.target_source`); `None` fans out to every host
/// running this protocol.
#[allow(clippy::too_many_arguments)]
pub fn artifact_section<'a>(
    fetch: &ArtifactFetch,
    this_prefix: &str,
    target_source: Option<&str>,
    kinds: &[KindStatus],
    active_prefix: Option<&str>,
    active_kind: Option<&str>,
    capture_form: Option<&CaptureForm>,
) -> Element<'a, Message> {
    let is_this = active_prefix == Some(this_prefix);

    // Active or paused: show the in-flight job's status + controls, worded for the
    // job's kind, with a progress bar whenever a fraction is known (producer
    // progress while generating, chunk counts while downloading/paused).
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
        let mut job = column![controls].spacing(space::XS);
        if let Some(frac) = fetch.progress_frac() {
            job = job.push(fraction_bar(frac));
        }
        return job.into();
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
                        producer: this_prefix.to_string(),
                        kind: ArtifactKind::Report {},
                        target_source: target_source.map(str::to_string),
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
                            producer: this_prefix.to_string(),
                            kind: ArtifactKind::Snapshot { dir: d.clone() },
                            target_source: target_source.map(str::to_string),
                        });
                    }
                    btns = btns.push(b);
                }
                col = col.push(column![header, btns].spacing(space::XS));
            }
            KindAdvert::Capture { .. } => {
                if let Some(form) = capture_form {
                    any = true;
                    col = col.push(capture_form_view(
                        form,
                        this_prefix,
                        target_source,
                        &ks.advert,
                        ks,
                        other_busy,
                    ));
                }
            }
            // A whole-store log export from the sensor's own card (#555). The
            // filter-prefilled variant driven from the Logs feed (#554) rides on
            // top of this same request path; here the selectors are left empty,
            // so the sensor exports everything it holds, clamped to its
            // configured `logbundle` line/byte caps.
            KindAdvert::LogBundle { max_lines } => {
                any = true;
                let label = if *max_lines > 0 {
                    format!("Export logs (zstd, ≤{max_lines} lines)")
                } else {
                    "Export logs (zstd)".to_string()
                };
                let mut btn = button(text(label).size(font::CAPTION));
                if !other_busy {
                    btn = btn.on_press(Message::StartArtifact {
                        producer: this_prefix.to_string(),
                        kind: ArtifactKind::LogBundle {
                            from: None,
                            to: None,
                            pattern: None,
                            severity_min: None,
                            unit: None,
                            app: None,
                            source: target_source.map(str::to_string),
                            format: LogBundleFormat::default(),
                        },
                        target_source: target_source.map(str::to_string),
                    });
                }
                col = col.push(btn);
            }
            // Empty snapshot advert / unknown kinds are hidden here.
            KindAdvert::Snapshot { .. } | KindAdvert::Unknown => {}
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

    fn capture_ks(max_duration_secs: u32, filter_allowed: bool, max_mib: u64) -> KindStatus {
        KindStatus {
            kind: "capture".into(),
            busy: false,
            current: None,
            max_bytes: max_mib * 1024 * 1024,
            cooldown_secs: 60,
            advert: KindAdvert::Capture {
                max_duration_secs,
                filter_allowed,
                snaplen_max: 0,
            },
        }
    }

    #[test]
    fn capture_form_validate_ok() {
        let ks = capture_ks(300, true, 256);
        let form = CaptureForm {
            duration_secs: "30".into(),
            filter: "udp and port 53".into(),
            max_mib: "64".into(),
            compress: true,
            decompress_on_save: true,
        };
        let kind = form.validate(&ks.advert, &ks).unwrap();
        assert_eq!(
            kind,
            ArtifactKind::Capture {
                duration_secs: 30,
                max_bytes: Some(64 * 1024 * 1024),
                filter: Some("udp and port 53".into()),
                snaplen: None,
                compress: true,
            }
        );
    }

    #[test]
    fn capture_form_validate_rejects() {
        let ks = capture_ks(300, true, 256);
        let with_duration = |d: &str| CaptureForm {
            duration_secs: d.into(),
            ..CaptureForm::default()
        };
        // non-numeric duration
        assert!(with_duration("abc").validate(&ks.advert, &ks).is_err());
        // over-cap duration
        assert!(
            with_duration("9999")
                .validate(&ks.advert, &ks)
                .unwrap_err()
                .contains("exceeds the sensor max")
        );
        // zero duration
        assert!(with_duration("0").validate(&ks.advert, &ks).is_err());
        // over-cap size
        let big_size = CaptureForm {
            max_mib: "1024".into(),
            ..CaptureForm::default()
        };
        assert!(
            big_size
                .validate(&ks.advert, &ks)
                .unwrap_err()
                .contains("size cap exceeds")
        );
        // filter when disallowed
        let ks_nf = capture_ks(300, false, 256);
        let filtered = CaptureForm {
            filter: "udp".into(),
            ..CaptureForm::default()
        };
        assert!(
            filtered
                .validate(&ks_nf.advert, &ks_nf)
                .unwrap_err()
                .contains("does not allow")
        );
    }

    #[test]
    fn capture_form_empty_optional_fields() {
        let ks = capture_ks(300, true, 256);
        let form = CaptureForm {
            duration_secs: "10".into(),
            filter: "  ".into(), // whitespace ⇒ no filter
            max_mib: "".into(),  // empty ⇒ sensor default
            compress: false,
            decompress_on_save: false,
        };
        let kind = form.validate(&ks.advert, &ks).unwrap();
        assert_eq!(
            kind,
            ArtifactKind::Capture {
                duration_secs: 10,
                max_bytes: None,
                filter: None,
                snaplen: None,
                compress: false,
            }
        );
    }

    #[test]
    fn active_states() {
        assert!(!ArtifactFetch::Idle.is_active());
        assert!(ArtifactFetch::Requesting.is_active());
        assert!(
            ArtifactFetch::Generating {
                detail: None,
                progress: None
            }
            .is_active()
        );
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
        // Producer-reported fraction while generating, clamped to [0,1].
        assert_eq!(
            ArtifactFetch::Generating {
                detail: None,
                progress: Some(0.4)
            }
            .progress_frac(),
            Some(0.4)
        );
        assert_eq!(
            ArtifactFetch::Generating {
                detail: None,
                progress: Some(1.5)
            }
            .progress_frac(),
            Some(1.0)
        );
        assert_eq!(
            ArtifactFetch::Generating {
                detail: None,
                progress: None
            }
            .progress_frac(),
            None
        );
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
            ArtifactFetch::Generating {
                detail: None,
                progress: None
            }
            .label("snapshot"),
            "Building snapshot…"
        );
        // A producer-supplied detail line overrides the default wording.
        assert_eq!(
            ArtifactFetch::Generating {
                detail: Some("capturing 12s/30s".into()),
                progress: Some(0.4),
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
