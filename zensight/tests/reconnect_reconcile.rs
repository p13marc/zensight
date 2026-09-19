//! A reconnect reconciles the bus state it was away for (#1116).
//!
//! `AlertsSeed`, `AckReceived`, `SilenceReceived` and `IncidentReceived` all
//! only **added**; only `EntitySeed` replaced. So a two-minute blip left the
//! GUI permanently wrong: an alert resolves while disconnected, its `Resolved`
//! sample and tombstone go to a subscriber that no longer exists, and the seed
//! on reconnect returns only what is *still* firing.
//!
//! The resolved alert therefore stayed in `alerts.external` for the life of the
//! process — **counted by the badge**, drawn on the topology overlay, grouped
//! into incidents, and un-acknowledgeable.

use zensight::view::alerts::AlertsState;
use zensight_common::{Alert, AlertKind, AlertSeverity, Protocol};

const ORIGIN: &str = "h-aabbccddeeff";

fn alert(rule: &str) -> Alert {
    Alert::new(
        "web01",
        Protocol::Sysinfo,
        AlertKind::Expectation,
        rule,
        AlertSeverity::Critical,
        format!("{rule} is unhappy"),
    )
}

/// **The acceptance criterion.** One firing alert, a disconnect, the alert
/// resolves on the bus, a reconnect: the badge is 0.
#[test]
fn an_alert_resolved_during_a_blip_is_gone_after_the_reconnect() {
    let mut state = AlertsState::default();

    // Connected: one alert fires and the GUI sees it.
    state.ingest_external_from(Some(ORIGIN.to_string()), alert("disk-full"));
    assert_eq!(state.external.len(), 1, "firing before the blip");

    // …disconnect. It resolves on the bus. The `Resolved` sample and the
    // tombstone are delivered to a subscriber that no longer exists — nothing
    // reaches this process, which is the whole point.

    // Reconnect. The seed GET answers with what is STILL firing: nothing.
    state.seed_external(Vec::<(Option<String>, Alert)>::new());

    assert_eq!(
        state.external.len(),
        0,
        "an empty seed is the answer \"nothing is firing\", and it must replace \
         just as loudly as a full one — this is the badge an operator was told \
         to go and look at"
    );
}

/// A seed that still carries the alert keeps it. The replace must not be a
/// clear.
#[test]
fn an_alert_still_firing_survives_the_reseed() {
    let mut state = AlertsState::default();
    state.ingest_external_from(Some(ORIGIN.to_string()), alert("disk-full"));
    state.ingest_external_from(Some(ORIGIN.to_string()), alert("load-high"));
    assert_eq!(state.external.len(), 2);

    // One resolved during the blip, one did not.
    state.seed_external([(Some(ORIGIN.to_string()), alert("disk-full"))]);
    assert_eq!(state.external.len(), 1);
    assert!(
        state.external.values().any(|a| a.rule == "disk-full"),
        "the one still firing is kept, with its origin: {:?}",
        state.external.values().map(|a| &a.rule).collect::<Vec<_>>()
    );
}

/// Focusing on one host drops the other hosts' alerts (#1116).
///
/// `SetFocusHost` re-declares the subscription narrowed to one origin, so the
/// other forty-nine hosts have **no subscriber that can retire them**. They
/// were frozen at whatever value they held when focus was entered, and
/// un-focusing did not fix it: the liveliness replay covers only tokens that
/// are *currently alive*, so a sensor that died during focus has no transition
/// left to deliver.
#[test]
fn focusing_drops_the_hosts_that_go_out_of_scope() {
    let mut state = AlertsState::default();
    state.ingest_external_from(Some(ORIGIN.to_string()), alert("mine"));
    state.ingest_external_from(Some("h-ffffffffffff".to_string()), alert("theirs"));
    assert_eq!(state.external.len(), 2);

    state.retain_origin(ORIGIN);
    assert_eq!(state.external.len(), 1, "only the focused host's");
    assert!(state.external.values().all(|a| a.rule == "mine"));
}

/// An alert whose origin was never recorded cannot be attributed, and a
/// focused view must not carry one: it is by definition not known to be this
/// host's.
#[test]
fn an_unattributed_alert_does_not_survive_a_focus() {
    let mut state = AlertsState::default();
    state.ingest_external_from(None, alert("from-nowhere"));
    assert_eq!(state.external.len(), 1, "it is shown while unfocused");

    state.retain_origin(ORIGIN);
    assert_eq!(
        state.external.len(),
        0,
        "and not while focused on a host it cannot be shown to belong to"
    );
}

/// The seed is yielded **even when empty**, and the catalog seed with it.
///
/// A source assertion, because it is the half the state machine cannot show:
/// `seed_external` replacing correctly is worth nothing if the subscription
/// suppresses the empty snapshot that triggers it — which is exactly what
/// `if !seeded.is_empty()` did.
#[test]
fn the_empty_seed_is_not_suppressed() {
    let src = include_str!("../src/subscription.rs");
    let alerts_seed = src
        .split("yield Message::AlertsSeed(seeded);")
        .next()
        .expect("the seed is yielded");
    let tail = &alerts_seed[alerts_seed.len().saturating_sub(400)..];
    assert!(
        !tail.contains("if !seeded.is_empty()"),
        "an empty alert seed must not be suppressed — it is the answer \
         \"nothing is firing\", and suppressing it is how a resolved alert \
         survives a reconnect"
    );
    assert!(
        src.contains("yield Message::CatalogSeed("),
        "acks/silences/incidents must arrive as one snapshot per class, not as \
         a stream of additive `*Received` messages"
    );
}
