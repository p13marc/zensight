//! The observation layer: what the host actually looks like (#821).
//!
//! Pure parsers over strings read from `/proc` and `/etc`, plus the thinnest
//! possible I/O wrappers — so every checker in [`crate::sentinel`] is
//! unit-testable from fixtures, and the I/O surface is auditable at a glance.
//!
//! The whole layer is read-only and unprivileged: `/proc/self/mountinfo`,
//! `/proc/net/tcp{,6}`, `lstat`/`readlink`, bounded file reads, and
//! `/etc/passwd`/`/etc/group`. **Nothing here executes anything** — the
//! assertion vocabulary is closed and deliberately has no binary/command
//! kind (#821: that would be a remote-execution surface wearing a
//! monitoring hat).

use std::collections::HashMap;
use std::net::IpAddr;

/// One `/proc/self/mountinfo` row, the fields the checkers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    /// `major:minor` of the filesystem backing this mount.
    pub dev: (u32, u32),
    /// The root of the mount *within* its filesystem — what makes a bind
    /// mount recognizable: a bind of `/scratch/tmp` has `root =
    /// /scratch/tmp` (or deeper) on the same device as `/scratch`.
    pub root: String,
    pub mount_point: String,
    /// Per-mount options (field 6).
    pub mount_opts: Vec<String>,
    pub fstype: String,
    pub source: String,
    /// Per-superblock options (after the fstype).
    pub super_opts: Vec<String>,
}

/// Decode mountinfo's octal escapes (`\040` = space, `\011`, `\012`, `\134`).
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let bytes = field.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && let Ok(n) = u8::from_str_radix(&field[i + 1..i + 4], 8)
        {
            out.push(n as char);
            i += 4;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Parse `/proc/self/mountinfo`. Malformed rows are skipped (the file is
/// kernel-written; a row this cannot parse is a kernel we do not know, and
/// the caller's "path is not a mount point" answer stays honest).
///
/// Order is preserved: with overmounts, the **last** entry for a mount point
/// is the visible one, and [`visible_mount`] relies on that.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        // 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
        // (1)(2)(3)  (4)   (5)   (6)        (7...)  (-) (9)  (10)      (11)
        let Some((head, tail)) = line.split_once(" - ") else {
            continue;
        };
        let h: Vec<&str> = head.split(' ').collect();
        let t: Vec<&str> = tail.split(' ').collect();
        if h.len() < 6 || t.len() < 3 {
            continue;
        }
        let Some((maj, min)) = h[2].split_once(':') else {
            continue;
        };
        let (Ok(maj), Ok(min)) = (maj.parse(), min.parse()) else {
            continue;
        };
        out.push(MountEntry {
            dev: (maj, min),
            root: unescape(h[3]),
            mount_point: unescape(h[4]),
            mount_opts: h[5].split(',').map(str::to_string).collect(),
            fstype: t[0].to_string(),
            source: unescape(t[1]),
            super_opts: t[2].split(',').map(str::to_string).collect(),
        });
    }
    out
}

/// The visible mount at exactly `path` (last entry wins — overmounts), or
/// `None` when `path` is not a mount point.
pub fn visible_mount<'a>(mounts: &'a [MountEntry], path: &str) -> Option<&'a MountEntry> {
    mounts.iter().rev().find(|m| m.mount_point == path)
}

/// The mount *containing* `path` (longest mount-point prefix, last entry
/// winning among equals), for computing what a bind of `path` would look
/// like: same device, root = containing.root ++ (path − containing.point).
pub fn containing_mount<'a>(mounts: &'a [MountEntry], path: &str) -> Option<&'a MountEntry> {
    mounts
        .iter()
        .rev()
        .filter(|m| {
            path == m.mount_point
                || (path.starts_with(&m.mount_point)
                    && (m.mount_point == "/" || path.as_bytes()[m.mount_point.len()] == b'/'))
        })
        .max_by_key(|m| m.mount_point.len())
}

/// One listening TCP socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenEntry {
    pub addr: IpAddr,
    pub port: u16,
}

/// Parse `/proc/net/tcp` (or `tcp6` with `v6 = true`), LISTEN state only.
///
/// The address is hex, and each 32-bit group is little-endian; a v4-mapped
/// v6 address (`::ffff:a.b.c.d`) is normalized to its v4 form so an
/// expectation written for `127.0.0.1` matches a dual-stack listener bound
/// that way.
pub fn parse_proc_net_tcp(text: &str, v6: bool) -> Vec<ListenEntry> {
    let mut out = Vec::new();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // sl local_address rem_address st ...
        if cols.len() < 4 || cols[3] != "0A" {
            continue;
        }
        let Some((addr_hex, port_hex)) = cols[1].split_once(':') else {
            continue;
        };
        let Ok(port) = u16::from_str_radix(port_hex, 16) else {
            continue;
        };
        let addr = if v6 {
            if addr_hex.len() != 32 {
                continue;
            }
            let mut octets = [0u8; 16];
            let mut ok = true;
            for group in 0..4 {
                match u32::from_str_radix(&addr_hex[group * 8..group * 8 + 8], 16) {
                    // Each 32-bit group is stored little-endian.
                    Ok(word) => {
                        octets[group * 4..group * 4 + 4].copy_from_slice(&word.to_le_bytes())
                    }
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if !ok {
                continue;
            }
            let v6addr = std::net::Ipv6Addr::from(octets);
            match v6addr.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(v6addr),
            }
        } else {
            if addr_hex.len() != 8 {
                continue;
            }
            let Ok(word) = u32::from_str_radix(addr_hex, 16) else {
                continue;
            };
            IpAddr::V4(std::net::Ipv4Addr::from(word.to_le_bytes()))
        };
        out.push(ListenEntry { addr, port });
    }
    out
}

/// What `lstat` (and, for symlinks, `readlink`) said about a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    pub is_symlink: bool,
    pub is_dir: bool,
    /// Permission bits (`mode & 0o7777`).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub mtime_unix: i64,
    /// `readlink` target, symlinks only — literal, never canonicalized (an
    /// expectation asserts what the link SAYS, not what it resolves to).
    pub symlink_target: Option<String>,
}

/// Three-valued observation: the difference between "not there" and "could
/// not look" is the difference between a fact and an excuse, and the
/// checkers treat them differently (an unreadable observation is never a
/// pass — [`crate::sentinel`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation<T> {
    Present(T),
    Absent,
    Unreadable(String),
}

/// uid/gid → name tables from `/etc/passwd` and `/etc/group`.
///
/// A plain-file parse, deliberately: it is unprivileged, dependency-free and
/// fixture-testable. Hosts resolving users through NSS/LDAP should write
/// numeric ids in their expectations — stated in the crate docs.
#[derive(Debug, Clone, Default)]
pub struct IdTables {
    pub users: HashMap<u32, String>,
    pub groups: HashMap<u32, String>,
}

pub fn parse_passwd(text: &str) -> HashMap<u32, String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let _pw = f.next()?;
            let uid: u32 = f.next()?.parse().ok()?;
            Some((uid, name.to_string()))
        })
        .collect()
}

pub fn parse_group(text: &str) -> HashMap<u32, String> {
    // Same first three columns as passwd: name:pw:gid.
    parse_passwd(text)
}

// ---------------------------------------------------------------------------
// The I/O wrappers — the only impure code in the crate besides main.rs.
// ---------------------------------------------------------------------------

/// Cap on `content` reads: a file past this is **unreadable-too-large**, not
/// silently truncated (a truncated read could miss the asserted needle and
/// fire a false alarm, or find it and mask a corrupt tail — both dishonest).
pub const CONTENT_CAP: u64 = 1024 * 1024;

pub fn read_mounts() -> Observation<Vec<MountEntry>> {
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(text) => Observation::Present(parse_mountinfo(&text)),
        Err(e) => Observation::Unreadable(e.to_string()),
    }
}

/// Every TCP listener visible in THIS network namespace (`/proc/net/tcp` +
/// `tcp6`). A containerized service's listener lives in its own namespace
/// and is invisible here — documented, not worked around.
pub fn read_listeners() -> Observation<Vec<ListenEntry>> {
    let v4 = std::fs::read_to_string("/proc/net/tcp");
    let v6 = std::fs::read_to_string("/proc/net/tcp6");
    match (v4, v6) {
        (Err(e4), Err(_)) => Observation::Unreadable(e4.to_string()),
        (v4, v6) => {
            let mut out = Vec::new();
            if let Ok(t) = v4 {
                out.extend(parse_proc_net_tcp(&t, false));
            }
            if let Ok(t) = v6 {
                out.extend(parse_proc_net_tcp(&t, true));
            }
            Observation::Present(out)
        }
    }
}

pub fn observe_path(path: &str) -> Observation<FileFacts> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(md) => {
            let is_symlink = md.file_type().is_symlink();
            let symlink_target = is_symlink
                .then(|| std::fs::read_link(path).ok())
                .flatten()
                .map(|t| t.to_string_lossy().into_owned());
            Observation::Present(FileFacts {
                is_symlink,
                is_dir: md.is_dir(),
                mode: (md.mode() & 0o7777),
                uid: md.uid(),
                gid: md.gid(),
                size: md.size(),
                mtime_unix: md.mtime(),
                symlink_target,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Observation::Absent,
        Err(e) => Observation::Unreadable(e.to_string()),
    }
}

pub fn read_content_capped(path: &str) -> Observation<String> {
    use std::io::Read;
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Observation::Absent,
        Err(e) => return Observation::Unreadable(e.to_string()),
    };
    if md.len() > CONTENT_CAP {
        return Observation::Unreadable(format!(
            "{} bytes exceeds the {} byte content-check cap",
            md.len(),
            CONTENT_CAP
        ));
    }
    let mut out = String::new();
    match std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut out)) {
        Ok(_) => Observation::Present(out),
        Err(e) => Observation::Unreadable(e.to_string()),
    }
}

pub fn read_id_tables() -> IdTables {
    IdTables {
        users: std::fs::read_to_string("/etc/passwd")
            .map(|t| parse_passwd(&t))
            .unwrap_or_default(),
        groups: std::fs::read_to_string("/etc/group")
            .map(|t| parse_group(&t))
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 61 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
29 61 0:25 / /dev/shm rw,nosuid,nodev shared:4 - tmpfs tmpfs rw,inode64
61 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw,errors=remount-ro
99 61 8:16 /tmp /var/tmp rw,noatime shared:40 - ext4 /dev/sdb rw
100 61 0:25 / /var/tmp rw shared:41 - tmpfs tmpfs rw,inode64
104 61 8:16 / /scratch rw,noatime shared:44 - ext4 /dev/sdb rw
120 61 0:30 /@home /home rw,relatime shared:50 - btrfs /dev/sda3 rw,subvol=/@home
130 61 8:2 /with\\040space /mnt/with\\040space rw - ext4 /dev/sda2 rw";

    #[test]
    fn mountinfo_parses_escapes_and_optional_fields() {
        let m = parse_mountinfo(MOUNTINFO);
        assert_eq!(m.len(), 8);
        assert_eq!(m[3].dev, (8, 16));
        assert_eq!(m[3].root, "/tmp");
        assert_eq!(m[3].fstype, "ext4");
        assert!(m[3].mount_opts.contains(&"noatime".to_string()));
        // \040 escapes decode on both root-ish and mount-point fields.
        assert_eq!(m[7].mount_point, "/mnt/with space");
        // btrfs subvolume: a PLAIN mount whose root is not "/" — the bind
        // check must compute expected roots, never assume "/" means plain.
        assert_eq!(m[6].root, "/@home");
    }

    /// Overmounts: two entries for /var/tmp; the LAST is the visible one.
    #[test]
    fn overmount_last_entry_wins() {
        let m = parse_mountinfo(MOUNTINFO);
        let vis = visible_mount(&m, "/var/tmp").unwrap();
        assert_eq!(vis.fstype, "tmpfs");
    }

    #[test]
    fn containing_mount_longest_prefix() {
        let m = parse_mountinfo(MOUNTINFO);
        assert_eq!(
            containing_mount(&m, "/scratch/tmp").unwrap().mount_point,
            "/scratch"
        );
        assert_eq!(containing_mount(&m, "/etc/hosts").unwrap().mount_point, "/");
        // Exactly at a mount point.
        assert_eq!(
            containing_mount(&m, "/scratch").unwrap().mount_point,
            "/scratch"
        );
    }

    const TCP: &str = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000
   1: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0
   2: 0100007F:8124 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000";

    const TCP6: &str = "\
  sl  local_address                         remote_address                        st
   0: 00000000000000000000000000000000:1F91 00000000000000000000000000000000:0000 0A
   1: 0000000000000000FFFF00000100007F:1F92 00000000000000000000000000000000:0000 0A";

    #[test]
    fn proc_net_tcp_listen_only_le_hex() {
        let v4 = parse_proc_net_tcp(TCP, false);
        assert_eq!(v4.len(), 2, "state 0A only — the ESTABLISHED row is out");
        assert_eq!(v4[0].addr.to_string(), "127.0.0.1");
        assert_eq!(v4[0].port, 0x1F90);
        assert_eq!(v4[1].addr.to_string(), "0.0.0.0");

        let v6 = parse_proc_net_tcp(TCP6, true);
        assert_eq!(v6[0].addr.to_string(), "::");
        // v4-mapped normalizes so an expectation for 127.0.0.1 matches.
        assert_eq!(v6[1].addr.to_string(), "127.0.0.1");
        assert_eq!(v6[1].port, 0x1F92);
    }

    #[test]
    fn passwd_group_parse() {
        let users = parse_passwd(
            "root:x:0:0:root:/root:/bin/bash\ndeploy:x:998:998::/:/usr/sbin/nologin\n",
        );
        assert_eq!(users.get(&0).unwrap(), "root");
        assert_eq!(users.get(&998).unwrap(), "deploy");
    }
}
