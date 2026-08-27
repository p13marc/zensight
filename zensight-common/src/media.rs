//! Consumer-side timing on the `@media` plane — the frame-age clock, in one
//! place (RFC 07 §1.3, epic #712).
//!
//! # Why this is not three lines inlined in the GUI
//!
//! Frame age is the input to every consumer-side behaviour the plane is
//! growing: the frame-age deadline (#716), the `frame_age_ms` /
//! `frame_age_max_ms` fields of [`crate::stream::MediaReceiverReport`] (#714),
//! and eventually a tier controller (#720). RFC 07 §1.3 answers "against which
//! clock is a frame late?" with three rules that are each easy to break by
//! accident, and a second implementation (the browser tile, #722) has to break
//! them the same way or the two consumers disagree about what they measured:
//!
//! 1. **The clock is the publisher's HLC sample timestamp** — the one the
//!    middleware stamps when a session enables timestamping, which
//!    [`crate::session`] forces on fleet-wide. It is *not* `FrameMeta`'s
//!    `pts_ns`/`dts_ns`: those are pipeline-clock values with an arbitrary
//!    monotonic origin, deliberately not comparable across hosts.
//! 2. **Unstamped is not asked, never zero.** A deployment that does not
//!    timestamp has no frame age; returning `0.0` there reads as "perfectly
//!    fresh" and silently disables every deadline built on it.
//! 3. **Negatives are reported, not clamped.** Frame age is *observed skewed
//!    latency* — an observation, never a verdict on the transport. A negative
//!    age is the clock-skew evidence, and clamping it to zero destroys the only
//!    signal that says the number cannot be trusted.

use std::time::SystemTime;

/// Observed skewed latency for one sample, in milliseconds: the publisher's
/// HLC timestamp subtracted from local arrival (RFC 07 §1.3).
///
/// `None` means the sample arrived **unstamped** — "not asked", which a caller
/// must keep distinct from a measured zero. Negative values are returned
/// unclamped.
///
/// `now` is a parameter rather than a [`SystemTime::now()`] call inside so the
/// rule is testable without a clock, and so a caller measuring several samples
/// against one arrival instant can do so.
///
/// # Example
/// ```
/// use std::time::{Duration, SystemTime};
/// use zensight_common::media::observed_frame_age_ms;
///
/// // An unstamped sample has no age — and is NOT age zero.
/// assert_eq!(observed_frame_age_ms(None, SystemTime::now()), None);
/// ```
pub fn observed_frame_age_ms(ts: Option<&zenoh::time::Timestamp>, now: SystemTime) -> Option<f64> {
    let published = ts?.get_time().to_system_time();
    // `duration_since` is unsigned and reports the reversed order as an error
    // rather than a sign. Recover the sign instead of clamping: a publisher
    // whose clock runs ahead of ours yields a negative age, which is exactly
    // the skew evidence RFC 07 §1.3 forbids destroying.
    Some(match now.duration_since(published) {
        Ok(elapsed) => elapsed.as_secs_f64() * 1000.0,
        Err(ahead) => -(ahead.duration().as_secs_f64() * 1000.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A timestamp with `time` as its HLC instant. The id is arbitrary — frame
    /// age reads the time half only. `NTP64` counts from the UNIX epoch, which
    /// is the same origin `to_system_time` reads it back against.
    fn stamped(time: SystemTime) -> zenoh::time::Timestamp {
        let since_epoch = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("test instants are after the epoch");
        zenoh::time::Timestamp::new(
            zenoh::time::NTP64::from(since_epoch),
            zenoh::time::TimestampId::try_from([1u8].as_slice()).expect("valid id"),
        )
    }

    #[test]
    fn an_unstamped_sample_has_no_age_rather_than_age_zero() {
        assert_eq!(
            observed_frame_age_ms(None, SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)),
            None,
            "unstamped is NOT ASKED (RFC 07 §1.3); a Some(0.0) here would read as \
             'perfectly fresh' and silently disable every deadline built on it"
        );
    }

    #[test]
    fn a_frame_published_before_it_arrived_has_a_positive_age() {
        let published = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let arrived = published + Duration::from_millis(250);
        let age = observed_frame_age_ms(Some(&stamped(published)), arrived).expect("stamped");
        assert!((age - 250.0).abs() < 1.0, "age = {age}");
    }

    #[test]
    fn a_publisher_clock_running_ahead_yields_a_negative_age_unclamped() {
        // The producer's HLC is 40 ms ahead of ours. RFC 07 §1.3: show it.
        let arrived = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let published = arrived + Duration::from_millis(40);
        let age = observed_frame_age_ms(Some(&stamped(published)), arrived).expect("stamped");
        assert!(
            age < 0.0,
            "a negative age IS the clock-skew evidence and must not be clamped (age = {age})"
        );
        assert!((age + 40.0).abs() < 1.0, "age = {age}");
    }
}
