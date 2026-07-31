//! Shared host identity — the envelope every sensor stamps onto its
//! registration, health, alerts, and evidence (#301).
//!
//! Read once at startup (plus a slow refresh for DHCP address churn) via
//! [`SharedIdentity`]. The machine-id is **confidential** per the systemd docs
//! and never leaves the host raw: `host_id` is `sha256(machine_id || salt)` with
//! a fixed app-scoped salt, so every ZenSight sensor on the same machine derives
//! the same stable, non-reversible identifier.

use std::path::Path;
use std::sync::{Arc, RwLock};

/// App-scoped salt for the machine-id hash (the `sd_id128_get_machine_app_specific`
/// spirit). Fixed — not configurable — so every sensor on a host agrees.
/// The application salt lives in ZenSight's [`zensight_common::PROFILE`]
/// (RFC 06 §1); routed through here for the doc trail. The wire `host_id`
/// IS the v1 origin id.
fn host_id_salt() -> &'static str {
    zensight_common::PROFILE.salt()
}

/// The identity envelope for the local host.
#[derive(Debug, Clone, Default)]
pub struct HostIdentity {
    /// `hex(sha256(machine_id + salt))` — stable across boots, never the raw id.
    /// `None` if `/etc/machine-id` is unreadable.
    pub host_id: Option<String>,
    /// Kernel boot id (`/proc/sys/kernel/random/boot_id`) — changes every boot.
    pub boot_id: Option<String>,
    /// Local hostname.
    pub hostname: String,
    /// Fully-qualified name, when the hostname carries a domain (v1 heuristic:
    /// a hostname containing a dot). Richer names come from evidence merging.
    pub fqdn: Option<String>,
    /// Non-loopback, non-link-local IP addresses (sorted, deduplicated).
    pub ips: Vec<String>,
    /// Non-loopback interface MAC addresses (sorted, deduplicated).
    pub macs: Vec<String>,
    /// Container this sensor process runs in, when containerized (#311) —
    /// parsed from `/proc/self/cgroup`. A host-scoped qualifier, not identity.
    pub container_id: Option<String>,
    /// Cloud-provider facts from the opt-in IMDS probe (#311). Not detected
    /// here (file reads only) — the runner sets it via
    /// [`SharedIdentity::set_cloud`] after the async probe.
    pub cloud: Option<zensight_common::CloudFacts>,
    /// OS display name (`PRETTY_NAME`, falling back to `NAME VERSION_ID`) —
    /// descriptive, display-only; never part of identity merging.
    pub os_name: Option<String>,
    /// os-release `ID` (`fedora`, `debian`, …).
    pub os_id: Option<String>,
    /// os-release `VERSION_ID`.
    pub os_version: Option<String>,
    /// Kernel release (`/proc/sys/kernel/osrelease`).
    pub kernel: Option<String>,
    /// CPU architecture this sensor was built for (`std::env::consts::ARCH`).
    pub arch: Option<String>,
}

/// Parsed `/etc/os-release` fields (the subset ZenSight carries). The format
/// is simple `KEY=value` with optional single/double quoting — see
/// os-release(5); values ZenSight reads never use shell escapes in practice,
/// so quote-stripping is the whole grammar here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OsRelease {
    pub name: Option<String>,
    pub pretty_name: Option<String>,
    pub id: Option<String>,
    pub version_id: Option<String>,
    pub version_codename: Option<String>,
}

impl OsRelease {
    /// Read the live system's os-release: `/etc/os-release`, with the
    /// os-release(5) documented fallback to `/usr/lib/os-release`.
    pub fn read_system() -> Self {
        Self::read(os_release_path())
    }

    /// Parse an os-release-shaped file. Missing/unreadable file ⇒ all `None`
    /// — absence is expected (minimal containers), not an error.
    pub fn read(path: &Path) -> Self {
        let Ok(src) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let mut out = Self::default();
        for line in src.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, raw)) = line.split_once('=') else {
                continue;
            };
            let value = raw.trim().trim_matches('"').trim_matches('\'').to_string();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "NAME" => out.name = Some(value),
                "PRETTY_NAME" => out.pretty_name = Some(value),
                "ID" => out.id = Some(value),
                "VERSION_ID" => out.version_id = Some(value),
                "VERSION_CODENAME" => out.version_codename = Some(value),
                _ => {}
            }
        }
        out
    }

    /// The one-line display form: `PRETTY_NAME`, else `NAME VERSION_ID`,
    /// else `NAME`.
    pub fn display_name(&self) -> Option<String> {
        if let Some(pretty) = &self.pretty_name {
            return Some(pretty.clone());
        }
        match (&self.name, &self.version_id) {
            (Some(n), Some(v)) => Some(format!("{n} {v}")),
            (Some(n), None) => Some(n.clone()),
            _ => None,
        }
    }
}

/// The os-release path to read: `/etc/os-release`, with the os-release(5)
/// documented fallback to `/usr/lib/os-release`.
fn os_release_path() -> &'static Path {
    let etc = Path::new("/etc/os-release");
    if etc.exists() {
        etc
    } else {
        Path::new("/usr/lib/os-release")
    }
}

impl HostIdentity {
    /// Detect the local host's identity from the live system.
    pub fn detect() -> Self {
        let mut identity = Self::detect_from(
            Path::new("/etc/machine-id"),
            Path::new("/proc/sys/kernel/random/boot_id"),
            Path::new("/sys/class/net"),
            Path::new("/proc/self/cgroup"),
            os_release_path(),
            Path::new("/proc/sys/kernel/osrelease"),
        );
        identity.ips = detect_ips();
        identity
    }

    /// File-based detection with injectable roots (fixture-testable). Live IP
    /// enumeration is done separately in [`detect`](Self::detect); the cloud
    /// probe (network) is the runner's job.
    fn detect_from(
        machine_id: &Path,
        boot_id: &Path,
        sys_class_net: &Path,
        self_cgroup: &Path,
        os_release: &Path,
        kernel_osrelease: &Path,
    ) -> Self {
        let host_id = std::fs::read_to_string(machine_id)
            .ok()
            .and_then(|raw| hash_machine_id(&raw));
        let boot_id = std::fs::read_to_string(boot_id)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let hostname = hostname::get()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".to_string());
        let fqdn = hostname.contains('.').then(|| hostname.clone());
        let macs = detect_macs(sys_class_net);
        // Not being containerized is the common case — None is expected, not
        // an error (#311).
        let container_id = std::fs::read_to_string(self_cgroup)
            .ok()
            .and_then(|c| crate::container::container_id_from_cgroup(&c));
        let os = OsRelease::read(os_release);
        let kernel = std::fs::read_to_string(kernel_osrelease)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        HostIdentity {
            host_id,
            boot_id,
            hostname,
            fqdn,
            ips: Vec::new(),
            macs,
            container_id,
            cloud: None,
            os_name: os.display_name(),
            os_id: os.id,
            os_version: os.version_id,
            kernel,
            arch: Some(std::env::consts::ARCH.to_string()),
        }
    }
}

/// Hash a raw machine-id into the wire-safe `host_id`. Returns `None` for an
/// empty/whitespace-only input.
pub(crate) fn hash_machine_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // The v1 origin id (RFC 06 §1): payload host_id == key origin == entity
    // id, so consumers group without a correlation join (epic #453).
    Some(
        zenkey::origin::HostId::from_machine_id(trimmed, host_id_salt())
            .as_str()
            .to_string(),
    )
}

/// Non-loopback interface MACs from a `/sys/class/net`-shaped directory.
fn detect_macs(sys_class_net: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sys_class_net) else {
        return Vec::new();
    };
    let mut macs: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_name() != "lo")
        .filter_map(|e| std::fs::read_to_string(e.path().join("address")).ok())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|mac| !mac.is_empty() && mac != "00:00:00:00:00:00")
        .collect();
    macs.sort();
    macs.dedup();
    macs
}

/// Non-loopback, non-link-local local IPs via getifaddrs.
fn detect_ips() -> Vec<String> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut ips: Vec<String> = ifaces
        .into_iter()
        .filter(|i| !i.is_loopback())
        .map(|i| i.ip())
        .filter(|ip| !is_link_local(ip))
        .map(|ip| ip.to_string())
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

fn is_link_local(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Shared, refreshable identity handle. Cheap to clone; the runner refreshes it
/// on a slow timer so DHCP address churn is eventually reflected.
#[derive(Clone)]
pub struct SharedIdentity(Arc<RwLock<HostIdentity>>);

impl SharedIdentity {
    /// Detect the live host identity and wrap it for sharing.
    pub fn detect() -> Self {
        SharedIdentity(Arc::new(RwLock::new(HostIdentity::detect())))
    }

    /// Wrap a pre-built identity (tests).
    pub fn from_identity(identity: HostIdentity) -> Self {
        SharedIdentity(Arc::new(RwLock::new(identity)))
    }

    /// Snapshot the current identity.
    pub fn get(&self) -> HostIdentity {
        self.0.read().expect("identity lock poisoned").clone()
    }

    /// Re-detect from the live system (DHCP churn refresh). Cloud facts are
    /// preserved: they come from the one-shot IMDS probe, not from files, and
    /// an instance's identity never changes while it runs.
    pub fn refresh(&self) {
        let mut fresh = HostIdentity::detect();
        let mut guard = self.0.write().expect("identity lock poisoned");
        fresh.cloud = guard.cloud.take();
        *guard = fresh;
    }

    /// Attach the IMDS probe result (#311). Called once by the runner when
    /// `identity.cloud_metadata` is enabled and the probe found a provider.
    pub fn set_cloud(&self, cloud: Option<zensight_common::CloudFacts>) {
        self.0.write().expect("identity lock poisoned").cloud = cloud;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_MACHINE_ID: &str = "0123456789abcdef0123456789abcdef";
    /// `h-` + first 12 hex of sha256(FIXTURE_MACHINE_ID + host_id_salt()) — the
    /// v1 origin id (RFC 06 §1), pinned so any change to the hashing scheme
    /// (which would silently re-identify every host) fails loudly.
    const FIXTURE_HOST_ID: &str = "h-4631a192d7bd";

    #[test]
    fn hash_is_pinned_and_never_leaks_raw_machine_id() {
        let hashed = hash_machine_id(FIXTURE_MACHINE_ID).unwrap();
        assert_eq!(hashed, FIXTURE_HOST_ID);
        // The raw machine-id must not be recoverable/embedded in the wire value.
        assert!(!hashed.contains(FIXTURE_MACHINE_ID));
        // Trailing newline (the on-disk format) must not change the hash.
        assert_eq!(
            hash_machine_id(&format!("{FIXTURE_MACHINE_ID}\n")).unwrap(),
            FIXTURE_HOST_ID
        );
    }

    #[test]
    fn empty_machine_id_yields_none() {
        assert_eq!(hash_machine_id(""), None);
        assert_eq!(hash_machine_id("  \n"), None);
    }

    #[test]
    fn detect_from_fixture_tree() {
        let dir = tempfile::tempdir().unwrap();
        let machine_id = dir.path().join("machine-id");
        std::fs::write(&machine_id, format!("{FIXTURE_MACHINE_ID}\n")).unwrap();
        let boot_id = dir.path().join("boot_id");
        std::fs::write(&boot_id, "aaaabbbb-cccc-dddd-eeee-ffff00001111\n").unwrap();
        let net = dir.path().join("net");
        for (iface, addr) in [
            ("lo", "00:00:00:00:00:00"),
            ("eth0", "AA:BB:CC:DD:EE:01"),
            ("wlan0", "aa:bb:cc:dd:ee:02"),
            ("dummy0", "00:00:00:00:00:00"),
        ] {
            let d = net.join(iface);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("address"), format!("{addr}\n")).unwrap();
        }

        // Containerized fixture: a docker-scope cgroup path yields container_id.
        let container_id_hex = "ab".repeat(32);
        let cgroup = dir.path().join("cgroup");
        std::fs::write(
            &cgroup,
            format!("0::/system.slice/docker-{container_id_hex}.scope\n"),
        )
        .unwrap();

        // os-release fixture: quoted values must strip; PRETTY_NAME wins.
        let os_release = dir.path().join("os-release");
        std::fs::write(
            &os_release,
            "NAME=\"Fedora Linux\"\nPRETTY_NAME=\"Fedora Linux 42 (Workstation Edition)\"\n\
             ID=fedora\nVERSION_ID=42\nVERSION_CODENAME=\n# comment\n",
        )
        .unwrap();
        let kernel = dir.path().join("osrelease");
        std::fs::write(&kernel, "6.15.3-200.fc42.x86_64\n").unwrap();

        let id =
            HostIdentity::detect_from(&machine_id, &boot_id, &net, &cgroup, &os_release, &kernel);
        assert_eq!(id.host_id.as_deref(), Some(FIXTURE_HOST_ID));
        assert_eq!(
            id.os_name.as_deref(),
            Some("Fedora Linux 42 (Workstation Edition)")
        );
        assert_eq!(id.os_id.as_deref(), Some("fedora"));
        assert_eq!(id.os_version.as_deref(), Some("42"));
        assert_eq!(id.kernel.as_deref(), Some("6.15.3-200.fc42.x86_64"));
        assert_eq!(id.arch.as_deref(), Some(std::env::consts::ARCH));
        assert_eq!(
            id.boot_id.as_deref(),
            Some("aaaabbbb-cccc-dddd-eeee-ffff00001111")
        );
        // lo skipped by name; all-zero MACs skipped by value; lowercased + sorted.
        assert_eq!(id.macs, vec!["aa:bb:cc:dd:ee:01", "aa:bb:cc:dd:ee:02"]);
        assert!(!id.hostname.is_empty());
        assert_eq!(id.container_id.as_deref(), Some(container_id_hex.as_str()));
        // Cloud facts are never file-detected — the async probe sets them.
        assert_eq!(id.cloud, None);
    }

    #[test]
    fn detect_from_missing_files_degrades_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let id = HostIdentity::detect_from(
            &dir.path().join("nope"),
            &dir.path().join("nope2"),
            &dir.path().join("nonet"),
            &dir.path().join("nocgroup"),
            &dir.path().join("no-os-release"),
            &dir.path().join("no-osrelease"),
        );
        assert_eq!(id.host_id, None);
        assert_eq!(id.boot_id, None);
        assert!(id.macs.is_empty());
        assert_eq!(id.container_id, None);
        // A minimal container has no os-release — absence, not error.
        assert_eq!(id.os_name, None);
        assert_eq!(id.os_id, None);
        assert_eq!(id.os_version, None);
        assert_eq!(id.kernel, None);
    }

    #[test]
    fn os_release_display_name_falls_back_without_pretty_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("os-release");
        std::fs::write(
            &p,
            "NAME='Debian GNU/Linux'\nID=debian\nVERSION_ID=\"12\"\n",
        )
        .unwrap();
        let os = OsRelease::read(&p);
        assert_eq!(os.display_name().as_deref(), Some("Debian GNU/Linux 12"));
        std::fs::write(&p, "NAME=Alpine\n").unwrap();
        assert_eq!(
            OsRelease::read(&p).display_name().as_deref(),
            Some("Alpine")
        );
        assert_eq!(OsRelease::default().display_name(), None);
    }

    #[test]
    fn refresh_preserves_probed_cloud_facts() {
        // set_cloud attaches the probe result; refresh (file re-detection)
        // must not wipe it — the IMDS probe runs once, not per refresh.
        let shared = SharedIdentity::from_identity(HostIdentity::default());
        let facts = zensight_common::CloudFacts {
            provider: "aws".into(),
            instance_id: "i-0abc".into(),
            region: None,
            account: None,
        };
        shared.set_cloud(Some(facts.clone()));
        shared.refresh();
        assert_eq!(shared.get().cloud, Some(facts));
    }
}
