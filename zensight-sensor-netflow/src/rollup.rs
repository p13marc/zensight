//! Per-exporter rollups + the bounded flows ring (RFC 11 §3).
//!
//! The v1 registry deliberately budgets netflow's telemetry to
//! `{exporter}/{metric...}` rollups — per-flow-pair keys are the unbounded
//! population the convention forbids (RFC 04 §1.2). The raw records stay
//! available as pull-only detail: a bounded in-memory ring served on the
//! `flows` read procedure (`?exporter=…;max=…`), mirroring the logs sensor's
//! per-line event ring (#358).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use zensight_common::page::Page;
use zensight_common::registry::netflow::Subject;
use zensight_common::telemetry::{TelemetryPoint, TelemetryValue};
use zensight_sensor_core::ring::BoundedRing;

use crate::fields::MAX_EXPORTERS;
use crate::receiver::{FlowFieldValue, FlowRecord, protocol_number_to_name};

/// Default reply cap when no `?max=` selector is supplied.
pub const DEFAULT_FLOWS_REPLY_MAX: usize = 500;

/// Flow-ring capacity (recent raw records held for the `flows` procedure),
/// and the most one reply may carry: a reply cap is a memory bound on this
/// process, and `partial` + `next_cursor` is how a caller asks for more.
pub const FLOWS_RING_CAPACITY: usize = 2048;

/// The bounded ring of recent flow records, shared between the intake loop
/// and the `flows` queryable tasks — the framework's [`BoundedRing`] since
/// #1156, the same shape the logs sensor's event ring uses.
pub type FlowRing = Arc<BoundedRing<FlowRecord>>;

/// Create an empty flow ring.
pub fn new_ring() -> FlowRing {
    Arc::new(BoundedRing::new(FLOWS_RING_CAPACITY))
}

/// The two spellings of the flow-detail read (#1156, the shape #1147 gave
/// the logs sensor). They run the same walk over the same ring and differ
/// only in what they put on the wire: `flows` cannot be changed in place —
/// RFC 08 §3 calls a changed reply type on an existing path incompatible and
/// the lock refuses it — so the envelope arrives as a sibling and `flows`
/// keeps its `Vec<FlowRecord>` contract for every caller built against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Procedure {
    /// `flows` — `Vec<FlowRecord>`. Cannot say "I stopped early".
    Bare,
    /// `flows/page` — `Page<FlowRecord>`, with `partial`, `next_cursor` and
    /// `scanned`.
    Paged,
}

impl Procedure {
    /// This procedure's `@rpc` key — built, not formatted: `flows/page` is two
    /// chunks, and `query_key` refuses an embedded `/`.
    #[must_use]
    pub fn key(self) -> String {
        match self {
            Self::Bare => zensight_common::command::query_key("netflow", "flows"),
            Self::Paged => zensight_common::command::nested_query_key("netflow", "flows", "page"),
        }
    }
}

/// Cumulative per-exporter counters. Counters (not deltas): consumers rate
/// them, and a sensor restart reads as a counter reset like every other
/// Counter series.
#[derive(Debug, Default)]
struct ExporterAgg {
    flows: u64,
    /// Bytes as counted, **scaled by the declared sampling interval** when the
    /// exporter declared one (#1075). Scaling at ingest rather than at publish
    /// keeps the counter monotonic across a change of interval: a router
    /// re-configured from 1-in-1000 to 1-in-100 mid-run would otherwise make
    /// the published total jump by a factor of ten and read as a reset.
    bytes: u64,
    packets: u64,
    by_proto: HashMap<String, u64>,
    /// The interval in force when this exporter was last heard, for the label.
    /// `None` means it never declared one — which is a different fact from
    /// declaring 1, and is labelled differently.
    sampling: Option<u32>,
    /// When this exporter was last heard, as a monotonic sequence rather than
    /// a clock — the eviction order only needs to be an order, and a counter
    /// makes it testable without sleeping (#1139).
    last_seen: u64,
}

/// Rollup accumulator for all exporters seen by this receiver.
///
/// **Bounded** (#1139). NetFlow is UDP with no handshake and the exporter name
/// defaults to the source address of the datagram, so this map grew by one
/// permanent aggregate per address ever seen: a /16 sweep was 65 000 entries,
/// a spoofing sender was unbounded — and every entry was re-published, at
/// three or more keys apiece, every rollup period **forever**. The parser map
/// next door had been capped with an LRU and a comment explaining exactly this
/// since it was written; evicting a parser did not evict its aggregate.
#[derive(Debug, Default)]
pub struct Rollups {
    per_exporter: HashMap<String, ExporterAgg>,
    /// Ticks once per ingest, so "least recently seen" is a total order.
    seq: u64,
}

/// The key chunk an exporter name becomes (names may be raw IPs when the
/// `exporter_names` map has no entry) — the `{exporter}` the generated
/// builder binds, read back off the subject. Test-only since #1274: the
/// builder slugs the raw name itself, and this is how the tests pin what it
/// does.
///
/// #1153: this used to be `name.replace(['.', ':'], "-")`, and the test beside
/// it was called `exporter_slug_is_one_chunk` — which was not true. A chunk
/// must begin and end alphanumeric (RFC 03 §1.5), and every address written
/// with a leading `::` breaks that: `::1` became `--1`, and
/// `::ffff:192.0.2.1` became `--ffff-192-0-2-1`. Both are illegal chunks, from
/// an ordinary loopback and an ordinary v4-mapped address.
///
/// It was also not injective — `a.b`, `a:b` and a device literally named
/// `a-b` all produced `a-b` — so three exporters could have shared one set of
/// counters.
///
/// The builder's slug is `zenkey::Chunk::slug`: legal by construction and
/// injective by its left inverse. An ordinary IPv4 address is already a legal
/// chunk, so `192.168.1.1` passes through **unchanged** and only the
/// colon-bearing forms move.
#[cfg(test)]
fn exporter_slug(name: &str) -> String {
    Subject::exporter_metric(name, ["flows_total"])
        .vars()
        .into_iter()
        .next()
        .map(|(_, chunk)| chunk)
        .expect("`{exporter}/{metric...}` binds the exporter first")
}

/// A rollup point beside the subject it publishes under (#1274).
pub type Built = (Subject, TelemetryPoint);

impl Rollups {
    /// Fold one flow record into its exporter's counters.
    ///
    /// Past [`MAX_EXPORTERS`] the exporter seen least recently is evicted, as
    /// the parser map does (#1139). An evicted exporter's counters restart
    /// from zero if it comes back, which a TSDB reads as a counter reset —
    /// the correct and recoverable answer, and a smaller lie than publishing
    /// an aggregate for an address that sent one spoofed datagram in March.
    pub fn ingest(&mut self, record: &FlowRecord, sampling: Option<u32>) {
        self.seq += 1;
        if !self.per_exporter.contains_key(&record.exporter_name)
            && self.per_exporter.len() >= MAX_EXPORTERS
            && let Some(stale) = self
                .per_exporter
                .iter()
                .min_by_key(|(_, a)| a.last_seen)
                .map(|(k, _)| k.clone())
        {
            tracing::warn!(
                evicted = %stale, arriving = %record.exporter_name, cap = MAX_EXPORTERS,
                "NetFlow: rollup exporter cap reached; evicting the least recently seen"
            );
            self.per_exporter.remove(&stale);
        }
        let seq = self.seq;
        let agg = self
            .per_exporter
            .entry(record.exporter_name.clone())
            .or_default();
        agg.last_seen = seq;
        agg.sampling = sampling;
        // Every version's fields reach these three names now (#1072). They used
        // to be v5/v7 spellings only: v9 minted `inbytes`/`inpkts` and IPFIX
        // minted `iana(octetdeltacount)`, so `bytes_total` and `packets_total`
        // stayed at zero forever on the two versions anyone deploys — while
        // `flows_total` counted correctly, which is what made the exporter look
        // healthy.
        agg.flows += 1;
        // A sampled exporter reports one flow in N; the bytes it reports are a
        // sample of the bytes that crossed. Scaling here is what makes the
        // published counter mean throughput rather than a thousandth of it.
        let scale = u64::from(sampling.unwrap_or(1).max(1));
        if let Some(FlowFieldValue::Uint(b)) = record.fields.get(crate::fields::BYTES) {
            agg.bytes = agg.bytes.saturating_add(b.saturating_mul(scale));
        }
        if let Some(FlowFieldValue::Uint(p)) = record.fields.get(crate::fields::PACKETS) {
            agg.packets = agg.packets.saturating_add(p.saturating_mul(scale));
        }
        let proto = match record.fields.get(crate::fields::PROTOCOL) {
            Some(FlowFieldValue::Uint(p)) => protocol_number_to_name(*p as u8),
            _ => "unknown".to_string(),
        };
        *agg.by_proto.entry(proto).or_default() += 1;
    }

    /// The current rollup series, one [`TelemetryPoint`] beside the
    /// `{exporter}/{metric...}` subject it publishes under (#1274). The
    /// point's metric is the key tail after the producer chunk and its
    /// `source` is the exporter; the builder is handed the exporter's raw
    /// name and slugs it once, exactly as [`exporter_slug`] does.
    pub fn points(&self, timestamp: i64) -> Vec<Built> {
        let mut out = Vec::new();
        for (exporter, agg) in &self.per_exporter {
            // A consumer cannot tell a scaled number from a raw one, so the
            // point says which it is (#1075). `sampled=false` and an absent
            // label are different claims: "the exporter told us it is not
            // sampling" against "the exporter never said", and only the second
            // leaves a reader with a reason to doubt the number.
            let mut volume_labels = HashMap::new();
            if let Some(n) = agg.sampling {
                volume_labels.insert("sampling".to_string(), n.to_string());
                volume_labels.insert("sampled".to_string(), (n > 1).to_string());
            }
            let point = |metric: &[&str], value: u64| {
                let subject = Subject::exporter_metric(exporter, metric);
                let mut point = TelemetryPoint::for_subject(
                    exporter.clone(),
                    &subject,
                    TelemetryValue::Counter(value),
                );
                point.timestamp = timestamp;
                (subject, point)
            };
            let volume_point = |metric: &[&str], value: u64| {
                let (subject, point) = point(metric, value);
                (subject, point.with_labels(volume_labels.clone()))
            };
            out.push(point(&["flows_total"], agg.flows));
            out.push(volume_point(&["bytes_total"], agg.bytes));
            out.push(volume_point(&["packets_total"], agg.packets));
            for (proto, flows) in &agg.by_proto {
                out.push(point(&["by_proto", proto, "flows"], *flows));
            }
        }
        out
    }
}

/// Pure reply builder for the flow-detail read: newest-first,
/// exporter-filtered, one page of at most `max`.
///
/// Returns a [`Page`] rather than a bare `Vec` (#1156): the ring holds 2048
/// records and the default reply is 500, so a busy exporter's walk stops
/// early on most calls — and the bare shape had nowhere to say so. Taking
/// `max + 1` and keeping `max` is how the walk learns there was a next row
/// without paying for it; the cursor is the last row **emitted** — its
/// `timestamp`, a value, never a position (RFC 05 §3.2).
fn filter_ring(
    records: &VecDeque<FlowRecord>,
    exporter: Option<&str>,
    max: usize,
) -> Page<FlowRecord> {
    let mut matches: Vec<FlowRecord> = records
        .iter()
        .rev()
        .filter(|r| exporter.is_none_or(|e| r.exporter_name == e))
        .take(max.saturating_add(1))
        .cloned()
        .collect();
    let scanned = matches.len() as u64;
    if matches.len() > max {
        matches.truncate(max);
        let cursor = matches
            .last()
            .map(|r| r.timestamp.to_string())
            .unwrap_or_default();
        return Page::more(matches, cursor).scanned(scanned);
    }
    Page::complete(matches).scanned(scanned)
}

/// Serve one of the two flow-detail procedures (`?exporter=…;max=…`, `limit=`
/// as the paginated alias) until the session closes. Replies newest-first on
/// the concrete key: `flows` the bare `Vec<FlowRecord>` its registry entry
/// declares, `flows/page` the RFC 05 §3.2 envelope.
pub async fn serve_flows(session: Arc<zenoh::Session>, ring: FlowRing, procedure: Procedure) {
    let key = procedure.key();
    let queryable = match zensight_common::served::serve_queryable(&session, &key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "flows: declare queryable failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand flows procedure ready");

    while let Ok(query) = queryable.recv_async().await {
        let params = query.parameters();
        // Percent-decoded (#1122, #1156): an exporter named `edge 01` reaches
        // the filter as itself, not as `edge%2001`.
        let exporter = params.get("exporter").map(zensight_common::percent_decode);
        let max = params
            .get("max")
            .or_else(|| params.get("limit"))
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_FLOWS_REPLY_MAX)
            .min(FLOWS_RING_CAPACITY);
        let page = ring.with(|r| filter_ring(r, exporter.as_deref(), max));
        debug_assert!(
            !page.is_contract_violation(),
            "a truncated page must carry a cursor (RFC 05 §3.2)"
        );
        let payload = match procedure {
            Procedure::Paged => serde_json::to_vec(&page),
            Procedure::Bare => serde_json::to_vec(&page.items),
        };
        match payload {
            Ok(payload) => {
                // Concrete reply key (RFC 05 §2.1).
                if let Err(e) = query.reply(key.as_str(), payload).await {
                    tracing::warn!(error = %e, key = %key, "flows: reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, key = %key, "flows: serialize failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(exporter: &str, proto: u64, bytes: u64) -> FlowRecord {
        let mut fields = HashMap::new();
        fields.insert("protocol".to_string(), FlowFieldValue::Uint(proto));
        fields.insert("bytes".to_string(), FlowFieldValue::Uint(bytes));
        fields.insert("packets".to_string(), FlowFieldValue::Uint(1));
        FlowRecord {
            exporter_ip: "10.0.0.1".to_string(),
            exporter_name: exporter.to_string(),
            version: 5,
            fields,
            timestamp: 1,
        }
    }

    #[test]
    fn rollups_accumulate_per_exporter() {
        let mut r = Rollups::default();
        r.ingest(&rec("router01", 6, 1500), None);
        r.ingest(&rec("router01", 17, 300), None);
        r.ingest(&rec("edge02", 6, 100), None);
        let points = r.points(42);
        let get = |metric: &str, source: &str| {
            points
                .iter()
                .find(|(_, p)| p.metric == metric && p.source == source)
                .map(|(_, p)| p)
                .unwrap_or_else(|| panic!("missing {source}/{metric}"))
        };
        assert_eq!(
            get("router01/flows_total", "router01").value,
            TelemetryValue::Counter(2)
        );
        assert_eq!(
            get("router01/bytes_total", "router01").value,
            TelemetryValue::Counter(1800)
        );
        assert_eq!(
            get("router01/by_proto/tcp/flows", "router01").value,
            TelemetryValue::Counter(1)
        );
        assert_eq!(
            get("router01/by_proto/udp/flows", "router01").value,
            TelemetryValue::Counter(1)
        );
        assert_eq!(
            get("edge02/flows_total", "edge02").value,
            TelemetryValue::Counter(1)
        );
    }

    /// A sampled exporter's counters are scaled and say so (#1075).
    ///
    /// Nothing read the sampling interval, so a router at `1-out-of-1000`
    /// published `bytes_total` at a **thousandth** of throughput as a plain
    /// `Counter`, and a consumer rating it was three orders of magnitude low
    /// with nothing on the wire to notice.
    #[test]
    fn a_sampled_exporter_is_scaled_and_labelled() {
        let mut r = Rollups::default();
        r.ingest(&rec("edge01", 6, 1500), Some(1000));
        let points = r.points(42);
        let get = |metric: &str| {
            points
                .iter()
                .find(|(_, p)| p.metric == metric)
                .map(|(_, p)| p)
                .unwrap_or_else(|| panic!("missing {metric}"))
        };
        assert_eq!(
            get("edge01/bytes_total").value,
            TelemetryValue::Counter(1_500_000),
            "1500 sampled bytes at 1-in-1000 is 1.5 MB of throughput"
        );
        assert_eq!(
            get("edge01/bytes_total")
                .labels
                .get("sampling")
                .map(String::as_str),
            Some("1000")
        );
        assert_eq!(
            get("edge01/bytes_total")
                .labels
                .get("sampled")
                .map(String::as_str),
            Some("true")
        );
        // The flow COUNT is not scaled: the exporter really did report one
        // flow, and inventing 999 more would be a different lie.
        assert_eq!(get("edge01/flows_total").value, TelemetryValue::Counter(1));
        assert!(get("edge01/flows_total").labels.is_empty());
    }

    /// "Nobody told us" carries no label at all, which is a weaker claim than
    /// `sampled=false` and the honest one (#1075).
    #[test]
    fn an_exporter_that_never_declared_carries_no_sampling_claim() {
        let mut r = Rollups::default();
        r.ingest(&rec("quiet01", 6, 1500), None);
        let points = r.points(42);
        let p = points
            .iter()
            .find(|(_, p)| p.metric == "quiet01/bytes_total")
            .map(|(_, p)| p)
            .unwrap();
        assert_eq!(p.value, TelemetryValue::Counter(1500), "unscaled");
        assert!(
            p.labels.is_empty(),
            "absent, not sampled=false: the exporter never said"
        );

        // And one that declares it is NOT sampling says so out loud.
        let mut r = Rollups::default();
        r.ingest(&rec("known01", 6, 1500), Some(1));
        let points = r.points(42);
        let p = points
            .iter()
            .find(|(_, p)| p.metric == "known01/bytes_total")
            .map(|(_, p)| p)
            .unwrap();
        assert_eq!(p.value, TelemetryValue::Counter(1500));
        assert_eq!(p.labels.get("sampled").map(String::as_str), Some("false"));
    }

    /// The keys every version reaches (#1072) — v5's literals and the canonical
    /// names v9/IPFIX now mint are the same three.
    #[test]
    fn the_rollup_reads_the_canonical_names() {
        let mut fields = HashMap::new();
        fields.insert(crate::fields::BYTES.to_string(), FlowFieldValue::Uint(700));
        fields.insert(crate::fields::PACKETS.to_string(), FlowFieldValue::Uint(3));
        fields.insert(
            crate::fields::PROTOCOL.to_string(),
            FlowFieldValue::Uint(17),
        );
        // …and the raw v9 spelling beside them, which must NOT be counted twice.
        fields.insert("inbytes".to_string(), FlowFieldValue::Uint(700));
        let record = FlowRecord {
            exporter_ip: "10.0.0.9".into(),
            exporter_name: "edge09".into(),
            version: 9,
            fields,
            timestamp: 1,
        };
        let mut r = Rollups::default();
        r.ingest(&record, None);
        let points = r.points(1);
        let get = |m: &str| {
            points
                .iter()
                .find(|(_, p)| p.metric == m)
                .map(|(_, p)| p)
                .unwrap()
        };
        assert_eq!(
            get("edge09/bytes_total").value,
            TelemetryValue::Counter(700)
        );
        assert_eq!(
            get("edge09/packets_total").value,
            TelemetryValue::Counter(3)
        );
        assert_eq!(
            get("edge09/by_proto/udp/flows").value,
            TelemetryValue::Counter(1)
        );
    }

    /// #1153: legal by construction, and the common case is untouched.
    #[test]
    fn exporter_slug_is_one_chunk() {
        // IPv4 and an ordinary name are already legal chunks — unchanged.
        assert_eq!(exporter_slug("192.168.1.1"), "192.168.1.1");
        assert_eq!(exporter_slug("core-router"), "core-router");

        // Every one of these is a legal chunk, which is the claim this test's
        // name has always made and the old mapping did not keep.
        for name in [
            "192.168.1.1",
            "core-router",
            "fe80::1",
            "::1",
            "::ffff:192.0.2.1",
            "2001:db8::1",
        ] {
            let c = exporter_slug(name);
            // `unslug_for_display` returns `Some` only for a chunk that both
            // PARSES as a chunk and is in the slug's image — so a round-trip
            // to the original name proves legality and injectivity at once.
            assert_eq!(
                zensight_common::slug::unslug_for_display(&c).as_deref(),
                Some(name),
                "{name:?} -> {c:?} is not a legal chunk that decodes back"
            );
        }
    }

    /// The old mapping merged three distinct exporters onto one set of
    /// counters, because `.`, `:` and `-` all became `-`.
    #[test]
    fn three_exporters_that_used_to_share_counters_no_longer_do() {
        let slugs = ["a.b", "a:b", "a-b"].map(exporter_slug);
        assert_ne!(slugs[0], slugs[1]);
        assert_ne!(slugs[1], slugs[2]);
        assert_ne!(slugs[0], slugs[2]);
    }

    #[test]
    fn ring_filters_newest_first_and_caps() {
        let ring = new_ring();
        for i in 0..10u64 {
            let exporter = if i % 2 == 0 { "a" } else { "b" };
            let mut record = rec(exporter, 6, i);
            record.timestamp = i as i64;
            ring.push(record);
        }
        let r = ring.with(|r| r.clone());
        let out = filter_ring(&r, Some("a"), 3);
        assert_eq!(out.items.len(), 3);
        assert!(out.items.iter().all(|f| f.exporter_name == "a"));
        assert_eq!(out.items[0].timestamp, 8, "newest matching first");
    }

    /// **#1139, the acceptance.** The rollup map is bounded, and evicts the
    /// exporter seen least recently.
    ///
    /// NetFlow is UDP with no handshake and the exporter name defaults to the
    /// datagram's source address, so this map grew by one permanent aggregate
    /// per address ever seen — a /16 sweep was 65 000 entries, a spoofing
    /// sender was unbounded — and every one of them was re-published at three
    /// or more keys apiece, every rollup period, forever. The parser map next
    /// door had been capped with an LRU and a comment explaining exactly this
    /// since it was written; evicting a parser did not evict its aggregate.
    #[test]
    fn a_spoofing_sender_cannot_grow_the_rollup_map_without_bound() {
        let mut r = Rollups::default();
        // One real exporter, kept fresh throughout.
        r.ingest(&rec("10.0.0.1", 6, 100), None);

        for i in 0..(MAX_EXPORTERS * 3) {
            r.ingest(&rec(&format!("198.51.100.{i}"), 6, 1), None);
            // The real one keeps sending, so it is never the least recent.
            r.ingest(&rec("10.0.0.1", 6, 100), None);
        }

        assert!(
            r.per_exporter.len() <= MAX_EXPORTERS,
            "{} exporters held, cap is {MAX_EXPORTERS}",
            r.per_exporter.len()
        );
        assert!(
            r.per_exporter.contains_key("10.0.0.1"),
            "the exporter that kept sending must survive the flood"
        );
        // And the published series are bounded with it — this is the cost the
        // issue is about: three keys per entry, every period.
        assert!(r.points(0).len() <= MAX_EXPORTERS * 8);
    }

    /// Eviction is least-recently-seen, not arbitrary: an exporter that is
    /// still sending outlives one that stopped.
    #[test]
    fn the_least_recently_seen_exporter_is_the_one_evicted() {
        let mut r = Rollups::default();
        r.ingest(&rec("quiet", 6, 1), None);
        for i in 0..(MAX_EXPORTERS - 1) {
            r.ingest(&rec(&format!("busy-{i}"), 6, 1), None);
        }
        assert_eq!(r.per_exporter.len(), MAX_EXPORTERS);
        assert!(r.per_exporter.contains_key("quiet"));

        // One more arrival: `quiet` has been silent longest.
        r.ingest(&rec("newcomer", 6, 1), None);
        assert_eq!(r.per_exporter.len(), MAX_EXPORTERS);
        assert!(!r.per_exporter.contains_key("quiet"), "the silent one goes");
        assert!(r.per_exporter.contains_key("newcomer"));
    }

    /// The three per-exporter maps in this crate share ONE cap (#1139). They
    /// were 256, 512 and unbounded, while the sampling registry's own comment
    /// claimed it matched "the parser map's own cap".
    #[test]
    fn every_per_exporter_map_shares_one_cap() {
        assert_eq!(
            crate::fields::SamplingRegistry::MAX_EXPORTERS,
            MAX_EXPORTERS
        );
    }

    fn flow(exporter: &str, ts: i64) -> FlowRecord {
        FlowRecord {
            exporter_ip: "10.0.0.1".into(),
            exporter_name: exporter.into(),
            version: 9,
            fields: HashMap::new(),
            timestamp: ts,
        }
    }

    /// The ring holds more than the page: `partial` says so and the cursor is
    /// the last row emitted, newest first (#1156).
    #[test]
    fn a_truncated_flow_page_says_so() {
        let ring: VecDeque<FlowRecord> = (0..10).map(|i| flow("edge01", 1_000 + i)).collect();
        let page = filter_ring(&ring, None, 3);
        assert!(page.partial);
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.items[0].timestamp, 1_009, "newest first");
        assert_eq!(
            page.next_cursor.as_deref(),
            Some("1007"),
            "the last row emitted"
        );
        assert_eq!(page.scanned, Some(4), "max + 1 is all the walk paid for");
        assert!(!page.is_contract_violation());
    }

    /// A page that holds the whole walk is complete, and an exporter filter
    /// scopes the walk before the cap.
    #[test]
    fn a_complete_flow_page_and_the_exporter_filter() {
        let mut ring: VecDeque<FlowRecord> = (0..4).map(|i| flow("edge01", 1_000 + i)).collect();
        ring.push_back(flow("edge02", 2_000));
        let page = filter_ring(&ring, Some("edge02"), 10);
        assert!(!page.partial);
        assert_eq!(page.next_cursor, None);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].exporter_name, "edge02");
        let all = filter_ring(&ring, None, 10);
        assert!(!all.partial);
        assert_eq!(all.items.len(), 5);
    }

    #[test]
    fn the_two_procedures_have_their_own_keys() {
        assert!(Procedure::Bare.key().ends_with("/@rpc/netflow/flows"));
        assert!(Procedure::Paged.key().ends_with("/@rpc/netflow/flows/page"));
    }
}
