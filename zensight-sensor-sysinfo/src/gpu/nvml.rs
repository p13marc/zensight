//! NVIDIA GPU telemetry through NVML (#954), behind `--features nvml`.
//!
//! The default build already reports NVIDIA cards through their DRM node —
//! vendor, driver, PCI id, and whatever `nvidia`/`nouveau` publishes in sysfs.
//! This adds the numbers **only the vendor library can give**: memory-controller
//! utilisation, ECC error counters, and per-process VRAM.
//!
//! # This path is compile-checked, not executed
//!
//! There is no NVIDIA hardware on the development machines or in CI, so the
//! `features` job type-checks this and nothing runs it. That is stated here
//! rather than discovered later.
//!
//! What follows from it shapes the module: **everything that can be tested
//! without a card is separated from the FFI and tested unconditionally** — the
//! PCI join, the per-process cap and ordering, and the metric shaping all live
//! in plain functions over [`NvmlMetrics`], compiled and tested in a *default*
//! build. Only [`read`] itself — the twenty lines that call into
//! `nvml-wrapper` — is feature-gated and unexercised.
//!
//! # Failure is silence, not zeros
//!
//! Every NVML call is allowed to fail on its own: a card with ECC disabled
//! returns `NotSupported` for the counters, and an older driver returns it for
//! the memory-controller utilisation. Each field is therefore an `Option`
//! filled independently, and a `None` means *NVML did not say* — never zero.
//! Initialisation failing at all (no driver, no library) means this host has
//! nothing to report, which is not an error either.

/// Per-card numbers NVML can supply that DRM sysfs cannot.
///
/// Plain data, always compiled. Every field independent, because NVML fails
/// per-call: ECC disabled, an older driver, a consumer card without a fan
/// sensor — each is a `None` here, and none of them should suppress the rest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NvmlMetrics {
    /// The PCI address this came from, normalised — the join key back to a DRM
    /// card.
    pub pci_addr: String,
    /// Percentage of the last sample period the GPU was busy.
    pub utilisation_pct: Option<f64>,
    /// Percentage of the last sample period the **memory controller** was
    /// busy. Distinct from VRAM occupancy, and the figure that tells a
    /// memory-bound workload from a compute-bound one — which is the whole
    /// reason to pay for NVML.
    pub memory_utilisation_pct: Option<f64>,
    pub vram_used_bytes: Option<f64>,
    pub vram_total_bytes: Option<f64>,
    pub temp_celsius: Option<f64>,
    pub power_watts: Option<f64>,
    pub fan_rpm: Option<f64>,
    /// SM clock, MHz.
    pub clock_mhz: Option<f64>,
    /// Uncorrected (double-bit) device-memory ECC errors since the driver
    /// loaded. **Uncorrected, not corrected**: a corrected error is the
    /// hardware working, and counting it as a fault would page on healthy
    /// cards.
    pub ecc_volatile_uncorrected: Option<f64>,
    /// The same, over the lifetime of the device.
    pub ecc_aggregate_uncorrected: Option<f64>,
    /// `(pid, vram_bytes)` for processes with memory on this card.
    pub processes: Vec<(u32, u64)>,
}

/// Cap on per-process VRAM rows published per card.
///
/// A training host can have hundreds of short-lived processes touching a GPU,
/// and one telemetry key per pid per card is an unbounded family keyed by
/// something that changes every few seconds. The registry declares the same
/// number, and [`process_cap_matches_the_registry`] pins the two together.
pub const MAX_PROCESSES_PER_CARD: usize = 32;

/// The per-process rows to publish: the largest consumers, capped, in a
/// deterministic order.
///
/// **Largest first**, because the cap has to drop something and the useful
/// answer to "what is using this GPU" is the big ones. Ties break on pid, so a
/// host whose processes hold identical amounts does not shuffle its published
/// set between ticks — which would look like processes appearing and vanishing.
pub fn top_processes(mut procs: Vec<(u32, u64)>) -> Vec<(u32, u64)> {
    procs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    procs.truncate(MAX_PROCESSES_PER_CARD);
    procs
}

/// Normalise a PCI address for comparison.
///
/// NVML prints the domain as **eight** hex digits (`00000000:01:00.0`); the
/// kernel's sysfs symlink prints it as **four** (`0000:01:00.0`). Comparing
/// them raw never matches, so every NVIDIA card would silently fail to join its
/// DRM node and the vendor numbers would be published against nothing.
///
/// Four is a *minimum*, not a truncation: the kernel uses `%04x`, so a domain
/// wider than four digits keeps its width on both sides. Narrowing it here
/// would join the wrong card on the rare host that has one.
pub fn normalise_pci(addr: &str) -> String {
    let addr = addr.trim().to_ascii_lowercase();
    match addr.split_once(':') {
        // Re-pad the domain to four hex digits, the kernel's width.
        Some((domain, rest)) => {
            let d = domain.trim_start_matches('0');
            let d = if d.is_empty() { "0" } else { d };
            format!("{d:0>4}:{rest}")
        }
        None => addr,
    }
}

/// Fold NVML's numbers into what DRM sysfs already read.
///
/// NVML **wins where it answers**: it is the vendor's own instrumentation and
/// strictly better informed than a sysfs node the open driver happens to
/// expose. Where NVML says nothing the sysfs value stands, so enabling the
/// feature can only add information, never remove it — which is the property
/// that makes turning it on safe.
pub fn merge(base: &mut super::GpuMetrics, nv: &NvmlMetrics) {
    let take = |dst: &mut Option<f64>, src: Option<f64>| {
        if src.is_some() {
            *dst = src;
        }
    };
    take(&mut base.utilisation_pct, nv.utilisation_pct);
    take(&mut base.vram_used_bytes, nv.vram_used_bytes);
    take(&mut base.vram_total_bytes, nv.vram_total_bytes);
    take(&mut base.temp_celsius, nv.temp_celsius);
    take(&mut base.power_watts, nv.power_watts);
    take(&mut base.fan_rpm, nv.fan_rpm);
    take(&mut base.clock_mhz, nv.clock_mhz);
}

/// The extra metric rows an `NvmlMetrics` contributes, as `(name, value)`
/// suffixes under `gpu/{card}/`.
///
/// Only the families DRM sysfs has no equivalent for — everything else went
/// through [`merge`] and is published by the shared path, so a metric never
/// arrives twice by two routes.
pub fn extra_metrics(nv: &NvmlMetrics) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for (name, value) in [
        ("memory_utilisation_pct", nv.memory_utilisation_pct),
        ("ecc_volatile_uncorrected", nv.ecc_volatile_uncorrected),
        ("ecc_aggregate_uncorrected", nv.ecc_aggregate_uncorrected),
    ] {
        if let Some(v) = value {
            out.push((name.to_string(), v));
        }
    }
    for (pid, bytes) in top_processes(nv.processes.clone()) {
        out.push((format!("process/{pid}/vram_bytes"), bytes as f64));
    }
    out
}

/// Read every NVML-visible card. `None` when NVML is unavailable on this host.
///
/// **The unexercised part.** Each call is allowed to fail independently — see
/// the module docs — so an ECC-disabled card still reports its utilisation.
#[cfg(feature = "nvml")]
pub fn read() -> Option<Vec<NvmlMetrics>> {
    use nvml_wrapper::Nvml;
    use nvml_wrapper::enum_wrappers::device::{
        Clock, EccCounter, MemoryError, MemoryLocation, TemperatureSensor,
    };
    use nvml_wrapper::enums::device::UsedGpuMemory;

    // No driver, no library, a container without /dev/nvidiactl: this host has
    // nothing to report, which is not an error.
    let nvml = Nvml::init().ok()?;
    let count = nvml.device_count().ok()?;

    let mut out = Vec::new();
    for i in 0..count {
        let Ok(dev) = nvml.device_by_index(i) else {
            continue;
        };
        // Without a PCI address there is no way to say WHICH card this is, and
        // publishing it against a guessed DRM node would attribute one card's
        // numbers to another.
        let Ok(pci) = dev.pci_info() else { continue };
        let util = dev.utilization_rates().ok();
        let mem = dev.memory_info().ok();

        let mut procs: Vec<(u32, u64)> = Vec::new();
        // Both lists, because a compute process and a graphics process are
        // both using the card and NVML reports them separately.
        for p in dev
            .running_compute_processes()
            .into_iter()
            .chain(dev.running_graphics_processes())
            .flatten()
        {
            // `Unavailable` is NVML saying it cannot attribute the memory —
            // recorded as nothing rather than as a zero-byte process.
            if let UsedGpuMemory::Used(b) = p.used_gpu_memory {
                procs.push((p.pid, b));
            }
        }

        out.push(NvmlMetrics {
            pci_addr: normalise_pci(&pci.bus_id),
            utilisation_pct: util.as_ref().map(|u| u.gpu as f64),
            memory_utilisation_pct: util.as_ref().map(|u| u.memory as f64),
            vram_used_bytes: mem.as_ref().map(|m| m.used as f64),
            vram_total_bytes: mem.as_ref().map(|m| m.total as f64),
            temp_celsius: dev
                .temperature(TemperatureSensor::Gpu)
                .ok()
                .map(|t| t as f64),
            // NVML reports milliwatts.
            power_watts: dev.power_usage().ok().map(|w| w as f64 / 1000.0),
            // NVML's fan speed is a PERCENTAGE of maximum, not RPM — a
            // different quantity from the sysfs `fan1_input` this would
            // otherwise merge into, so it is deliberately not filled here.
            // Publishing a percentage on a series named `fan_rpm` would be a
            // wrong number rather than a missing one.
            fan_rpm: None,
            clock_mhz: dev.clock_info(Clock::SM).ok().map(|c| c as f64),
            ecc_volatile_uncorrected: dev
                .memory_error_counter(
                    MemoryError::Uncorrected,
                    EccCounter::Volatile,
                    MemoryLocation::Device,
                )
                .ok()
                .map(|c| c as f64),
            ecc_aggregate_uncorrected: dev
                .memory_error_counter(
                    MemoryError::Uncorrected,
                    EccCounter::Aggregate,
                    MemoryLocation::Device,
                )
                .ok()
                .map(|c| c as f64),
            processes: procs,
        });
    }
    Some(out)
}

/// Without the feature there is nothing to read, and the caller's code path is
/// identical — no `#[cfg]` at the call site.
#[cfg(not(feature = "nvml"))]
pub fn read() -> Option<Vec<NvmlMetrics>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NVML and the kernel spell a PCI address differently, and comparing them
    /// raw never matches.
    #[test]
    fn nvml_and_kernel_pci_addresses_compare_equal_after_normalising() {
        // NVML pads the domain to eight hex digits; sysfs uses four.
        assert_eq!(normalise_pci("00000000:01:00.0"), "0000:01:00.0");
        assert_eq!(normalise_pci("0000:01:00.0"), "0000:01:00.0");
        // Case, and a non-zero domain.
        assert_eq!(normalise_pci("00000000:65:00.0"), "0000:65:00.0");
        assert_eq!(normalise_pci("0000:0A:00.0"), "0000:0a:00.0");
        // NVML prints the domain as EIGHT hex digits, so domain 1 is
        // "00000001" — not "00010000", which is domain 0x10000.
        assert_eq!(normalise_pci("00000001:01:00.0"), "0001:01:00.0");
        // A domain too wide for four digits keeps its width, because that is
        // what the kernel prints for it too (`%04x` is a minimum, not a
        // truncation). Narrowing it here would join the wrong card.
        assert_eq!(normalise_pci("00010000:01:00.0"), "10000:01:00.0");
        // Whatever this is, it is returned rather than mangled into a
        // plausible-looking address that might match the wrong card.
        assert_eq!(normalise_pci("nonsense"), "nonsense");
    }

    /// The cap keeps the largest consumers, and the order does not wander.
    #[test]
    fn per_process_rows_are_capped_largest_first_and_stable() {
        let procs: Vec<(u32, u64)> = (0..100).map(|i| (i as u32, (i as u64) * 10)).collect();
        let top = top_processes(procs);
        assert_eq!(top.len(), MAX_PROCESSES_PER_CARD);
        // Largest first: the cap has to drop something, and "what is using
        // this GPU" is answered by the big ones.
        assert_eq!(top[0], (99, 990));
        assert_eq!(top[MAX_PROCESSES_PER_CARD - 1], (68, 680));

        // Ties break on pid, so a host whose processes hold identical amounts
        // does not shuffle its published set between ticks — which would look
        // like processes appearing and vanishing.
        let tied = vec![(9u32, 100u64), (3, 100), (7, 100)];
        assert_eq!(
            top_processes(tied.clone()),
            vec![(3, 100), (7, 100), (9, 100)]
        );
        let mut shuffled = tied;
        shuffled.reverse();
        assert_eq!(
            top_processes(shuffled),
            vec![(3, 100), (7, 100), (9, 100)],
            "input order must not reach the published set"
        );
    }

    /// NVML wins where it answers; sysfs stands where it does not.
    #[test]
    fn merging_can_only_add_information() {
        let mut base = crate::gpu::GpuMetrics {
            utilisation_pct: None,
            vram_used_bytes: Some(1.0),
            vram_total_bytes: Some(2.0),
            temp_celsius: Some(40.0),
            power_watts: None,
            fan_rpm: Some(900.0),
            clock_mhz: Some(1000.0),
        };
        let nv = NvmlMetrics {
            pci_addr: "0000:01:00.0".into(),
            utilisation_pct: Some(75.0),
            temp_celsius: Some(61.0),
            // NVML said nothing about these two.
            fan_rpm: None,
            clock_mhz: None,
            ..Default::default()
        };
        merge(&mut base, &nv);
        // Filled a gap, and overrode where better informed.
        assert_eq!(base.utilisation_pct, Some(75.0));
        assert_eq!(base.temp_celsius, Some(61.0));
        // Left standing where NVML was silent — enabling the feature must
        // never remove information.
        assert_eq!(base.fan_rpm, Some(900.0));
        assert_eq!(base.clock_mhz, Some(1000.0));
        assert_eq!(base.vram_used_bytes, Some(1.0));
    }

    /// Only families sysfs has no equivalent for come through `extra_metrics`.
    #[test]
    fn extra_metrics_carry_only_what_sysfs_cannot() {
        let nv = NvmlMetrics {
            pci_addr: "0000:01:00.0".into(),
            // These went through `merge`, so they must NOT appear again here —
            // a metric arriving twice by two routes is a double publish.
            utilisation_pct: Some(75.0),
            temp_celsius: Some(61.0),
            memory_utilisation_pct: Some(30.0),
            ecc_volatile_uncorrected: Some(0.0),
            ecc_aggregate_uncorrected: Some(4.0),
            processes: vec![(1234, 500), (99, 9000)],
            ..Default::default()
        };
        let names: Vec<String> = extra_metrics(&nv).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec![
                "memory_utilisation_pct",
                "ecc_volatile_uncorrected",
                "ecc_aggregate_uncorrected",
                // Largest first.
                "process/99/vram_bytes",
                "process/1234/vram_bytes",
            ]
        );
    }

    /// A zero ECC count is a measurement and must be published; an absent one
    /// must not become a zero.
    #[test]
    fn a_zero_ecc_count_is_published_and_an_absent_one_is_not() {
        let measured = NvmlMetrics {
            ecc_volatile_uncorrected: Some(0.0),
            ..Default::default()
        };
        // "ECC is on and has seen nothing" is exactly what an operator wants
        // to see, and it is not the same as "this card does not report ECC".
        assert_eq!(
            extra_metrics(&measured),
            vec![("ecc_volatile_uncorrected".to_string(), 0.0)]
        );
        assert!(extra_metrics(&NvmlMetrics::default()).is_empty());
    }

    /// A card with no NVML numbers contributes nothing rather than a row of
    /// zeros.
    #[test]
    fn an_empty_reading_contributes_nothing() {
        let mut base = crate::gpu::GpuMetrics::default();
        merge(&mut base, &NvmlMetrics::default());
        assert!(base.is_empty());
    }

    /// Without the feature, `read` is `None` and the caller needs no `#[cfg]`.
    #[cfg(not(feature = "nvml"))]
    #[test]
    fn a_build_without_the_feature_reads_nothing() {
        assert!(read().is_none());
    }

    /// The cap here and the `cardinality` in the registry must be the same
    /// number: they drift, and either the sensor publishes past a declared
    /// budget — which the conformance judge fails, elsewhere and later — or it
    /// refuses rows it was allowed to publish.
    #[test]
    fn process_cap_matches_the_registry() {
        use zensight_common::registry::sysinfo::Subject;
        assert_eq!(
            Subject::gpu_process_vram_bytes("card0", "1").cardinality(),
            Some(MAX_PROCESSES_PER_CARD as u64),
        );
    }
}
