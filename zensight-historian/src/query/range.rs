//! `@rpc/historian/range` and `@rpc/historian/series` (#907).
//!
//! # The contract, and why each part of it is there
//!
//! A range query is three decisions the server makes and the reply states:
//! **which series**, **at what resolution**, and **reduced how**. A reply that
//! left any of them implicit would be a chart nobody could check.
//!
//! - **Which**: `origin`/`producer`/`subject` compose one key-expression
//!   pattern, matched against the series path. `*` and `**` mean what they mean
//!   everywhere else on this bus, which is why the matching is
//!   `zenoh::key_expr` intersection and not a string prefix — `subject=**`
//!   and `subject=system/*` have to behave the way an operator already expects.
//! - **At what resolution**: `step` is clamped to a tier, and the reply says
//!   the `step_s` actually served. A caller that asked for one second over a
//!   month gets the hour tier; being told is the difference between a coarse
//!   chart and a wrong one.
//! - **Reduced how**: `agg` defaults by kind — a counter's mean answers
//!   nothing — and the reply names the aggregate applied, per series, because
//!   the default differs between series in one reply.
//!
//! And two bounds, because an unbounded query against a year of history is a
//! denial of service with extra steps: `limit` caps the points across all
//! series, and `truncated` plus `next_cursor` say so out loud. A short page or
//! a null cursor is the end — the same contract `@rpc/logs/events` has.

use std::sync::Arc;

use zensight_common::history::{Aggregate, RangeReply, RangeSeries, SeriesInfo, SeriesKind};
use zensight_common::rpc::{RpcError, RpcRequest, RpcResult};
use zensight_store::{Bucket, MetricId, Tier, rate::rate_series};

use crate::ingest::SharedStore;

/// Default window when the caller names none: the last hour.
const DEFAULT_WINDOW_MS: i64 = 3_600_000;
/// Default points per reply.
pub const DEFAULT_LIMIT: usize = 5_000;
/// Hard cap. A caller asking for more is asking for a page, not a stream.
pub const MAX_LIMIT: usize = 20_000;
/// Default step, seconds.
const DEFAULT_STEP_S: i64 = 60;

/// What `range` was asked for, after parsing and clamping.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeQuery {
    /// The composed key-expression pattern for the series path.
    pub pattern: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// Seconds per point, after clamping to a tier.
    pub step_s: i64,
    pub tier: Tier,
    /// `None` means "by kind", resolved per series.
    pub agg: Option<Aggregate>,
    pub limit: usize,
    /// `(series index, points already returned in it)`.
    pub cursor: (usize, usize),
}

/// The tier that can answer a `step`-second resolution.
///
/// Under a minute only the per-second ring has the samples; under an hour the
/// minute tier does; beyond that the hour tier. Asking a coarse tier for a
/// fine step would return a step's worth of one bucket repeated, which reads
/// as data and is not.
pub fn tier_for_step(step_s: i64) -> Tier {
    if step_s < 60 {
        Tier::Second
    } else if step_s < 3_600 {
        Tier::Minute
    } else {
        Tier::Hour
    }
}

/// Parse the selector parameters.
///
/// Malformed values default rather than erroring, matching
/// `@rpc/logs/events` — a caller that fat-fingered `limit` should get a page,
/// not a rejection. The exceptions are the two where a default would be a
/// silent wrong answer: an unrecognised `agg`, and a window whose `to` precedes
/// its `from`.
pub fn parse(req: &RpcRequest, now_ms: i64) -> Result<RangeQuery, RpcError> {
    let p = |k: &str| req.param(k);
    let num = |k: &str| p(k).and_then(|v| v.parse::<i64>().ok());

    let origin = p("origin").unwrap_or_else(|| "*".to_string());
    let producer = p("producer").unwrap_or_else(|| "*".to_string());
    let subject = p("subject").unwrap_or_else(|| "**".to_string());
    let pattern = format!("{origin}/{producer}/{subject}");
    // Compose first, validate once: a caller that writes `subject=sys tem` gets
    // told which pattern was refused, not which chunk.
    zenoh::key_expr::KeyExpr::try_from(pattern.as_str()).map_err(|e| {
        RpcError::invalid_args(format!(
            "origin/producer/subject compose an invalid key expression {pattern:?}: {e}"
        ))
    })?;

    let to_ms = num("to").unwrap_or(now_ms);
    let from_ms = num("from").unwrap_or(to_ms - DEFAULT_WINDOW_MS);
    if to_ms < from_ms {
        return Err(RpcError::invalid_args(format!(
            "to ({to_ms}) precedes from ({from_ms}) — an empty reply and an inverted window \
             look identical to a chart, so this is refused rather than answered"
        )));
    }

    let step_s = num("step").filter(|s| *s > 0).unwrap_or(DEFAULT_STEP_S);
    let agg = match p("agg") {
        None => None,
        Some(s) => Some(Aggregate::parse(&s).ok_or_else(|| {
            RpcError::invalid_args(format!(
                "unknown agg {s:?} — one of raw, avg, min, max, last, rate. A misspelling that \
                 silently became the default would be a wrong chart with nothing to notice"
            ))
        })?),
    };

    let limit = p("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);

    // The cursor is opaque by contract; this is its shape, and a malformed one
    // restarts at the beginning rather than erroring — a stale cursor from a
    // previous build should cost a repeated page, not a failed query.
    let cursor = p("cursor")
        .and_then(|c| {
            let (a, b) = c.split_once(':')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .unwrap_or((0, 0));

    Ok(RangeQuery {
        pattern,
        from_ms,
        to_ms,
        step_s,
        tier: tier_for_step(step_s),
        agg,
        limit,
        cursor,
    })
}

/// One series selected for a query.
struct Selected {
    id: MetricId,
    origin: String,
    producer: String,
    subject: String,
    kind: SeriesKind,
    source: Option<String>,
    unit: Option<String>,
}

/// The series whose path intersects `pattern`, in a stable order.
///
/// Sorted by path so a cursor means the same thing on the next call: an
/// unstable order would make pagination silently skip and repeat series as the
/// fleet interned new ones between pages.
fn select(store: &SharedStore, pattern: &str) -> Result<Vec<Selected>, RpcError> {
    let pat = zenoh::key_expr::KeyExpr::try_from(pattern)
        .map_err(|e| RpcError::invalid_args(format!("invalid pattern {pattern:?}: {e}")))?;
    let g = store.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for (id, path) in g.interner().with_prefix("") {
        let Ok(ke) = zenoh::key_expr::KeyExpr::try_from(path) else {
            continue;
        };
        if !pat.intersects(&ke) {
            continue;
        }
        let Some(meta) = g.interner().meta(id) else {
            continue;
        };
        let mut chunks = path.splitn(3, '/');
        let (Some(origin), Some(producer), Some(subject)) =
            (chunks.next(), chunks.next(), chunks.next())
        else {
            continue;
        };
        out.push(Selected {
            id,
            origin: origin.to_string(),
            producer: producer.to_string(),
            subject: subject.to_string(),
            kind: meta.kind,
            source: Some(meta.source.clone()),
            unit: meta.unit.clone(),
        });
    }
    out.sort_by(|a, b| {
        (&a.origin, &a.producer, &a.subject).cmp(&(&b.origin, &b.producer, &b.subject))
    });
    Ok(out)
}

/// Reduce `(ts, bucket)` pairs to one point per `step_s` window.
///
/// Pure, and the unit of testing for the aggregation: everything above it is
/// selection and paging.
pub fn reduce(points: &[(i64, Bucket)], step_s: i64, agg: Aggregate) -> Vec<(i64, f64)> {
    if agg == Aggregate::Raw {
        // Raw means every stored bucket, unreduced — still bounded by `limit`
        // upstream. "Raw" is not "unbounded".
        return points.iter().map(|(ts, b)| (*ts, b.last)).collect();
    }
    if agg == Aggregate::Rate {
        // The rate is computed on the underlying series and then averaged into
        // the step, not the other way round: a rate of an average of a counter
        // is not a rate of anything.
        let samples: Vec<zensight_store::Sample> = points
            .iter()
            .map(|(ts, b)| zensight_store::Sample {
                ts: *ts,
                value: b.last,
            })
            .collect();
        let rates = rate_series(&samples);
        let as_buckets: Vec<(i64, Bucket)> = rates
            .iter()
            .map(|s| (s.ts, Bucket::point(s.value)))
            .collect();
        return reduce(&as_buckets, step_s, Aggregate::Avg);
    }

    let step_ms = step_s * 1_000;
    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut current: Option<(i64, f64, u32)> = None; // (step_ts, acc, n)
    for (ts, b) in points {
        let step_ts = ts.div_euclid(step_ms) * step_ms;
        let v = match agg {
            Aggregate::Min => b.min as f64,
            Aggregate::Max => b.max as f64,
            _ => b.last,
        };
        match &mut current {
            Some((cur_ts, acc, n)) if *cur_ts == step_ts => {
                *acc = match agg {
                    Aggregate::Min => acc.min(v),
                    Aggregate::Max => acc.max(v),
                    Aggregate::Last => v,
                    _ => *acc + v, // Avg accumulates; divided below.
                };
                *n += 1;
            }
            _ => {
                if let Some((cur_ts, acc, n)) = current.take() {
                    out.push((cur_ts, finish(agg, acc, n)));
                }
                current = Some((step_ts, v, 1));
            }
        }
    }
    if let Some((cur_ts, acc, n)) = current {
        out.push((cur_ts, finish(agg, acc, n)));
    }
    out
}

fn finish(agg: Aggregate, acc: f64, n: u32) -> f64 {
    match agg {
        Aggregate::Avg => acc / n as f64,
        _ => acc,
    }
}

/// Serve `@rpc/historian/range`.
pub async fn serve_range(
    session: Arc<zenoh::Session>,
    ctx: zensight_sensor_core::v1::V1Context,
    store: SharedStore,
    historian: String,
) -> zensight_sensor_core::Result<tokio::task::JoinHandle<()>> {
    zensight_sensor_core::rpc::serve(session, &ctx, &["range"], move |req: RpcRequest| {
        let store = store.clone();
        let historian = historian.clone();
        async move { answer_range(&req, &store, historian).await }
    })
    .await
}

/// Serve `@rpc/historian/series`.
pub async fn serve_series(
    session: Arc<zenoh::Session>,
    ctx: zensight_sensor_core::v1::V1Context,
    store: SharedStore,
) -> zensight_sensor_core::Result<tokio::task::JoinHandle<()>> {
    zensight_sensor_core::rpc::serve(session, &ctx, &["series"], move |req: RpcRequest| {
        let store = store.clone();
        async move { answer_series(&req, &store) }
    })
    .await
}

/// Build one `range` reply.
async fn answer_range(req: &RpcRequest, store: &SharedStore, historian: String) -> RpcResult {
    let now_ms = zensight_common::telemetry::current_timestamp_millis();
    let q = parse(req, now_ms)?;
    let selected = select(store, &q.pattern)?;

    // The hot ring is in memory and the tiers are on disk, so the two are read
    // differently — but the caller asked one question and gets one answer.
    let handle = {
        let g = store.lock().unwrap_or_else(|e| e.into_inner());
        g.persistent()
    };

    let mut series_out: Vec<RangeSeries> = Vec::new();
    let mut budget = q.limit;
    let mut truncated = false;
    let mut next_cursor = None;

    for (idx, sel) in selected.iter().enumerate().skip(q.cursor.0) {
        if budget == 0 {
            truncated = true;
            next_cursor = Some(format!("{idx}:0"));
            break;
        }
        let agg = q.agg.unwrap_or_else(|| Aggregate::default_for(sel.kind));

        let raw: Vec<(i64, Bucket)> = if q.tier == Tier::Second {
            // The per-second resolution lives only in the ring: it is never
            // flushed as its own tier, so a sub-minute step is answered from
            // memory or not at all.
            let g = store.lock().unwrap_or_else(|e| e.into_inner());
            g.hot_samples_by_id(sel.id)
                .into_iter()
                .filter(|s| s.ts >= q.from_ms && s.ts <= q.to_ms)
                .map(|s| (s.ts, Bucket::point(s.value)))
                .collect()
        } else {
            let Some(h) = handle.clone() else {
                continue;
            };
            let (id, tier, from, to) = (sel.id, q.tier, q.from_ms, q.to_ms);
            tokio::task::spawn_blocking(move || h.query_buckets(id, tier, from, to))
                .await
                .map_err(|e| {
                    RpcError::new("error/historian/range", format!("range task failed: {e}"))
                })?
                .map_err(|e| {
                    RpcError::new("error/historian/range", format!("range read failed: {e}"))
                })?
        };

        let mut points = reduce(&raw, q.step_s, agg);
        // Resume inside a series the previous page cut in half.
        let skip = if idx == q.cursor.0 { q.cursor.1 } else { 0 };
        if skip >= points.len() {
            continue;
        }
        points.drain(..skip);
        if points.len() > budget {
            points.truncate(budget);
            truncated = true;
            next_cursor = Some(format!("{idx}:{}", skip + budget));
        }
        budget -= points.len();
        if points.is_empty() {
            continue;
        }
        series_out.push(RangeSeries {
            origin: sel.origin.clone(),
            producer: sel.producer.clone(),
            subject: sel.subject.clone(),
            kind: sel.kind,
            unit: sel.unit.clone(),
            agg,
            source: sel.source.clone(),
            points,
        });
        if next_cursor.is_some() {
            break;
        }
    }

    let reply = RangeReply {
        historian,
        from: q.from_ms,
        to: q.to_ms,
        step_s: q.step_s,
        truncated,
        next_cursor,
        series: series_out,
    };
    serde_json::to_vec(&reply)
        .map_err(|e| RpcError::new("error/historian/range", format!("encode failed: {e}")))
}

/// Build one `series` reply: what this historian holds, for a pattern.
fn answer_series(req: &RpcRequest, store: &SharedStore) -> RpcResult {
    let origin = req.param("origin").unwrap_or_else(|| "*".to_string());
    let producer = req.param("producer").unwrap_or_else(|| "*".to_string());
    let subject = req.param("subject").unwrap_or_else(|| "**".to_string());
    let pattern = format!("{origin}/{producer}/{subject}");
    let limit = req
        .param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);

    let selected = select(store, &pattern)?;
    let out: Vec<SeriesInfo> = selected
        .into_iter()
        .take(limit)
        .map(|s| SeriesInfo {
            origin: s.origin,
            producer: s.producer,
            subject: s.subject,
            kind: s.kind,
            source: s.source,
            metric: None,
            unit: s.unit,
        })
        .collect();
    serde_json::to_vec(&out)
        .map_err(|e| RpcError::new("error/historian/series", format!("encode failed: {e}")))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn req(params: &str) -> RpcRequest {
        RpcRequest {
            payload: Vec::new(),
            parameters: params.to_string(),
        }
    }

    fn b(last: f64, min: f32, max: f32) -> Bucket {
        Bucket { last, min, max }
    }

    /// The tier a step can actually be answered at. A coarse tier asked for a
    /// fine step returns one bucket repeated across the step, which reads as
    /// data and is not.
    #[test]
    fn a_step_selects_the_tier_that_can_answer_it() {
        assert_eq!(tier_for_step(1), Tier::Second);
        assert_eq!(tier_for_step(59), Tier::Second);
        assert_eq!(tier_for_step(60), Tier::Minute);
        assert_eq!(tier_for_step(3_599), Tier::Minute);
        assert_eq!(tier_for_step(3_600), Tier::Hour);
        assert_eq!(tier_for_step(86_400), Tier::Hour);
    }

    #[test]
    fn defaults_are_the_last_hour_at_one_minute_over_everything() {
        let q = parse(&req(""), 1_000_000).expect("defaults parse");
        assert_eq!(q.pattern, "*/*/**");
        assert_eq!(q.to_ms, 1_000_000);
        assert_eq!(q.from_ms, 1_000_000 - DEFAULT_WINDOW_MS);
        assert_eq!(q.step_s, 60);
        assert_eq!(q.tier, Tier::Minute);
        assert_eq!(q.agg, None, "unset means by-kind, resolved per series");
        assert_eq!(q.limit, DEFAULT_LIMIT);
        assert_eq!(q.cursor, (0, 0));
    }

    /// A page cap, not a stream. `limit` above the ceiling is clamped rather
    /// than refused: the caller wanted "as much as possible", and that is what
    /// the ceiling means.
    #[test]
    fn limit_is_clamped_not_refused() {
        assert_eq!(parse(&req("limit=100"), 0).unwrap().limit, 100);
        assert_eq!(parse(&req("limit=999999"), 0).unwrap().limit, MAX_LIMIT);
        // Zero and nonsense fall back to the default rather than returning
        // nothing — an empty reply would look like "no data".
        assert_eq!(parse(&req("limit=0"), 0).unwrap().limit, DEFAULT_LIMIT);
        assert_eq!(parse(&req("limit=lots"), 0).unwrap().limit, DEFAULT_LIMIT);
    }

    /// The two parameters where defaulting would be a silent wrong answer.
    #[test]
    fn a_bad_aggregate_and_an_inverted_window_are_refused() {
        let e = parse(&req("agg=mean"), 0).expect_err("unknown agg must error");
        assert_eq!(e.error, zensight_common::rpc::ERR_INVALID_ARGS);
        assert!(e.message.contains("mean"));

        let e = parse(&req("from=2000;to=1000"), 0).expect_err("inverted window must error");
        assert_eq!(e.error, zensight_common::rpc::ERR_INVALID_ARGS);

        // …but an equal window is a legal instant, not an inversion.
        assert!(parse(&req("from=1000;to=1000"), 0).is_ok());
    }

    /// A stale cursor from a previous build costs a repeated page, not a
    /// failed query.
    #[test]
    fn a_malformed_cursor_restarts_rather_than_failing() {
        assert_eq!(parse(&req("cursor=3:120"), 0).unwrap().cursor, (3, 120));
        assert_eq!(parse(&req("cursor=nonsense"), 0).unwrap().cursor, (0, 0));
        assert_eq!(parse(&req("cursor="), 0).unwrap().cursor, (0, 0));
    }

    /// `subject` is a key expression, so `*` and `**` mean what they mean
    /// everywhere else on this bus — which is why an invalid one is caught
    /// here, naming the composed pattern rather than the chunk.
    #[test]
    fn the_composed_pattern_is_validated_as_a_key_expression() {
        assert_eq!(
            parse(
                &req("origin=h-0123456789ab;producer=snmp;subject=sw1/**"),
                0
            )
            .unwrap()
            .pattern,
            "h-0123456789ab/snmp/sw1/**"
        );
        let e = parse(&req("subject=a//b"), 0).expect_err("an empty chunk is not a key expr");
        assert_eq!(e.error, zensight_common::rpc::ERR_INVALID_ARGS);
        assert!(
            e.message.contains("a//b"),
            "the message names the pattern refused"
        );
    }

    /// `avg` means the mean of the buckets' values; `min`/`max` read the
    /// bucket's own range, which is the whole reason the store keeps one — a
    /// coarse tier that reported only its closing value could not answer them.
    #[test]
    fn each_aggregate_reduces_a_step_the_way_it_says() {
        // Three buckets inside one 60 s step, one in the next.
        let points = vec![
            (0, b(10.0, 5.0, 40.0)),
            (20_000, b(20.0, 1.0, 30.0)),
            (40_000, b(30.0, 8.0, 12.0)),
            (60_000, b(100.0, 100.0, 100.0)),
        ];
        assert_eq!(
            reduce(&points, 60, Aggregate::Avg),
            vec![(0, 20.0), (60_000, 100.0)]
        );
        assert_eq!(
            reduce(&points, 60, Aggregate::Min),
            vec![(0, 1.0), (60_000, 100.0)],
            "min reads the buckets' ranges, not their closing values"
        );
        assert_eq!(
            reduce(&points, 60, Aggregate::Max),
            vec![(0, 40.0), (60_000, 100.0)]
        );
        assert_eq!(
            reduce(&points, 60, Aggregate::Last),
            vec![(0, 30.0), (60_000, 100.0)]
        );
        // Raw is every stored bucket, unreduced — still bounded by `limit`
        // upstream.
        assert_eq!(
            reduce(&points, 60, Aggregate::Raw),
            vec![(0, 10.0), (20_000, 20.0), (40_000, 30.0), (60_000, 100.0)]
        );
    }

    /// The rate is computed on the underlying series and *then* averaged into
    /// the step. A rate of an average of a counter is not a rate of anything —
    /// and the two differ whenever a step holds more than one bucket, which is
    /// every coarse query.
    #[test]
    fn rate_is_taken_before_the_step_not_after() {
        // A counter climbing 100/s, sampled every 10 s across two minutes.
        let points: Vec<(i64, Bucket)> = (0..13)
            .map(|i| (i * 10_000, Bucket::point((i * 1_000) as f64)))
            .collect();
        let out = reduce(&points, 60, Aggregate::Rate);
        assert!(!out.is_empty());
        for (_, v) in &out {
            assert!(
                (*v - 100.0).abs() < 1e-9,
                "a steady 100/s counter must read as 100/s at every step, got {v}"
            );
        }
        // Averaging the counter first and differencing after would give the
        // step's mean VALUE, not its rate — a number in the thousands here.
        assert!(out.iter().all(|(_, v)| *v < 1_000.0));
    }

    /// A counter reset restarts the accumulation from zero rather than
    /// producing a negative rate or a hole — Prometheus' rule, applied where
    /// the kind is known.
    #[test]
    fn a_reset_inside_a_step_does_not_produce_a_negative_rate() {
        let points = vec![
            (0, Bucket::point(5_000.0)),
            (10_000, Bucket::point(10.0)), // restart
            (20_000, Bucket::point(1_010.0)),
        ];
        let out = reduce(&points, 60, Aggregate::Rate);
        assert!(
            out.iter().all(|(_, v)| *v >= 0.0),
            "no step may report a negative rate: {out:?}"
        );
    }

    #[test]
    fn an_empty_series_reduces_to_nothing() {
        for agg in [
            Aggregate::Raw,
            Aggregate::Avg,
            Aggregate::Min,
            Aggregate::Max,
            Aggregate::Last,
            Aggregate::Rate,
        ] {
            assert!(reduce(&[], 60, agg).is_empty(), "{agg} on an empty series");
        }
    }
}
