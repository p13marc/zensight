//! Canonical, injective slugging of foreign values into key chunks (RFC 03 §2).
//!
//! Rules:
//! - **IPs are always slugged**, even charset-legal dotted IPv4 (dotted forms
//!   are non-canonical chunks). IPv6 is canonicalized per RFC 5952 and IPv4 to
//!   minimal dotted-quad first, then `.`/`:` → `-`.
//! - Other values (unit names, filenames, config names) stay literal when
//!   already legal per the chunk charset; otherwise each excluded character is
//!   escaped losslessly as `_xNN_` (lowercase hex of the byte). Plain `-`
//!   substitution is forbidden — it is not injective.

use std::net::IpAddr;

use crate::grammar::is_valid_plain_chunk;

/// Slug an IP address (RFC 03 §2). `std`'s `Display` for `Ipv6Addr` is
/// RFC 5952-conformant (lowercase, `::` compression), so parsing + formatting
/// *is* the canonicalization.
pub fn ip_slug(ip: IpAddr) -> String {
    ip.to_string().replace(['.', ':'], "-")
}

/// Parse-and-slug a textual IP; returns `None` when the text is not an IP
/// (callers then fall back to [`chunk_slug`] for hostnames).
pub fn ip_slug_str(text: &str) -> Option<String> {
    text.parse::<IpAddr>().ok().map(ip_slug)
}

/// Slug an arbitrary value (unit name, filename, device name) into a single
/// legal chunk, losslessly (RFC 03 §2).
pub fn chunk_slug(value: &str) -> String {
    if is_valid_plain_chunk(value) {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len() + 8);
    for &b in value.as_bytes() {
        let c = b as char;
        let legal_inner =
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-';
        if legal_inner {
            out.push(c);
        } else {
            out.push_str(&format!("_x{b:02x}_"));
        }
    }
    // The escape may leave an illegal first/last byte (e.g. leading '.') —
    // guard the boundary bytes the same lossless way.
    let bytes = out.as_bytes();
    let boundary_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let fix_first = !bytes.is_empty() && !boundary_ok(bytes[0]) && bytes[0] != b'_';
    let fix_last =
        bytes.len() > 1 && !boundary_ok(bytes[bytes.len() - 1]) && bytes[bytes.len() - 1] != b'_';
    let mut fixed = String::new();
    for (i, &b) in out.as_bytes().iter().enumerate() {
        let at_first = i == 0 && fix_first;
        let at_last = i == out.len() - 1 && fix_last;
        if at_first || at_last {
            fixed.push_str(&format!("_x{b:02x}_"));
        } else {
            fixed.push(b as char);
        }
    }
    // `_xNN_` starts/ends with '_', which the charset forbids at boundaries;
    // pad with the sentinel 'e' ("escaped") on each affected side.
    let mut result = fixed;
    if result.starts_with('_') {
        result.insert(0, 'e');
    }
    if result.ends_with('_') {
        result.push('e');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ipv4_always_slugged() {
        assert_eq!(ip_slug_str("10.0.0.7").unwrap(), "10-0-0-7");
        assert_eq!(ip_slug_str("93.184.216.34").unwrap(), "93-184-216-34");
    }

    #[test]
    fn ipv6_rfc5952_canonical_before_slugging() {
        // Two spellings of one address MUST slug identically (RFC 03 §2).
        let a = ip_slug_str("2001:db8::1").unwrap();
        let b = ip_slug_str("2001:db8:0:0:0:0:0:1").unwrap();
        let c = ip_slug_str("2001:DB8::1").unwrap();
        assert_eq!(a, "2001-db8--1");
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn legal_values_stay_literal() {
        assert_eq!(chunk_slug("sshd.service"), "sshd.service");
        assert_eq!(chunk_slug("cam0"), "cam0");
    }

    #[test]
    fn escape_is_injective() {
        // The RFC's motivating counterexample: these MUST NOT share a chunk.
        let a = chunk_slug("foo@1.service");
        let b = chunk_slug("foo-1.service");
        assert_ne!(a, b);
        assert_eq!(a, "foo_x40_1.service");

        // Property check over a corpus of near-collisions.
        let corpus = [
            "getty@tty1.service",
            "getty-tty1.service",
            "a b",
            "a_b",
            "a-b",
            "A",
            "a",
            "Ab",
            "a.b",
            ".ab",
            "ab.",
            "café",
            "unit@.service",
        ];
        let slugs: Vec<String> = corpus.iter().map(|v| chunk_slug(v)).collect();
        let unique: HashSet<&String> = slugs.iter().collect();
        assert_eq!(unique.len(), corpus.len(), "collision in {slugs:?}");
        for s in &slugs {
            assert!(
                crate::grammar::is_valid_plain_chunk(s),
                "illegal slug {s:?}"
            );
        }
    }
}
