//! Wire types for the historian's read procedures (#905, epic #898).
//!
//! Telemetry was the only wire class with no history path for a second
//! process: logs have `@rpc/logs/events` over a durable store, events have a
//! router `fs` storage plus a startup GET, state has seed storages, and
//! telemetry had the AdvancedPublisher's ten-sample cache and a redb file
//! private to one GUI. These are the shapes the service that closes that gap
//! replies with.
//!
//! Three things are deliberate in them.
//!
//! **A series is named `(origin, producer, subject)`** — the wire key minus
//! the class chunk. Not an entity id: entity resolution is a query-time join,
//! and a storage key that depended on it would change meaning whenever the
//! correlator merged two hosts or went down. This name is derivable from a
//! sample alone.
//!
//! **Every reply says whether it is complete.** `truncated` and `next_cursor`
//! are not conveniences; a chart that silently renders a partial window is
//! making a claim about the world that nobody checked. A short page or a null
//! cursor is the end — the same contract `@rpc/logs/events` already has.
//!
//! **The aggregate is named in the reply**, not assumed from the request. A
//! caller that asked for `rate` and a server that clamped `step` to a coarser
//! tier are looking at different numbers, and the reply is where that gets
//! said out loud.

use serde::{Deserialize, Serialize};

/// The `limit` a caller should ask for when it has no reason to ask for less.
///
/// Named here rather than left to each caller, because it is half of a
/// contract: the server's default and ceiling live in the historian, and a
/// caller that hard-coded a different number would silently disagree with the
/// documentation both sides point at.
pub const RANGE_LIMIT_DEFAULT: usize = 5_000;

/// What a series is: counter, gauge, or bool.
///
/// **The one kind vocabulary in the tree.** `zensight-store` introduced this
/// distinction for its `metrics` rows (#904) as its own `MetricKind`; that is
/// now an alias of this type, so the on-disk code and the wire token are two
/// encodings of one enum rather than two enums that have to be kept in step.
/// The store crate depends on this one, so the alias runs the only direction
/// it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SeriesKind {
    /// Monotonic until whatever counts it restarts.
    Counter,
    /// A level: it may fall, and falling means it fell.
    Gauge,
    /// A 0/1 step series — interface up/carrier, route present, wg up.
    Bool,
}

impl SeriesKind {
    /// The wire token.
    pub fn as_str(&self) -> &'static str {
        match self {
            SeriesKind::Counter => "counter",
            SeriesKind::Gauge => "gauge",
            SeriesKind::Bool => "bool",
        }
    }
}

impl std::fmt::Display for SeriesKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How samples in a step were reduced to one point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Aggregate {
    /// Every stored bucket in the window, unreduced. Bounded by `limit` like
    /// any other reply — "raw" is not "unbounded".
    Raw,
    /// Arithmetic mean of the buckets in the step.
    Avg,
    /// Lowest value any sample in the step took, from the bucket's own range —
    /// which is why the store keeps one (#904). A coarse tier that reported
    /// only its closing value could not answer this.
    Min,
    /// Highest value any sample in the step took.
    Max,
    /// The last observation in the step.
    Last,
    /// Per-second rate, with a counter reset restarting from zero.
    Rate,
}

impl Aggregate {
    /// The wire token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Aggregate::Raw => "raw",
            Aggregate::Avg => "avg",
            Aggregate::Min => "min",
            Aggregate::Max => "max",
            Aggregate::Last => "last",
            Aggregate::Rate => "rate",
        }
    }

    /// Parse a wire token; `None` for anything else, which the server turns
    /// into `error/invalid-args` rather than quietly picking a default. A
    /// misspelled aggregate that silently became `avg` would be a wrong chart
    /// with no way to notice.
    pub fn parse(s: &str) -> Option<Aggregate> {
        Some(match s {
            "raw" => Aggregate::Raw,
            "avg" => Aggregate::Avg,
            "min" => Aggregate::Min,
            "max" => Aggregate::Max,
            "last" => Aggregate::Last,
            "rate" => Aggregate::Rate,
            _ => return None,
        })
    }

    /// The aggregate to use when the caller named none.
    ///
    /// A counter's useful reading is its rate — asking for the mean of a
    /// monotonically climbing number answers nothing. A bool's is `max`: over
    /// a minute, "did it flap at all" is the question, and an average of
    /// 0.03 hides a bounce that `1` states.
    pub fn default_for(kind: SeriesKind) -> Aggregate {
        match kind {
            SeriesKind::Counter => Aggregate::Rate,
            SeriesKind::Gauge => Aggregate::Avg,
            SeriesKind::Bool => Aggregate::Max,
        }
    }
}

impl std::fmt::Display for Aggregate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One series' points in a [`RangeReply`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RangeSeries {
    /// The publishing host's v1 origin (`h-<12hex>`).
    pub origin: String,
    /// The producer chunk (`sysinfo`, `snmp`, …).
    pub producer: String,
    /// The subject tail, `/`-joined — everything after the producer chunk.
    pub subject: String,
    /// What this series is.
    pub kind: SeriesKind,
    /// UCUM-style unit (`"By"`, `"By/s"`, `"%"`, `"s"`), when the producer
    /// declared one. Absent is "unknown", never "dimensionless".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// How the points were reduced — the aggregate actually applied, which is
    /// not always the one requested.
    pub agg: Aggregate,
    /// The observed device this series belongs to: the publishing host for a
    /// host sensor, the polled device for a proxy sensor. Not derivable from
    /// `subject` without un-slugging a device chunk, so it is carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The display metric name — the subject minus a proxy producer's leading
    /// device chunk.
    ///
    /// Carried for the same reason `source` is, and for one caller in
    /// particular: a chart labels its series by metric, and reconstructing
    /// that from `subject` means knowing which producers are proxies and how
    /// their device chunks are slugged. That is a rule the store already
    /// recorded at ingest; making every consumer re-derive it is how two
    /// consumers come to disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// `(epoch_ms, value)` pairs, oldest first.
    pub points: Vec<(i64, f64)>,
}

/// The reply to `@rpc/historian/range`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RangeReply {
    /// The origin of the historian that answered. Several may answer one
    /// fleet selector (RFC 05 §2.1) and the caller merges per series; when two
    /// disagree, this is what names which said what.
    pub historian: String,
    /// Window start actually served (epoch ms).
    pub from: i64,
    /// Window end actually served (epoch ms).
    pub to: i64,
    /// Seconds per point, after the server clamped the request to a tier.
    /// A caller that asked for 1 s over a month gets the hour tier and is told
    /// so here rather than left to infer it from the point spacing.
    pub step_s: i64,
    /// True when the reply was cut short by `limit` rather than by the window.
    /// A chart that renders a truncated window without saying so is lying.
    ///
    /// **Superseded by `partial`, and kept for one release** (#1067). It says
    /// less — a window the chosen tier could not cover is also a short answer,
    /// and this field could not say so — and it is spelled wrong for the
    /// generic reader: `zenkey_fleet::CallAnswer::page_signal()` looks for a
    /// boolean `partial` and nothing else, so for as long as this was the only
    /// marker, `@rpc/historian/range` was not merely a *bad* RFC 05 §3.2
    /// envelope, it was not seen as one at all.
    pub truncated: bool,
    /// The producer stopped before completing the walk — the `limit` was
    /// spent, or the tier could not cover the window asked for (#1067). The
    /// RFC 05 §3.2 marker, and the only field the generic reader keys off.
    #[serde(default)]
    pub partial: bool,
    /// Opaque cursor for the next page, or absent at the end.
    ///
    /// A **value** cursor since #1068: the last emitted series name and the
    /// points consumed within it. It was a positional index into a freshly
    /// sorted list, which keeps the *order* stable but not the *indices* — a
    /// series interned before the cut made page two repeat one, retention
    /// removing one made it skip one, and neither was signalled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Buckets read to build this page, so an expensive empty answer can be
    /// told from a cheap one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned: Option<u64>,
    /// The oldest instant the chosen tier could have answered for, as an RFC
    /// 3339 string — present only when that is **later** than `from`, i.e.
    /// when the answer is narrower than the question (#1067).
    ///
    /// A sub-minute `step` is served from the hot ring, which holds minutes;
    /// a caller asking twenty-four hours at `step=10` used to get whatever the
    /// ring held with `truncated: false` and a null cursor — and the reply
    /// echoes `from`/`to` unchanged, so a chart drew a 24-hour axis with ten
    /// minutes of data at the right edge and no gap marker.
    ///
    /// A string, not epoch millis: the generic reader takes it with `as_str()`
    /// and a number is read as absent — see
    /// [`crate::page::instant_from_epoch_ms`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covers_from: Option<String>,
    /// One entry per series, in a stable order so a cursor means the same
    /// thing on the next call.
    pub series: Vec<RangeSeries>,
}

/// One series this historian holds, from `@rpc/historian/series`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SeriesInfo {
    /// The publishing host's v1 origin (`h-<12hex>`).
    pub origin: String,
    /// The producer chunk.
    pub producer: String,
    /// The subject tail, `/`-joined.
    pub subject: String,
    /// What this series is.
    pub kind: SeriesKind,
    /// The observed device (`TelemetryPoint::source`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The display metric name (`TelemetryPoint::metric`) — the subject minus
    /// a proxy producer's leading device chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// UCUM-style unit, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// What kind of thing a [`TimelineEntry`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TimelineKind {
    /// An `events`-class record (an SNMP trap, say).
    Event,
    /// An alert appearing or clearing.
    Alert,
}

impl TimelineKind {
    /// The wire token.
    pub fn as_str(&self) -> &'static str {
        match self {
            TimelineKind::Event => "event",
            TimelineKind::Alert => "alert",
        }
    }
}

impl std::fmt::Display for TimelineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One durable timeline record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TimelineEntry {
    /// Time-sortable uid — also the pagination cursor.
    pub uid: String,
    /// When it happened (epoch ms).
    pub ts: i64,
    /// Event or alert.
    pub kind: TimelineKind,
    /// The publishing host's v1 origin.
    pub origin: String,
    /// The key it rode, so a reader can go back to the source of truth.
    pub key: String,
    /// Whether this is the thing appearing or clearing. An alert timeline that
    /// recorded only firings would show every incident as permanent.
    pub active: bool,
    /// A short human summary; the full record is at `key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// The reply to `@rpc/historian/timeline`, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TimelineReply {
    /// The origin of the historian that answered.
    pub historian: String,
    /// The entries, newest first.
    pub entries: Vec<TimelineEntry>,
    /// The uid to pass as `after_uid` for the next page, absent at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Rows held in one tier, from [`HistorianStats`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TierRows {
    /// `second`, `minute` or `hour`.
    pub tier: String,
    /// Buckets stored in it.
    pub rows: u64,
}

/// The reply to `@rpc/historian/stats`.
///
/// Every retention default in the store was chosen on a laptop; the reference
/// fleet is six 1–2 GB VMs where the sensor bundle has already OOM-killed one.
/// This is the surface that says whether the defaults survive it (#911) —
/// which is why it reports bytes and durations, not just counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct HistorianStats {
    /// The origin of the historian that answered.
    pub historian: String,
    /// Distinct series held.
    pub series: u64,
    /// Buckets per tier.
    pub rows_by_tier: Vec<TierRows>,
    /// Database file size in bytes.
    pub db_bytes: u64,
    /// Samples dropped since start — by shedding, by an undecodable payload,
    /// or by a full queue. Zero is a claim; absent would not be.
    pub dropped_total: u64,
    /// Wall time of the last prune pass, milliseconds. Absent before the first
    /// one has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prune_ms: Option<u64>,
    /// Oldest bucket held, epoch ms — the honest answer to "how far back can I
    /// ask", which retention makes a moving target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_ts: Option<i64>,
    /// Live bytes — stored rows plus engine metadata — as opposed to
    /// `db_bytes`, the file, which only grows (#1064). The ceiling is judged
    /// against this one. Absent on a memory-only historian.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_bytes: Option<u64>,
    /// Prune passes in which `max_db_bytes`, not the retention, removed
    /// history (#1064). Non-zero means the configured retention does not fit
    /// the disk it was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling_prunes_total: Option<u64>,
    /// Samples that arrived out of order and were inserted at their place in
    /// the hot ring (#1062). Expected traffic — the AdvancedSubscriber's
    /// recovery retransmits — but a large number beside a small `recorded`
    /// says the link is losing samples, which is not visible anywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reordered_total: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A counter's default aggregate is its rate. Asking for the mean of a
    /// monotonically climbing number answers nothing, and every caller that
    /// forgot to say so got exactly that before the default was written down.
    #[test]
    fn the_default_aggregate_follows_the_kind() {
        assert_eq!(Aggregate::default_for(SeriesKind::Counter), Aggregate::Rate);
        assert_eq!(Aggregate::default_for(SeriesKind::Gauge), Aggregate::Avg);
        // Over a minute, "did it flap at all" is the question; an average of
        // 0.03 hides a bounce that `1` states.
        assert_eq!(Aggregate::default_for(SeriesKind::Bool), Aggregate::Max);
    }

    /// An unrecognised aggregate is a parse failure, not a default. A
    /// misspelled `agg=` that quietly became `avg` would be a wrong chart with
    /// nothing to notice.
    #[test]
    fn an_unknown_aggregate_does_not_silently_become_a_default() {
        assert_eq!(Aggregate::parse("rate"), Some(Aggregate::Rate));
        assert_eq!(Aggregate::parse("mean"), None);
        assert_eq!(Aggregate::parse(""), None);
        assert_eq!(
            Aggregate::parse("AVG"),
            None,
            "the wire spelling is lowercase"
        );
    }

    /// The wire tokens are the registry's vocabulary: they are what a stored
    /// reply and a `describe` schema agree on, so a rename is a retirement.
    #[test]
    fn wire_tokens_round_trip_through_serde() {
        for (kind, token) in [
            (SeriesKind::Counter, "\"counter\""),
            (SeriesKind::Gauge, "\"gauge\""),
            (SeriesKind::Bool, "\"bool\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), token);
            assert_eq!(kind.as_str(), token.trim_matches('"'));
            assert_eq!(serde_json::from_str::<SeriesKind>(token).unwrap(), kind);
        }
        for agg in [
            Aggregate::Raw,
            Aggregate::Avg,
            Aggregate::Min,
            Aggregate::Max,
            Aggregate::Last,
            Aggregate::Rate,
        ] {
            let token = serde_json::to_string(&agg).unwrap();
            assert_eq!(token, format!("\"{}\"", agg.as_str()));
            assert_eq!(Aggregate::parse(agg.as_str()), Some(agg));
        }
        for kind in [TimelineKind::Event, TimelineKind::Alert] {
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
    }

    /// The completeness signals are the only ones a caller has that a window is
    /// partial, so they must survive the round trip even when the reply is
    /// otherwise empty.
    #[test]
    fn an_empty_reply_still_states_its_window_and_completeness() {
        let reply = RangeReply {
            historian: "h-0123456789ab".into(),
            from: 1_000,
            to: 2_000,
            step_s: 60,
            truncated: false,
            partial: false,
            next_cursor: None,
            scanned: None,
            covers_from: None,
            series: vec![],
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: RangeReply = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reply);
        assert!(
            json.contains("\"truncated\":false"),
            "completeness is never elided: a reader must not have to guess it"
        );
        assert!(
            json.contains("\"partial\":false"),
            "the RFC 05 §3.2 marker is never elided either — a reply without a \
             boolean `partial` is not read as a bad envelope, it is not read as \
             an envelope at all (#1067)"
        );
    }

    /// A reply written by a historian from before #1067 still parses: the four
    /// new fields all default. The reverse — an old *reader* against a new
    /// reply — is the additive case serde already handles.
    #[test]
    fn a_reply_from_before_the_envelope_still_parses() {
        let old = r#"{"historian":"h-0123456789ab","from":1000,"to":2000,
                      "step_s":60,"truncated":true,"next_cursor":"3:0","series":[]}"#;
        let back: RangeReply = serde_json::from_str(old).unwrap();
        assert!(back.truncated);
        assert!(
            !back.partial,
            "absent defaults to false, never to `truncated`"
        );
        assert_eq!(back.next_cursor.as_deref(), Some("3:0"));
        assert!(back.scanned.is_none());
        assert!(back.covers_from.is_none());
    }
}
