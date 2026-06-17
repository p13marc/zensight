//! Builds the netring `Monitor` from config and wires its callbacks to ZenSight
//! publishing via channels (handlers stay cheap; publishing happens off the
//! capture path).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use zensight_common::TelemetryPoint;

use flowscope::EndReason;
use flowscope::detect::patterns::{PortScanDetector, ScanScore, ScanVerdict};
use flowscope::extract::FiveTupleKey;
use netring::anomaly::shipped_sinks::ChannelSink;
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
    /// TCP RST counters: total resets and the subset that are connection refusals
    /// (zero-payload RST = "connection refused" vs a mid-transfer abort).
    pub tcp_resets: Arc<AtomicU64>,
    pub tcp_refused: Arc<AtomicU64>,
}

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
/// Returns the built `Monitor` and the receiving ends. The caller spawns the
/// monitor's run loop and the drain tasks.
pub fn build(
    cfg: &NetringConfig,
) -> Result<(netring::monitor::Monitor, MonitorChannels), Box<dyn std::error::Error>> {
    let (tel_tx, tel_rx) = mpsc::unbounded_channel::<TelemetryPoint>();
    let (anom_tx, anom_rx) = mpsc::unbounded_channel::<flowscope::OwnedAnomaly>();
    let flow_started = Arc::new(AtomicU64::new(0));
    let flow_ended = Arc::new(AtomicU64::new(0));
    let flow_bytes = Arc::new(AtomicU64::new(0));
    let flow_packets = Arc::new(AtomicU64::new(0));
    let flow_retransmits = Arc::new(AtomicU64::new(0));
    let tcp_resets = Arc::new(AtomicU64::new(0));
    let tcp_refused = Arc::new(AtomicU64::new(0));

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
        b = b.on_ctx::<FlowEnded<Tcp>>(move |e: &FlowEnded<Tcp>, _ctx: &mut Ctx<'_>| {
            ended.fetch_add(1, Ordering::Relaxed);
            bytes.fetch_add(e.stats.total_bytes(), Ordering::Relaxed);
            packets.fetch_add(e.stats.total_packets(), Ordering::Relaxed);
            retransmits.fetch_add(e.stats.total_retransmits(), Ordering::Relaxed);
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
            tcp_resets,
            tcp_refused,
        },
    ))
}
