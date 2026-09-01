//! The systemd sentinel's wire types (#277, moved here in #849): the unit
//! expectation vocabulary an operator declares and the sensor evaluates.
//!
//! They live in zensight-common for the same reason hostspec's do (#816):
//! they are **wire contracts** with three consumers — the sensor (evaluates
//! them), the GUI (authors them, via `@rpc/systemd/expectations/set`), and the
//! `@desired` fleet author (publishes them per host). RFC 08 §7's schema gate
//! requires a real schemars-generated schema for every state-class payload,
//! and a sensor-crate type can never provide one: `zensight-common` cannot
//! depend on a sensor, so `describe` could only ever carry a summary stub. The
//! #815 gate refused exactly that, correctly, which is why `@desired` shipped
//! carrying hostspec's topic alone.
//!
//! Checking logic stays in the sensor. These are data.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_eval_interval_secs() -> u64 {
    10
}
fn default_for_secs() -> u64 {
    15
}

/// "expect service `<unit>` active".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceActiveExpectation {
    pub unit: String,
}

/// "expect target `<target>` active".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TargetActiveExpectation {
    pub target: String,
}

/// A timer expectation, in one of two strengths (#824):
///
/// - `within_secs` — "the timer **fired** within the window". Proves the
///   schedule elapsed, and nothing else.
/// - `succeeded_within_secs` — "the timer fired within the window **and its
///   triggered service's last run succeeded**". The one-word difference that
///   catches the failure `within_secs` cannot: a timer firing hourly, on
///   schedule, whose service failed hourly for eight days.
///
/// Both may be set (both are checked, under their own rules); an expectation
/// with neither is inert and warned about at sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TimerExpectation {
    pub timer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub succeeded_within_secs: Option<u64>,
}

/// "expect service `<unit>` restarts_rate < `<max>` per `<window_secs>`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RestartRateExpectation {
    pub unit: String,
    pub max: u32,
    pub window_secs: u64,
}

/// The full declarative expectation set (seeded from config, hot-swappable).
///
/// `Default` is the EMPTY set with the same cadence serde would fill in for
/// `{}` — not the derived all-zeros, which would make `eval_interval_secs: 0`
/// the stock install's set and fail the sentinel's own validation at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExpectationsConfig {
    #[serde(default = "default_eval_interval_secs")]
    pub eval_interval_secs: u64,
    #[serde(default = "default_for_secs")]
    pub for_secs: u64,
    #[serde(default)]
    pub services_active: Vec<ServiceActiveExpectation>,
    #[serde(default)]
    pub targets_active: Vec<TargetActiveExpectation>,
    #[serde(default)]
    pub timers: Vec<TimerExpectation>,
    #[serde(default)]
    pub restart_rates: Vec<RestartRateExpectation>,
    /// "forbid any unit in state failed".
    #[serde(default)]
    pub forbid_failed: bool,
}

impl Default for ExpectationsConfig {
    fn default() -> Self {
        ExpectationsConfig {
            eval_interval_secs: default_eval_interval_secs(),
            for_secs: default_for_secs(),
            services_active: Vec::new(),
            targets_active: Vec::new(),
            timers: Vec::new(),
            restart_rates: Vec::new(),
            forbid_failed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason this type moved at all: `@desired` carries it as a
    /// state-class payload, and RFC 08 §7's gate wants a real schema — not
    /// the summary stub a sensor-crate type could only ever offer.
    #[test]
    fn the_expectation_set_has_a_real_schema() {
        let schema = serde_json::to_value(schemars::schema_for!(ExpectationsConfig)).unwrap();
        let props = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("a real schema describes its properties");
        for field in [
            "eval_interval_secs",
            "for_secs",
            "services_active",
            "targets_active",
            "timers",
            "restart_rates",
            "forbid_failed",
        ] {
            assert!(props.contains_key(field), "{field} missing from the schema");
        }
    }

    /// The wire shape is unchanged by the move — the same JSON an operator
    /// already pushes to `@rpc/systemd/expectations/set` must still decode.
    #[test]
    fn the_shipped_json_shape_still_decodes() {
        let doc = serde_json::json!({
            "eval_interval_secs": 30,
            "services_active": [{ "unit": "sshd.service" }],
            "timers": [{ "timer": "backup.timer", "succeeded_within_secs": 93600 }],
            "restart_rates": [{ "unit": "caddy.service", "max": 3, "window_secs": 600 }],
            "forbid_failed": true,
        });
        let cfg: ExpectationsConfig = serde_json::from_value(doc).unwrap();
        assert_eq!(cfg.eval_interval_secs, 30);
        assert_eq!(
            cfg.for_secs, 15,
            "an absent field keeps its documented default"
        );
        assert_eq!(cfg.services_active[0].unit, "sshd.service");
        assert_eq!(cfg.timers[0].succeeded_within_secs, Some(93600));
        assert_eq!(cfg.targets_active.len(), 0);
        assert!(cfg.forbid_failed);
    }
}
