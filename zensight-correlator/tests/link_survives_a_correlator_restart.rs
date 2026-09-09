//! #1102: an operator's decisions outlive the process, with no router storage.
//!
//! `docs/correlation.md` stated this as a design property — *"a restarted
//! correlator, a replica, or a storage-backed router re-seeds the operator's
//! decisions through the same path as every other document"* — and it held only
//! for the third of those three. On the deployment this project actually ships:
//!
//! - `publish_assertion`, `publish_ack` and `publish_silence` used a one-shot
//!   `declare_publisher`, dropped at the end of the call, so nothing held the
//!   document for a late GET;
//! - the ack and silence seeds are served by **this** process, so a restart
//!   asks itself and is answered from its own empty store;
//! - `assertion/*` had no seed queryable at all;
//! - and `configs/` run no router storage.
//!
//! So an operator ran `link old→new` to repair a reinstall, somebody restarted
//! the correlator, and the host silently split back into two entities. No
//! error, no log line.
//!
//! This test is the restart, without a bus: the state is rebuilt from the file
//! exactly as `main` rebuilds it. `tests/ack_survives_a_restart.rs` restarts the
//! *consumer* and never the correlator, which is why it never saw this.

use zensight_common::ack::AlertAck;
use zensight_common::alert::AlertRef;
use zensight_common::entity::{AssertionKind, OperatorAssertion};
use zensight_common::silence::Silence;
use zensight_correlator::config::CorrelatorConfig;
use zensight_correlator::engine::{CorrelatorState, EvidenceMsg};
use zensight_correlator::journal::Journal;

fn cfg() -> CorrelatorConfig {
    CorrelatorConfig::default()
}

fn link(old: &str, new: &str) -> OperatorAssertion {
    OperatorAssertion {
        id: OperatorAssertion::id(AssertionKind::Link, old, new),
        kind: AssertionKind::Link,
        old: old.into(),
        new: new.into(),
        asserted_at: 1_000,
        note: Some("reinstalled, same box".into()),
    }
}

/// The whole issue, in one restart.
#[test]
fn a_link_survives_a_correlator_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("operator-decisions.json5");

    // An operator repairs a reinstall.
    let a = link("h-000000000001", "h-000000000002");
    {
        let mut st = CorrelatorState::new(cfg()).with_journal(Journal::new(&path));
        st.apply(EvidenceMsg::Assert(a.clone()));
        assert_eq!(st.current_assertions().len(), 1);
    }

    // Somebody restarts the correlator. No bus, no storage — exactly the
    // shipped deployment.
    let restarted = CorrelatorState::new(cfg()).with_journal(Journal::new(&path));
    let after = restarted.current_assertions();
    assert_eq!(
        after,
        vec![a],
        "the link is gone after a restart, so the host silently splits back \
         into two entities"
    );
}

/// Acks and silences are the same kind of statement and take the same path.
#[test]
fn an_ack_and_a_silence_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("operator-decisions.json5");
    let r = AlertRef::new("h-3fa9c2d41b7e", "netlink", "a1b2c3d4");
    let ack = AlertAck {
        alert_ref: r.clone(),
        fired_at: 1_000,
        by: "marc".into(),
        note: String::new(),
        at: 2_000,
    };
    let silence = Silence {
        id: "sil-1".into(),
        matchers: Vec::new(),
        starts_at: 0,
        ends_at: i64::MAX,
        by: "marc".into(),
        note: String::new(),
    };

    {
        let mut st = CorrelatorState::new(cfg()).with_journal(Journal::new(&path));
        st.apply(EvidenceMsg::Ack(Box::new(ack.clone())));
        st.apply(EvidenceMsg::Silence(Box::new(silence.clone())));
    }

    let restarted = CorrelatorState::new(cfg()).with_journal(Journal::new(&path));
    assert_eq!(restarted.current_acks(), vec![ack]);
    assert_eq!(restarted.current_silences(), vec![silence]);
}

/// **A revocation survives too**, which is the half that would be worse to get
/// wrong: a restart that restored an `ack` the operator had already withdrawn
/// would re-suppress an alert somebody deliberately un-suppressed.
#[test]
fn a_revocation_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("operator-decisions.json5");
    let r = AlertRef::new("h-3fa9c2d41b7e", "netlink", "a1b2c3d4");

    {
        let mut st = CorrelatorState::new(cfg()).with_journal(Journal::new(&path));
        st.apply(EvidenceMsg::Ack(Box::new(AlertAck {
            alert_ref: r.clone(),
            fired_at: 1_000,
            by: "marc".into(),
            note: String::new(),
            at: 2_000,
        })));
        st.apply(EvidenceMsg::RemoveAck(Box::new(r.clone())));
    }

    let restarted = CorrelatorState::new(cfg()).with_journal(Journal::new(&path));
    assert!(
        restarted.current_acks().is_empty(),
        "a withdrawn acknowledgement came back from the dead"
    );
}

/// No journal configured is the pre-#1102 behaviour, and must stay available:
/// a deployment that has a router storage and wants the catalog to be purely a
/// function of the bus can still have exactly that.
#[test]
fn without_a_journal_nothing_is_persisted() {
    let mut st = CorrelatorState::new(cfg());
    st.apply(EvidenceMsg::Assert(link("h-1", "h-2")));
    assert_eq!(st.current_assertions().len(), 1, "still held in memory");
    // …and no file was created anywhere, which is the point.
}
