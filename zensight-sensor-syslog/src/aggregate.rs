//! Build the aggregated [`LogEvent`] from a parsed syslog message (feature
//! `aggregate-publishers`).
//!
//! The wire **type** [`LogEvent`] and the time-sortable id generator
//! [`zensight_aggregates::next_uid`] live in the zenoh-free
//! [`zensight_aggregates`] crate so external consumers can use them without
//! pulling zenoh. This module keeps only the mapping that depends on this
//! crate's [`SyslogMessage`], which cannot live in the pure types crate.

use zensight_aggregates::LogEvent;

use crate::parser::SyslogMessage;

/// Build a [`LogEvent`] from a parsed syslog message.
///
/// `pid` is taken from the syslog `proc_id` tag only when it parses as a `u32`
/// (RFC 3164/5424 allow non-numeric tags). `uid` and `category` are supplied by
/// the caller (`uid` from `next_uid`, `category` from the known-event classifier).
pub fn build_log_event(
    msg: &SyslogMessage,
    timestamp: i64,
    uid: String,
    category: Option<String>,
) -> LogEvent {
    let pid = msg
        .proc_id
        .as_deref()
        .and_then(|s| s.trim().parse::<u32>().ok());
    LogEvent {
        uid,
        timestamp,
        severity: msg.severity.as_str().to_string(),
        facility: msg.facility.as_str().to_string(),
        app: msg.app_name.clone(),
        pid,
        message: msg.message.clone(),
        category,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn build_from_message_extracts_fields() {
        let msg = parser::parse("<34>Jan  5 14:30:00 myhost sshd[1234]: Connection from 10.0.0.1")
            .unwrap();
        let event = build_log_event(&msg, 1_700_000_000_000, "uid-1".to_string(), None);
        assert_eq!(event.uid, "uid-1");
        assert_eq!(event.timestamp, 1_700_000_000_000);
        assert_eq!(event.severity, "crit");
        assert_eq!(event.facility, "auth");
        assert_eq!(event.app.as_deref(), Some("sshd"));
        assert_eq!(event.pid, Some(1234));
        assert_eq!(event.message, "Connection from 10.0.0.1");
        assert_eq!(event.category, None);
    }

    #[test]
    fn build_from_message_non_numeric_pid_is_none() {
        // RFC 3164 tag without a numeric proc id.
        let msg = parser::parse("<14>Jan  5 14:30:00 localhost app: test message").unwrap();
        let event = build_log_event(
            &msg,
            1_700_000_000_000,
            "uid-2".to_string(),
            Some("coredump".to_string()),
        );
        assert_eq!(event.pid, None);
        assert_eq!(event.app.as_deref(), Some("app"));
        assert_eq!(event.category.as_deref(), Some("coredump"));
    }
}
