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
    /// Several hosts produced their own artifact under the shared request id
    /// — the operator picks which host's to download. Cancel is offered; the
    /// unpicked hosts' artifacts simply age out by TTL.
    PickingHolder {
        /// One entry per producing origin.
        holders: Vec<ArtifactHolder>,
    },
    /// A tree artifact was verified pre-download (root-fetched index + holder
    /// probe): show what it actually contains and who still serves it, then
    /// let the operator pick a folder — before committing to a potentially
    /// multi-gigabyte fetch, not after.
    ConfirmingTree {
        /// The verified summary.
        verify: TreeVerify,
    },
    /// Verifying / reconstructing (done inside `zenoh-blob`) before save.
    Verifying,
    /// Saved to `path`, with the producer's caveat about what the artifact
    /// left out when there is one (#602) — a truncated bundle says so here,
    /// not only inside itself.
    Saved { path: String, note: Option<String> },
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
                | ArtifactFetch::PickingHolder { .. }
                | ArtifactFetch::ConfirmingTree { .. }
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
            ArtifactFetch::PickingHolder { holders } => {
                format!(
                    "{} hosts produced this artifact — choose one",
                    holders.len()
                )
            }
            ArtifactFetch::ConfirmingTree { verify } => format!(
                "Verified snapshot: {} files, {}",
                verify.file_count,
                crate::view::formatting::format_bytes(verify.total_bytes as f64)
            ),
            ArtifactFetch::Verifying => match kind {
                "snapshot" => "Reconstructing…".into(),
                _ => "Verifying…".into(),
            },
            ArtifactFetch::Saved { path, note } => match note {
                Some(note) => format!("Saved to {path} — {note}"),
                None => format!("Saved to {path}"),
            },
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
    /// Producer caveat about what the artifact had to leave out (#602), set
    /// once `Ready` — e.g. a truncated log bundle. Shown with the result, so
    /// the operator learns it before opening the file rather than after.
    pub note: Option<String>,
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
            note: None,
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
        // Poll every 500ms for a scaled window (2 iters/sec). The request
        // fanned out under one shared ULID, so EVERY host running the
        // producer may be building its own artifact for it — a round's
        // outcome is decided over all of their states (`poll_round_outcome`),
        // not whichever host answered first.
        let iters = (60 + extra_secs) * 2;
        for _ in 0..iters {
            let states = poll_status_all(&session, &status_key, &slug, id).await;
            match poll_round_outcome(states) {
                RoundOutcome::Ready(ready) => {
                    yield Message::ArtifactRequested(Ok(ready));
                    return;
                }
                RoundOutcome::Generating { detail, progress } => {
                    yield Message::ArtifactGenerating { detail, progress };
                }
                RoundOutcome::Failed(reason) => {
                    yield Message::ArtifactRequested(Err(reason));
                    return;
                }
                RoundOutcome::Pending => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        yield Message::ArtifactRequested(Err("timed out waiting for artifact".into()));
    }
}

/// The decision a status-poll round yields over every host's state.
enum RoundOutcome {
    /// At least one host is Ready and none is still Generating — done.
    Ready(Vec<ArtifactState>),
    /// Someone is still producing (the first one's progress is surfaced);
    /// keep polling — a slower host is not dropped because a fast one
    /// finished.
    Generating {
        /// Producer-reported progress line.
        detail: Option<String>,
        /// Producer-reported fraction.
        progress: Option<f32>,
    },
    /// Every host that answered failed or expired — done, with the first
    /// host's reason.
    Failed(String),
    /// No host has answered for this id yet.
    Pending,
}

/// Decide a poll round: Ready-and-quiet completes with every Ready state;
/// any Generating keeps waiting; a Failed/Expired counts only when no host
/// is Ready or producing (one host's refusal must not mask another's
/// artifact).
fn poll_round_outcome(states: Vec<ArtifactState>) -> RoundOutcome {
    if let Some(g) = states.iter().find_map(|s| match s {
        ArtifactState::Generating {
            detail, progress, ..
        } => Some((detail.clone(), *progress)),
        _ => None,
    }) {
        return RoundOutcome::Generating {
            detail: g.0,
            progress: g.1,
        };
    }
    let ready: Vec<ArtifactState> = states
        .iter()
        .filter(|s| matches!(s, ArtifactState::Ready { .. }))
        .cloned()
        .collect();
    if !ready.is_empty() {
        return RoundOutcome::Ready(ready);
    }
    for s in states {
        match s {
            ArtifactState::Failed { reason, .. } => return RoundOutcome::Failed(reason),
            ArtifactState::Expired { .. } => {
                return RoundOutcome::Failed("artifact expired".into());
            }
            _ => {}
        }
    }
    RoundOutcome::Pending
}

/// The verified pre-download summary of a tree artifact — built from the
/// root-fetched index, never from the sensor's self-reported `TreeSummary`
/// (which is display-only by its own contract).
#[derive(Debug, Clone)]
pub struct TreeVerify {
    /// Files in the snapshot (from the verified index).
    pub file_count: u64,
    /// Total uncompressed bytes.
    pub total_bytes: u64,
    /// Distinct content-addressed chunks — the download's true unit count,
    /// the same unit `Progress::Chunk` reports in.
    pub distinct_chunks: u64,
    /// The largest few entries (path, bytes), for the confirm card.
    pub largest: Vec<(String, u64)>,
    /// Who still serves the snapshot.
    pub source: TreeSource,
}

/// Who answered for a verified snapshot.
#[derive(Debug, Clone)]
pub enum TreeSource {
    /// The producing sensor is live (it answered the `have` probe).
    Producer {
        /// The answering origin's probe prefix.
        origin: String,
        /// Distinct chunks it holds.
        present: u32,
        /// Distinct chunks the snapshot references.
        total: u32,
    },
    /// No live server answered the probe, but the index was still fetchable —
    /// a router storage (or another replica) holds the snapshot. Storages
    /// never answer `have` (they serve only stored PUT keys), so this verdict
    /// is detected by the index fetch succeeding where the probe was silent.
    RouterReplica,
}

/// Verify a tree artifact before committing to the fetch: probe who serves it
/// (`<root>/have` — four numbers whatever the snapshot's size) and fetch the
/// root-verified index (no chunk store needed). Both go to the delivery's own
/// concrete prefixes; the index is pinned by construction, so the summary is
/// trustworthy whoever answered.
pub async fn verify_tree(
    session: Arc<Session>,
    root: zblob::Hash,
    store_prefix: String,
    tree_prefix: String,
) -> Result<TreeVerify, String> {
    let concrete = |prefix: String| -> Result<zblob::QueryPrefix, String> {
        let p = zblob::QueryPrefix::new(prefix).map_err(|e| e.to_string())?;
        if !p.is_concrete() {
            return Err("refusing a wildcard-origin fetch prefix (RFC 07 §3)".into());
        }
        Ok(p)
    };
    let client = TreeClient::new(&session, concrete(store_prefix)?, concrete(tree_prefix)?);

    let probes = client
        .probe_snapshot(&root.to_string())
        .await
        .unwrap_or_default();
    let index = match client.fetch_index_by_root(&root).await {
        Ok(index) => index,
        Err(e) if probes.is_empty() => {
            return Err(format!(
                "snapshot unreachable — producer offline and no replica answered ({e})"
            ));
        }
        Err(e) => return Err(format!("snapshot index fetch failed: {e}")),
    };

    let mut largest: Vec<(String, u64)> = index
        .files()
        .map(|(path, size, _)| (path.to_string(), size))
        .collect();
    largest.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    largest.truncate(5);

    let source = probes
        .iter()
        .find(|(_, p)| p.have_index)
        .or_else(|| probes.first())
        .map(|(origin, p)| TreeSource::Producer {
            origin: origin.as_str().to_string(),
            present: p.chunks_present,
            total: p.chunks_total,
        })
        .unwrap_or(TreeSource::RouterReplica);

    Ok(TreeVerify {
        file_count: index.file_count() as u64,
        total_bytes: index.total_size(),
        distinct_chunks: index.needed_chunks().len() as u64,
        largest,
        source,
    })
}

/// One host's Ready artifact for the shared request id — the pick-a-holder
/// unit when several hosts produced under the same ULID.
#[derive(Debug, Clone)]
pub struct ArtifactHolder {
    /// The producing host's origin chunk (`h-…`), read off the delivery's
    /// concrete blob/tree prefix.
    pub origin: String,
    /// That host's full Ready state (delivery, expiry).
    pub state: ArtifactState,
}

/// Reduce a Ready set to one holder per origin. The origin comes from the
/// delivery's own concrete prefix — for trees every host has a *different
/// root* (each built its own snapshot), so holders are alternatives to pick
/// between, never stripes of one transfer. A malformed prefix is skipped: a
/// holder the download path would refuse anyway is not worth offering.
pub fn holders_from_states(states: Vec<ArtifactState>) -> Vec<ArtifactHolder> {
    let mut out: Vec<ArtifactHolder> = Vec::new();
    for state in states {
        let ArtifactState::Ready { delivery, .. } = &state else {
            continue;
        };
        let prefix = match delivery {
            Delivery::Blob { blob_prefix, .. } => blob_prefix,
            Delivery::Tree { tree_prefix, .. } => tree_prefix,
        };
        let Some(origin) = prefix
            .split('/')
            .nth(1)
            .filter(|c| zenkey::origin::RemoteOrigin::parse(c).is_ok())
            .map(str::to_string)
        else {
            continue;
        };
        if out.iter().any(|h| h.origin == origin) {
            continue;
        }
        out.push(ArtifactHolder { origin, state });
    }
    out
}

/// GET the status queryable and collect EVERY host's current state for this
/// `slug` + `id` (a sensor lists one entry per kind it produces).
///
/// Fleet fan-in (RFC 05 §2.1): every host serving the producer replies, and
/// every host may have accepted the shared request id — so all matching
/// states come back, not the first. First-reply-wins silently dropped every
/// other host's artifact.
async fn poll_status_all(
    session: &Session,
    status_key: &str,
    slug: &str,
    id: Ulid,
) -> Vec<ArtifactState> {
    let mut out = Vec::new();
    let Ok(replies) = session
        .get(status_key)
        .target(zenoh::query::QueryTarget::All)
        .await
    else {
        return out;
    };
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
            out.push(state);
        }
    }
    out
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
    temps: Arc<zblob::gc::TempTags>,
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
                    // `temp_tags` is the sweep contract's other half: the
                    // client registers fetched-but-unmaterialized chunks so a
                    // concurrent chunk-cache sweep cannot collect them
                    // mid-transfer (they look exactly like garbage until the
                    // tree materializes).
                    let client = TreeClient::builder(
                        &session,
                        concrete(store_prefix)?,
                        concrete(tree_prefix)?,
                    )
                    .temp_tags(temps)
                    .build();
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
            match p {
                Progress::Chunk { received, total, .. }
                // A resume seeds the bar at the recovered count (#624) —
                // without this a warm restart renders like a cold start until
                // the first fresh chunk (and never moves if zero chunks
                // remain).
                | Progress::Resumed { received, total } => {
                    yield Message::ArtifactProgress { got: received as u64, total: total as u64 };
                }
                // Tree downloads verify + materialize after the last chunk
                // (zblob emits this before `reconstruct_tree`); blob
                // downloads verify as they write and never emit it.
                Progress::Verifying => yield Message::ArtifactVerifying,
                // `Progress` is #[non_exhaustive].
                _ => {}
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
            //
            // `Overwrite::Replace` (#624): this path stages under the
            // *sensor's* artifact id, so a crash between download-complete
            // and Save-as leaves a completed staged file that would trip
            // `DestinationExists` on every re-download of that capture,
            // permanently. Replacing a stale staged copy of the same
            // content-addressed blob is always right.
            let staged = client
                .download_staged(&req, &dir)
                .overwrite(zblob::Overwrite::Replace)
                .progress(&sink)
                .cancel(&cancel)
                .await?;
            Ok(staged.path)
        });
        while let Some(p) = rx.recv().await {
            match p {
                Progress::Chunk { received, total, .. }
                // Seed the bar on resume (#624) — see `download_stream`.
                | Progress::Resumed { received, total } => {
                    yield Message::ArtifactProgress { got: received as u64, total: total as u64 };
                }
                // Blob downloads never emit `Verifying`; `Progress` is
                // #[non_exhaustive].
                _ => {}
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
        // The holder pick: one button per producing host (origin + size),
        // plus Cancel. Rendered before the generic controls — this state has
        // no progress to bar and no pause to offer.
        if let ArtifactFetch::PickingHolder { holders } = fetch {
            let col = column![text(fetch.label(kind)).size(font::CAPTION)].spacing(space::XS);
            let mut btns = Row::new().spacing(space::SM).align_y(Alignment::Center);
            for (i, h) in holders.iter().enumerate() {
                let size = match &h.state {
                    ArtifactState::Ready {
                        delivery: Delivery::Blob { manifest, .. },
                        ..
                    } => crate::view::formatting::format_bytes(manifest.total_len as f64),
                    ArtifactState::Ready {
                        delivery: Delivery::Tree { summary, .. },
                        ..
                    } => crate::view::formatting::format_bytes(summary.total_bytes as f64),
                    _ => String::new(),
                };
                btns = btns.push(
                    button(text(format!("{} · {size}", h.origin)).size(font::CAPTION))
                        .on_press(Message::ArtifactHolderChosen(i)),
                );
            }
            btns = btns
                .push(button(text("Cancel").size(font::CAPTION)).on_press(Message::CancelArtifact));
            return col.push(btns).into();
        }
        // The verified pre-download confirm: what the snapshot actually
        // contains (root-verified, not self-reported) and who still serves
        // it, then the folder pick — before the multi-gigabyte fetch.
        if let ArtifactFetch::ConfirmingTree { verify } = fetch {
            let source_line = match &verify.source {
                TreeSource::Producer {
                    origin,
                    present,
                    total,
                } => {
                    if present >= total {
                        format!("served by {origin} (all {total} chunks present)")
                    } else {
                        format!("served by {origin} ({present}/{total} chunks present)")
                    }
                }
                TreeSource::RouterReplica => {
                    "producer not answering — a router replica holds the snapshot".to_string()
                }
            };
            let mut col = column![
                text(fetch.label(kind)).size(font::CAPTION),
                text(source_line).size(font::CAPTION),
            ]
            .spacing(space::XS);
            for (path, size) in &verify.largest {
                col = col.push(
                    text(format!(
                        "  {path} · {}",
                        crate::view::formatting::format_bytes(*size as f64)
                    ))
                    .size(font::CAPTION),
                );
            }
            let controls = row![
                button(text("Choose folder & download").size(font::CAPTION))
                    .on_press(Message::ArtifactTreeConfirmed),
                button(text("Cancel").size(font::CAPTION)).on_press(Message::CancelArtifact),
            ]
            .spacing(space::SM)
            .align_y(Alignment::Center);
            return col.push(controls).into();
        }
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
    if is_this
        && matches!(
            fetch,
            ArtifactFetch::Saved { .. } | ArtifactFetch::Failed(_)
        )
    {
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
        assert!(
            !ArtifactFetch::Saved {
                path: "x".into(),
                note: None
            }
            .is_active()
        );
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

    fn ready(origin: &str, kind: &str) -> ArtifactState {
        ArtifactState::Ready {
            id: Ulid::from_parts(1, 1),
            kind: kind.into(),
            delivery: Delivery::Tree {
                root: zblob::Hash::of(origin.as_bytes()),
                store_prefix: format!("v1/{origin}/@blob/store"),
                tree_prefix: format!("v1/{origin}/@blob/tree"),
                summary: zensight_common::TreeSummary {
                    file_count: 1,
                    total_bytes: 1,
                },
            },
            expires_ms: 0,
            note: None,
        }
    }

    /// The fan-in round rule: any Generating keeps waiting (a slow host is
    /// not dropped because a fast one finished), Ready-and-quiet completes
    /// with EVERY Ready state, and a failure counts only when nobody is
    /// Ready or producing.
    #[test]
    fn poll_round_outcome_rules() {
        let producing = ArtifactState::Generating {
            id: Ulid::from_parts(1, 1),
            kind: "snapshot".into(),
            detail: None,
            progress: Some(0.5),
        };
        let failed = ArtifactState::Failed {
            id: Ulid::from_parts(1, 1),
            kind: "snapshot".into(),
            reason: "disk full".into(),
        };

        // Ready + Generating → keep polling.
        assert!(matches!(
            poll_round_outcome(vec![ready("h-3fa9c2d41b7e", "snapshot"), producing.clone()]),
            RoundOutcome::Generating { .. }
        ));
        // Ready + Failed → the failure must not mask the artifact.
        match poll_round_outcome(vec![failed.clone(), ready("h-3fa9c2d41b7e", "snapshot")]) {
            RoundOutcome::Ready(states) => assert_eq!(states.len(), 1),
            _ => panic!("a failure must not mask a Ready artifact"),
        }
        // Two Ready hosts → both come back.
        match poll_round_outcome(vec![
            ready("h-3fa9c2d41b7e", "snapshot"),
            ready("h-0123456789ab", "snapshot"),
        ]) {
            RoundOutcome::Ready(states) => assert_eq!(states.len(), 2),
            _ => panic!("both hosts' artifacts must come back"),
        }
        // Only failures → failed, with the reason.
        assert!(matches!(
            poll_round_outcome(vec![failed]),
            RoundOutcome::Failed(r) if r == "disk full"
        ));
        // Nothing yet → pending.
        assert!(matches!(poll_round_outcome(vec![]), RoundOutcome::Pending));
    }

    /// Holder reduction: one holder per origin, origin read off the concrete
    /// delivery prefix, malformed prefixes skipped.
    #[test]
    fn holders_reduce_by_origin() {
        let states = vec![
            ready("h-3fa9c2d41b7e", "snapshot"),
            ready("h-3fa9c2d41b7e", "snapshot"), // duplicate origin
            ready("h-0123456789ab", "snapshot"),
            ready("*", "snapshot"), // wildcard origin: not a fetchable holder
        ];
        let holders = holders_from_states(states);
        assert_eq!(holders.len(), 2);
        assert_eq!(holders[0].origin, "h-3fa9c2d41b7e");
        assert_eq!(holders[1].origin, "h-0123456789ab");
    }
}
