//! The one slug at the key boundary (#1153).
//!
//! A sensor names things it did not choose the names of: mount points,
//! interfaces, hwmon chips, BMC endpoints, container names, exporter
//! addresses. Those become **key chunks**, and RFC 03 §1.5 is strict about
//! what a chunk may be — lowercase alphanumerics, `.`, `-`, `_`, beginning and
//! ending alphanumeric. Something has to bridge the two, and until this module
//! every crate bridged it differently:
//!
//! - `Chunk::slug` in bmc, pve, probe, parallax, systemd, container — correct;
//! - a hand-rolled `sanitize_key` in sysinfo — **lossy**, see below;
//! - a hand-rolled `exporter_slug` in netflow — `.`/`:` → `-`, which can emit
//!   a chunk that is not even legal (`::1` → `--1`, and a chunk may not begin
//!   with `-`);
//! - raw `format!` in netlink and gnmi behind a `debug_assert!`, which is
//!   **compiled out in release** — so the check that was supposed to catch a
//!   bad chunk does not run anywhere it matters.
//!
//! # Why a lossy slug is a correctness bug, not a cosmetic one
//!
//! sysinfo's `sanitize_key` mapped every run of illegal characters to a single
//! `_` and lowercased everything. It is not injective, and the collisions are
//! ordinary paths on an ordinary host:
//!
//! | These two values | …became this one chunk |
//! |---|---|
//! | `/var/lib/docker` and `/var/lib_docker` | `var_lib_docker` |
//! | `/srv/A` and `/srv/a` | `srv_a` |
//! | `/` and any path with no alphanumerics | `root` |
//! | `nvme0n1p1` and `NVME0N1P1` | `nvme0n1p1` |
//!
//! Two mounts that collide publish to **one key**. Last writer wins, every
//! interval, so one filesystem's usage is reported as another's — and nothing
//! anywhere reports a problem, because both documents are individually
//! well-formed. That is this repository's characteristic defect: a document a
//! consumer accepts and is quietly wrong.
//!
//! # What [`device_chunk`] guarantees
//!
//! It is `zenkey::Chunk::slug`, and nothing else, so the property comes from
//! the grammar crate rather than from this one: `chunk_slug` has a documented
//! **left inverse** (`chunk_unslug(&chunk_slug(v)) == Some(v)` for every `v`),
//! which is injectivity. Distinct values cannot share a chunk.
//!
//! The cost is that a value needing escapes is not readable in the key:
//! `/var/lib/docker` becomes `x-_x2fvar_x2flib_x2fdocker`. That is the RFC 03
//! §2 bargain and it is the right way round — **the key is an identifier, the
//! label is the name.** A consumer that wants to show the path calls
//! [`unslug_for_display`], which gives back `/var/lib/docker` — the *true*
//! path, which is strictly better than the old `var_lib_docker` that could
//! have been either of two mounts.
//!
//! # The second half (#1153, the API move)
//!
//! The lossy and illegal slugs above were replaced in #1249. What remained was
//! that six crates still spelled `zenkey::Chunk::slug` themselves at twenty
//! sites, and that the only production-time check on a built key —
//! `zensight_common::metric_guard` — *waved through* any key that was not a
//! v1 key at all (a malformed origin, a bare name), so the worst-formed keys
//! were invisible exactly where it mattered. (An illegal or empty chunk
//! *inside* a registered subject was already refused there, chunk by chunk —
//! #559.) Now every producer slug is [`device_chunk`] (the name says what
//! the value is, and there is one place to grep), and the guard refuses a key
//! outside the v1 grammar on every put: a `debug_assert!` in tests, a
//! once-per-key `warn!` in release — cheap, since the guard already parsed
//! the key. The typed spelling of that path is the generated
//! `zensight_common::registry` builders, and since #1274 a publisher takes
//! one (`Publisher::publish_subject`, `TelemetryPoint::for_subject`): the
//! builder slugs the foreign value itself, so a caller hands it the raw
//! value — never a chunk, which the injective slug would escape again. The
//! `&str` suffix form is the interim while the sensors move.

use zenkey::Chunk;

// The consuming half lives in `zensight-common` (#1153): the GUI and the
// exporters need it and cannot depend on this crate, while only a producer
// ever slugs. Re-exported here so one module still names the whole boundary.
pub use zensight_common::slug::{display_chunk, unslug_for_display};

/// Slug a foreign value into a key chunk. **The only way to make one.**
///
/// See the module docs for why this exists and what it guarantees. Use it for
/// anything whose spelling comes from outside this process: a mount point, an
/// interface, a chip label, an endpoint, a container name.
///
/// Do not use it for values that are already chunks by construction (a
/// producer name, a registry-declared literal) — [`Chunk::parse`] is the
/// check for those, and it *fails* rather than silently escaping a typo.
#[must_use]
pub fn device_chunk(value: impl AsRef<str>) -> Chunk {
    Chunk::slug(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The collisions that motivated this module, as a regression test.
    ///
    /// Each pair is two values an ordinary host really has. Under sysinfo's
    /// old `sanitize_key` each pair produced one chunk; under `device_chunk`
    /// no pair does.
    #[test]
    fn values_that_used_to_collide_no_longer_do() {
        for (a, b) in [
            ("/var/lib/docker", "/var/lib_docker"),
            ("/srv/A", "/srv/a"),
            ("/", "/!!!"),
            ("nvme0n1p1", "NVME0N1P1"),
            ("Package id 0", "package-id-0"),
        ] {
            assert_ne!(
                device_chunk(a).as_str(),
                device_chunk(b).as_str(),
                "{a:?} and {b:?} must not share a chunk"
            );
        }
    }

    /// Injectivity, from the property that gives it: a left inverse. If this
    /// ever fails, two devices are sharing a key somewhere.
    #[test]
    fn every_slug_round_trips_to_the_value_it_came_from() {
        for v in [
            "/",
            "/var/lib/docker",
            "/srv/A",
            "eth0",
            "eth0.100",
            "enp3s0",
            "nvme0n1",
            "coretemp",
            "Package id 0",
            "BAT0",
            "192.168.1.1",
            "::1",
            "2001:db8::1",
            "fe80::1%eth0",
            "a b c",
            "üñî",
            "",
        ] {
            let c = device_chunk(v);
            assert_eq!(
                c.unslug().as_deref(),
                Some(v),
                "{v:?} -> {} did not round-trip",
                c.as_str()
            );
            assert!(
                Chunk::parse(c.as_str()).is_ok(),
                "{v:?} -> {} is not a legal chunk",
                c.as_str()
            );
        }
    }

    /// The slug table, pinned (#1153, upstream zenkey #418).
    ///
    /// `Chunk::slug` lives in a crate this workspace pins by version. If an
    /// upstream release changes the escaping, every key derived from a foreign
    /// name moves — silently, on the next `cargo update`, for an entire fleet.
    /// This test is the tripwire: it fails in CI rather than re-keying a
    /// deployment, and a deliberate upstream change is a deliberate edit here
    /// with a migration note beside it.
    #[test]
    fn the_slug_table_is_pinned() {
        for (value, expected) in [
            ("eth0", "eth0"),
            ("enp3s0", "enp3s0"),
            ("eth0.100", "eth0.100"),
            ("package-0", "package-0"),
            ("coretemp", "coretemp"),
            ("/", "x-_x2f"),
            ("/var", "x-_x2fvar"),
            ("/var/lib/docker", "x-_x2fvar_x2flib_x2fdocker"),
            ("/var/lib_docker", "x-_x2fvar_x2flib_x5fdocker"),
            ("/srv/A", "x-_x2fsrv_x2f_x41"),
            ("/srv/a", "x-_x2fsrv_x2fa"),
            ("BAT0", "x-_x42_x41_x540"),
            ("Package id 0", "x-_x50ackage_x20id_x200"),
            ("", "x-_x"),
        ] {
            assert_eq!(
                device_chunk(value).as_str(),
                expected,
                "the slug table moved for {value:?} — see this test's doc comment"
            );
        }
    }

    /// A chunk shown to a person is the value, not the escape.
    #[test]
    fn display_gives_back_the_real_name() {
        assert_eq!(
            display_chunk("x-_x2fvar_x2flib_x2fdocker"),
            "/var/lib/docker"
        );
        assert_eq!(display_chunk("eth0"), "eth0");
        // Not in the slug's image — shown verbatim rather than hidden.
        assert_eq!(display_chunk("x-notaslug"), "x-notaslug");
    }
}
