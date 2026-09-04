//! Two descriptive facts about the host this sensor runs on: who made the
//! machine, and what it runs (#935).
//!
//! # Why they were `None`
//!
//! [`crate::identity::HostIdentity`] answers *who is this host* — the machine-id
//! hash, the boot id, the addresses. Those are **identifying**: the catalog
//! joins on them. `vendor` and `platform` are **descriptive**, they are joined
//! on by nothing, and so nothing ever filled them: every sensor published
//! `vendor: None, platform: None` on its self-report, and the catalog's
//! `HostEntity` showed a self-reported host as having no vendor and no
//! platform while showing an SNMP-polled switch as having both.
//!
//! That stopped being cosmetic when fleet policy (#902) proposed selecting
//! classes of host by `platform` — a selector over a field nothing populates
//! matches nothing at all.
//!
//! # What is read, and what is deliberately not
//!
//! Only the **world-readable, descriptive** DMI files, and `/etc/os-release`.
//! `product_uuid` and `product_serial` are mode 0400 and are *identifying* —
//! they would be a second machine identity travelling beside the hashed one,
//! which is exactly what [`crate::identity`] is careful not to do (the
//! machine-id "never leaves the host raw"). This module reads neither, and
//! wants no privilege.
//!
//! `product_name` ("Standard PC (i440FX + PIIX, 1996)", "PowerEdge R740") is
//! read by nothing here: `platform` is the OS on a self-report, and there is
//! no field for a hardware model. Inventing one for a string only a tooltip
//! would show is not worth a wire change; when something needs it, it gets a
//! field of its own rather than a second meaning for this one.

use std::path::Path;

/// The descriptive pair, as this host reports them about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFacts {
    /// Hardware vendor from DMI `sys_vendor` — `"QEMU"`, `"Dell Inc."`,
    /// `"VMware, Inc."`. On a virtual machine this is the most direct
    /// statement that it *is* one.
    pub vendor: Option<String>,
    /// OS identity as `<ID>-<VERSION_ID>` from `/etc/os-release` —
    /// `"debian-13"`, `"ubuntu-24.04"` — or `"proxmox-<version>"` on a
    /// Proxmox VE node.
    pub platform: Option<String>,
}

impl HostFacts {
    /// Read both from the live system.
    pub fn detect() -> Self {
        Self::detect_from(
            Path::new("/sys/class/dmi/id"),
            Path::new("/etc/os-release"),
            Path::new("/etc/pve"),
        )
    }

    /// File-based detection with injectable roots, so the fixture tests below
    /// exercise the real parser rather than a second one — the same shape
    /// [`crate::identity::HostIdentity`] uses.
    pub fn detect_from(dmi_root: &Path, os_release: &Path, pve_marker: &Path) -> Self {
        HostFacts {
            vendor: read_trimmed(&dmi_root.join("sys_vendor")).and_then(meaningful),
            platform: platform_from(os_release, pve_marker),
        }
    }
}

/// `<ID>-<VERSION_ID>`, with Proxmox replacing the id.
///
/// **Why the version is in it.** A policy class selecting `platform` wants to
/// say "every Debian 13 host"; a bare `debian` cannot express that, and the
/// version is the half that decides whether a package name or a unit path is
/// right. A selector that wants the family globs `debian-*`, which is cheaper
/// than a class that silently stops matching after a point-release upgrade
/// because the string it pinned was the pretty name.
///
/// **Why not `PRETTY_NAME`.** `"Debian GNU/Linux 13 (trixie)"` is display text:
/// it carries spaces, parentheses and a codename that changes independently of
/// anything a policy cares about. It is used here only when `ID` is missing,
/// where a slug of it beats nothing.
///
/// **Why Proxmox is special-cased.** A PVE node's `/etc/os-release` says
/// `debian`, because it is one — and the thing a fleet needs to select on is
/// that it is a hypervisor. `/etc/pve` exists only where `pve-cluster` is
/// installed and mounted, so it is a positive statement rather than a guess
/// from a package list.
fn platform_from(os_release: &Path, pve_marker: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(os_release).ok()?;
    let mut id = None;
    let mut version_id = None;
    let mut pretty = None;
    for line in raw.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = unquote(v.trim());
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "ID" => id = Some(v),
            "VERSION_ID" => version_id = Some(v),
            "PRETTY_NAME" => pretty = Some(v),
            _ => {}
        }
    }

    let id = match id {
        Some(id) => id,
        // No `ID` at all: an os-release that thin is unusual enough that
        // saying *something* true beats saying nothing.
        None => slug(&pretty?),
    };
    let id = if pve_marker.exists() {
        "proxmox".to_string()
    } else {
        slug(&id)
    };
    Some(match version_id {
        Some(v) => format!("{id}-{}", slug(&v)),
        None => id,
    })
}

/// Strip the shell quoting `/etc/os-release` values may carry.
fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

/// Lowercase, and everything outside `[a-z0-9._]` becomes `-`.
///
/// `platform` ends up in an entity document that a policy selector matches on,
/// so it must not depend on whether a distribution capitalised its own name
/// this release.
fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            out.push(c);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn read_trimmed(p: &Path) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Reject the placeholders motherboard vendors ship instead of leaving the
/// field empty.
///
/// `"To Be Filled By O.E.M."` in a `vendor` column is worse than a blank one:
/// it looks like an answer, it groups every unbranded machine in the fleet
/// under one made-up manufacturer, and a policy class keyed on vendor would
/// happily select them all.
fn meaningful(v: String) -> Option<String> {
    const PLACEHOLDERS: &[&str] = &[
        "to be filled by o.e.m.",
        "to be filled by oem",
        "system manufacturer",
        "default string",
        "unknown",
        "not specified",
        "not applicable",
        "none",
        "o.e.m.",
        "oem",
        "chassis manufacture",
        "empty",
    ];
    let norm = v.trim().to_ascii_lowercase();
    (!PLACEHOLDERS.contains(&norm.as_str())).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(os_release: &str, sys_vendor: Option<&str>) -> (tempfile::TempDir, HostFacts) {
        let dir = tempfile::tempdir().expect("tempdir");
        let dmi = dir.path().join("dmi");
        std::fs::create_dir_all(&dmi).expect("dmi dir");
        if let Some(v) = sys_vendor {
            std::fs::write(dmi.join("sys_vendor"), v).expect("sys_vendor");
        }
        let osr = dir.path().join("os-release");
        std::fs::write(&osr, os_release).expect("os-release");
        let facts = HostFacts::detect_from(&dmi, &osr, &dir.path().join("no-pve"));
        (dir, facts)
    }

    const DEBIAN: &str = r#"PRETTY_NAME="Debian GNU/Linux 13 (trixie)"
NAME="Debian GNU/Linux"
VERSION_ID="13"
VERSION="13 (trixie)"
ID=debian
"#;

    #[test]
    fn a_debian_host_reports_its_id_and_version() {
        let (_d, f) = fixture(DEBIAN, Some("QEMU\n"));
        assert_eq!(f.platform.as_deref(), Some("debian-13"));
        assert_eq!(f.vendor.as_deref(), Some("QEMU"));
    }

    /// A PVE node's own `/etc/os-release` says `debian`, because it is one.
    /// What a fleet needs to select on is that it is a hypervisor.
    #[test]
    fn a_proxmox_node_says_proxmox_not_debian() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dmi = dir.path().join("dmi");
        std::fs::create_dir_all(&dmi).expect("dmi dir");
        let osr = dir.path().join("os-release");
        std::fs::write(&osr, DEBIAN).expect("os-release");
        let pve = dir.path().join("pve");
        std::fs::create_dir_all(&pve).expect("pve marker");

        let f = HostFacts::detect_from(&dmi, &osr, &pve);
        assert_eq!(f.platform.as_deref(), Some("proxmox-13"));
    }

    #[test]
    fn quoting_and_case_do_not_reach_the_wire() {
        let (_d, f) = fixture("ID=\"Ubuntu\"\nVERSION_ID=\"24.04\"\n", None);
        assert_eq!(f.platform.as_deref(), Some("ubuntu-24.04"));
        assert_eq!(f.vendor, None, "an absent sys_vendor is absent, not empty");
    }

    /// A placeholder in a vendor column is worse than a blank one: it looks
    /// like an answer and groups every unbranded machine under one
    /// manufacturer that does not exist.
    #[test]
    fn vendor_placeholders_are_refused() {
        for junk in [
            "To Be Filled By O.E.M.",
            "System manufacturer",
            "Default string",
            "  unknown  ",
        ] {
            let (_d, f) = fixture(DEBIAN, Some(junk));
            assert_eq!(f.vendor, None, "{junk:?} is not a vendor");
        }
        let (_d, f) = fixture(DEBIAN, Some("Dell Inc."));
        assert_eq!(f.vendor.as_deref(), Some("Dell Inc."));
    }

    #[test]
    fn a_missing_id_falls_back_to_the_pretty_name() {
        let (_d, f) = fixture("PRETTY_NAME=\"Frobnix OS 7\"\n", None);
        assert_eq!(f.platform.as_deref(), Some("frobnix-os-7"));
    }

    /// Neither file exists on a great many systems. That is not an error and
    /// must not become one.
    #[test]
    fn a_host_with_neither_file_reports_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let f = HostFacts::detect_from(
            &dir.path().join("no-dmi"),
            &dir.path().join("no-os-release"),
            &dir.path().join("no-pve"),
        );
        assert_eq!(f, HostFacts::default());
    }

    /// The identifying DMI files are mode 0400 and are a second machine
    /// identity travelling beside the hashed one. No code path here may name
    /// them.
    ///
    /// Comments may — the module header names all three to explain why they
    /// are refused, and a guard that could not tell prose from a file path
    /// would force that explanation out of the file it belongs in.
    #[test]
    fn the_identifying_dmi_files_are_never_read() {
        let src = include_str!("hostfacts.rs");
        let code: String = src
            .split("#[cfg(test)]")
            .next()
            .expect("non-test half")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in ["product_uuid", "product_serial", "chassis_serial"] {
            assert!(
                !code.contains(forbidden),
                "{forbidden} is identifying and root-only; it has no place here"
            );
        }
        // The guard is only worth having if it can still fail.
        assert!(code.contains("sys_vendor"), "the guard reads the real code");
    }
}
