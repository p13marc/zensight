//! The per-device PDU budget (#825 item 2).
//!
//! An SNMP sensor's characteristic failure is **hammering a device weaker than
//! itself** — an eight-year-old switch CPU, or a UPS management card that
//! reboots under load. Until now every device polled on its own timer with no
//! cap on outstanding requests and no ceiling on PDU rate: correct, and
//! entirely dependent on the operator having picked a gentle interval.
//!
//! This is the SNMP-shaped instance of the fleet-wide budget work in #812. The
//! resource being bounded is **someone else's device**, which is why it is
//! declared per device rather than per sensor: one switch's tolerance says
//! nothing about another's.
//!
//! # Honest accounting
//!
//! A GET is one PDU and is charged one token. A **walk is not**: the client
//! issues GETBULK requests carrying up to `max_repetitions` rows each, and how
//! many that takes is not knowable before the table is read. Estimating it in
//! advance would make `max_pdus_per_sec` a number that means something other
//! than it says.
//!
//! So a walk is charged **after the fact**, from the rows it actually
//! returned: `ceil(rows / max_repetitions) + 1` for GETBULK and `rows + 1` for
//! the GETNEXT fallback — the `+ 1` being the request that discovers the end
//! of the subtree. A large table therefore drains the bucket and delays the
//! *next* operation, which is exactly the behaviour wanted — the device gets a
//! rest proportional to the work it just did — and the published number stays
//! true.

use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore};

/// A per-device token bucket plus a concurrency gate.
///
/// Both are optional and independent: a device may be given a rate ceiling
/// without a concurrency cap, or the reverse. An unconfigured budget is a
/// no-op, so existing deployments behave exactly as before.
pub struct DeviceBudget {
    /// Tokens per second. `None` = no rate ceiling.
    rate: Option<f64>,
    /// Burst size, in tokens. One second's worth, so a device that has been
    /// idle can absorb a poll cycle without being throttled mid-cycle.
    burst: f64,
    state: Mutex<Bucket>,
    /// Outstanding operations. `None` = unbounded.
    gate: Option<Semaphore>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl DeviceBudget {
    pub fn new(max_pdus_per_sec: Option<f64>, max_concurrent: Option<usize>) -> Self {
        let rate = max_pdus_per_sec.filter(|r| *r > 0.0);
        let burst = rate.unwrap_or(0.0).max(1.0);
        Self {
            rate,
            burst,
            state: Mutex::new(Bucket {
                tokens: burst,
                last: Instant::now(),
            }),
            gate: max_concurrent.filter(|c| *c > 0).map(Semaphore::new),
        }
    }

    /// Whether this budget constrains anything at all.
    pub fn is_active(&self) -> bool {
        self.rate.is_some() || self.gate.is_some()
    }

    /// Acquire a concurrency slot, if one is configured. Held until dropped.
    pub async fn slot(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        match &self.gate {
            Some(s) => s.acquire().await.ok(),
            None => None,
        }
    }

    /// Wait until `cost` PDUs may be issued, then debit them.
    ///
    /// Sleeps rather than failing: a monitoring sensor that *drops* a poll to
    /// stay under budget has traded a device's health for a gap in its own
    /// telemetry, which is the wrong trade. Slowing down is the whole point.
    pub async fn charge(&self, cost: f64) {
        let Some(rate) = self.rate else { return };
        let wait = {
            let mut b = self.state.lock().await;
            let now = Instant::now();
            let elapsed = now.duration_since(b.last).as_secs_f64();
            b.last = now;
            b.tokens = (b.tokens + elapsed * rate).min(self.burst);
            b.tokens -= cost;
            if b.tokens >= 0.0 {
                None
            } else {
                // The debt is paid by waiting; the bucket is already debited,
                // so concurrent callers queue behind this rather than all
                // sleeping for the same deficit.
                Some(Duration::from_secs_f64(-b.tokens / rate))
            }
        };
        if let Some(d) = wait {
            tokio::time::sleep(d).await;
        }
    }

    /// Tokens currently available — for tests and for the health document.
    pub async fn available(&self) -> f64 {
        let b = self.state.lock().await;
        b.tokens
    }
}

/// How many PDUs a completed walk actually cost.
///
/// GETBULK carries up to `max_repetitions` varbinds per response, plus the one
/// request that discovers the end of the subtree. GETNEXT (v1) is one PDU per
/// row, plus the terminating one.
pub fn walk_pdu_cost(rows: usize, max_repetitions: u32, bulk: bool) -> f64 {
    if !bulk {
        return rows as f64 + 1.0;
    }
    let per = max_repetitions.max(1) as f64;
    (rows as f64 / per).ceil() + 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unconfigured_budget_constrains_nothing() {
        let b = DeviceBudget::new(None, None);
        assert!(!b.is_active());
        let start = Instant::now();
        for _ in 0..1000 {
            b.charge(1.0).await;
        }
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "an absent budget must not cost anything"
        );
        assert!(b.slot().await.is_none());
    }

    /// The bucket starts full — one second's worth — so a device that has been
    /// idle absorbs a whole poll cycle without being throttled mid-cycle.
    #[tokio::test]
    async fn a_fresh_bucket_absorbs_one_seconds_burst_without_waiting() {
        let b = DeviceBudget::new(Some(20.0), None);
        let start = Instant::now();
        for _ in 0..20 {
            b.charge(1.0).await;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "the burst is free: {elapsed:?}"
        );
        // The bucket refills WHILE the loop runs, so "drained" is only true to
        // within what `elapsed` could have put back — and comparing against
        // that is the only form of this assertion that is not a bet on the
        // machine.
        //
        // It used to read `available() <= 0.001`. At 20 tokens/s that constant
        // is 50 MICROSECONDS of wall time for the whole twenty-iteration loop,
        // and the loop measures ~30 µs on an idle developer box — under 2x
        // margin, against a shared CI runner. It failed there on 2026-09-05.
        //
        // `available()` does not refill (only `charge` does), so the reading is
        // frozen at the last charge and `elapsed` is measured after it: the
        // bound below is exact, not generous. A charge that failed to debit
        // would leave ~20 tokens and still be caught.
        let refilled = 20.0 * elapsed.as_secs_f64();
        let available = b.available().await;
        assert!(
            available <= refilled + f64::EPSILON,
            "the twenty charges must all have been debited: {available} tokens left,              and only {refilled} could have refilled in {elapsed:?}"
        );
    }

    /// Past the burst it slows down rather than dropping work: a sensor that
    /// skips a poll to stay under budget has traded the device's health for a
    /// gap in its own telemetry.
    #[tokio::test]
    async fn past_the_burst_it_waits_rather_than_dropping() {
        let b = DeviceBudget::new(Some(20.0), None);
        for _ in 0..20 {
            b.charge(1.0).await;
        }
        let start = Instant::now();
        b.charge(4.0).await; // 4 tokens at 20/s = 200 ms
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(150),
            "it must actually wait: {waited:?}"
        );
    }

    #[tokio::test]
    async fn the_concurrency_gate_bounds_outstanding_operations() {
        let b = DeviceBudget::new(None, Some(2));
        assert!(b.is_active());
        let a = b.slot().await;
        let c = b.slot().await;
        assert!(a.is_some() && c.is_some());
        // A third must not be immediately available.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), b.slot())
                .await
                .is_err(),
            "the third caller has to queue"
        );
        drop(a);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), b.slot())
                .await
                .is_ok(),
            "and is admitted when a slot frees"
        );
    }

    /// The walk charge is computed from rows that were REALLY returned, so the
    /// published `max_pdus_per_sec` means what it says rather than resting on
    /// a guess made before the table was read.
    #[tokio::test]
    async fn a_walk_is_charged_for_the_pdus_it_really_took() {
        // 100 rows at 20 repetitions = 5 responses + 1 terminator.
        assert_eq!(walk_pdu_cost(100, 20, true), 6.0);
        // A partial last response still costs one.
        assert_eq!(walk_pdu_cost(101, 20, true), 7.0);
        // GETNEXT is one PDU per row.
        assert_eq!(walk_pdu_cost(100, 20, false), 101.0);
        // An empty walk still cost the request that discovered it was empty.
        assert_eq!(walk_pdu_cost(0, 20, true), 1.0);
        // max_repetitions of 0 must not divide by zero.
        assert_eq!(walk_pdu_cost(5, 0, true), 6.0);
    }

    /// A zero or negative ceiling is "no ceiling", not "no PDUs" — the latter
    /// would be a config typo that silently stops all polling.
    #[tokio::test]
    async fn a_zero_ceiling_means_unlimited_not_forbidden() {
        let b = DeviceBudget::new(Some(0.0), Some(0));
        assert!(!b.is_active());
        b.charge(1000.0).await;
    }
}
