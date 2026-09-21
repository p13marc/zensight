//! The regression suite for the declarative views (#1260, design §8 phase 3).
//!
//! `specialized/bmc.rs` and `specialized/probe.rs` were the freshest and
//! best-tested hand-written views (#1126/#1127). Their simulator tests are
//! kept here, assertion for assertion, and run against the declarative
//! renderer over the bundled `views.toml` of each producer — if the
//! document cannot pass the tests written for the hand-built view, the
//! format is not ready, which is the point of choosing these. The fold-level
//! assertions on the deleted Rust (`chassis_rows`, `target_rows`) became
//! assertions on `definition::render`'s rows.

use iced_test::simulator;
use zensight::message::DeviceId;
use zensight::view::components::limit_table::LimitVerdict;
use zensight::view::definition::{Definition, render};
use zensight::view::device::{DeviceDetailState, FamilyPanel, generic_device_view};
use zensight::view::family::FamilyModel;
use zensight_common::{TelemetryPoint, TelemetryValue};

/// A device of `producer` with the bundled definition attached — what
/// `select_device` builds for a producer the fleet has not served a
/// definition for.
fn declarative(producer: &str, source: &str, metrics: &[(&str, f64)]) -> DeviceDetailState {
    let mut state = DeviceDetailState::new(DeviceId::fixture(producer, source));
    for (metric, v) in metrics {
        state.metrics.insert(
            (*metric).to_string(),
            TelemetryPoint::new(source, (*metric).to_string(), TelemetryValue::Gauge(*v)),
        );
    }
    state.family = FamilyModel::for_producer(producer);
    state.definition = Definition::bundled(producer);
    state
}

fn panel<'a>(panels: &'a [FamilyPanel], title: &str) -> &'a FamilyPanel {
    panels.iter().find(|p| p.title == title).unwrap_or_else(|| {
        panic!(
            "no panel {title:?}; have {:?}",
            panels.iter().map(|p| &p.title).collect::<Vec<_>>()
        )
    })
}

// ── bmc (was specialized/bmc.rs) ────────────────────────────────────────────

/// The issue's own example: a reading rendered without its limit reads the
/// same whether it is fine or nearly cooked. The limit must arrive beside
/// it, and it must be the BMC's number — 89 °C here, which no threshold
/// this GUI could have invented would have matched.
#[test]
fn bmc_a_reading_arrives_beside_the_limits_its_publisher_declared() {
    let state = declarative(
        "bmc",
        "bmc01",
        &[
            ("bmc01-1/thermal/inlet/celsius", 58.5),
            ("bmc01-1/thermal/inlet/upper_warning_c", 75.0),
            ("bmc01-1/thermal/inlet/upper_critical_c", 89.0),
        ],
    );
    let r = render(&state, state.definition.as_ref().unwrap());
    assert!(r.failures.is_empty(), "{:?}", r.failures);
    let thermal = panel(&r.panels, "Temperatures");
    assert_eq!(thermal.rows.len(), 1);
    assert!(thermal.rows[0].cells.iter().any(|c| c.text == "58.5°C"));
    assert_eq!(
        thermal.rows[0].limits.as_deref(),
        Some("warn 75.0°C · crit 89.0°C")
    );
    assert_eq!(thermal.rows[0].verdict, Some(LimitVerdict::Ok));

    let mut ui = simulator(generic_device_view(&state));
    assert!(ui.find("58.5°C").is_ok(), "the reading renders");
    assert!(
        ui.find("warn 75.0°C · crit 89.0°C").is_ok(),
        "and so do the limits it is graded against"
    );
}

/// A BMC that publishes a supply's rated capacity but never its draw has
/// not told us the draw is zero. "not metered" is the only honest reading,
/// and an idle machine is not the same as an unmetered one.
#[test]
fn bmc_a_supply_that_reports_only_capacity_is_not_metered() {
    let state = declarative(
        "bmc",
        "bmc01",
        &[
            ("bmc01-1/psu/1/capacity_watts", 750.0),
            ("bmc01-1/psu/1/present", 1.0),
        ],
    );
    let r = render(&state, state.definition.as_ref().unwrap());
    let psu = panel(&r.panels, "Power supplies");
    assert_eq!(psu.rows.len(), 1);
    assert!(
        psu.rows[0]
            .cells
            .iter()
            .any(|c| c.field == "input_watts" && c.text == "not metered")
    );
    assert_eq!(psu.rows[0].verdict, None, "no reading, so nothing to grade");
    assert_eq!(psu.rows[0].limits.as_deref(), Some("crit 750 W"));
    let mut ui = simulator(generic_device_view(&state));
    assert!(ui.find("not metered").is_ok());
}

/// An empty bay is not a supply drawing 0 W, and it is not graded against
/// a capacity it does not have.
#[test]
fn bmc_an_empty_bay_is_absent_rather_than_idle() {
    let state = declarative(
        "bmc",
        "bmc01",
        &[
            ("bmc01-1/psu/2/present", 0.0),
            ("bmc01-1/psu/2/capacity_watts", 750.0),
        ],
    );
    let r = render(&state, state.definition.as_ref().unwrap());
    let psu = panel(&r.panels, "Power supplies");
    assert!(
        psu.rows[0]
            .cells
            .iter()
            .any(|c| c.field == "input_watts" && c.text == "absent")
    );
    assert_eq!(psu.rows[0].verdict, None);
    assert_eq!(
        psu.rows[0].limits, None,
        "an absent bay is graded against nothing"
    );
    let mut ui = simulator(generic_device_view(&state));
    assert!(ui.find("absent").is_ok());
}

/// #1130's composite chunk carried through to the GUI: a Redfish service in
/// front of a blade enclosure fronts several chassis, and each one gets its
/// own card (`group_by = "{chassis}"`). Folding on `split('/')` position
/// would be correct here by accident; the family model's `{chassis}`
/// binding is correct because that chunk is what the sensor minted, `-`
/// and all.
#[test]
fn bmc_two_chassis_behind_one_endpoint_get_one_panel_each() {
    let state = declarative(
        "bmc",
        "bmc01",
        &[
            ("bmc01-blade1/thermal/inlet/celsius", 41.0),
            ("bmc01-blade2/thermal/inlet/celsius", 67.0),
        ],
    );
    let r = render(&state, state.definition.as_ref().unwrap());
    let cards: Vec<&FamilyPanel> = r
        .panels
        .iter()
        .filter(|p| p.title == "Temperatures")
        .collect();
    assert_eq!(cards.len(), 2, "one card per chassis, not per endpoint");
    assert_eq!(cards[0].group.as_deref(), Some("bmc01-blade1"));
    assert_eq!(cards[1].group.as_deref(), Some("bmc01-blade2"));
    assert!(cards[0].rows[0].cells.iter().any(|c| c.text == "41.0°C"));
    assert!(cards[1].rows[0].cells.iter().any(|c| c.text == "67.0°C"));

    let mut ui = simulator(generic_device_view(&state));
    assert!(ui.find("41.0°C").is_ok());
    assert!(
        ui.find("67.0°C").is_ok(),
        "the hot blade is not overwritten"
    );
}

/// The readings below a silent BMC are the last it gave. Saying so is the
/// difference between "the inlet is at 41 °C" and "the inlet was at 41 °C
/// when we last heard" — and a rack fire happens in between.
#[test]
fn bmc_a_silent_bmc_says_its_readings_are_stale() {
    let state = declarative(
        "bmc",
        "bmc01",
        &[
            ("bmc01-1/reachable", 0.0),
            ("bmc01-1/thermal/inlet/celsius", 41.0),
        ],
    );
    let mut ui = simulator(generic_device_view(&state));
    assert!(
        ui.find("The BMC did not answer this cycle — the readings below are the last it gave")
            .is_ok()
    );

    let live = declarative(
        "bmc",
        "bmc01",
        &[
            ("bmc01-1/reachable", 1.0),
            ("bmc01-1/thermal/inlet/celsius", 41.0),
        ],
    );
    let mut ui = simulator(generic_device_view(&live));
    assert!(ui.find("The BMC answered this cycle").is_ok());
}

/// The gate on the panels themselves: a device the bmc sensor has
/// published nothing for gets no panel, and one that published only
/// `reachable = 0` gets a panel that says so.
#[test]
fn bmc_the_panels_appear_for_a_bmc_that_has_spoken_at_all() {
    let silent = declarative("bmc", "bmc01", &[]);
    assert!(
        render(&silent, silent.definition.as_ref().unwrap())
            .panels
            .is_empty()
    );
    let spoke = declarative("bmc", "bmc01", &[("bmc01-1/reachable", 0.0)]);
    let r = render(&spoke, spoke.definition.as_ref().unwrap());
    assert!(
        !r.panels.is_empty(),
        "an unreachable BMC still has something to show"
    );
    assert_eq!(
        panel(&r.panels, "Chassis").rows[0].note.as_deref(),
        Some("The BMC did not answer this cycle — the readings below are the last it gave")
    );
}

/// Fans carry no verdict: the BMC publishes no fan threshold, and a number
/// this GUI invented would be a guess about somebody else's cooling. A
/// stopped fan still reads `0` — that is a measurement.
#[test]
fn bmc_fans_are_shown_without_a_verdict() {
    let state = declarative(
        "bmc",
        "bmc01",
        &[
            ("bmc01-1/fan/fan1/rpm", 4200.0),
            ("bmc01-1/fan/fan2/rpm", 0.0),
        ],
    );
    let r = render(&state, state.definition.as_ref().unwrap());
    let fans = panel(&r.panels, "Fans");
    assert_eq!(fans.rows.len(), 2);
    assert!(
        fans.rows
            .iter()
            .all(|r| r.verdict.is_none() && r.limits.is_none())
    );
    assert!(
        fans.rows
            .iter()
            .any(|r| r.cells.iter().any(|c| c.text == "0"))
    );
}

// ── probe (was specialized/probe.rs) ────────────────────────────────────────

fn vantage(host: &str, metrics: &[(&str, f64)]) -> DeviceDetailState {
    declarative("probe", host, metrics)
}

fn targets(state: &DeviceDetailState) -> Vec<zensight::view::device::FamilyRow> {
    let r = render(state, state.definition.as_ref().unwrap());
    assert!(r.failures.is_empty(), "{:?}", r.failures);
    panel(&r.panels, "Targets").rows.clone()
}

/// The sentence the sensor exists to print, and the one the eight-day
/// outage needed.
#[test]
fn probe_a_timeout_is_its_own_state_and_says_how_long_it_hung() {
    let state = vantage(
        "probe01",
        &[
            ("api-example-com/up", 0.0),
            ("api-example-com/timeout", 1.0),
            ("api-example-com/duration_ms", 20_000.0),
        ],
    );
    let rows = targets(&state);
    assert_eq!(rows[0].note.as_deref(), Some("timed out after 20.0s"));
    assert_ne!(rows[0].note.as_deref(), Some("failed"));

    let mut ui = simulator(generic_device_view(&state));
    assert!(ui.find("timed out after 20.0s").is_ok());
}

/// A refusal is not a hang. Same `up = 0`, different diagnosis, and the
/// HTTP status is what distinguishes them.
#[test]
fn probe_a_refusal_reads_differently_from_a_hang() {
    let state = vantage(
        "probe01",
        &[
            ("api-example-com/up", 0.0),
            ("api-example-com/timeout", 0.0),
            ("api-example-com/http_status", 503.0),
        ],
    );
    let rows = targets(&state);
    assert_eq!(rows[0].note.as_deref(), Some("failed (HTTP 503)"));
    let mut ui = simulator(generic_device_view(&state));
    assert!(ui.find("failed (HTTP 503)").is_ok());
}

/// Total loss is not a p95 of zero milliseconds: the sensor publishes no
/// `rtt_p95_ms` when nothing answered, and the view must not print one.
#[test]
fn probe_total_loss_is_not_a_p95_of_zero_milliseconds() {
    let state = vantage("probe01", &[("gw-lan/up", 0.0), ("gw-lan/loss_pct", 100.0)]);
    let rows = targets(&state);
    assert_eq!(
        rows[0].note.as_deref(),
        Some("no probe answered · 100% loss")
    );
    let mut ui = simulator(generic_device_view(&state));
    assert!(ui.find("no probe answered · 100% loss").is_ok());
    assert!(
        ui.find("p95 0.0 ms").is_err(),
        "a latency that does not exist is not 0.0 ms"
    );
}

/// An expired certificate is a finding, not `0 days left`.
#[test]
fn probe_an_expired_certificate_is_not_zero_days_left() {
    let state = vantage("probe01", &[("old-example-com/tls_days_to_expiry", -3.0)]);
    let rows = targets(&state);
    assert_eq!(
        rows[0]
            .note
            .as_deref()
            .map(|n| n.starts_with("EXPIRED 3 days ago")),
        Some(true),
        "{:?}",
        rows[0].note
    );
    assert_eq!(
        rows[0].verdict, None,
        "no floor grade in the vocabulary: the finding is the note, never a false `ok`"
    );
    let mut ui = simulator(generic_device_view(&state));
    assert!(ui.find("EXPIRED 3 days ago").is_ok());
    assert!(ui.find("0 days left").is_err());
}

/// Stratum 0 is a kiss-o'-death refusal, never a valid time source, and it
/// is named as such rather than shown as a number.
#[test]
fn probe_stratum_zero_is_named_as_a_refusal_not_shown_as_a_number() {
    let state = vantage(
        "probe01",
        &[
            ("ntp-example-com/up", 1.0),
            ("ntp-example-com/ntp_stratum", 0.0),
        ],
    );
    let rows = targets(&state);
    assert_eq!(
        rows[0].note.as_deref(),
        Some("kiss-o'-death: this server refuses to be a time source")
    );
    let mut ui = simulator(generic_device_view(&state));
    assert!(
        ui.find("kiss-o'-death: this server refuses to be a time source")
            .is_ok()
    );
}

/// The worst outcome leads the table: a hang, then a refusal, then the
/// targets that answered.
#[test]
fn probe_the_worst_outcome_leads_the_table() {
    let state = vantage(
        "probe01",
        &[
            ("a-ok/up", 1.0),
            ("b-failed/up", 0.0),
            ("c-hung/up", 0.0),
            ("c-hung/timeout", 1.0),
        ],
    );
    let rows = targets(&state);
    let order: Vec<&str> = rows.iter().map(|r| r.instance.as_str()).collect();
    assert_eq!(order, vec!["c-hung", "b-failed", "a-ok"]);
}

/// The vantage's own totals are the facts panel, not a target row.
#[test]
fn probe_the_vantage_totals_are_facts_not_a_target() {
    let state = vantage(
        "probe01",
        &[
            ("a/up", 1.0),
            ("targets/total", 3.0),
            ("targets/failing", 1.0),
        ],
    );
    let r = render(&state, state.definition.as_ref().unwrap());
    let facts = panel(&r.panels, "This vantage");
    assert!(!facts.is_table);
    assert!(
        facts.rows[0]
            .cells
            .iter()
            .any(|c| c.field == "failing" && c.text == "1")
    );
    assert_eq!(panel(&r.panels, "Targets").rows.len(), 1);
}

// ── pve (was overview/pve.rs) ───────────────────────────────────────────────
//
// The pve overview is a FLEET view: its rows come from every pve device at
// once, and its cluster verdict must weigh each node's own word. These are
// the fold-level tests of the deleted `backup_rows`/`aggregate`, against
// `definition::render_fleet` over the bundled document.

use zensight::view::dashboard::DeviceState;
use zensight::view::definition::render_fleet;

fn pve_device(source: &str, metrics: &[(&str, f64)]) -> DeviceState {
    let id = DeviceId::fixture("pve", source);
    let mut state = DeviceState::new(id);
    for (metric, v) in metrics {
        state.metrics.insert(
            (*metric).to_string(),
            TelemetryPoint::new(source, (*metric).to_string(), TelemetryValue::Gauge(*v)),
        );
    }
    state
}

fn pve_fleet(devices: &[DeviceState]) -> Vec<FamilyPanel> {
    let model = FamilyModel::for_producer("pve").unwrap();
    let def = Definition::bundled("pve").unwrap();
    let r = render_fleet(&model, devices.iter(), &def);
    assert!(r.failures.is_empty(), "{:?}", r.failures);
    r.panels
}

/// The whole reason the table walks guests and not backups: vmid 101 has a
/// fresh backup; vmid 102 has no `backup/*` subject at all. A table built
/// from the backup subjects would contain exactly one row — the guest that
/// is fine — and would have silently dropped the only row anyone opens this
/// table to find.
#[test]
fn pve_a_guest_with_no_backup_subject_is_the_top_row_not_a_missing_one() {
    let panels = pve_fleet(&[pve_device(
        "pve01",
        &[
            ("guest/101/running", 1.0),
            ("backup/101/age_secs", 3600.0),
            ("backup/101/ok", 1.0),
            ("guest/102/running", 1.0),
        ],
    )]);
    let rows = &panel(&panels, "Backup freshness").rows;
    assert_eq!(rows.len(), 2, "both guests get a row");
    assert_eq!(rows[0].instance, "vmid 102");
    assert_eq!(rows[0].note.as_deref(), Some("never backed up"));
    assert!(
        rows[0]
            .cells
            .iter()
            .any(|c| c.field == "backup.age_secs" && c.text == "never")
    );
    assert_eq!(rows[1].instance, "vmid 101");
    // Graded against a literal, and the row says whose number that is.
    assert_eq!(
        rows[1].note.as_deref(),
        Some("limit 172800 is the gui's, not the producer's")
    );
}

/// A stopped guest still has a disk, and that disk still wants backing up.
/// It publishes no cpu or uptime, so a row keyed on liveness would lose it.
#[test]
fn pve_a_stopped_guest_still_earns_a_backup_row() {
    let panels = pve_fleet(&[pve_device("pve01", &[("guest/103/disk_bytes", 4.2e10)])]);
    let rows = &panel(&panels, "Backup freshness").rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].instance, "vmid 103");
    assert!(
        rows[0]
            .cells
            .iter()
            .any(|c| c.field == "running" && c.text == "—")
    );
    assert_eq!(rows[0].note.as_deref(), Some("never backed up"));
}

/// A recent backup that *failed* is worse than an old one that worked, and
/// the age alone cannot say so.
#[test]
fn pve_a_fresh_failed_backup_outranks_a_merely_stale_one() {
    let panels = pve_fleet(&[pve_device(
        "pve01",
        &[
            ("guest/201/running", 1.0),
            ("backup/201/age_secs", 600.0),
            ("backup/201/ok", 0.0),
            ("guest/202/running", 1.0),
            ("backup/202/age_secs", 400_000.0),
            ("backup/202/ok", 1.0),
        ],
    )]);
    let rows = &panel(&panels, "Backup freshness").rows;
    assert_eq!(rows[0].instance, "vmid 201");
    assert!(
        rows[0]
            .note
            .as_deref()
            .is_some_and(|n| n.starts_with("last backup FAILED")),
        "{:?}",
        rows[0].note
    );
    assert_eq!(rows[1].instance, "vmid 202");
    assert_eq!(
        rows[1].verdict,
        Some(LimitVerdict::Critical),
        "4.6 days against the GUI's two-day window"
    );
}

/// One node reporting `quorate = 0` outweighs the majority still reporting
/// `1` — a split cluster's majority is not the cluster.
#[test]
fn pve_one_node_reporting_lost_quorum_outweighs_the_others() {
    let panels = pve_fleet(&[
        pve_device(
            "pve01",
            &[("cluster/quorate", 1.0), ("cluster/guests_running", 4.0)],
        ),
        pve_device(
            "pve02",
            &[("cluster/quorate", 1.0), ("cluster/guests_running", 4.0)],
        ),
        pve_device(
            "pve03",
            &[("cluster/quorate", 0.0), ("cluster/guests_running", 4.0)],
        ),
    ]);
    let cluster = panel(&panels, "Cluster");
    let cell = |f: &str| {
        cluster.rows[0]
            .cells
            .iter()
            .find(|c| c.field == f)
            .map(|c| c.text.clone())
            .unwrap_or_else(|| panic!("no fact {f}"))
    };
    assert_eq!(cell("nodes reporting"), "3");
    assert_eq!(cell("quorate"), "no — a node reports quorum lost");
    assert_eq!(cell("guests running"), "4");
}

/// A standalone node publishes no quorum at all: the answer is "no quorum
/// data", not a verdict either way.
#[test]
fn pve_a_standalone_node_reports_no_quorum_rather_than_a_verdict() {
    let panels = pve_fleet(&[pve_device("pve01", &[("cluster/guests_running", 2.0)])]);
    let cluster = panel(&panels, "Cluster");
    assert!(
        cluster.rows[0]
            .cells
            .iter()
            .any(|c| c.field == "quorate" && c.text == "no quorum data")
    );
}

/// Only overcommitted storage is listed: a pool at 0.4 is not a finding.
#[test]
fn pve_only_overcommitted_storage_is_listed() {
    let panels = pve_fleet(&[pve_device(
        "pve01",
        &[
            ("storage/local/overcommit_ratio", 0.4),
            ("storage/ceph/overcommit_ratio", 2.4),
            ("storage/ceph/used_ratio", 0.7),
        ],
    )]);
    let rows = &panel(&panels, "Overcommitted storage").rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].instance, "ceph");
    assert!(
        rows[0]
            .cells
            .iter()
            .any(|c| c.field == "overcommit_ratio" && c.text == "2.4")
    );
}

// ── container (was overview/containers.rs) ──────────────────────────────────

fn container_device(source: &str, metrics: &[(&str, f64)]) -> DeviceState {
    let id = DeviceId::fixture("container", source);
    let mut state = DeviceState::new(id);
    for (metric, v) in metrics {
        state.metrics.insert(
            (*metric).to_string(),
            TelemetryPoint::new(source, (*metric).to_string(), TelemetryValue::Gauge(*v)),
        );
    }
    state
}

fn container_fleet(devices: &[DeviceState]) -> Vec<FamilyPanel> {
    let model = FamilyModel::for_producer("container").unwrap();
    let def = Definition::bundled("container").unwrap();
    let r = render_fleet(&model, devices.iter(), &def);
    assert!(r.failures.is_empty(), "{:?}", r.failures);
    r.panels
}

fn fact(panels: &[FamilyPanel], f: &str) -> String {
    panel(panels, "Fleet").rows[0]
        .cells
        .iter()
        .find(|c| c.field == f)
        .map(|c| c.text.clone())
        .unwrap_or_else(|| panic!("no fact {f}"))
}

/// The rule this table exists to hold. `image_behind_upstream` is published
/// only when the digest was actually resolved, so its absence means nobody
/// looked. Counting `nginx` as current here would answer "is my fleet
/// patched?" with a confident yes about a container nothing checked.
#[test]
fn container_an_unchecked_image_is_never_counted_as_current() {
    let panels = container_fleet(&[container_device(
        "host01",
        &[
            ("redis/image_behind_upstream", 1.0),
            ("caddy/image_behind_upstream", 0.0),
            // No `image_behind_upstream` — the collector is off for it.
            ("nginx/memory_bytes", 1.0e8),
        ],
    )]);
    assert_eq!(fact(&panels, "containers"), "3");
    assert_eq!(fact(&panels, "image behind upstream"), "1");
    assert_eq!(
        fact(&panels, "image unchecked"),
        "1",
        "nginx is unchecked, not current"
    );
    let nginx = panel(&panels, "Containers")
        .rows
        .iter()
        .find(|r| r.instance == "host01/nginx")
        .unwrap();
    assert_eq!(nginx.note.as_deref(), Some("image not checked"));
}

/// A container name is unique on its host and nowhere else. Two hosts each
/// running `redis` are two containers, and folding them on the bare name
/// would report one host's OOM kills against the other's.
#[test]
fn container_two_hosts_running_the_same_image_are_two_containers() {
    let panels = container_fleet(&[
        container_device("host01", &[("redis/oom_kills_total", 3.0)]),
        container_device("host02", &[("redis/oom_kills_total", 0.0)]),
    ]);
    let rows = &panel(&panels, "Containers").rows;
    assert_eq!(rows.len(), 2);
    let labels: Vec<&str> = rows.iter().map(|r| r.instance.as_str()).collect();
    assert!(labels.contains(&"host01/redis"));
    assert!(labels.contains(&"host02/redis"));
    let one = rows.iter().find(|r| r.instance == "host01/redis").unwrap();
    assert!(
        one.cells
            .iter()
            .any(|c| c.field == "oom_kills_total" && c.text == "3")
    );
    assert_eq!(fact(&panels, "with OOM kills"), "1");
}

/// Drifted first, then most-OOM-killed — the triage order.
#[test]
fn container_drift_outranks_oom_kills_in_the_sort() {
    let panels = container_fleet(&[container_device(
        "host01",
        &[
            ("a/oom_kills_total", 99.0),
            ("a/image_behind_upstream", 0.0),
            ("b/image_behind_upstream", 1.0),
        ],
    )]);
    let rows = &panel(&panels, "Containers").rows;
    assert_eq!(rows[0].instance, "host01/b", "drift first");
    assert_eq!(rows[1].instance, "host01/a");
}

/// The worst of the three PSI gauges is the one shown, and it is named —
/// "PSI 40" without saying *which* pressure sends you to the wrong place.
#[test]
fn container_the_worst_pressure_gauge_is_the_one_reported_and_it_is_named() {
    let panels = container_fleet(&[container_device(
        "host01",
        &[
            ("db/cpu_pressure_avg10", 2.0),
            ("db/memory_pressure_avg10", 41.5),
            ("db/io_pressure_avg10", 8.0),
        ],
    )]);
    let rows = &panel(&panels, "Containers").rows;
    assert_eq!(
        rows[0].note.as_deref(),
        Some("image not checked · memory pressure 41.5%")
    );
}

/// A fleet nobody checked reports that, rather than "no drift".
#[test]
fn container_a_wholly_unchecked_fleet_does_not_report_a_clean_bill() {
    let panels = container_fleet(&[container_device("host01", &[("redis/memory_bytes", 1.0)])]);
    assert_eq!(fact(&panels, "image behind upstream"), "0");
    assert_eq!(
        fact(&panels, "image unchecked"),
        fact(&panels, "containers")
    );
}
