//! The stream catalogue: which video sources this host advertises.
//!
//! Built once at startup by merging three source families:
//! - enumerated local V4L2 cameras (`enumerate_v4l2: true`),
//! - configured remote RTSP cameras,
//! - configured synthetic test-pattern sources (demo mode / CI).
//!
//! Every entry becomes one `StreamDescriptor` on the `@rpc/parallax/streams`
//! catalogue and one `state/parallax/device/<stream>/alive` liveliness token; the
//! `<stream>` name is the key chunk under `@media/parallax/`.

use std::collections::HashSet;

use zensight_common::stream::{StreamDescriptor, TierSpec};

use crate::config::{ParallaxConfig, RtspSourceConfig, TestSourceConfig};

/// How to reach one video source (drives pipeline construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    /// Synthetic `VideoTestSrc` pattern (identical downstream path to a real
    /// camera — demo mirrors the contract).
    Test {
        pattern: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    /// Local V4L2 camera.
    V4l2 {
        /// Device path (e.g. `/dev/video0`).
        device: String,
    },
    /// Remote RTSP camera.
    Rtsp {
        url: String,
        username: Option<String>,
        password: Option<String>,
    },
}

impl SourceKind {
    /// Whether this source can be captured by only ONE pipeline at a time.
    ///
    /// A single V4L2 device (`/dev/videoX`) can't be streamed by two pipelines
    /// at once — the second `REQBUFS`/`S_FMT` fails `EBUSY` — and most RTSP
    /// cameras cap concurrent sessions. The synthetic test source has no such
    /// limit (each pipeline generates independently). This gates whether
    /// opening a new video tier must first release a sibling tier's capture
    /// (see `SessionManager::open`): exclusive sources serve one video tier per
    /// stream, shareable sources allow concurrent tiers (true simulcast).
    pub fn is_exclusive(&self) -> bool {
        matches!(self, SourceKind::V4l2 { .. } | SourceKind::Rtsp { .. })
    }
}

/// One advertised stream: name + how to open it + its native capabilities.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// Stream identifier (single key chunk, unique within the catalogue).
    pub name: String,
    pub kind: SourceKind,
    /// Native capture width/height/framerate, when known (`None` for an RTSP
    /// source whose SDP we have not read, or a V4L2 device we could not probe).
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f32>,
    /// Codecs this source can be opened with — probed per source, not a
    /// hardcoded pair (#507).
    pub codecs: Vec<String>,
    /// Human-readable description (camera model / pattern).
    pub description: Option<String>,
}

/// The full, ordered stream catalogue for this host.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    entries: Vec<CatalogEntry>,
}

impl Catalog {
    /// Build the catalogue from config: enumerated V4L2 devices, then RTSP,
    /// then test sources. Later families skip names already taken (config
    /// validation already guarantees rtsp/test uniqueness among themselves).
    pub fn build(config: &ParallaxConfig) -> Self {
        let mut entries: Vec<CatalogEntry> = Vec::new();
        let mut names: HashSet<String> = HashSet::new();

        if config.enumerate_v4l2 {
            match parallax::elements::device::enumerate_video_devices() {
                Ok(devices) => {
                    for dev in devices {
                        let name = v4l2_stream_name(&dev.id);
                        if !names.insert(name.clone()) {
                            tracing::warn!(device = %dev.id, stream = %name,
                                "v4l2 device name collides with an existing stream; skipped");
                            continue;
                        }
                        let mut description = dev.name.clone();
                        if let Some(model) = &dev.model
                            && model != &dev.name
                        {
                            description.push_str(&format!(" ({model})"));
                        }
                        // Best-effort native-capability probe: open the device,
                        // read its negotiated geometry, drop it (releasing the
                        // camera). A busy/failing device just advertises no
                        // native size — honest "unknown until opened".
                        let (width, height, fps) = probe_v4l2(&dev.id);
                        entries.push(CatalogEntry {
                            name,
                            kind: SourceKind::V4l2 {
                                device: dev.id.clone(),
                            },
                            width,
                            height,
                            fps,
                            codecs: vec!["h264".to_string(), "mjpeg".to_string()],
                            description: Some(description),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "v4l2 enumeration failed; no local cameras advertised");
                }
            }
        }

        for rtsp in &config.rtsp {
            if !names.insert(rtsp.name.clone()) {
                tracing::warn!(stream = %rtsp.name, "rtsp stream name collides; skipped");
                continue;
            }
            entries.push(rtsp_entry(rtsp));
        }

        for test in &config.test_sources {
            if !names.insert(test.name.clone()) {
                tracing::warn!(stream = %test.name, "test stream name collides; skipped");
                continue;
            }
            entries.push(test_entry(test));
        }

        Self { entries }
    }

    /// Look one stream up by name.
    pub fn get(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// All entries, in catalogue order.
    pub fn entries(&self) -> &[CatalogEntry] {
        &self.entries
    }

    /// The advertised stream names, in catalogue order.
    pub fn stream_names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// Serve-ready descriptors, stamping `active` from the currently open set
    /// and the tiers each stream offers from the sensor's `ladder` (filtered to
    /// what the camera can actually feed — no 720p tier for a 480p camera).
    pub fn descriptors(
        &self,
        open: &HashSet<String>,
        ladder: &[TierSpec],
    ) -> Vec<StreamDescriptor> {
        self.entries
            .iter()
            .map(|e| StreamDescriptor {
                stream: e.name.clone(),
                codecs: e.codecs.clone(),
                active: open.contains(&e.name),
                width: e.width,
                height: e.height,
                fps: e.fps,
                tiers: offered_tiers(e.height, ladder),
                description: e.description.clone(),
            })
            .collect()
    }
}

/// The tiers a source with native height `native_h` can honestly offer: a tier
/// with a fixed `max_height` above the camera's native height would only be
/// upscaled (the scaler never upscales), so drop it. A `None`-capped tier (use
/// native) and every tier when the native size is unknown are always offered.
fn offered_tiers(native_h: Option<u32>, ladder: &[TierSpec]) -> Vec<TierSpec> {
    ladder
        .iter()
        .filter(|t| match (native_h, t.max_height) {
            (Some(nh), Some(mh)) => mh <= nh,
            _ => true,
        })
        .cloned()
        .collect()
}

/// Best-effort probe of a V4L2 device's native geometry (open → read → drop).
fn probe_v4l2(device: &str) -> (Option<u32>, Option<u32>, Option<f32>) {
    match parallax::elements::V4l2Src::new(device) {
        Ok(src) => {
            let fps = src
                .framerate()
                .map(|(num, den)| num as f32 / den.max(1) as f32);
            (Some(src.width()), Some(src.height()), fps)
        }
        Err(e) => {
            tracing::debug!(device = %device, error = %e,
                "v4l2 capability probe failed; advertising no native size");
            (None, None, None)
        }
    }
}

fn rtsp_entry(rtsp: &RtspSourceConfig) -> CatalogEntry {
    CatalogEntry {
        name: rtsp.name.clone(),
        kind: SourceKind::Rtsp {
            url: rtsp.url.clone(),
            username: rtsp.username.clone(),
            password: rtsp.password.clone(),
        },
        // Native size is unknown until the SDP is read at connect; the preview
        // (JPEG) needs those dims, so RTSP advertises only h264 passthrough.
        width: None,
        height: None,
        fps: None,
        codecs: vec!["h264".to_string()],
        // Never leak credentials: description is either the configured text
        // or the bare URL (which the operator wrote without inline creds).
        description: rtsp.description.clone().or_else(|| Some(rtsp.url.clone())),
    }
}

fn test_entry(test: &TestSourceConfig) -> CatalogEntry {
    CatalogEntry {
        name: test.name.clone(),
        kind: SourceKind::Test {
            pattern: test.pattern.clone(),
            width: test.width,
            height: test.height,
            fps: test.fps,
        },
        // A synthetic source's geometry is exactly its config — no probe needed.
        width: Some(test.width),
        height: Some(test.height),
        fps: Some(test.fps as f32),
        codecs: vec!["h264".to_string(), "mjpeg".to_string()],
        description: Some(format!("test pattern {}", test.pattern)),
    }
}

/// Derive a stream name from a V4L2 device path: `/dev/video0` → `video0`.
fn v4l2_stream_name(device_id: &str) -> String {
    device_id
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(device_id)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ParallaxConfig {
        let json = r#"{
            enumerate_v4l2: false,
            rtsp: [
                { name: "door", url: "rtsp://cam.local/1", description: "front door" },
                { name: "yard", url: "rtsp://cam.local/2" },
            ],
            test_sources: [
                { name: "test0", pattern: "smpte", width: 640, height: 360, fps: 15 },
            ],
        }"#;
        json5::from_str(json).unwrap()
    }

    #[test]
    fn build_merges_rtsp_and_test_sources() {
        let catalog = Catalog::build(&test_config());
        let names: Vec<_> = catalog.stream_names().collect();
        assert_eq!(names, vec!["door", "yard", "test0"]);

        assert!(matches!(
            &catalog.get("door").unwrap().kind,
            SourceKind::Rtsp { url, .. } if url == "rtsp://cam.local/1"
        ));
        // Undescribed RTSP entries fall back to the URL.
        assert_eq!(
            catalog.get("yard").unwrap().description.as_deref(),
            Some("rtsp://cam.local/2")
        );
        assert!(matches!(
            &catalog.get("test0").unwrap().kind,
            SourceKind::Test {
                width: 640,
                height: 360,
                fps: 15,
                ..
            }
        ));
        assert!(catalog.get("nope").is_none());
    }

    #[test]
    fn descriptors_stamp_active_from_open_set() {
        let catalog = Catalog::build(&test_config());
        let open: HashSet<String> = ["test0".to_string()].into();
        let ladder = vec![
            TierSpec {
                name: "low".into(),
                max_height: Some(240),
                fps: 10,
                bitrate_kbps: 400,
            },
            TierSpec {
                name: "high".into(),
                max_height: None,
                fps: 30,
                bitrate_kbps: 4000,
            },
        ];
        let descs = catalog.descriptors(&open, &ladder);
        assert_eq!(descs.len(), 3);
        for d in &descs {
            assert_eq!(d.active, d.stream == "test0");
        }
        // RTSP advertises h264-only (passthrough); the test source advertises
        // both codecs and its native geometry (no more "resolution rides the
        // description").
        let door = descs.iter().find(|d| d.stream == "door").unwrap();
        assert_eq!(door.codecs, vec!["h264"]);
        assert_eq!(door.width, None);
        let test0 = descs.iter().find(|d| d.stream == "test0").unwrap();
        assert_eq!(test0.codecs, vec!["h264", "mjpeg"]);
        assert_eq!((test0.width, test0.height), (Some(640), Some(360)));
        // A 360-high source is offered both tiers (240 fits, high is uncapped).
        let tier_names: Vec<&str> = test0.tiers.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tier_names, vec!["low", "high"]);
    }

    #[test]
    fn v4l2_names_derive_from_device_path() {
        assert_eq!(v4l2_stream_name("/dev/video0"), "video0");
        assert_eq!(v4l2_stream_name("video7"), "video7");
    }

    #[test]
    fn exclusive_sources_are_single_capture() {
        // A single camera can't feed two captures at once → one video tier at a
        // time (drives the tier-switch hand-over). The test source is shareable.
        assert!(
            SourceKind::V4l2 {
                device: "/dev/video0".into()
            }
            .is_exclusive()
        );
        assert!(
            SourceKind::Rtsp {
                url: "rtsp://cam/1".into(),
                username: None,
                password: None,
            }
            .is_exclusive()
        );
        assert!(
            !SourceKind::Test {
                pattern: "smpte".into(),
                width: 320,
                height: 240,
                fps: 30,
            }
            .is_exclusive()
        );
    }

    #[test]
    fn enumeration_is_safe_headless() {
        // On a camera-less host enumerate_v4l2 must contribute nothing and
        // not fail the build.
        let mut config = test_config();
        config.enumerate_v4l2 = true;
        let catalog = Catalog::build(&config);
        // The configured streams are always present; any real cameras on the
        // host would only add to them.
        assert!(catalog.get("door").is_some());
        assert!(catalog.get("test0").is_some());
    }
}
