//! Secret indirection for sensor configs (#538).
//!
//! Credentials do not belong in plaintext JSON5. A config value in any of
//! these shapes resolves at startup:
//!
//! - `${ENV_VAR}` — the environment variable's value;
//! - `file:/path` — the file's contents, trailing whitespace trimmed
//!   (systemd credentials, Kubernetes secrets, agenix, ... all mount as
//!   files);
//! - anything else — the literal value (inline stays the escape hatch).
//!
//! A missing variable or unreadable file is a **hard error**: a sensor that
//! silently polls with an empty community is worse than one that refuses to
//! start.

use crate::error::{Result, SensorError};

/// Resolve one possibly-indirect secret value.
pub fn resolve_secret(value: &str) -> Result<String> {
    if let Some(var) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
        return std::env::var(var).map_err(|_| {
            SensorError::Config(format!(
                "secret indirection ${{{var}}}: environment variable not set"
            ))
        });
    }
    if let Some(path) = value.strip_prefix("file:") {
        return std::fs::read_to_string(path)
            .map(|s| s.trim_end().to_string())
            .map_err(|e| SensorError::Config(format!("secret indirection file:{path}: {e}")));
    }
    Ok(value.to_string())
}

/// Resolve an optional secret in place.
pub fn resolve_secret_opt(value: &mut Option<String>) -> Result<()> {
    if let Some(v) = value {
        *v = resolve_secret(v)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_passes_through() {
        assert_eq!(resolve_secret("public").unwrap(), "public");
    }

    #[test]
    fn env_indirection_resolves_and_fails_loudly() {
        // SAFETY: test-local variable name, no concurrent reader cares.
        unsafe { std::env::set_var("ZENSIGHT_TEST_SECRET_538", "s3cr3t") };
        assert_eq!(
            resolve_secret("${ZENSIGHT_TEST_SECRET_538}").unwrap(),
            "s3cr3t"
        );
        let err = resolve_secret("${ZENSIGHT_TEST_SECRET_538_MISSING}").unwrap_err();
        assert!(err.to_string().contains("not set"), "{err}");
    }

    #[test]
    fn file_indirection_resolves_and_fails_loudly() {
        let dir = std::env::temp_dir().join(format!("zensight-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("community");
        std::fs::write(&path, "hunter2\n").unwrap();
        assert_eq!(
            resolve_secret(&format!("file:{}", path.display())).unwrap(),
            "hunter2"
        );
        let err = resolve_secret("file:/definitely/not/a/file").unwrap_err();
        assert!(err.to_string().contains("file:"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
