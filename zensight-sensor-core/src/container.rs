//! Container-id extraction from cgroup paths (#311).
//!
//! When a sensor runs inside a container, its `/proc/self/cgroup` path embeds
//! the container id in a runtime-specific shape. We parse the id out so
//! self-report evidence can carry `container_id` (the OTel `container.id`
//! semconv) as a host-scoped qualifier — "this sensor's view is from inside
//! container X". Extraction failure is **expected** (bare-metal hosts, exotic
//! runtimes, cgroup v1 without a name hierarchy) and never an error.
//!
//! Recognized shapes, per runtime (last path segment of the cgroup path):
//!
//! | runtime          | segment shape                          | id           |
//! |------------------|----------------------------------------|--------------|
//! | docker (cgroupfs)| `/docker/<64hex>`                      | the 64 hex   |
//! | docker (systemd) | `docker-<64hex>.scope`                 | the 64 hex   |
//! | containerd / CRI | `cri-containerd-<64hex>.scope`         | the 64 hex   |
//! | podman           | `libpod-<64hex>.scope`                 | the 64 hex   |
//! | systemd-nspawn   | `machine-<name>.scope`                 | the name     |
//! | generic          | any `*.scope` embedding a 64-hex run   | the 64 hex   |

/// Read the calling process's container id from `/proc/self/cgroup`.
/// `None` when not containerized (the common case) or unreadable.
pub fn detect_self_container_id() -> Option<String> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    container_id_from_cgroup(&content)
}

/// Extract a container id from the full text of a `/proc/<pid>/cgroup` file.
///
/// Scans every hierarchy line (`<id>:<controllers>:<path>`) — on cgroup v2
/// there is a single `0::` line, on v1 the container id usually appears on all
/// of them — and returns the first id any line yields.
pub fn container_id_from_cgroup(content: &str) -> Option<String> {
    content
        .lines()
        .filter_map(|line| line.splitn(3, ':').nth(2))
        .find_map(container_id_from_path)
}

/// Extract a container id from one cgroup *path* (the part after the second
/// `:` of a cgroup line, e.g. `/system.slice/docker-<id>.scope`).
pub fn container_id_from_path(path: &str) -> Option<String> {
    // Only the leaf segment names the container; parent slices never do.
    let segment = path.trim().trim_end_matches('/').rsplit('/').next()?;

    if let Some(scope) = segment.strip_suffix(".scope") {
        // systemd-managed scopes: `<prefix>-<id>.scope`. Known prefixes first
        // (they pin what the id means), then a generic 64-hex fallback so new
        // runtimes that follow the same convention still work.
        for prefix in ["cri-containerd-", "docker-", "libpod-", "crio-"] {
            if let Some(id) = scope.strip_prefix(prefix)
                && is_hex64(id)
            {
                return Some(id.to_string());
            }
        }
        // systemd-nspawn: `machine-<name>.scope` — the machine *name* is the
        // container id (nspawn has no 64-hex id).
        if let Some(name) = scope.strip_prefix("machine-")
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
        // Generic: any 64-hex run inside the scope name.
        return find_hex64(scope).map(str::to_string);
    }

    // cgroupfs driver (no systemd): `/docker/<64hex>`, `/kubepods/.../ <64hex>`
    // — the leaf segment *is* the id.
    if is_hex64(segment) {
        return Some(segment.to_string());
    }
    None
}

/// Whether `s` is exactly 64 lowercase-hex characters (a container id).
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Find a 64-hex-char run inside `s` bounded by non-hex characters (or the
/// string ends), so an 80-hex blob doesn't yield a bogus 64-char slice.
fn find_hex64(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if !bytes[start].is_ascii_hexdigit() {
            start += 1;
            continue;
        }
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
            end += 1;
        }
        if end - start == 64 {
            return Some(&s[start..end]);
        }
        start = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn docker_cgroupfs_path() {
        assert_eq!(
            container_id_from_path(&format!("/docker/{ID}")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn docker_systemd_scope() {
        assert_eq!(
            container_id_from_path(&format!("/system.slice/docker-{ID}.scope")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn cri_containerd_scope() {
        // The kubelet systemd driver shape: kubepods slices + cri-containerd scope.
        let path = format!(
            "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-podx.slice/cri-containerd-{ID}.scope"
        );
        assert_eq!(container_id_from_path(&path).as_deref(), Some(ID));
    }

    #[test]
    fn podman_libpod_scope() {
        assert_eq!(
            container_id_from_path(&format!(
                "/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-{ID}.scope"
            ))
            .as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn nspawn_machine_scope_uses_name() {
        assert_eq!(
            container_id_from_path("/machine.slice/machine-mycontainer.scope").as_deref(),
            Some("mycontainer")
        );
    }

    #[test]
    fn generic_scope_with_embedded_hex64() {
        assert_eq!(
            container_id_from_path(&format!("/a.slice/newruntime-{ID}.scope")).as_deref(),
            Some(ID)
        );
    }

    #[test]
    fn non_container_paths_yield_none() {
        // Plain services / user sessions / the root cgroup are not containers.
        assert_eq!(container_id_from_path("/system.slice/sshd.service"), None);
        assert_eq!(container_id_from_path("/user.slice/session-2.scope"), None);
        assert_eq!(container_id_from_path("/"), None);
        assert_eq!(container_id_from_path(""), None);
        // 63 or 65 hex chars are not a container id.
        assert_eq!(
            container_id_from_path(&format!("/docker/{}", &ID[..63])),
            None
        );
        assert_eq!(container_id_from_path(&format!("/docker/{ID}0")), None);
    }

    #[test]
    fn cgroup_file_v2_line() {
        let content = format!("0::/system.slice/docker-{ID}.scope\n");
        assert_eq!(container_id_from_cgroup(&content).as_deref(), Some(ID));
    }

    #[test]
    fn cgroup_file_v1_multiline_finds_id_on_any_hierarchy() {
        // v1: many hierarchy lines; the id appears on each. The name=systemd
        // line has an extra ':' inside the controller field — splitn(3) keeps
        // the full path intact.
        let content = format!(
            "12:cpuset:/docker/{ID}\n11:memory:/docker/{ID}\n1:name=systemd:/docker/{ID}\n"
        );
        assert_eq!(container_id_from_cgroup(&content).as_deref(), Some(ID));
    }

    #[test]
    fn cgroup_file_bare_metal_yields_none() {
        assert_eq!(
            container_id_from_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n"),
            None
        );
        assert_eq!(container_id_from_cgroup(""), None);
    }
}
