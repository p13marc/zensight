//! Operator decisions, in a file the correlator owns (#1102).
//!
//! # Why the catalog has a file at all
//!
//! RFC 06 §5 says the catalog is a pure function of live bus state — "no
//! private database, no migration state" — and every derived document obeys
//! that: entities, edges, incidents and aliases are recomputed from evidence on
//! every pass, so a restart loses nothing it cannot rebuild.
//!
//! **Operator writes are the exception, and they always were.** A `link`, an
//! `unlink`, an `ack` and a `silence` are *not* evidence: nothing on the bus
//! implies them, and no amount of recomputation produces one. #473 recognised
//! that and published them as ordinary catalog state so a restart would re-seed
//! them "through exactly the same path as everything else". That reasoning is
//! sound and the mechanism was not:
//!
//! - the three publish helpers used a one-shot `declare_publisher`, dropped at
//!   the end of the call — no `AdvancedPublisher` cache, so nothing on the bus
//!   held the document for a late GET;
//! - the recovery path is a history GET, and the ack and silence seeds are
//!   served by **this process**, so a restart asks itself and is answered from
//!   its own empty store;
//! - `assertion/*` had no seed queryable at all (added alongside this);
//! - and the shipped `configs/` run no router storage.
//!
//! So `docs/correlation.md`'s "a restarted correlator … re-seeds the operator's
//! decisions through the same path as every other document" was true only on a
//! deployment nobody ships. An operator ran `link old→new` to repair a
//! reinstall, somebody restarted the correlator, and the host silently split
//! back into two entities — no error, no log line.
//!
//! # A file, not redb
//!
//! The issue suggested redb. This is a JSON5 file written atomically, on the
//! shape `zensight-desired`'s `overrides.rs` already uses, because the data is
//! a handful of operator decisions rather than a time series: it is small,
//! rarely written (a human presses a button), and worth being able to read,
//! diff and delete by hand when a merge has gone wrong. A key-value store would
//! add a dependency and take away the one property that matters most when the
//! catalog has fused two machines and somebody needs to undo it at 3 a.m.
//!
//! # The file is a floor, not a source of truth
//!
//! Loaded **before** the bus seed, so a live document still wins: the catalog
//! stays a function of the bus wherever the bus has an answer, and the file
//! only supplies what the bus has forgotten.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zensight_common::ack::AlertAck;
use zensight_common::entity::OperatorAssertion;
use zensight_common::silence::Silence;

const HEADER: &str = "// Operator decisions the catalog cannot derive (#1102).\n\
                      // Written by zensight-correlator; safe to read, diff and delete.\n\
                      // Deleting it forgets every link/unlink, ack and silence.\n";

/// Everything an operator told the catalog that the bus cannot re-derive.
///
/// Lists rather than maps: every record already carries its own key
/// (`OperatorAssertion::id`, `AlertAck::alert_ref`, `Silence::id`), an
/// `AlertRef` is a struct and cannot be a JSON object key at all, and a list of
/// decisions is what somebody opening this file at 3 a.m. expects to find.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Decisions {
    /// `link` / `unlink`.
    #[serde(default)]
    pub assertions: Vec<OperatorAssertion>,
    /// Acknowledgements.
    #[serde(default)]
    pub acks: Vec<AlertAck>,
    /// Suppressions.
    #[serde(default)]
    pub silences: Vec<Silence>,
    /// Entity id lineage: current id → the ids it has superseded (#1107).
    ///
    /// Not an operator *decision*, but it belongs in the same file for the same
    /// reason: it is derived from a **transition**, and a transition is visible
    /// for exactly one recompute. Nothing on the bus after that pass implies it,
    /// so a restart cannot rebuild it — and a consumer holding a superseded id
    /// (a Grafana link, a runbook, a `fleet-policy.json5` `hosts:` key) then
    /// dangles.
    #[serde(default)]
    pub lineage: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

impl Decisions {
    /// Whether there is nothing worth writing.
    pub fn is_empty(&self) -> bool {
        self.assertions.is_empty()
            && self.acks.is_empty()
            && self.silences.is_empty()
            && self.lineage.is_empty()
    }
}

/// The file, and the two operations on it.
#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load, treating a missing file as empty.
    ///
    /// Missing is the normal state — most deployments never link anything — so
    /// it must not be an error, or every fresh install starts by logging a
    /// failure about a file it was right not to have.
    pub fn load(&self) -> Result<Decisions, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => json5::from_str(&text).map_err(|e| format!("{}: {e}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Decisions::default()),
            Err(e) => Err(format!("{}: {e}", self.path.display())),
        }
    }

    /// Write atomically: a temp file in the same directory, then a rename.
    ///
    /// A truncating write that is interrupted leaves a half-file the next start
    /// cannot parse — which for this file means starting with **no operator
    /// decisions**, silently re-splitting every host somebody linked. A rename
    /// within one filesystem is the cheap way not to have that failure mode.
    pub fn save(&self, d: &Decisions) -> Result<(), String> {
        let body = serde_json::to_string_pretty(d).map_err(|e| format!("serialize: {e}"))?;
        let dir = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = self.path.with_extension("json5.tmp");
        std::fs::write(&tmp, format!("{HEADER}{body}\n"))
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assertion(id: &str) -> OperatorAssertion {
        OperatorAssertion {
            id: id.to_string(),
            kind: zensight_common::entity::AssertionKind::Link,
            old: "h-000000000001".into(),
            new: "h-000000000002".into(),
            asserted_at: 1_000,
            note: Some("reinstalled, same box".into()),
        }
    }

    #[test]
    fn a_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path().join("never-written.json5"));
        assert_eq!(j.load().expect("missing is fine"), Decisions::default());
    }

    #[test]
    fn a_decision_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path().join("decisions.json5"));
        let d = Decisions {
            assertions: vec![assertion("link-a-b")],
            ..Default::default()
        };
        j.save(&d).expect("save");
        assert_eq!(j.load().expect("load"), d);
    }

    /// The write must not be able to leave a half-file: that would read as
    /// "no operator decisions" on the next start, silently undoing every merge
    /// somebody made by hand.
    #[test]
    fn the_write_is_atomic_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.json5");
        let j = Journal::new(&path);
        let d = Decisions {
            assertions: vec![assertion("link-a-b")],
            ..Default::default()
        };
        j.save(&d).unwrap();
        j.save(&d).unwrap();
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec!["decisions.json5".to_string()], "{left:?}");
    }

    /// The file is created under a directory that does not exist yet — the
    /// normal case for a `StateDirectory=` on a first boot.
    #[test]
    fn a_missing_directory_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::new(dir.path().join("nested/deeper/decisions.json5"));
        j.save(&Decisions::default()).expect("save into a new dir");
        assert!(j.path().exists());
    }
}
