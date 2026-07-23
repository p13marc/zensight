//! Per-exporter rollups + the bounded flows ring (RFC 11 §3).
//!
//! The v1 registry deliberately budgets netflow's telemetry to
//! `{exporter}/{metric...}` rollups — per-flow-pair keys are the unbounded
//! population the convention forbids (RFC 04 §1.2). The raw records stay
//! available as pull-only detail: a bounded in-memory ring served on the
//! `flows` read procedure (`?exporter=…;max=…`), mirroring the logs sensor's
//! per-line event ring (#358).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use zensight_common::telemetry::{Protocol, TelemetryPoint, TelemetryValue};

use crate::receiver::{FlowFieldValue, FlowRecord, protocol_number_to_name};

/// Default reply cap when no `?max=` selector is supplied.
pub const DEFAULT_FLOWS_REPLY_MAX: usize = 500;

/// Flow-ring capacity (recent raw records held for the `flows` procedure).
pub const FLOWS_RING_CAPACITY: usize = 2048;

/// The bounded ring of recent flow records, shared between the intake loop
/// and the `flows` queryable task.
pub type FlowRing = Arc<Mutex<VecDeque<FlowRecord>>>;

/// Create an empty flow ring.
pub fn new_ring() -> FlowRing {
    Arc::new(Mutex::new(VecDeque::with_capacity(FLOWS_RING_CAPACITY)))
}

/// Append one record, evicting the oldest past capacity.
pub fn push(ring: &FlowRing, record: FlowRecord) {
    if let Ok(mut r) = ring.lock() {
        r.push_back(record);
        while r.len() > FLOWS_RING_CAPACITY {
            r.pop_front();
        }
    }
}

/// Cumulative per-exporter counters. Counters (not deltas): consumers rate
/// them, and a sensor restart reads as a counter reset like every other
/// Counter series.
#[derive(Debug, Default)]
struct ExporterAgg {
    flows: u64,
    bytes: u64,
    packets: u64,
    by_proto: HashMap<String, u64>,
}

/// Rollup accumulator for all exporters seen by this receiver.
#[derive(Debug, Default)]
pub struct Rollups {
    per_exporter: HashMap<String, ExporterAgg>,
}

/// One key chunk from an exporter name (names may be raw IPs when the
/// `exporter_names` map has no entry) — same `.`/`:` → `-` mapping as the
/// netring name-observation keys.
pub fn exporter_slug(name: &str) -> String {
    name.replace(['.', ':'], "-")
}

impl Rollups {
    /// Fold one flow record into its exporter's counters.
    pub fn ingest(&mut self, record: &FlowRecord) {
        let agg = self
            .per_exporter
            .entry(record.exporter_name.clone())
            .or_default();
        agg.flows += 1;
        if let Some(FlowFieldValue::Uint(b)) = record.fields.get("bytes") {
            agg.bytes += b;
        }
        if let Some(FlowFieldValue::Uint(p)) = record.fields.get("packets") {
            agg.packets += p;
        }
        let proto = match record.fields.get("protocol") {
            Some(FlowFieldValue::Uint(p)) => protocol_number_to_name(*p as u8),
            _ => "unknown".to_string(),
        };
        *agg.by_proto.entry(proto).or_default() += 1;
    }

    /// The current rollup series, one [`TelemetryPoint`] per
    /// `{exporter}/{metric...}` subject (metric = the key tail after the
    /// producer chunk; `source` = the exporter).
    pub fn points(&self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut out = Vec::new();
        for (exporter, agg) in &self.per_exporter {
            let slug = exporter_slug(exporter);
            let point = |metric: String, value: u64| TelemetryPoint {
                timestamp,
                source: exporter.clone(),
                protocol: Protocol::Netflow,
                metric,
                value: TelemetryValue::Counter(value),
                labels: HashMap::new(),
                unit: None,
            };
            out.push(point(format!("{slug}/flows_total"), agg.flows));
            out.push(point(format!("{slug}/bytes_total"), agg.bytes));
            out.push(point(format!("{slug}/packets_total"), agg.packets));
            for (proto, flows) in &agg.by_proto {
                out.push(point(format!("{slug}/by_proto/{proto}/flows"), *flows));
            }
        }
        out
    }
}

/// Pure reply builder for the `flows` procedure: newest-first,
/// exporter-filtered, capped at `max`.
fn filter_ring(
    records: &VecDeque<FlowRecord>,
    exporter: Option<&str>,
    max: usize,
) -> Vec<FlowRecord> {
    records
        .iter()
        .rev()
        .filter(|r| exporter.is_none_or(|e| r.exporter_name == e))
        .take(max)
        .cloned()
        .collect()
}

/// Serve the `flows` read procedure (`?exporter=…;max=…`) until the session
/// closes. Replies newest-first JSON `Vec<FlowRecord>` on the concrete key.
pub async fn serve_flows(session: Arc<zenoh::Session>, key: String, ring: FlowRing) {
    let queryable = match session.declare_queryable(&key).await {
        Ok(q) => q,
        Err(e) => {
            tracing::error!(error = %e, key = %key, "flows: declare queryable failed");
            return;
        }
    };
    tracing::info!(key = %key, "on-demand flows procedure ready");

    while let Ok(query) = queryable.recv_async().await {
        let params = query.parameters();
        let exporter = params.get("exporter").map(str::to_string);
        let max = params
            .get("max")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_FLOWS_REPLY_MAX);
        let records: Vec<FlowRecord> = match ring.lock() {
            Ok(r) => filter_ring(&r, exporter.as_deref(), max),
            Err(_) => Vec::new(),
        };
        match serde_json::to_vec(&records) {
            Ok(payload) => {
                // Concrete reply key (RFC 05 §2.1).
                if let Err(e) = query.reply(key.as_str(), payload).await {
                    tracing::warn!(error = %e, "flows: reply failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "flows: serialize failed"),
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
        r.ingest(&rec("router01", 6, 1500));
        r.ingest(&rec("router01", 17, 300));
        r.ingest(&rec("edge02", 6, 100));
        let points = r.points(42);
        let get = |metric: &str, source: &str| {
            points
                .iter()
                .find(|p| p.metric == metric && p.source == source)
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

    #[test]
    fn exporter_slug_is_one_chunk() {
        assert_eq!(exporter_slug("192.168.1.1"), "192-168-1-1");
        assert_eq!(exporter_slug("fe80::1"), "fe80--1");
        assert_eq!(exporter_slug("core-router"), "core-router");
    }

    #[test]
    fn ring_filters_newest_first_and_caps() {
        let ring = new_ring();
        for i in 0..10u64 {
            let exporter = if i % 2 == 0 { "a" } else { "b" };
            let mut record = rec(exporter, 6, i);
            record.timestamp = i as i64;
            push(&ring, record);
        }
        let r = ring.lock().unwrap();
        let out = filter_ring(&r, Some("a"), 3);
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|f| f.exporter_name == "a"));
        assert_eq!(out[0].timestamp, 8, "newest matching first");
    }
}
