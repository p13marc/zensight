//! Runtime helper for the aggregated interface snapshot (feature
//! `aggregate-publishers`).
//!
//! The wire **types** (`NetIface`, `HostInterfaces`) and the **pure** helpers
//! (`iface_state`, `rate_bps`, `parse_socket_inode`, `bound_pids_by_iface`) live
//! in the zenoh-free [`zensight_aggregates`] crate so external consumers can use
//! them without pulling zenoh. This module keeps only the **impure** piece that
//! cannot be pure: scanning `/proc` to resolve socket inodes to PIDs.

use std::collections::HashMap;

use zensight_aggregates::parse_socket_inode;

/// Build a `socket inode -> owning PIDs` map by scanning `/proc/<pid>/fd`.
///
/// Best-effort: pids/fds that disappear or are unreadable mid-scan are skipped
/// (no panic, no error). Returns an empty map where `/proc` is unavailable.
/// Only compiled into the feature build, and only invoked when the aggregate is
/// produced, so it adds no cost to the default sensor.
pub fn socket_inode_pids() -> HashMap<u64, Vec<u32>> {
    let mut map: HashMap<u64, Vec<u32>> = HashMap::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return map;
    };
    for entry in proc_dir.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            if let Some(inode) = parse_socket_inode(&target.to_string_lossy()) {
                map.entry(inode).or_default().push(pid);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_inode_pids_scans_self_without_panicking() {
        // The current process owns at least one fd; the scan must succeed and
        // return a (possibly empty) map. Primarily a smoke test that the /proc
        // walk is robust to permission/disappearance errors.
        let map = socket_inode_pids();
        // Every recorded inode maps to a non-empty pid list.
        for pids in map.values() {
            assert!(!pids.is_empty());
        }
    }
}
