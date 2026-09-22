//! OCI container sensor (#819) — the whole workload, previously invisible.
//!
//! Every service on the reference fleet is a Podman Quadlet container, and no
//! sensor knew what a container *was*. sysinfo's cgroups collector can be
//! pointed at explicit paths, is off by default and does not enumerate. The
//! systemd sensor sees `caddy.service` as a unit — not that the unit is Caddy
//! 2.11.4, nor which digest it runs, nor that its healthcheck has been failing
//! since the day it was deployed. netlink surfaces the podman bridges'
//! containers as eleven catalog rows with IPs and nothing else.
//!
//! Four findings of the 2026-08-28 audit are fields this sensor publishes:
//!
//! | Found by hand | Published here |
//! |---|---|
//! | garage reported `unhealthy` from deployment day while serving traffic perfectly | [`HealthState::NeverRan`] — the probe cannot run, which is not the same fact as a failing service |
//! | cosign silently signed nothing for eight days | signature presence |
//! | 12 pinned images behind upstream, surfaced by a monthly mail | a live digest comparison |
//! | the 2026-08-17 OOM blamed on "the bundle" for eleven days | per-container `memory.current`, `memory.max` and `oom_kill` |
//!
//! # Two sources, joined
//!
//! The **runtime socket** knows what a container is: image reference and
//! digest, healthcheck state, restart count, exit code, ports, mounts, and —
//! through the `PODMAN_SYSTEMD_UNIT` label — the unit that owns it, which is
//! what makes a container join up with the systemd sensor's view instead of
//! sitting beside it. The **kernel** knows what it is doing: cgroup-v2 memory,
//! CPU, throttling, OOM counts and PSI.
//!
//! # What it deliberately is not
//!
//! - **No action surface.** The socket client has two methods, both GETs.
//!   Stopping a container is a different threat model (see the pve sensor,
//!   #818, for the same argument).
//! - **No egress by default.** Exactly one collector leaves the host — the
//!   upstream-digest and signature checks — and it is off, and when on it is
//!   restricted to a named registry allowlist. `validate()` refuses to start
//!   with egress enabled and no allowlist.
//! - **No credentials.** Registry requests are anonymous; a private registry
//!   answers 401 and the result is "not checked", which is honest, rather than
//!   "unsigned", which would not be.

pub mod alerts;
pub mod cgroup;
pub mod config;
pub mod inspect;
pub mod poller;
pub mod runtime;
pub mod upstream;

#[cfg(test)]
mod typed_subjects {
    use zensight_common::registry::container::Subject;
    use zensight_common::subject::TelemetrySubject;

    /// The generated subjects render the tails this sensor published by hand
    /// (#1274): byte-identical keys, so every consumer's series carries over.
    /// The counter-as-gauge guard (#1071) now runs in
    /// `TelemetryPoint::for_subject` for every sensor — see
    /// `zensight_common::subject::tests`.
    #[test]
    fn the_registered_families_render_their_tails() {
        for (subject, tail) in [
            (Subject::memory_bytes("caddy"), "caddy/memory_bytes"),
            (Subject::healthy("caddy"), "caddy/healthy"),
            (Subject::oom_kills_total("caddy"), "caddy/oom_kills_total"),
            (Subject::restart_count("caddy"), "caddy/restart_count"),
            (Subject::ContainersTotal, "containers/total"),
        ] {
            assert_eq!(subject.tail(), tail);
        }
        let chunk = zensight_sensor_core::key::device_chunk("zensight-sensor-logs");
        assert_eq!(
            Subject::pids("zensight-sensor-logs").tail(),
            format!("{}/pids", chunk.as_str())
        );
    }
}
