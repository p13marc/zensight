//! A relative time window shared by the feeds that page over history.
//!
//! Introduced for the Logs feed (#554) and reused by the SNMP event feed
//! (#578): the picker stores the selection, and the update handler resolves
//! it to an absolute lower bound (epoch ms) against `now` when applied, so
//! the pure views never need the clock. The bound feeds both the sensor
//! query (`from=`) and any filtered export.

/// A relative time window, as offered by a feed's range picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeRange {
    /// No lower bound — everything the sensor/ring holds.
    #[default]
    All,
    /// The last 15 minutes.
    Last15m,
    /// The last hour.
    LastHour,
    /// The last 6 hours.
    Last6h,
    /// The last 24 hours.
    Last24h,
    /// The last 7 days.
    Last7d,
}

impl TimeRange {
    /// The pick-list options, in display order.
    pub const ALL: [TimeRange; 6] = [
        TimeRange::All,
        TimeRange::Last15m,
        TimeRange::LastHour,
        TimeRange::Last6h,
        TimeRange::Last24h,
        TimeRange::Last7d,
    ];

    /// Window length in milliseconds, or `None` for "all time" (no lower bound).
    pub fn window_ms(self) -> Option<i64> {
        let mins = match self {
            TimeRange::All => return None,
            TimeRange::Last15m => 15,
            TimeRange::LastHour => 60,
            TimeRange::Last6h => 6 * 60,
            TimeRange::Last24h => 24 * 60,
            TimeRange::Last7d => 7 * 24 * 60,
        };
        Some(mins * 60_000)
    }

    /// The label shown in the picker.
    pub fn label(self) -> &'static str {
        match self {
            TimeRange::All => "All time",
            TimeRange::Last15m => "Last 15 min",
            TimeRange::LastHour => "Last hour",
            TimeRange::Last6h => "Last 6 hours",
            TimeRange::Last24h => "Last 24 hours",
            TimeRange::Last7d => "Last 7 days",
        }
    }
}

impl std::fmt::Display for TimeRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}
