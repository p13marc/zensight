//! Static host facts — `state/sysinfo/system/info`.
//!
//! Collected at startup and refreshed slowly (the doc only changes on a
//! kernel or distro update), published LWW through a cached publisher so a
//! late-joining GUI seeds the current doc instead of waiting for the next
//! refresh. Distinct from the evidence plane: evidence carries the display
//! summary for entity cards; this doc carries the full os-release detail for
//! the sysinfo specialized view.

use std::sync::Arc;
use std::time::Duration;

use sysinfo::System;
use zenoh::Session;
use zensight_common::SystemInfo;
use zensight_sensor_core::identity::OsRelease;

/// Refresh period. Near-static data: the publisher cache serves late joiners
/// in between, and the registry TTL (7200 s) outlives three refreshes.
const REFRESH: Duration = Duration::from_secs(600);

/// Gather the current host facts. Cheap one-shot file reads.
pub fn collect() -> SystemInfo {
    let os = OsRelease::read_system();
    SystemInfo {
        os_pretty_name: os.pretty_name,
        os_name: os.name,
        os_id: os.id,
        os_version: os.version_id,
        os_codename: os.version_codename,
        kernel: System::kernel_version(),
        arch: Some(System::cpu_arch()),
        hostname: System::host_name(),
        boot_time_ms: Some((System::boot_time() as i64) * 1000),
        timestamp: chrono::Utc::now().timestamp_millis(),
    }
}

/// Publish the doc at startup and on the slow refresh tick, forever.
pub async fn run_publisher(session: Arc<Session>, format: zensight_sensor_core::Format) {
    let ctx =
        zensight_sensor_core::v1::V1Context::for_producer(&zensight_common::PROFILE, "sysinfo");
    let registry = zensight_sensor_core::AdvancedPublisherRegistry::new(
        session,
        ctx.telemetry_prefix(),
        format,
        zensight_sensor_core::AdvancedPublisherConfig::cache_only(1),
    )
    .with_qos(zensight_common::QosClass::HealthLiveness);
    let key: String = ctx.state_key(&["system", "info"]).into();
    loop {
        let doc = collect();
        if let Err(e) = registry.publish_serializable(&key, &doc).await {
            tracing::warn!(error = %e, "system-info publish failed");
        }
        tokio::time::sleep(REFRESH).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_reports_kernel_and_arch_on_a_real_host() {
        // The CI host is a Linux box with a kernel and an arch; os-release
        // fields may legitimately be absent in a minimal container.
        let doc = collect();
        assert!(doc.kernel.is_some());
        assert!(doc.arch.is_some());
        assert!(doc.timestamp > 0);
    }
}
