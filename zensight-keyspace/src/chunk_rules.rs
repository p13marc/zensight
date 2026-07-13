// Chunk lexical rules (RFC 03 §2, §1.3). This file is `include!`d by both
// the library (grammar module) and build.rs, so the registry linter and the
// runtime validate with byte-identical rules.

/// RFC 03 §2: `[a-z0-9]([a-z0-9._-]*[a-z0-9])?` — lowercase, must start and
/// end alphanumeric, no wildcards, no `%`, no uppercase.
pub fn is_valid_plain_chunk(chunk: &str) -> bool {
    let bytes = chunk.as_bytes();
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    match bytes {
        [] => false,
        [one] => alnum(*one),
        [first, mid @ .., last] => {
            alnum(*first)
                && alnum(*last)
                && mid
                    .iter()
                    .all(|&b| alnum(b) || b == b'.' || b == b'_' || b == b'-')
        }
    }
}

/// RFC 03 §2: `@[a-z0-9][a-z0-9_-]*` (the `@v<int>` version form is a special
/// case of this shape).
pub fn is_valid_verbatim_chunk(chunk: &str) -> bool {
    let Some(rest) = chunk.strip_prefix('@') else {
        return false;
    };
    let bytes = rest.as_bytes();
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    match bytes {
        [] => false,
        [first, rest @ ..] => {
            alnum(*first) && rest.iter().all(|&b| alnum(b) || b == b'_' || b == b'-')
        }
    }
}

/// RFC 03 §1.3: host origins MUST match `h-[0-9a-f]{12}` exactly.
pub fn is_valid_host_origin(chunk: &str) -> bool {
    let Some(hex) = chunk.strip_prefix("h-") else {
        return false;
    };
    hex.len() == 12
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
