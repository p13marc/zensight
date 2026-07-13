//! Host-origin minting: `h-<12hex>` (RFC 06 §1).
//!
//! Byte-precise reference derivation — two independent implementations MUST
//! mint the same id for the same machine:
//!
//! ```text
//! input   = machine_id_hex ++ salt        (UTF-8, no separator)
//! machine_id_hex = the 32 lowercase-hex chars of /etc/machine-id, trimmed
//! origin  = "h-" ++ lowercase_hex(sha256(input))[0..12]
//! ```
//!
//! The salt is an **application constant** (RFC 06 §1); ZenSight's is
//! [`ZENSIGHT_SALT`] — compiled in, not operator-configurable, identical
//! across deployments. Changing it re-keys every fleet.

use sha2::{Digest, Sha256};
use std::fmt;
use std::io::Write as _;
use std::path::Path;

use crate::grammar::{KeyError, is_valid_host_origin};

/// ZenSight's application salt. Same value the shipped correlator has always
/// used for `host_id` — the v1 origin equals the entity id (RFC 06 §2), only
/// the separator changes (`h_` → `h-`).
pub const ZENSIGHT_SALT: &str = "zensight-host-id-v1";

/// A validated `h-<12hex>` host origin id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostId(String);

impl HostId {
    pub fn parse(s: &str) -> Result<Self, KeyError> {
        if !is_valid_host_origin(s) {
            return Err(KeyError::InvalidHostOrigin(s.to_string()));
        }
        Ok(HostId(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reference derivation from a machine-id (RFC 06 §1).
    ///
    /// `machine_id` is trimmed of surrounding whitespace/newlines (the
    /// `/etc/machine-id` file ends in a newline) and lowercased before
    /// hashing, so both file forms derive identically.
    pub fn from_machine_id(machine_id: &str, salt: &str) -> Self {
        let normalized = machine_id.trim().to_ascii_lowercase();
        Self::digest(normalized.as_bytes(), salt)
    }

    /// Fallback derivation from the most stable hardware identity available
    /// (primary MAC, serial) — RFC 06 §1.1 option 2. Same function, different
    /// input; the catalog's evidence model absorbs the confidence difference.
    pub fn from_hardware_id(hardware_id: &str, salt: &str) -> Self {
        let normalized = hardware_id.trim().to_ascii_lowercase();
        Self::digest(normalized.as_bytes(), salt)
    }

    fn digest(id_bytes: &[u8], salt: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(id_bytes);
        hasher.update(salt.as_bytes());
        let hex = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        HostId(format!("h-{}", &hex[..12]))
    }

    /// The full minting ladder (RFC 06 §1/§1.1):
    /// 1. `/etc/machine-id` (or the platform equivalent at `machine_id_path`);
    /// 2. a persisted random id at `fallback_path`, created atomically
    ///    (create-exclusive; a racing loser re-reads the winner's file);
    /// 3. as a last resort, a fresh random id persisted best-effort.
    pub fn mint(machine_id_path: &Path, fallback_path: &Path, salt: &str) -> Self {
        if let Ok(machine_id) = std::fs::read_to_string(machine_id_path) {
            let trimmed = machine_id.trim();
            if !trimmed.is_empty() {
                return Self::from_machine_id(trimmed, salt);
            }
        }
        Self::mint_persisted(fallback_path)
    }

    /// System default paths: `/etc/machine-id`, falling back to a persisted id
    /// under the application state directory.
    pub fn mint_system(fallback_path: &Path) -> Self {
        Self::mint(Path::new("/etc/machine-id"), fallback_path, ZENSIGHT_SALT)
    }

    fn mint_persisted(path: &Path) -> Self {
        // Fast path: the file exists and holds a valid id.
        if let Ok(existing) = std::fs::read_to_string(path)
            && let Ok(id) = HostId::parse(existing.trim())
        {
            return id;
        }
        let fresh = Self::random();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Atomic create-exclusive: exactly one racing producer wins; losers
        // re-read the winner's id (RFC 06 §1.1).
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut f) => {
                let _ = f.write_all(fresh.as_str().as_bytes());
                fresh
            }
            Err(_) => match std::fs::read_to_string(path) {
                Ok(existing) => HostId::parse(existing.trim()).unwrap_or(fresh),
                Err(_) => fresh,
            },
        }
    }

    /// 6 random bytes as 12 hex (RFC 06 §1.1 option 1). Not stable on its
    /// own — always persisted by [`Self::mint`].
    fn random() -> Self {
        // No rand dependency: hash process-unique entropy sources. This runs
        // once per host lifetime (then persists), so quality over speed.
        let mut hasher = Sha256::new();
        hasher.update(std::process::id().to_le_bytes());
        if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            hasher.update(now.as_nanos().to_le_bytes());
        }
        if let Ok(hn) = std::env::var("HOSTNAME") {
            hasher.update(hn.as_bytes());
        }
        let hex = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        HostId(format!("h-{}", &hex[..12]))
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 06 §1 normative test vector: implementations MUST reproduce it.
    #[test]
    fn rfc_test_vector() {
        let id = HostId::from_machine_id("b642b4217b34b1e8d3bd915fc65c4452", "example-salt-v1");
        assert_eq!(id.as_str(), "h-20609002f7b6");
    }

    #[test]
    fn machine_id_trim_and_case_are_normalized() {
        let a = HostId::from_machine_id("b642b4217b34b1e8d3bd915fc65c4452\n", "s");
        let b = HostId::from_machine_id("  B642B4217B34B1E8D3BD915FC65C4452  ", "s");
        assert_eq!(a, b);
    }

    #[test]
    fn parse_enforces_shape() {
        assert!(HostId::parse("h-20609002f7b6").is_ok());
        assert!(HostId::parse("h_20609002f7b6").is_err()); // legacy separator
        assert!(HostId::parse("h-20609002f7b").is_err()); // 11 hex
        assert!(HostId::parse("h-20609002F7B6").is_err()); // uppercase
    }

    #[test]
    fn persisted_fallback_is_stable_and_atomic() {
        let dir = std::env::temp_dir().join(format!("zsks-test-{}", std::process::id()));
        let path = dir.join("host-id");
        let _ = std::fs::remove_file(&path);
        let first = HostId::mint(Path::new("/nonexistent/machine-id"), &path, "s");
        let second = HostId::mint(Path::new("/nonexistent/machine-id"), &path, "s");
        assert_eq!(first, second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
