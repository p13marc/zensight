//! Out-of-band hardware health, as the BMC reports it (#1127).
//!
//! The bmc sensor publishes `thermal/{s}/celsius` with
//! `upper_warning_c`/`upper_critical_c` as **sibling subjects**, `fan/{f}/rpm`,
//! `psu/{p}/input_watts` beside `capacity_watts` and `present`, and a chassis
//! rollup — and nothing in the GUI read any of it. A fan at 12 000 RPM against
//! a 14 000 limit rendered like one at 3 000, because neither rendered at all.
//!
//! **Every verdict here is the BMC's.** The thresholds come off the bus as the
//! hardware's own, which is the only kind that means anything: they are set by
//! the vendor against the board they are measuring. This view compares a
//! reading with a limit and colours the result; it never invents a limit, and
//! a reading with none declared is shown plainly. That is the same rule the
//! sensor is built on, held one layer up.
//!
//! One panel per **chassis**, not per endpoint (#1130): a Redfish service in
//! front of a blade enclosure fronts several, and the `{chassis}` chunk is
//! `{endpoint}-{id}` precisely so they do not overwrite each other.

use std::collections::BTreeMap;

use iced::widget::{Column, column, scrollable, text};
use iced::{Element, Length};

use zensight_common::TelemetryValue;
use zensight_common::registry::bmc::Subject;

use crate::message::Message;
use crate::view::components::{LimitRow, card, limit_table, section_header};
use crate::view::device::DeviceDetailState;
use crate::view::tokens::{font, space};

/// Whether this device has anything a BMC panel would show.
#[must_use]
pub fn has_bmc_readings(state: &DeviceDetailState) -> bool {
    state
        .metrics
        .keys()
        .any(|k| Subject::parse_metric(k).is_some())
}

/// One power supply, as its four sibling subjects arrive.
///
/// Every field is an `Option` because they arrive as separate samples and a
/// BMC may publish any subset: a supply that reports its rating but not its
/// draw is ordinary, and is not a supply drawing nothing.
#[derive(Debug, Default)]
struct PsuReadings {
    input_watts: Option<f64>,
    capacity_watts: Option<f64>,
    present: Option<bool>,
}

/// One thermal sensor and the two limits its BMC declared for it.
#[derive(Debug, Default)]
struct ThermalReadings {
    celsius: Option<f64>,
    upper_warning_c: Option<f64>,
    upper_critical_c: Option<f64>,
}

/// What one chassis told us this cycle.
#[derive(Debug, Default)]
struct ChassisReadings {
    psu: BTreeMap<String, PsuReadings>,
    /// `fan id -> rpm`.
    fan: BTreeMap<String, f64>,
    thermal: BTreeMap<String, ThermalReadings>,
    /// `1` when the BMC answered this cycle, `0` when it did not.
    reachable: Option<f64>,
}

fn value(state: &DeviceDetailState, metric: &str) -> Option<f64> {
    state.metrics.get(metric).and_then(|p| match &p.value {
        TelemetryValue::Counter(v) => Some(*v as f64),
        TelemetryValue::Gauge(v) => Some(*v),
        _ => None,
    })
}

/// Fold this device's metrics into one entry per chassis.
///
/// Through the registry's typed subjects rather than `split('/')`: the
/// `{chassis}` chunk is `{endpoint}-{id}` and contains a `-`, so reading it
/// positionally is how a two-chassis enclosure gets mixed back together.
fn fold(state: &DeviceDetailState) -> BTreeMap<String, ChassisReadings> {
    let mut out: BTreeMap<String, ChassisReadings> = BTreeMap::new();
    for key in state.metrics.keys() {
        let Some(subject) = Subject::parse_metric(key) else {
            continue;
        };
        let v = value(state, key);
        match subject {
            Subject::PsuInputWatts { chassis, psu } => {
                out.entry(chassis.to_string())
                    .or_default()
                    .psu
                    .entry(psu.to_string())
                    .or_default()
                    .input_watts = v;
            }
            Subject::PsuCapacityWatts { chassis, psu } => {
                out.entry(chassis.to_string())
                    .or_default()
                    .psu
                    .entry(psu.to_string())
                    .or_default()
                    .capacity_watts = v;
            }
            Subject::PsuPresent { chassis, psu } => {
                out.entry(chassis.to_string())
                    .or_default()
                    .psu
                    .entry(psu.to_string())
                    .or_default()
                    .present = v.map(|n| n != 0.0);
            }
            Subject::FanRpm { chassis, fan } => {
                if let Some(rpm) = v {
                    out.entry(chassis.to_string())
                        .or_default()
                        .fan
                        .insert(fan.to_string(), rpm);
                }
            }
            Subject::ThermalCelsius { chassis, sensor } => {
                out.entry(chassis.to_string())
                    .or_default()
                    .thermal
                    .entry(sensor.to_string())
                    .or_default()
                    .celsius = v;
            }
            Subject::ThermalUpperWarningC { chassis, sensor } => {
                out.entry(chassis.to_string())
                    .or_default()
                    .thermal
                    .entry(sensor.to_string())
                    .or_default()
                    .upper_warning_c = v;
            }
            Subject::ThermalUpperCriticalC { chassis, sensor } => {
                out.entry(chassis.to_string())
                    .or_default()
                    .thermal
                    .entry(sensor.to_string())
                    .or_default()
                    .upper_critical_c = v;
            }
            Subject::Reachable { chassis } => {
                out.entry(chassis.to_string()).or_default().reachable = v;
            }
            _ => {}
        }
    }
    out
}

/// The rows for one chassis, in the three blocks an operator reads.
///
/// Public so a test can assert the **rows** rather than the pixels: what
/// matters is that a reading arrives beside its publisher's limits, and a
/// rendered widget tree is a poor place to check that.
#[must_use]
pub fn chassis_rows(
    state: &DeviceDetailState,
    chassis: &str,
) -> (Vec<LimitRow>, Vec<LimitRow>, Vec<LimitRow>) {
    let folded = fold(state);
    let Some(r) = folded.get(chassis) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    (
        thermal_rows(chassis, r),
        fan_rows(chassis, r),
        psu_rows(chassis, r),
    )
}

fn thermal_rows(chassis: &str, r: &ChassisReadings) -> Vec<LimitRow> {
    r.thermal
        .iter()
        .map(|(sensor, t)| {
            LimitRow::new(format!("{chassis}/{sensor}"), t.celsius, "°C")
                .with_limits(t.upper_warning_c, t.upper_critical_c)
                .with_precision(1)
        })
        .collect()
}

fn fan_rows(chassis: &str, r: &ChassisReadings) -> Vec<LimitRow> {
    r.fan
        .iter()
        // No declared limits: the BMC publishes no fan thresholds, and a
        // number this GUI invented would be a guess about somebody else's
        // cooling. A stopped fan still reads "0 RPM" — that is a measurement.
        .map(|(fan, rpm)| LimitRow::new(format!("{chassis}/{fan}"), Some(*rpm), " RPM"))
        .collect()
}

fn psu_rows(chassis: &str, r: &ChassisReadings) -> Vec<LimitRow> {
    r.psu
        .iter()
        .map(|(psu, p)| {
            // `present` is published as a gauge every cycle, so its absence
            // means this build never saw the bay — not that the bay is empty.
            // Default to present: an unknown bay showing "absent" would be
            // this GUI asserting something the BMC did not say.
            let is_present = p.present.unwrap_or(true);
            LimitRow::new(format!("{chassis}/{psu}"), p.input_watts, " W")
                // The supply's rated capacity is the only limit a BMC gives
                // for draw, and it is a ceiling rather than a warning — so it
                // is the critical one, with no warning beneath it invented.
                .with_limits(None, is_present.then_some(p.capacity_watts).flatten())
                .with_present(is_present)
        })
        .collect()
}

/// The BMC device view.
pub fn bmc_chassis_view(state: &DeviceDetailState) -> Element<'_, Message> {
    let folded = fold(state);
    let mut col = Column::new().spacing(space::MD);

    if folded.is_empty() {
        let body: Element<'_, Message> = column![
            section_header("Out-of-band hardware", None),
            crate::view::components::empty_state(
                "No chassis readings yet — the BMC has not answered a sweep",
                None,
            ),
        ]
        .spacing(space::SM)
        .into();
        return card(body);
    }

    for (chassis, r) in &folded {
        let mut body = Column::new().spacing(space::SM);
        body = body.push(section_header("Chassis", None));
        body = body.push(text(chassis.clone()).size(font::EMPHASIS));

        // Reachability first, and stated rather than implied: a BMC that did
        // not answer publishes `0` every interval precisely so its silence is
        // a reading. Panels below then show the LAST values, which is why
        // this line has to be above them.
        if let Some(reachable) = r.reachable {
            body = body.push(
                text(if reachable == 0.0 {
                    "The BMC did not answer this cycle — the readings below are the last it gave"
                } else {
                    "The BMC answered this cycle"
                })
                .size(font::CAPTION),
            );
        }

        body = body.push(text("Temperatures").size(font::BODY));
        body = body.push(limit_table(
            &thermal_rows(chassis, r),
            "No thermal sensors on this chassis",
        ));

        body = body.push(text("Fans").size(font::BODY));
        body = body.push(limit_table(
            &fan_rows(chassis, r),
            "No fan reported in RPM — some BMCs report only a percentage of maximum",
        ));

        body = body.push(text("Power supplies").size(font::BODY));
        body = body.push(limit_table(
            &psu_rows(chassis, r),
            "No power supplies on this chassis",
        ));

        let body: Element<'_, Message> = body.into();
        col = col.push(card(body));
    }

    scrollable(col).height(Length::Fill).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::DeviceId;
    use iced_test::simulator;
    use zensight_common::TelemetryPoint;

    fn device(metrics: &[(&str, f64)]) -> DeviceDetailState {
        let mut state = DeviceDetailState::new(DeviceId::fixture("bmc", "bmc01"));
        for (metric, v) in metrics {
            state.metrics.insert(
                (*metric).to_string(),
                TelemetryPoint::new("bmc01", (*metric).to_string(), TelemetryValue::Gauge(*v)),
            );
        }
        state
    }

    /// The issue's own example: a reading rendered without its limit reads the
    /// same whether it is fine or nearly cooked. The limit must arrive beside
    /// it, and it must be the BMC's number — 89 °C here, which no threshold
    /// this GUI could have invented would have matched.
    #[test]
    fn a_reading_arrives_beside_the_limits_its_publisher_declared() {
        let state = device(&[
            ("bmc01-1/thermal/inlet/celsius", 58.5),
            ("bmc01-1/thermal/inlet/upper_warning_c", 75.0),
            ("bmc01-1/thermal/inlet/upper_critical_c", 89.0),
        ]);
        let (thermal, _, _) = chassis_rows(&state, "bmc01-1");
        assert_eq!(thermal.len(), 1);
        assert_eq!(thermal[0].reading_label(), "58.5°C");
        assert_eq!(
            thermal[0].limit_label().as_deref(),
            Some("warn 75.0°C · crit 89.0°C")
        );

        let mut ui = simulator(bmc_chassis_view(&state));
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
    fn a_supply_that_reports_only_capacity_is_not_metered() {
        let state = device(&[
            ("bmc01-1/psu/1/capacity_watts", 750.0),
            ("bmc01-1/psu/1/present", 1.0),
        ]);
        let (_, _, psu) = chassis_rows(&state, "bmc01-1");
        assert_eq!(psu.len(), 1);
        assert_eq!(psu[0].reading_label(), "not metered");
        assert_eq!(psu[0].verdict(), None, "no reading, so nothing to grade");
        assert_eq!(psu[0].limit_label().as_deref(), Some("crit 750 W"));
    }

    /// An empty bay is not a supply drawing 0 W, and it is not graded against
    /// a capacity it does not have.
    #[test]
    fn an_empty_bay_is_absent_rather_than_idle() {
        let state = device(&[
            ("bmc01-1/psu/2/present", 0.0),
            ("bmc01-1/psu/2/capacity_watts", 750.0),
        ]);
        let (_, _, psu) = chassis_rows(&state, "bmc01-1");
        assert_eq!(psu[0].reading_label(), "absent");
        assert_eq!(psu[0].verdict(), None);
        assert_eq!(
            psu[0].limit_label(),
            None,
            "an absent bay is graded against nothing"
        );
    }

    /// #1130's composite chunk carried through to the GUI: a Redfish service in
    /// front of a blade enclosure fronts several chassis, and each one gets its
    /// own panel. Folding on `split('/')` position would be correct here by
    /// accident; folding on the registry's `{chassis}` chunk is correct because
    /// that chunk is what the sensor minted, `-` and all.
    /// #1257: the family model derives from the bmc slice what `fold` does
    /// by hand — one instance per `(chassis, sensor)` with `celsius` and its
    /// declared limits as sibling columns, one per `(chassis, psu)`, and the
    /// `{chassis}` family carrying `reachable` — so the two agree on every
    /// row and every reading of this fixture.
    #[test]
    fn the_family_model_reproduces_the_hand_written_fold() {
        use crate::view::family::FamilyModel;
        let state = device(&[
            ("bmc01-blade1/thermal/inlet/celsius", 41.0),
            ("bmc01-blade1/thermal/inlet/upper_critical_c", 89.0),
            ("bmc01-blade1/thermal/outlet/celsius", 55.0),
            ("bmc01-blade2/thermal/inlet/celsius", 67.0),
            ("bmc01-blade1/psu/1/input_watts", 210.0),
            ("bmc01-blade1/psu/1/capacity_watts", 750.0),
            ("bmc01-blade1/psu/1/present", 1.0),
            ("bmc01-blade1/fan/fan1/rpm", 4200.0),
            ("bmc01-blade1/reachable", 1.0),
        ]);
        let folded = fold(&state);
        let model = FamilyModel::for_producer("bmc").expect("bmc is compiled in");
        let derived = model.instances(state.metrics.iter());
        let family = |path: &str| {
            let idx = model
                .families
                .iter()
                .position(|f| f.path == path)
                .unwrap_or_else(|| panic!("no family {path}"));
            derived.iter().find(|f| f.family == idx)
        };

        // Thermal: every (chassis, sensor) the fold saw, with the same numbers.
        let thermal = family("{chassis}/thermal/{sensor}").expect("thermal instances");
        let mut expected: Vec<(String, String)> = Vec::new();
        for (chassis, r) in &folded {
            for (sensor, t) in &r.thermal {
                expected.push((chassis.clone(), sensor.clone()));
                let inst = thermal
                    .instances
                    .iter()
                    .find(|i| i.id == format!("{chassis}/{sensor}"))
                    .unwrap_or_else(|| panic!("{chassis}/{sensor} missing"));
                assert_eq!(inst.number("celsius"), t.celsius);
                assert_eq!(inst.number("upper_critical_c"), t.upper_critical_c);
                assert_eq!(inst.number("upper_warning_c"), t.upper_warning_c);
            }
        }
        assert_eq!(thermal.instances.len(), expected.len());

        // PSU and fans and the chassis-level `reachable`.
        let psu = family("{chassis}/psu/{psu}").expect("psu instances");
        let p = &psu.instances[0];
        assert_eq!(p.id, "bmc01-blade1/1");
        assert_eq!(
            p.number("input_watts"),
            folded["bmc01-blade1"].psu["1"].input_watts
        );
        assert_eq!(p.state("present"), folded["bmc01-blade1"].psu["1"].present);
        let fan = family("{chassis}/fan/{fan}").expect("fan instances");
        assert_eq!(fan.instances[0].number("rpm"), Some(4200.0));
        let chassis = family("{chassis}").expect("chassis facts");
        assert_eq!(chassis.instances[0].id, "bmc01-blade1");
        assert_eq!(
            chassis.instances[0].number("reachable"),
            folded["bmc01-blade1"].reachable
        );
        // The slice's own limit column is a declared field with a unit, so
        // a default renderer can grade against it without a GUI constant.
        let f = model.family("{chassis}/thermal/{sensor}").unwrap();
        assert_eq!(
            f.field("upper_critical_c").unwrap().unit.as_deref(),
            Some("Cel")
        );
    }

    #[test]
    fn two_chassis_behind_one_endpoint_get_one_panel_each() {
        let state = device(&[
            ("bmc01-blade1/thermal/inlet/celsius", 41.0),
            ("bmc01-blade2/thermal/inlet/celsius", 67.0),
        ]);
        let folded = fold(&state);
        assert_eq!(folded.len(), 2, "one entry per chassis, not per endpoint");

        let (one, _, _) = chassis_rows(&state, "bmc01-blade1");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].reading_label(), "41.0°C");
        let (two, _, _) = chassis_rows(&state, "bmc01-blade2");
        assert_eq!(two[0].reading_label(), "67.0°C");

        let mut ui = simulator(bmc_chassis_view(&state));
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
    fn a_silent_bmc_says_its_readings_are_stale() {
        let state = device(&[
            ("bmc01-1/reachable", 0.0),
            ("bmc01-1/thermal/inlet/celsius", 41.0),
        ]);
        let mut ui = simulator(bmc_chassis_view(&state));
        assert!(
            ui.find("The BMC did not answer this cycle — the readings below are the last it gave")
                .is_ok()
        );

        let live = device(&[
            ("bmc01-1/reachable", 1.0),
            ("bmc01-1/thermal/inlet/celsius", 41.0),
        ]);
        let mut ui = simulator(bmc_chassis_view(&live));
        assert!(ui.find("The BMC answered this cycle").is_ok());
    }

    /// The gate on the tab itself: a device the bmc sensor has published
    /// nothing for gets no panel, and one that published only `reachable = 0`
    /// gets a panel that says so.
    #[test]
    fn the_tab_appears_for_a_bmc_that_has_spoken_at_all() {
        assert!(!has_bmc_readings(&device(&[])));
        assert!(
            has_bmc_readings(&device(&[("bmc01-1/reachable", 0.0)])),
            "an unreachable BMC still has something to show"
        );
    }
}
