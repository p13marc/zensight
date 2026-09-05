//! Per-host overrides, in a file the daemon owns (#939).
//!
//! # Why this is not the policy file
//!
//! #902 specified `override/set` as *"persisted into the policy's `hosts`
//! section and re-planned — durable by construction"*. That does not work, and
//! the reason is the property the policy file exists for.
//!
//! `fleet-policy.json5` is hand-written, **commented**, and hand-ordered — its
//! class order *is* the overlay order. Deserializing it, mutating `hosts` and
//! re-serializing would strip every comment, including the ones explaining why
//! each class exists, and normalise the ordering. The first press of a GUI
//! button would turn a document an operator maintains into one a machine
//! emitted. That is a worse outcome than not having the feature.
//!
//! So the daemon writes a file whose **entire content it owns**, and a serde
//! round trip is lossless by construction. What that buys, beyond not
//! destroying anything:
//!
//! - the reviewable file stays exactly as written, so `git diff` on it means
//!   what it means;
//! - what a GUI adopted is visible in one place, separable from what a human
//!   decided;
//! - an adoption is reverted by deleting a file, not by un-editing a merge.
//!
//! The overrides overlay **last**, after the policy's own `hosts` section — an
//! explicit adoption is the most specific statement there is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zensight_common::desired::DesiredOverride;

/// The whole overrides file: host → `"<producer>/<topic>"` → document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overrides {
    #[serde(default)]
    pub hosts: BTreeMap<String, BTreeMap<String, Entry>>,
}

/// One recorded override, with the provenance that makes it reviewable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub doc: serde_json::Value,
    /// Who asked, from the call's `?actor=`. Recorded, never trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub at: i64,
}

impl Overrides {
    /// Load, treating a missing file as empty.
    ///
    /// Missing is the normal state — most deployments never adopt anything —
    /// so it must not be an error, or every fresh install starts by logging a
    /// failure about a file it was right not to have.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => json5::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// Write atomically: a temp file in the same directory, then a rename.
    ///
    /// A truncating write that is interrupted leaves a half-file, and the next
    /// start would refuse to parse it — which for this daemon means starting
    /// with **no overrides**, silently un-adopting every device a GUI ever
    /// added. A rename within one filesystem is the cheap way not to have that
    /// failure mode at all.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let body =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize overrides: {e}"))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp: PathBuf = path.with_extension("json5.tmp");
        std::fs::write(&tmp, format!("{HEADER}{body}\n"))
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(())
    }

    /// Apply one `override/set`. Returns whether anything changed.
    ///
    /// `doc: None` removes, which is the same deletion idiom the policy
    /// overlay uses for a field — one rule rather than two.
    pub fn apply(&mut self, o: &DesiredOverride) -> bool {
        let key = o.key();
        match &o.doc {
            Some(doc) => {
                let entry = Entry {
                    doc: doc.clone(),
                    by: o.by.clone(),
                    note: o.note.clone(),
                    at: o.at,
                };
                let slot = self.hosts.entry(o.host.clone()).or_default();
                // Compare the document only: re-recording the same adoption
                // with a new timestamp would rewrite the file, and a file that
                // changes when nothing changed is a file nobody trusts a diff
                // of.
                if slot.get(&key).map(|e| &e.doc) == Some(doc) {
                    return false;
                }
                slot.insert(key, entry);
                true
            }
            None => {
                let Some(slot) = self.hosts.get_mut(&o.host) else {
                    return false;
                };
                let removed = slot.remove(&key).is_some();
                if slot.is_empty() {
                    self.hosts.remove(&o.host);
                }
                removed
            }
        }
    }

    /// The overrides for one host, as the compiler's last overlay layer.
    pub fn for_host(&self, host: &str) -> BTreeMap<String, serde_json::Value> {
        self.hosts
            .get(host)
            .map(|m| m.iter().map(|(k, e)| (k.clone(), e.doc.clone())).collect())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

const HEADER: &str = "\
// Written by zensight-desired (#939). Edit the POLICY, not this file.
//
// These are per-host adoptions recorded through @rpc/@desired/override/set —
// what a GUI adopted, not what a human decided. They overlay LAST, after every
// class and after the policy's own `hosts` section.
//
// It is a separate file because fleet-policy.json5 is commented and
// hand-ordered, and a machine round trip would strip the comments and
// normalise the class order — which IS the overlay order. Deleting an entry
// here reverts that host to what the policy alone yields.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn over(host: &str, topic: &str, doc: Option<serde_json::Value>) -> DesiredOverride {
        DesiredOverride {
            host: host.into(),
            producer: "sysinfo".into(),
            topic: topic.into(),
            doc,
            by: Some("alice".into()),
            note: None,
            at: 1,
        }
    }

    #[test]
    fn an_override_round_trips_through_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("overrides.json5");

        let mut o = Overrides::default();
        assert!(o.apply(&over(
            "h-aaaaaaaaaaaa",
            "thresholds",
            Some(serde_json::json!({"rules": []}))
        )));
        o.save(&path).expect("save");

        let back = Overrides::load(&path).expect("load");
        assert_eq!(
            back.for_host("h-aaaaaaaaaaaa").get("sysinfo/thresholds"),
            Some(&serde_json::json!({"rules": []}))
        );
        assert_eq!(
            back.hosts["h-aaaaaaaaaaaa"]["sysinfo/thresholds"]
                .by
                .as_deref(),
            Some("alice")
        );
    }

    /// Most deployments never adopt anything, so a missing file is the normal
    /// state and must not be an error.
    #[test]
    fn a_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let o = Overrides::load(&dir.path().join("nope.json5")).expect("missing is fine");
        assert!(o.is_empty());
    }

    /// A file that changes when nothing changed is a file nobody trusts a diff
    /// of — and this one is meant to be reviewed.
    #[test]
    fn re_recording_the_same_adoption_changes_nothing() {
        let mut o = Overrides::default();
        let doc = Some(serde_json::json!({"rules": []}));
        assert!(o.apply(&over("h-aaaaaaaaaaaa", "thresholds", doc.clone())));
        let mut again = over("h-aaaaaaaaaaaa", "thresholds", doc);
        again.at = 999;
        assert!(!o.apply(&again), "a new timestamp is not a change");
    }

    #[test]
    fn a_null_doc_removes_the_entry_and_then_the_host() {
        let mut o = Overrides::default();
        o.apply(&over(
            "h-aaaaaaaaaaaa",
            "thresholds",
            Some(serde_json::json!({})),
        ));
        assert!(!o.is_empty());
        assert!(o.apply(&over("h-aaaaaaaaaaaa", "thresholds", None)));
        assert!(o.is_empty(), "an empty host leaves no husk behind");
        assert!(
            !o.apply(&over("h-aaaaaaaaaaaa", "thresholds", None)),
            "removing what is not there is a no-op, not an error"
        );
    }

    /// The written file must be loadable by the loader that reads it — a
    /// header comment is only safe because the format is JSON5.
    #[test]
    fn the_written_file_carries_its_explanation_and_still_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("overrides.json5");
        let mut o = Overrides::default();
        o.apply(&over(
            "h-aaaaaaaaaaaa",
            "thresholds",
            Some(serde_json::json!({})),
        ));
        o.save(&path).expect("save");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("Edit the POLICY, not this file"));
        Overrides::load(&path).expect("a commented file still parses");
    }
}
