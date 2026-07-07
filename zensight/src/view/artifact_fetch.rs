//! Operator artifact download over the unified `@/artifact` channel — the client
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
use zenoh::Session;
use zenoh_blob::{
    BlobClient, CancelToken, ContentStore, Format, Progress, ProgressSink, TreeClient,
};
use zensight_common::{
    ArtifactKind, ArtifactRequest, ArtifactState, ArtifactStatus, Delivery, KindAdvert, KindStatus,
    artifact_request_key, artifact_status_key,
};

use crate::message::Message;
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
    registry: Arc<zensight_common::PublisherRegistry>,
    key_prefix: String,
    kind: ArtifactKind,
    id: Ulid,
    target_source: Option<String>,
) -> Result<ArtifactState, String> {
    let slug = kind.slug().to_string();
    let extra_secs = match &kind {
        ArtifactKind::Capture { duration_secs, .. } => *duration_secs as u64,
        _ => 0,
    };
    // The artifact channel key is protocol-scoped, so every host running this
    // sensor sees the request; `target_source` makes exactly one produce it.
    // `None` preserves the fan-out (all hosts produce).
    let req = ArtifactRequest {
        id,
        kind,
        opts: zensight_common::ArtifactOptions { target_source },
    };
    let payload = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    registry
        .put(
            &artifact_request_key(&key_prefix),
            payload,
            zensight_common::QosClass::Command,
        )
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

/// Download an already-registered blob by id (#327: triggered captures listed on
/// `@/query/captures`). Unlike [`download_stream`] there is no request/produce
/// phase and no `Delivery` in hand — the `BlobClient` fetches the manifest by id
/// itself. Pause/resume is not offered on this path (no stored delivery); cancel
/// works through the token.
pub fn download_blob_direct(
    session: Arc<Session>,
    blob_prefix: String,
    id: String,
    dest: PathBuf,
    cancel: CancelToken,
) -> impl Stream<Item = Message> {
    async_stream::stream! {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
        let dl = tokio::spawn(async move {
            struct Sink(tokio::sync::mpsc::UnboundedSender<Progress>);
            impl ProgressSink for Sink {
                fn emit(&self, p: Progress) {
                    let _ = self.0.send(p);
                }
            }
            let sink = Sink(tx);
            let client = BlobClient::new(session, blob_prefix, Format::Json);
            client.download_cancellable(&id, &dest, &sink, &cancel).await
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
    key_prefix: &str,
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
    let kp = key_prefix.to_string();
    let max_mib = ks.max_bytes / (1024 * 1024);

    let edit = |field: CaptureField| {
        let kp = kp.clone();
        move |value: String| Message::CaptureFormEdited {
            key_prefix: kp.clone(),
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
            key_prefix: kp_c.clone(),
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
                    key_prefix: kp_d.clone(),
                    field: CaptureToggle::DecompressOnSave,
                }),
        );
    }

    let validation = form.validate(advert, ks);
    let mut submit = button(text("Start capture").size(font::CAPTION));
    if !disabled && let Ok(kind) = &validation {
        submit = submit.on_press(Message::StartArtifact {
            key_prefix: kp.clone(),
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
                            key_prefix: this_prefix.to_string(),
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
            // Empty snapshot advert / unknown kinds are hidden.
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
