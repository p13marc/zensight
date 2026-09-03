//! The host's own clock discipline (#959, SYS-SUP-013).
//!
//! ZenSight's correlation assumes fleet time discipline — an operating
//! assumption written down in `docs/plans/rerun/` — and until now nothing
//! measured it. The only NTP coverage was "is `chronyd` active", which is true
//! of a `chronyd` that has never reached a server: the daemon runs, the unit is
//! green, and the clock is wrong.
//!
//! This reads the *local* discipline, which is the half `probe`'s `ntp` check
//! cannot see. That check measures the offset between a server's clock and
//! **this host's**, so a host that is itself an hour out reports every server
//! as an hour out. Only the local view can say which of the two is adrift.
//!
//! # Two sources, in order, and nothing invented
//!
//! 1. **chrony** — `chronyc -c tracking`, a stable comma-separated format
//!    chrony documents for exactly this purpose.
//! 2. **systemd-timesyncd** — `timedatectl show`, for hosts that run no chrony.
//!
//! When neither is present the family is **absent**, not zero. A zero offset is
//! the single most misleading value this document could carry: it is what a
//! perfectly disciplined clock looks like, and publishing it for a host with no
//! time daemon at all would report the opposite of the truth.

use std::process::Command;

pub use zensight_common::timesync::TimesyncStatus;

/// Read the local clock discipline, or `None` when no time daemon answers.
pub fn read() -> Option<TimesyncStatus> {
    read_with(run)
}

/// Injectable form, so the parsers are tested against fixtures rather than
/// against whatever daemon the test machine happens to run.
fn read_with(run: impl Fn(&str, &[&str]) -> Option<String>) -> Option<TimesyncStatus> {
    if let Some(out) = run("chronyc", &["-c", "tracking"])
        && let Some(s) = parse_chronyc(&out)
    {
        return Some(s);
    }
    if let Some(out) = run("timedatectl", &["show"])
        && let Some(s) = parse_timedatectl(&out)
    {
        return Some(s);
    }
    None
}

fn run(bin: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(bin).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse `chronyc -c tracking` — the documented comma-separated form.
///
/// Fields, in order: reference id, reference name, stratum, ref time, system
/// time offset, last offset, RMS offset, frequency, residual frequency, skew,
/// root delay, root dispersion, update interval, leap status.
///
/// Synchronisation is read from the **leap status**, not from the offset:
/// chrony reports `Not synchronised` there when it has no usable source, and a
/// small offset from a clock nothing is disciplining is a coincidence rather
/// than a measurement.
pub fn parse_chronyc(out: &str) -> Option<TimesyncStatus> {
    let line = out.lines().next()?.trim();
    let f: Vec<&str> = line.split(',').collect();
    if f.len() < 14 {
        return None;
    }
    let leap = f[13].trim();
    Some(TimesyncStatus {
        // chrony's own words. "Normal" is disciplined; "Not synchronised" is
        // exactly the chronyd-that-never-reached-a-server case.
        synchronised: leap.eq_ignore_ascii_case("Normal")
            || leap.eq_ignore_ascii_case("Insert leap second")
            || leap.eq_ignore_ascii_case("Delete leap second"),
        source: "chrony".to_string(),
        // Field 4 is the system-time offset in seconds; the sign convention is
        // chrony's, and it is passed through rather than reinterpreted.
        offset_ms: f[4].trim().parse::<f64>().ok().map(|s| s * 1000.0),
        stratum: f[2].trim().parse().ok(),
        reference: (!f[1].trim().is_empty()).then(|| f[1].trim().to_string()),
        last_update_age_s: f[12].trim().parse().ok(),
    })
}

/// Parse `timedatectl show` — `key=value` lines.
///
/// `timedatectl` reports no offset and no stratum, so those stay **absent**
/// rather than being filled with a plausible number.
pub fn parse_timedatectl(out: &str) -> Option<TimesyncStatus> {
    let mut ntp = None;
    let mut synced = None;
    for line in out.lines() {
        match line.split_once('=') {
            Some(("NTP", v)) => ntp = Some(v.trim() == "yes"),
            Some(("NTPSynchronized", v)) => synced = Some(v.trim() == "yes"),
            _ => {}
        }
    }
    // No `NTP` key at all means this is not a timesyncd host answering — an
    // absent family, not an unsynchronised one.
    let ntp = ntp?;
    if !ntp {
        // NTP is switched off. That is a real, reportable state: the clock is
        // undisciplined and somebody chose that.
        return Some(TimesyncStatus {
            synchronised: false,
            source: "systemd-timesyncd".to_string(),
            offset_ms: None,
            stratum: None,
            reference: None,
            last_update_age_s: None,
        });
    }
    Some(TimesyncStatus {
        synchronised: synced.unwrap_or(false),
        source: "systemd-timesyncd".to_string(),
        offset_ms: None,
        stratum: None,
        reference: None,
        last_update_age_s: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disciplined chrony, from real `chronyc -c tracking` output.
    #[test]
    fn chronyc_reports_a_disciplined_clock() {
        let out = "7F7F0101,ntp.example.test,2,1756900000.0,-0.000123456,\
                   -0.000001,0.000045,1.234,0.001,0.5,0.012,0.034,64.0,Normal\n";
        let s = parse_chronyc(out).expect("parses");
        assert!(s.synchronised);
        assert_eq!(s.source, "chrony");
        assert_eq!(s.stratum, Some(2));
        assert_eq!(s.reference.as_deref(), Some("ntp.example.test"));
        assert!((s.offset_ms.unwrap() - -0.123456).abs() < 1e-6, "{s:?}");
        assert_eq!(s.last_update_age_s, Some(64.0));
    }

    /// The case that motivated this whole issue: a chronyd that is running,
    /// whose unit is green, and which has never reached a server.
    #[test]
    fn chronyc_reports_a_running_daemon_that_is_not_synchronised() {
        let out = "00000000,,0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,Not synchronised\n";
        let s = parse_chronyc(out).expect("parses");
        assert!(
            !s.synchronised,
            "a running daemon that never reached a server: the unit is green and the clock is wrong"
        );
        assert_eq!(s.stratum, Some(0));
        assert_eq!(s.reference, None);
    }

    #[test]
    fn chronyc_leap_second_states_are_still_synchronised() {
        for leap in ["Insert leap second", "Delete leap second"] {
            let out = format!("A,b,2,0,0,0,0,0,0,0,0,0,64.0,{leap}");
            assert!(parse_chronyc(&out).unwrap().synchronised, "{leap}");
        }
    }

    #[test]
    fn a_truncated_chronyc_line_is_not_parsed_into_a_status() {
        assert!(parse_chronyc("").is_none());
        assert!(parse_chronyc("A,b,2\n").is_none());
    }

    #[test]
    fn timedatectl_reports_sync_without_an_offset() {
        let out = "Timezone=UTC\nNTP=yes\nNTPSynchronized=yes\nRTCInLocalTZ=no\n";
        let s = parse_timedatectl(out).expect("parses");
        assert!(s.synchronised);
        assert_eq!(s.source, "systemd-timesyncd");
        // timedatectl reports neither, and a plausible-looking zero would be
        // worse than nothing.
        assert_eq!(s.offset_ms, None);
        assert_eq!(s.stratum, None);
    }

    #[test]
    fn timedatectl_reports_ntp_switched_off_as_unsynchronised() {
        let s = parse_timedatectl("NTP=no\nNTPSynchronized=no\n").expect("parses");
        assert!(!s.synchronised);
    }

    #[test]
    fn output_without_an_ntp_key_is_not_a_timesync_answer() {
        assert!(parse_timedatectl("Timezone=UTC\n").is_none());
    }

    /// chrony wins when both answer; neither present ⇒ the family is absent.
    #[test]
    fn the_family_is_absent_when_no_daemon_answers() {
        assert!(read_with(|_, _| None).is_none());

        let chrony_only = read_with(|bin, _| {
            (bin == "chronyc").then(|| "A,b,2,0,0,0,0,0,0,0,0,0,64.0,Normal".to_string())
        });
        assert_eq!(chrony_only.unwrap().source, "chrony");

        let both = read_with(|bin, _| {
            Some(match bin {
                "chronyc" => "A,b,2,0,0,0,0,0,0,0,0,0,64.0,Normal".to_string(),
                _ => "NTP=yes\nNTPSynchronized=yes\n".to_string(),
            })
        });
        assert_eq!(
            both.unwrap().source,
            "chrony",
            "chrony reports more, so it is preferred when both are present"
        );

        let timesyncd_only = read_with(|bin, _| {
            (bin != "chronyc").then(|| "NTP=yes\nNTPSynchronized=yes\n".to_string())
        });
        assert_eq!(timesyncd_only.unwrap().source, "systemd-timesyncd");
    }
}
