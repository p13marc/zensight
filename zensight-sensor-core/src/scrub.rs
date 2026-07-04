//! Argv secret scrubber (#302) — Datadog-style, ON by default.
//!
//! Process command lines routinely carry secrets (`--password hunter2`,
//! `MYSQL_PWD=...`). Before a `ProcessRecord.cmdline` leaves the host — even on
//! a query channel — the *value* of any argv element whose *key* matches the
//! sensitive-word list is replaced. Complements [`redact`](crate::redact),
//! which does the same for JSON config keys.

/// Argv keys whose value is scrubbed (case-insensitive). Matched against the
/// token's key with leading dashes stripped, in both `key=value` and
/// `--key value` shapes.
pub const DEFAULT_SENSITIVE_KEYS: &[&str] = &[
    "password",
    "passwd",
    "mysql_pwd",
    "access_token",
    "auth_token",
    "api_key",
    "apikey",
    "secret",
    "credentials",
    "stripetoken",
];

/// Replacement marker for scrubbed values.
const SCRUBBED: &str = "********";

/// Hard cap on a published cmdline, bytes (bounds `@/query/processes` payloads).
pub const CMDLINE_CAP_BYTES: usize = 512;

/// Compiled scrubber: the default sensitive keys plus user-supplied
/// `custom_sensitive_words` (which may contain `*` wildcards, e.g. `*_token`).
pub struct ArgScrubber {
    /// Lowercased patterns; `*` matches any run of characters.
    patterns: Vec<String>,
}

impl ArgScrubber {
    /// Build a scrubber from the default list + custom words (with `*` globs).
    pub fn new(custom_sensitive_words: &[String]) -> Self {
        let patterns = DEFAULT_SENSITIVE_KEYS
            .iter()
            .map(|s| s.to_string())
            .chain(
                custom_sensitive_words
                    .iter()
                    .map(|s| s.to_ascii_lowercase()),
            )
            .collect();
        ArgScrubber { patterns }
    }

    fn key_matches(&self, key: &str) -> bool {
        let key = key.to_ascii_lowercase();
        self.patterns.iter().any(|p| glob_match(p, &key))
    }

    /// Scrub an argv vector. Handles both shapes:
    /// - `key=value` / `--key=value` → value replaced in place
    /// - `--key value` / `-key value` → the *next* element replaced (unless it
    ///   is itself an option)
    pub fn scrub(&self, argv: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(argv.len());
        let mut scrub_next = false;
        for token in argv {
            if scrub_next {
                scrub_next = false;
                if !token.starts_with('-') {
                    out.push(SCRUBBED.to_string());
                    continue;
                }
                // The "value" turned out to be another option — fall through and
                // process it normally (nothing was leaked).
            }
            let stripped = token.trim_start_matches('-');
            if let Some((key, _value)) = stripped.split_once('=') {
                if self.key_matches(key) {
                    // Preserve the original dashes + key, replace the value.
                    let prefix_len = token.len() - stripped.len() + key.len();
                    out.push(format!("{}={}", &token[..prefix_len], SCRUBBED));
                    continue;
                }
            } else if self.key_matches(stripped) {
                out.push(token.clone());
                scrub_next = true;
                continue;
            }
            out.push(token.clone());
        }
        out
    }

    /// Scrub + join argv into a display cmdline, truncated to `cap` bytes on a
    /// char boundary (`…` appended when truncated).
    pub fn scrub_cmdline(&self, argv: &[String], cap: usize) -> String {
        let mut s = self.scrub(argv).join(" ");
        if s.len() > cap {
            let mut end = cap;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
            s.push('…');
        }
        s
    }
}

/// Minimal `*`-glob matcher (case handled by the caller). Iterative
/// backtracking — no regex dependency.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut star_ti) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(sp) = star {
            // Backtrack: let the last `*` swallow one more char.
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scrubs_key_value_shape() {
        let s = ArgScrubber::new(&[]);
        assert_eq!(
            s.scrub(&argv(&[
                "mysqld",
                "--password=hunter2",
                "-p=x",
                "MYSQL_PWD=abc"
            ])),
            argv(&[
                "mysqld",
                "--password=********",
                "-p=x",
                "MYSQL_PWD=********"
            ])
        );
    }

    #[test]
    fn scrubs_separate_value_shape() {
        let s = ArgScrubber::new(&[]);
        assert_eq!(
            s.scrub(&argv(&["curl", "--api_key", "abc123", "-H", "x"])),
            argv(&["curl", "--api_key", "********", "-H", "x"])
        );
        // Single-dash form too.
        assert_eq!(
            s.scrub(&argv(&["tool", "-secret", "s3cr3t"])),
            argv(&["tool", "-secret", "********"])
        );
    }

    #[test]
    fn next_token_option_is_not_swallowed() {
        let s = ArgScrubber::new(&[]);
        // `--password` followed by another option: nothing to scrub, keep both.
        assert_eq!(
            s.scrub(&argv(&["tool", "--password", "--verbose"])),
            argv(&["tool", "--password", "--verbose"])
        );
    }

    #[test]
    fn case_insensitive_and_benign_preserved() {
        let s = ArgScrubber::new(&[]);
        assert_eq!(
            s.scrub(&argv(&["app", "--PassWord=X", "--port=8080"])),
            argv(&["app", "--PassWord=********", "--port=8080"])
        );
    }

    #[test]
    fn custom_words_with_wildcards() {
        let s = ArgScrubber::new(&["*_key_file".to_string(), "sess*id".to_string()]);
        assert_eq!(
            s.scrub(&argv(&[
                "app",
                "--tls_key_file=/run/k",
                "--session_id=abc",
                "--id=1"
            ])),
            argv(&[
                "app",
                "--tls_key_file=********",
                "--session_id=********",
                "--id=1"
            ])
        );
    }

    #[test]
    fn cmdline_cap_respects_char_boundaries() {
        let s = ArgScrubber::new(&[]);
        // Multibyte payload: truncation must not split a UTF-8 char.
        let long = format!("app --note={}", "é".repeat(600));
        let out = s.scrub_cmdline(&argv(&[&long]), CMDLINE_CAP_BYTES);
        assert!(out.len() <= CMDLINE_CAP_BYTES + '…'.len_utf8());
        assert!(out.ends_with('…'));
        // And an untruncated line stays untouched.
        assert_eq!(s.scrub_cmdline(&argv(&["ls", "-la"]), 512), "ls -la");
    }

    #[test]
    fn glob_matcher_basics() {
        assert!(glob_match("api_key", "api_key"));
        assert!(glob_match("*_token", "auth_token"));
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(!glob_match("a*c", "ab"));
        assert!(!glob_match("api_key", "api_keys"));
    }
}
