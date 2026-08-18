//! registry ⊆ *emittable*: the subject half of RFC 08 §6.1 (#648).
//!
//! [`crate::served`] does the procedure half at run time, and can, because a
//! procedure is served by a declaration the process makes once and
//! unconditionally at startup — an observable event on a known key.
//!
//! A subject has no such event, and this is the reason the two halves are not
//! symmetric:
//!
//! - **Publishers are declared lazily.** [`crate::PublisherRegistry`] declares
//!   one on the *first put* for a key, so at `introspect` time — where the
//!   procedure check runs — a perfectly healthy producer has declared nothing.
//! - **Long after startup it is still incomplete, correctly.** The served set
//!   is the intersection of "this build can emit it" with "this host has that
//!   hardware, traffic and permission this minute". A box with no WireGuard
//!   never publishes `wireguard/*`; a kernel without eBPF never publishes
//!   `sockets/tcp/connlat_us_*`. Both are right. A runtime check cannot
//!   separate a registry that lies from a host that is simply boring, so every
//!   threshold it could pick is a false-positive generator.
//!
//! What *is* observable is whether the build's mappers can produce a name in
//! each registered family — a question about code, with a test-time answer.
//! This module is that check. Each sensor supplies its own fixtures, because
//! only the sensor knows what a fully-populated sample of its input looks like.
//!
//! Producers whose telemetry tree is device-defined (a rest-var family like
//! snmp's `{device}/{metric...}`) are **exempt and say so** — see
//! [`has_catchall_telemetry`]. The check is vacuous for them: the tree is
//! whatever the polled device exposes, so there is no finite set of families to
//! cover. Passing them silently would be its own small lie.

use std::collections::BTreeSet;

/// Every telemetry subject pattern `producer`'s registry slice declares, in
/// file order (`"iface/{iface}/rx_bytes"`, …).
///
/// Empty for an unknown producer or an unparseable slice — the same silent-pass
/// posture [`crate::served::unserved_procedures`] takes, for the same reason: a
/// missing slice is a build-time error elsewhere, not this check's business.
pub fn registered_telemetry_patterns(producer: &str) -> Vec<String> {
    let Some(toml) = crate::registry::registry_toml(producer) else {
        return Vec::new();
    };
    let Ok(slice) = zenkey::parse_slice(toml) else {
        return Vec::new();
    };
    slice
        .subjects
        .iter()
        .filter(|s| s.class == "telemetry")
        .map(|s| s.path.clone())
        .collect()
}

/// Whether `producer`'s telemetry tree is a rest-var catch-all
/// (`{device}/{metric...}` — snmp, modbus, gnmi, netflow).
///
/// Family coverage is vacuous for these, so [`assert_families_covered`] refuses
/// to run rather than passing them for free.
pub fn has_catchall_telemetry(producer: &str) -> bool {
    registered_telemetry_patterns(producer)
        .iter()
        .any(|p| p.contains("..."))
}

/// Registered telemetry families that no name in `emitted` resolves to.
///
/// `pattern_of` is the producer's generated resolver — pass
/// `|m| registry::netlink::Subject::parse_metric(m).map(|s| s.pattern())`.
/// Using the generated resolver rather than matching patterns by hand is what
/// keeps this honest: it is the same code the publish path uses to decide a
/// name is registered, so the two cannot disagree about what a family is.
pub fn uncovered_families<S: AsRef<str>>(
    producer: &str,
    emitted: impl IntoIterator<Item = S>,
    pattern_of: impl Fn(&str) -> Option<&'static str>,
) -> Vec<String> {
    let covered: BTreeSet<&'static str> = emitted
        .into_iter()
        .filter_map(|m| pattern_of(m.as_ref()))
        .collect();
    registered_telemetry_patterns(producer)
        .into_iter()
        .filter(|p| !covered.contains(p.as_str()))
        .collect()
}

/// Assert every registered telemetry family is either emitted by `emitted` or
/// listed in `conditional` with a reason.
///
/// `conditional` is a **ledger**, not a suppression list: entries are
/// `(registry pattern, why this build may never emit it)`, and they are
/// themselves checked. Three ways to fail, all deliberate:
///
/// 1. a registered family nobody emits and nobody excused — the lie #648 is
///    about, and the one this whole module exists to catch;
/// 2. a `conditional` entry that *is* now emitted — the code caught up, so
///    delete the excuse;
/// 3. a `conditional` entry that is no longer registered — the TOML moved, so
///    delete the excuse.
///
/// (2) and (3) are what stop the ledger decaying into a permanent excuse for
/// whatever happened to be unimplemented the day it was written.
///
/// # Panics
///
/// Also panics for a catch-all producer, rather than passing vacuously.
#[track_caller]
pub fn assert_families_covered<S: AsRef<str>>(
    producer: &str,
    emitted: impl IntoIterator<Item = S>,
    pattern_of: impl Fn(&str) -> Option<&'static str>,
    conditional: &[(&str, &str)],
) {
    assert!(
        !has_catchall_telemetry(producer),
        "`{producer}` declares a rest-var telemetry family, so family coverage is vacuous \
         for it — the tree is whatever the polled device exposes. Do not call \
         assert_families_covered here; the forward check (crate::metric_guard) is the \
         only one that means anything for a catch-all producer."
    );

    let registered: BTreeSet<String> = registered_telemetry_patterns(producer)
        .into_iter()
        .collect();
    assert!(
        !registered.is_empty(),
        "`{producer}` has no registered telemetry subjects — wrong producer name?"
    );

    let emitted: Vec<String> = emitted
        .into_iter()
        .map(|s| s.as_ref().to_string())
        .collect();
    let covered: BTreeSet<&'static str> = emitted.iter().filter_map(|m| pattern_of(m)).collect();

    let excused: BTreeSet<&str> = conditional.iter().map(|(p, _)| *p).collect();

    // (3) the ledger cites a family the registry no longer declares.
    let stale: Vec<&str> = excused
        .iter()
        .copied()
        .filter(|p| !registered.contains(*p))
        .collect();
    assert!(
        stale.is_empty(),
        "the conditional ledger for `{producer}` excuses families that are no longer \
         registered: {stale:?}. The registry moved; delete these entries."
    );

    // (2) the ledger excuses a family this build demonstrably emits.
    let now_emitted: Vec<&str> = excused
        .iter()
        .copied()
        .filter(|p| covered.contains(*p))
        .collect();
    assert!(
        now_emitted.is_empty(),
        "the conditional ledger for `{producer}` excuses families this build DOES emit: \
         {now_emitted:?}. The code caught up; delete these entries."
    );

    // (1) the actual lie: registered, unemitted, unexcused.
    let missing: Vec<String> = registered
        .iter()
        .filter(|p| !covered.contains(p.as_str()) && !excused.contains(p.as_str()))
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "`{producer}` registers {} telemetry families this build never emits, and the \
         conditional ledger does not excuse them:\n  {}\n\n\
         `introspect` advertises each of these to the fleet as a subject this build \
         publishes (RFC 08 §6.1). Either emit them, or remove them from \
         zensight-common/registry/{producer}.toml, or — if they are genuinely \
         host-conditional — add them to the ledger WITH the condition that gates them.",
        missing.len(),
        missing.join("\n  ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catchall_producers_are_recognised() {
        // snmp's telemetry tree is `{device}/{metric...}` — device-defined.
        assert!(has_catchall_telemetry("snmp"));
        assert!(has_catchall_telemetry("modbus"));
        // netlink/sysinfo enumerate every family they publish.
        assert!(!has_catchall_telemetry("netlink"));
        assert!(!has_catchall_telemetry("sysinfo"));
    }

    #[test]
    fn registered_patterns_are_the_telemetry_half_only() {
        let pats = registered_telemetry_patterns("netlink");
        assert!(
            pats.iter().any(|p| p == "iface/{iface}/rx_bytes"),
            "expected a known netlink family, got {} patterns",
            pats.len()
        );
        // `state` and `events` subjects are a different question and not here.
        assert!(!pats.iter().any(|p| p == "health"), "{pats:?}");
        assert!(registered_telemetry_patterns("not-a-producer").is_empty());
    }

    #[test]
    fn uncovered_is_everything_nothing_emitted_resolves_to() {
        let all = registered_telemetry_patterns("netlink");
        let none: Vec<String> = Vec::new();
        assert_eq!(
            uncovered_families("netlink", &none, |_| None).len(),
            all.len(),
            "emitting nothing covers nothing"
        );
        // Resolving one name to one family covers exactly that family.
        let uncovered = uncovered_families("netlink", ["x"], |_| Some("iface/{iface}/rx_bytes"));
        assert_eq!(uncovered.len(), all.len() - 1);
        assert!(!uncovered.iter().any(|p| p == "iface/{iface}/rx_bytes"));
    }

    #[test]
    #[should_panic(expected = "family coverage is vacuous")]
    fn a_catchall_producer_is_refused_not_passed() {
        assert_families_covered("snmp", ["anything"], |_| None, &[]);
    }

    #[test]
    #[should_panic(expected = "no longer registered")]
    fn a_stale_ledger_entry_fails() {
        assert_families_covered(
            "netlink",
            Vec::<String>::new(),
            |_| None,
            &[("gone/from/the/registry", "reason")],
        );
    }

    #[test]
    #[should_panic(expected = "DOES emit")]
    fn a_ledger_entry_the_build_emits_fails() {
        assert_families_covered(
            "netlink",
            ["x"],
            |_| Some("iface/{iface}/rx_bytes"),
            &[("iface/{iface}/rx_bytes", "stale excuse")],
        );
    }

    #[test]
    #[should_panic(expected = "never emits")]
    fn an_unexcused_unemitted_family_fails() {
        assert_families_covered("netlink", Vec::<String>::new(), |_| None, &[]);
    }
}
