//! GPU telemetry from the kernel's DRM sysfs (#954, SYS-SUP-009/012).
//!
//! GPU was absent from the whole platform:
//! `grep -ri 'nvidia\|nvml\|amdgpu\|/sys/class/drm'` matched nothing, while
//! `sysinfo` carried twenty-five other `collect.*` families. The requirement
//! sheet names GPU twice.
//!
//! # Kernel sysfs, not a vendor library
//!
//! The default build reads `/sys/class/drm/card*` and nothing else — no
//! `libnvidia-ml`, no ROCm, no runtime dependency a package manager has to
//! satisfy on every host in the fleet. What that costs is stated rather than
//! hidden: **amdgpu exposes a busy percentage and Intel does not**, so
//! utilisation is absent on i915/xe in a default build. An absent metric is
//! the honest answer; a zero would say the GPU is idle.
//!
//! The `nvml` build feature adds the NVIDIA library for the numbers only it
//! can give.
//!
//! # Per VM
//!
//! Passthrough and vGPU both surface as a DRM card *inside the guest*, so a
//! guest running `sysinfo` reports its own GPU with no host-side work — which
//! is how SYS-SUP-012 is met. Host-side attribution of which guest owns which
//! card (joining `hostpci` from the pve guest config to the host's DRM
//! inventory) is deliberately **not** done here and is a `pve` follow-up.

pub mod nvml;

use std::path::{Path, PathBuf};

pub use zensight_common::gpu::{GpuInfo, vendor_name};

/// Where the DRM cards live. Injectable so the parsers are fixture-tested
/// rather than tested against whatever hardware the runner happens to have —
/// which for CI is none, and for a developer box is one vendor.
pub const DRM_ROOT: &str = "/sys/class/drm";

/// The numbers a card exposes this tick.
///
/// **Every field is optional and absent means the driver did not publish it.**
/// That distinction is the whole design: a zero utilisation is an idle GPU, a
/// zero temperature is a broken sensor, and a zero VRAM total is nonsense —
/// none of them is "this driver does not report that".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuMetrics {
    pub utilisation_pct: Option<f64>,
    pub vram_used_bytes: Option<f64>,
    pub vram_total_bytes: Option<f64>,
    pub temp_celsius: Option<f64>,
    pub power_watts: Option<f64>,
    pub fan_rpm: Option<f64>,
    pub clock_mhz: Option<f64>,
}

impl GpuMetrics {
    /// Whether anything at all was read. A card that publishes no numbers
    /// still gets its `state/sysinfo/gpu/{card}` document — knowing a GPU
    /// exists and reports nothing is itself worth publishing — but it
    /// contributes no telemetry.
    pub fn is_empty(&self) -> bool {
        *self == GpuMetrics::default()
    }
}

/// Enumerate the GPUs under `root`, with whatever each one reports.
///
/// Sorted by card name, so the published set does not depend on directory
/// iteration order.
pub fn read_cards(root: &Path) -> Vec<(GpuInfo, GpuMetrics)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        // No DRM at all — a headless server, a container without the sysfs
        // mount. Not an error, and not a zero.
        return Vec::new();
    };
    let mut cards: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        // `card0`, not `card0-DP-1` (a connector) and not `renderD128` (the
        // render node, which is the same device seen twice).
        .filter(|n| n.starts_with("card") && n["card".len()..].chars().all(|c| c.is_ascii_digit()))
        .collect();
    cards.sort();

    cards
        .into_iter()
        .filter_map(|card| {
            let dev = root.join(&card).join("device");
            // `/sys/class/drm/<card>/device` is a symlink into the PCI tree;
            // its last component is the address. That is the join key to a
            // vendor library's view of the same card — matching by
            // enumeration index instead would silently pair the wrong two
            // devices on a host with more than one GPU.
            let pci_addr = std::fs::read_link(&dev)
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .filter(|a| a.contains(':'));
            // No `device/vendor` means this is not a PCI GPU node we can
            // describe. Skipped rather than published as an anonymous card.
            let vendor_id = read_trimmed(&dev.join("vendor"))?;
            let device_id = read_trimmed(&dev.join("device"));
            let info = GpuInfo {
                card: card.clone(),
                vendor: vendor_name(&vendor_id),
                driver: std::fs::read_link(dev.join("driver"))
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
                pci_id: device_id.map(|d| format!("{vendor_id}:{d}")),
                name: read_trimmed(&dev.join("product_name")),
                pci_addr,
            };
            let metrics = read_metrics(&root.join(&card), &dev);
            Some((info, metrics))
        })
        .collect()
}

fn read_metrics(card_dir: &Path, dev: &Path) -> GpuMetrics {
    // hwmon, where the thermals and power live for both amdgpu and i915.
    // Units are the kernel's: millidegrees, microwatts, Hz.
    let hwmon = first_hwmon(dev);
    let hw = |name: &str| hwmon.as_ref().and_then(|h| read_num(&h.join(name)));

    let mut m = GpuMetrics {
        // amdgpu. `gpu_busy_percent` is the one driver-published utilisation
        // figure available without perf counters; Intel has no equivalent,
        // which is why utilisation stays absent there.
        utilisation_pct: read_num(&dev.join("gpu_busy_percent")),
        vram_used_bytes: read_num(&dev.join("mem_info_vram_used")),
        vram_total_bytes: read_num(&dev.join("mem_info_vram_total")),
        temp_celsius: hw("temp1_input").map(|v| v / 1000.0),
        power_watts: hw("power1_average").map(|v| v / 1_000_000.0),
        fan_rpm: hw("fan1_input"),
        clock_mhz: hw("freq1_input").map(|v| v / 1_000_000.0),
    };

    // i915/xe publish the current frequency under the GT node instead, already
    // in MHz. Only consulted when hwmon gave nothing, so an amdgpu card does
    // not get two answers.
    if m.clock_mhz.is_none() {
        m.clock_mhz = read_num(&card_dir.join("gt/gt0/rps_cur_freq_mhz"));
    }
    m
}

/// The first `hwmon*` directory under `device/hwmon/`.
///
/// Sorted, because a card with two hwmon nodes must not report a different
/// one each tick — the numbers would jump between sensors with no visible
/// cause.
fn first_hwmon(dev: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(dev.join("hwmon"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("hwmon"))
        })
        .collect();
    dirs.sort();
    dirs.into_iter().next()
}

fn read_trimmed(p: &Path) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Read a sysfs file as a number. A file that exists but does not parse yields
/// `None` — the same as absent, because a driver that wrote `"unknown"` there
/// has told us nothing, and inventing a 0 from it would be worse.
fn read_num(p: &Path) -> Option<f64> {
    read_trimmed(p)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, contents: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    /// An amdgpu card, with the files that driver actually publishes.
    fn amd_card(root: &Path, card: &str) {
        let dev = root.join(card).join("device");
        write(&dev.join("vendor"), "0x1002\n");
        write(&dev.join("device"), "0x73ff\n");
        write(&dev.join("product_name"), "Radeon RX 6600\n");
        write(&dev.join("gpu_busy_percent"), "42\n");
        write(&dev.join("mem_info_vram_used"), "1073741824\n");
        write(&dev.join("mem_info_vram_total"), "8589934592\n");
        let hw = dev.join("hwmon/hwmon3");
        write(&hw.join("temp1_input"), "54000\n");
        write(&hw.join("power1_average"), "35000000\n");
        write(&hw.join("fan1_input"), "1200\n");
        write(&hw.join("freq1_input"), "1815000000\n");
        std::fs::create_dir_all(root.join("_drivers/amdgpu")).unwrap();
        std::os::unix::fs::symlink(root.join("_drivers/amdgpu"), dev.join("driver")).unwrap();
    }

    #[test]
    fn an_amdgpu_card_is_described_and_measured_in_sane_units() {
        let tmp = tempfile::tempdir().unwrap();
        amd_card(tmp.path(), "card0");
        let cards = read_cards(tmp.path());
        assert_eq!(cards.len(), 1);
        let (info, m) = &cards[0];
        assert_eq!(info.card, "card0");
        assert_eq!(info.vendor, "AMD");
        assert_eq!(info.driver.as_deref(), Some("amdgpu"));
        assert_eq!(info.pci_id.as_deref(), Some("0x1002:0x73ff"));
        assert_eq!(info.name.as_deref(), Some("Radeon RX 6600"));

        assert_eq!(m.utilisation_pct, Some(42.0));
        assert_eq!(m.vram_used_bytes, Some(1_073_741_824.0));
        assert_eq!(m.vram_total_bytes, Some(8_589_934_592.0));
        // The kernel's units are millidegrees, microwatts and Hz. Publishing
        // them raw would put 54000 on a chart labelled °C.
        assert_eq!(m.temp_celsius, Some(54.0));
        assert_eq!(m.power_watts, Some(35.0));
        assert_eq!(m.fan_rpm, Some(1200.0));
        assert_eq!(m.clock_mhz, Some(1815.0));
    }

    /// Intel publishes no busy percentage, and utilisation stays **absent**.
    #[test]
    fn an_intel_card_has_no_utilisation_and_says_so_by_omission() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("card0/device");
        write(&dev.join("vendor"), "0x8086\n");
        write(&dev.join("device"), "0x9a49\n");
        write(&tmp.path().join("card0/gt/gt0/rps_cur_freq_mhz"), "1300\n");

        let cards = read_cards(tmp.path());
        let (info, m) = &cards[0];
        assert_eq!(info.vendor, "Intel");
        assert_eq!(info.name, None, "i915 publishes no product name");
        // The point: a zero here would say the GPU is idle, which is a
        // different and false claim.
        assert_eq!(
            m.utilisation_pct, None,
            "i915 exposes no busy percentage without perf counters"
        );
        // The GT node is consulted only because hwmon gave nothing.
        assert_eq!(m.clock_mhz, Some(1300.0));
    }

    /// Render nodes and connectors are not cards.
    #[test]
    fn render_nodes_and_connectors_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        amd_card(tmp.path(), "card0");
        // The same device seen twice, and a connector — both would otherwise
        // become duplicate or anonymous entries on the map.
        write(&tmp.path().join("renderD128/device/vendor"), "0x1002\n");
        write(&tmp.path().join("card0-DP-1/device/vendor"), "0x1002\n");
        let cards = read_cards(tmp.path());
        assert_eq!(
            cards
                .iter()
                .map(|(i, _)| i.card.as_str())
                .collect::<Vec<_>>(),
            vec!["card0"]
        );
    }

    /// Cards come out in a stable order.
    #[test]
    fn cards_are_sorted_not_directory_ordered() {
        let tmp = tempfile::tempdir().unwrap();
        for c in ["card2", "card0", "card1"] {
            amd_card(tmp.path(), c);
        }
        let names: Vec<String> = read_cards(tmp.path())
            .into_iter()
            .map(|(i, _)| i.card)
            .collect();
        assert_eq!(names, vec!["card0", "card1", "card2"]);
    }

    /// A node with no `device/vendor` is not a GPU this can describe.
    #[test]
    fn a_node_without_a_vendor_is_not_published() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("card0/device")).unwrap();
        assert!(read_cards(tmp.path()).is_empty());
    }

    /// No DRM at all is not an error and not a zero.
    #[test]
    fn a_host_with_no_drm_reports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_cards(&tmp.path().join("absent")).is_empty());
        assert!(read_cards(tmp.path()).is_empty());
    }

    /// A file that exists but does not parse is absent, not zero.
    #[test]
    fn an_unparseable_value_is_absent_rather_than_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("card0/device");
        write(&dev.join("vendor"), "0x1002\n");
        // Some drivers write this when the counter is unavailable.
        write(&dev.join("gpu_busy_percent"), "unknown\n");
        let (_, m) = &read_cards(tmp.path())[0];
        assert_eq!(m.utilisation_pct, None);
        assert!(m.is_empty(), "nothing was measured: {m:?}");
    }

    /// Two hwmon nodes resolve to the same one every tick.
    #[test]
    fn a_card_with_two_hwmon_nodes_picks_one_stably() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("card0/device");
        write(&dev.join("vendor"), "0x1002\n");
        write(&dev.join("hwmon/hwmon9/temp1_input"), "90000\n");
        write(&dev.join("hwmon/hwmon2/temp1_input"), "50000\n");
        // Without sorting, the reported temperature would jump between two
        // sensors with no visible cause.
        for _ in 0..3 {
            let (_, m) = &read_cards(tmp.path())[0];
            assert_eq!(m.temp_celsius, Some(50.0));
        }
    }

    #[test]
    fn an_unknown_vendor_id_is_returned_verbatim() {
        assert_eq!(vendor_name("0x1002"), "AMD");
        assert_eq!(vendor_name("0x8086"), "Intel");
        assert_eq!(vendor_name("0x10DE"), "NVIDIA");
        // The id is the useful thing; "unknown" would discard it.
        assert_eq!(vendor_name("0xbeef"), "0xbeef");
    }
}
