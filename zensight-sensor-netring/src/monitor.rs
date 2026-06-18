//! Builds the netring `Monitor` from config and wires its callbacks to ZenSight
//! publishing via channels (handlers stay cheap; publishing happens off the
//! capture path).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use zensight_common::{FlowRecord, TelemetryPoint, TlsRecord};

/// Bounded ring of recent ended-flow records served via `@/query/flows`.
pub type FlowRing = Arc<Mutex<VecDeque<FlowRecord>>>;

/// Max recent flows retained for the on-demand `@/query/flows` channel.
const FLOW_RING_CAP: usize = 512;

use flowscope::EndReason;
use flowscope::detect::patterns::{PortScanDetector, ScanScore, ScanVerdict};
use flowscope::extract::FiveTupleKey;
use netring::anomaly::shipped_sinks::ChannelSink;
use netring::monitor::fingerprint::TlsFingerprint;
use netring::prelude::*;
use netring::protocol::event_typed::FlowEnded;

use crate::config::NetringConfig;
use crate::map::AnomalyView;

/// Channels the monitor emits on, drained by [`crate::publish`] tasks.
pub struct MonitorChannels {
    pub telemetry: mpsc::UnboundedReceiver<TelemetryPoint>,
    pub anomalies: mpsc::UnboundedReceiver<flowscope::OwnedAnomaly>,
    /// Shared flow counters (started, ended) for the periodic aggregate task.
    pub flow_started: Arc<AtomicU64>,
    pub flow_ended: Arc<AtomicU64>,
    /// Flow volume RED counters, accumulated from ended-flow stats: total bytes,
    /// packets and retransmits across all completed flows.
    pub flow_bytes: Arc<AtomicU64>,
    pub flow_packets: Arc<AtomicU64>,
    pub flow_retransmits: Arc<AtomicU64>,
    /// Per-flow durations (ms) of flows ended since the last aggregate tick. The
    /// drain task takes (clears) this each window to compute duration percentiles,
    /// so it stays bounded and yields a windowed distribution.
    pub flow_durations_ms: Arc<Mutex<Vec<u64>>>,
    /// Bounded ring of recent ended-flow detail records (5-tuple, volume,
    /// duration, close reason) for the on-demand `@/query/flows` channel.
    pub flow_records: FlowRing,
    /// TCP RST counters: total resets and the subset that are connection refusals
    /// (zero-payload RST = "connection refused" vs a mid-transfer abort).
    pub tcp_resets: Arc<AtomicU64>,
    pub tcp_refused: Arc<AtomicU64>,
    /// Total TLS handshakes seen (ClientHello fingerprinted).
    pub tls_handshakes: Arc<AtomicU64>,
    /// Passive TLS asset inventory keyed by (sni, ja4): the served `@/query/tls`.
    pub tls_inventory: TlsInventory,
}

/// Passive TLS fingerprint inventory: (sni, ja4) → record with a hit count.
pub type TlsInventory = Arc<Mutex<std::collections::HashMap<(String, String), TlsRecord>>>;

/// Max distinct TLS fingerprints retained (cardinality guard).
const TLS_INVENTORY_CAP: usize = 4096;

/// Detector wrapper bridging `feed`→`verdict` for the TRW port-scan detector.
struct PortScan {
    detector: PortScanDetector<FiveTupleKey>,
    last_score: Option<ScanScore<FiveTupleKey>>,
}

/// Map a flowscope severity onto a ZenSight alert severity.
pub fn map_severity(s: flowscope::event::Severity) -> zensight_common::AlertSeverity {
    use flowscope::event::Severity as S;
    use zensight_common::AlertSeverity;
    match s {
        S::Info => AlertSeverity::Info,
        S::Warning => AlertSeverity::Warning,
        _ => AlertSeverity::Critical,
    }
}

/// Decompose a `flowscope::OwnedAnomaly` into the pure [`AnomalyView`].
pub fn to_view(a: &flowscope::OwnedAnomaly) -> AnomalyView {
    let fmt = |ip: Option<std::net::IpAddr>, port: Option<u16>| {
        ip.map(|ip| match port {
            Some(p) => format!("{ip}:{p}"),
            None => ip.to_string(),
        })
    };
    AnomalyView {
        kind: a.kind.to_string(),
        severity: map_severity(a.severity),
        src: fmt(a.src_ip, a.src_port),
        dst: fmt(a.dest_ip, a.dest_port),
        proto: a.proto.map(|p| p.to_string()),
        observations: a
            .observations
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        metrics: a.metrics.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
    }
}

/// Build a netring `Monitor` from config plus the channels it emits on.
///
/// Returns the built `Monitor`, the receiving ends, and a telemetry-channel
/// **keepalive** sender. The caller must hold the keepalive for the monitor's
/// lifetime (move it into the monitor-run task): it keeps the telemetry channel
/// open even when no telemetry-producing collector (e.g. bandwidth) is enabled,
/// so the drain's "monitor finished" detection (channel close) fires only when
/// the monitor actually stops — not immediately at startup.
pub fn build(
    cfg: &NetringConfig,
) -> Result<
    (
        netring::monitor::Monitor,
        MonitorChannels,
        mpsc::UnboundedSender<TelemetryPoint>,
    ),
    Box<dyn std::error::Error>,
> {
    let (tel_tx, tel_rx) = mpsc::unbounded_channel::<TelemetryPoint>();
    let (anom_tx, anom_rx) = mpsc::unbounded_channel::<flowscope::OwnedAnomaly>();
    let flow_started = Arc::new(AtomicU64::new(0));
    let flow_ended = Arc::new(AtomicU64::new(0));
    let flow_bytes = Arc::new(AtomicU64::new(0));
    let flow_packets = Arc::new(AtomicU64::new(0));
    let flow_retransmits = Arc::new(AtomicU64::new(0));
    let flow_durations_ms = Arc::new(Mutex::new(Vec::<u64>::new()));
    let flow_records: FlowRing = Arc::new(Mutex::new(VecDeque::with_capacity(FLOW_RING_CAP)));
    let tcp_resets = Arc::new(AtomicU64::new(0));
    let tcp_refused = Arc::new(AtomicU64::new(0));
    let tls_handshakes = Arc::new(AtomicU64::new(0));
    let tls_inventory: TlsInventory = Arc::new(Mutex::new(std::collections::HashMap::new()));

    let mut b = Monitor::builder();
    b = b.name(cfg.sensor_id.clone());

    // Source: pcap replay (privilege-free) or live interfaces.
    if let Some(pcap) = &cfg.pcap {
        b = b.pcap_source(pcap);
    } else {
        for iface in &cfg.interfaces {
            b = b.interface(iface);
        }
    }

    b = b.protocol::<Tcp>();

    // Flow lifecycle counters.
    if cfg.collect.flows {
        use netring::protocol::event_typed::FlowStarted;
        let started = flow_started.clone();
        b = b.on_ctx::<FlowStarted<Tcp>>(move |_e: &FlowStarted<Tcp>, _ctx: &mut Ctx<'_>| {
            started.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        let ended = flow_ended.clone();
        let bytes = flow_bytes.clone();
        let packets = flow_packets.clone();
        let retransmits = flow_retransmits.clone();
        let durations = flow_durations_ms.clone();
        let records = flow_records.clone();
        b = b.on_ctx::<FlowEnded<Tcp>>(move |e: &FlowEnded<Tcp>, _ctx: &mut Ctx<'_>| {
            ended.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(e.stats.total_bytes(), Ordering::Relaxed);
            packets.fetch_add(e.stats.total_packets(), Ordering::Relaxed);
            retransmits.fetch_add(e.stats.total_retransmits(), Ordering::Relaxed);
            let duration_ms = e.stats.duration().as_millis() as u64;
            // Record the flow's lifetime for windowed duration percentiles. Guard
            // against unbounded growth if the drain task ever stalls.
            if let Ok(mut d) = durations.lock()
                && d.len() < 1_000_000
            {
                d.push(duration_ms);
            }
            // Record the full flow detail into the bounded ring for @/query/flows.
            let proto = e.l4.map(|p| p.to_string().to_lowercase()).unwrap_or_else(|| "tcp".into());
            let reason = format!("{:?}", e.reason).to_lowercase();
            let rec = crate::map::flow_record(
                e.key.a.to_string(),
                e.key.b.to_string(),
                &proto,
                e.stats.total_bytes(),
                e.stats.total_packets(),
                duration_ms,
                &reason,
            );
            if let Ok(mut r) = records.lock() {
                if r.len() == FLOW_RING_CAP {
                    r.pop_front();
                }
                r.push_back(rec);
            }
            Ok(())
        });
    }

    // TCP resets (connection refused vs mid-transfer abort).
    if cfg.collect.tcp_resets {
        use netring::protocol::event_typed::TcpRst;
        let resets = tcp_resets.clone();
        let refused = tcp_refused.clone();
        b = b.on_tcp_reset(move |rst: &TcpRst, _ctx: &mut Ctx<'_>| {
            resets.fetch_add(1, Ordering::Relaxed);
            if rst.zero_payload {
                refused.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        });
    }

    // Passive TLS fingerprinting (ClientHello → SNI + JA3/JA4 asset inventory).
    if cfg.collect.tls {
        let count = tls_handshakes.clone();
        let inventory = tls_inventory.clone();
        b = b.on_fingerprint(move |fp: &TlsFingerprint, _ctx: &mut Ctx<'_>| {
            count.fetch_add(1, Ordering::Relaxed);
            // Key by (sni, ja4) so repeat handshakes of the same client/host
            // collapse to one inventory entry with a hit count (cardinality).
            let key = (
                fp.sni.clone().unwrap_or_default(),
                fp.ja4.clone().unwrap_or_default(),
            );
            if let Ok(mut inv) = inventory.lock() {
                if let Some(rec) = inv.get_mut(&key) {
                    rec.count += 1;
                } else if inv.len() < TLS_INVENTORY_CAP {
                    inv.insert(
                        key,
                        TlsRecord {
                            sni: fp.sni.clone(),
                            alpn: fp.alpn.clone(),
                            ja3: fp.ja3.clone(),
                            ja4: fp.ja4.clone(),
                            count: 1,
                        },
                    );
                }
            }
            Ok(())
        });
    }

    // Per-application bandwidth (periodic report).
    if cfg.collect.bandwidth {
        let tx = tel_tx.clone();
        let sensor_id = cfg.sensor_id.clone();
        b = b.on_bandwidth(
            Duration::from_secs(cfg.bandwidth_period_secs.max(1)),
            move |bw: &BandwidthReport<'_>| {
                for (app, bps) in bw.top(20) {
                    let _ = tx.send(crate::map::bandwidth_point(&sensor_id, app, bps));
                }
                Ok(())
            },
        );
    }

    // Port-scan detector (Pillar A).
    if cfg.anomalies.port_scan {
        let scan = netring::pattern_detector! {
            name: "PortScanTRW",
            event: FlowEnded<Tcp>,
            detector: PortScan { detector: PortScanDetector::new(), last_score: None },
            feed: |evt, w| {
                let success = matches!(evt.reason, EndReason::Fin | EndReason::IdleTimeout);
                w.last_score = Some(w.detector.observe(evt.key, success));
            },
            verdict: |_evt, w| {
                w.last_score.as_ref().and_then(|s| {
                    if matches!(s.verdict, ScanVerdict::Scanner) {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
            },
        };
        b = b.detect(scan);
    }

    // Anomaly sink → channel → drain → AlertReporter.
    b = b.sink(ChannelSink::new(anom_tx));

    let monitor = b.build()?;
    // Keepalive: a spare sender the caller holds for the monitor's lifetime.
    let keepalive = tel_tx.clone();
    Ok((
        monitor,
        MonitorChannels {
            telemetry: tel_rx,
            anomalies: anom_rx,
            flow_started,
            flow_ended,
            flow_bytes,
            flow_packets,
            flow_retransmits,
            flow_durations_ms,
            flow_records,
            tcp_resets,
            tcp_refused,
            tls_handshakes,
            tls_inventory,
        },
        keepalive,
    ))
}
