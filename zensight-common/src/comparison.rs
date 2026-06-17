//! Numeric comparison operator shared between the frontend's local threshold
//! rules and the sensors' headless `metric-threshold` expectations.
//!
//! Having one definition lets a GUI threshold rule be *promoted* to a sensor-side
//! expectation (the alerting redesign's P4 rule-promotion path) without the two
//! sides drifting out of sync.

use serde::{Deserialize, Serialize};

/// Comparison operators for threshold rules / metric expectations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ComparisonOp {
    #[default]
    GreaterThan,
    GreaterOrEqual,
    LessThan,
    LessOrEqual,
    Equal,
    NotEqual,
}

impl ComparisonOp {
    /// All comparison operators (for GUI pickers).
    pub const ALL: &'static [ComparisonOp] = &[
        ComparisonOp::GreaterThan,
        ComparisonOp::GreaterOrEqual,
        ComparisonOp::LessThan,
        ComparisonOp::LessOrEqual,
        ComparisonOp::Equal,
        ComparisonOp::NotEqual,
    ];

    /// The operator's symbol (`>`, `>=`, ...).
    pub fn symbol(&self) -> &'static str {
        match self {
            ComparisonOp::GreaterThan => ">",
            ComparisonOp::GreaterOrEqual => ">=",
            ComparisonOp::LessThan => "<",
            ComparisonOp::LessOrEqual => "<=",
            ComparisonOp::Equal => "==",
            ComparisonOp::NotEqual => "!=",
        }
    }

    /// Evaluate `value <op> threshold`.
    pub fn evaluate(&self, value: f64, threshold: f64) -> bool {
        match self {
            ComparisonOp::GreaterThan => value > threshold,
            ComparisonOp::GreaterOrEqual => value >= threshold,
            ComparisonOp::LessThan => value < threshold,
            ComparisonOp::LessOrEqual => value <= threshold,
            ComparisonOp::Equal => (value - threshold).abs() < f64::EPSILON,
            ComparisonOp::NotEqual => (value - threshold).abs() >= f64::EPSILON,
        }
    }
}

impl std::fmt::Display for ComparisonOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.symbol())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_all_ops() {
        assert!(ComparisonOp::GreaterThan.evaluate(5.0, 3.0));
        assert!(!ComparisonOp::GreaterThan.evaluate(3.0, 3.0));
        assert!(ComparisonOp::GreaterOrEqual.evaluate(3.0, 3.0));
        assert!(ComparisonOp::LessThan.evaluate(2.0, 3.0));
        assert!(ComparisonOp::LessOrEqual.evaluate(3.0, 3.0));
        assert!(ComparisonOp::Equal.evaluate(3.0, 3.0));
        assert!(ComparisonOp::NotEqual.evaluate(3.0, 4.0));
    }

    #[test]
    fn symbols_and_all() {
        assert_eq!(ComparisonOp::ALL.len(), 6);
        assert_eq!(ComparisonOp::GreaterOrEqual.symbol(), ">=");
        assert_eq!(format!("{}", ComparisonOp::LessThan), "<");
    }
}
