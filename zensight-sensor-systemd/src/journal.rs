//! Journal store health (#279, opt-in `collect.journal`).
//!
//! Unprivileged: sums the on-disk sizes of the journal files under the persistent
//! (`/var/log/journal`) and volatile (`/run/log/journal`) stores, and reads the
//! filesystem free space via `statvfs`. Both are best-effort — a store the process
//! can't read simply contributes nothing.

use std::path::Path;

/// Default journal store locations (persistent + volatile).
pub const DEFAULT_JOURNAL_PATHS: [&str; 2] = ["/var/log/journal", "/run/log/journal"];

/// Recursively sum the byte size of regular files under `paths`. Missing or
/// unreadable directories are skipped. Bounded to a sane depth to avoid pathology.
pub fn usage_bytes<P: AsRef<Path>>(paths: &[P]) -> u64 {
    let mut total = 0u64;
    for p in paths {
        total = total.saturating_add(dir_size(p.as_ref(), 0));
    }
    total
}

fn dir_size(dir: &Path, depth: u32) -> u64 {
    // Journal trees are shallow (machine-id subdirs); cap defensively.
    if depth > 8 {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            total = total.saturating_add(dir_size(&entry.path(), depth + 1));
        } else if ft.is_file()
            && let Ok(md) = entry.metadata()
        {
            total = total.saturating_add(md.len());
        }
    }
    total
}

/// Free bytes on the filesystem backing `path`, via `statvfs` (`f_bavail *
/// f_frsize`). `None` if the path can't be stat'd.
pub fn available_bytes(path: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(path).ok()?;
    Some(s.f_bavail.saturating_mul(s.f_frsize))
}

/// The first existing journal store path (for the `available_bytes` statvfs).
pub fn primary_store<P: AsRef<Path>>(paths: &[P]) -> Option<&Path> {
    paths.iter().map(|p| p.as_ref()).find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn usage_sums_nested_files() {
        let dir =
            std::env::temp_dir().join(format!("zensight-journal-test-{}", std::process::id()));
        let sub = dir.join("machine-id");
        std::fs::create_dir_all(&sub).unwrap();
        let mut f = std::fs::File::create(sub.join("system.journal")).unwrap();
        f.write_all(&vec![0u8; 4096]).unwrap();
        let mut f2 = std::fs::File::create(dir.join("user-1000.journal")).unwrap();
        f2.write_all(&vec![0u8; 1024]).unwrap();

        let total = usage_bytes(&[&dir]);
        assert_eq!(total, 4096 + 1024);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn usage_of_missing_path_is_zero() {
        assert_eq!(usage_bytes(&["/nonexistent/zensight/journal"]), 0);
    }

    #[test]
    fn available_bytes_for_temp_dir_is_some() {
        // The temp dir's filesystem always has a statvfs.
        assert!(available_bytes(&std::env::temp_dir()).is_some());
    }
}
