//! The stream catalogue: which video sources this host advertises.
//!
//! Built once at startup by merging three source families:
//! - enumerated local V4L2 cameras (`enumerate_v4l2: true`),
//! - configured remote RTSP cameras,
//! - configured synthetic test-pattern sources (demo mode / CI).
//!
//! Every entry becomes one `StreamDescriptor` on the `@/query/streams`
//! catalogue and one `@/devices/<stream>/alive` liveliness token; the
//! `<stream>` name is the key chunk under `@media/`.

use std::collections::HashSet;

use zensight_common::stream::StreamDescriptor;

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

/// One advertised stream: name + how to open it.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    /// Stream identifier (single key chunk, unique within the catalogue).
    pub name: String,
    pub kind: SourceKind,
    /// Human-readable description (camera model / pattern + resolution).
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
                        entries.push(CatalogEntry {
                            name,
                            kind: SourceKind::V4l2 {
                                device: dev.id.clone(),
                            },
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

    /// Serve-ready descriptors, stamping `active` from the currently open set.
    pub fn descriptors(&self, open: &HashSet<String>) -> Vec<StreamDescriptor> {
        self.entries
            .iter()
            .map(|e| StreamDescriptor {
                stream: e.name.clone(),
                codecs: vec!["h264".to_string(), "mjpeg".to_string()],
                active: open.contains(&e.name),
                description: e.description.clone(),
            })
            .collect()
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
        // StreamDescriptor carries no width/height fields — resolution rides
        // the description by convention.
        description: Some(format!(
            "test pattern {} {}x{}@{}",
            test.pattern, test.width, test.height, test.fps
        )),
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
        let descs = catalog.descriptors(&open);
        assert_eq!(descs.len(), 3);
        for d in &descs {
            assert_eq!(d.codecs, vec!["h264", "mjpeg"]);
            assert_eq!(d.active, d.stream == "test0");
        }
        // Resolution rides the description (StreamDescriptor has no w/h).
        assert_eq!(
            descs[2].description.as_deref(),
            Some("test pattern smpte 640x360@15")
        );
    }

    #[test]
    fn v4l2_names_derive_from_device_path() {
        assert_eq!(v4l2_stream_name("/dev/video0"), "video0");
        assert_eq!(v4l2_stream_name("video7"), "video7");
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
