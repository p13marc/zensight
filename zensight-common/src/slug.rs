//! Reading a slugged chunk back (#1153).
//!
//! A producer turns a foreign name — a mount point, an interface, a chip label
//! — into a key chunk with `zensight_sensor_core::key::device_chunk`, which is
//! `zenkey::Chunk::slug`. That encoding is **injective**: it has a documented
//! left inverse, so distinct values cannot share a chunk. The price is that a
//! value needing escapes is unreadable in the key —
//! `/var/lib/docker` becomes `x-_x2fvar_x2flib_x2fdocker`.
//!
//! **The key is an identifier; the label is the name.** Anything showing a
//! chunk to a person — a view, an exporter attribute — decodes it here, and
//! gets back the *true* value. That is strictly better than what it replaced:
//! sysinfo's old reduction produced `var_lib_docker`, which is readable and
//! ambiguous, because `/var/lib_docker` produced it too.
//!
//! These live in this crate rather than in `sensor-core` because the consumers
//! are the frontend and the exporters, and neither depends on `sensor-core`.

use zenkey::Chunk;

/// Recover the value a chunk was slugged from.
///
/// `None` when the chunk is not in the slug's image — a spelling `slug` could
/// not have produced. Prefer [`display_chunk`] when the result is going on
/// screen.
#[must_use]
pub fn unslug_for_display(chunk: &str) -> Option<String> {
    Chunk::parse(chunk).ok().and_then(|c| c.unslug())
}

/// The human-facing spelling of a chunk: the value it was slugged from, or the
/// chunk verbatim when it was never slugged.
///
/// A chunk that needed no escaping decodes to itself, so `eth0` stays `eth0`
/// and only the escaped ones change. A chunk that cannot be decoded is shown
/// as-is rather than hidden — a consumer must never silently drop a series it
/// does not recognise.
#[must_use]
pub fn display_chunk(chunk: &str) -> String {
    unslug_for_display(chunk).unwrap_or_else(|| chunk.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slugged_mount_decodes_to_the_mount() {
        assert_eq!(
            display_chunk("x-_x2fvar_x2flib_x2fdocker"),
            "/var/lib/docker"
        );
        assert_eq!(display_chunk("x-_x2f"), "/");
    }

    /// The common case is unchanged: an interface name is already a legal
    /// chunk, so it passes through both ways untouched.
    #[test]
    fn an_unescaped_chunk_is_its_own_display() {
        for c in ["eth0", "enp3s0", "nvme0n1", "coretemp", "package-0"] {
            assert_eq!(display_chunk(c), c);
        }
    }

    /// Not in the slug's image — shown verbatim, never dropped.
    #[test]
    fn an_undecodable_chunk_is_shown_as_is() {
        assert_eq!(display_chunk("x-notaslug"), "x-notaslug");
        assert_eq!(unslug_for_display("x-notaslug"), None);
    }
}
