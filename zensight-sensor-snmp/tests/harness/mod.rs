//! In-process SNMP test harness (issue #540).
//!
//! Runs a real SNMP agent (async-snmp's agent framework) on localhost UDP and
//! drives the sensor's `SnmpPoller` against it over an isolated in-process
//! Zenoh peer — no external services, CI-safe.
//!
//! Components:
//! - [`SimMib`]: a mutable OID→value store implementing the agent's
//!   `MibHandler`, with builders for a synthetic system group and
//!   ifTable/ifXTable. Values can be changed between polls (counter
//!   advancement, status flips, sysUpTime resets).
//! - [`SimAgent`]: an async-snmp agent bound to `127.0.0.1:0` serving a
//!   `SimMib`; v2c community and/or SNMPv3 USM users per test.
//! - [`FlakyProxy`]: a UDP forwarder in front of the agent with drop-next-N
//!   and blackhole knobs — simulates loss, timeouts, and unreachable devices
//!   for any SNMP client, and gives the agent a stable front address across
//!   restarts.
//! - Poller/bus glue: isolated Zenoh session, telemetry subscriber, and
//!   `TelemetryPoint` collection helpers.

// The harness API is intentionally broader than any single test binary uses —
// later epic issues (#527 counter fixtures, #528 agent kill/restart) reuse it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_snmp::handler::{
    BoxFuture, GetNextResult, GetResult, HandlerResult, MibHandler, OidTable, RequestContext,
};
use async_snmp::{Agent, AgentBuilder, Oid, Value};
use bytes::Bytes;
use tokio::net::UdpSocket;
use zensight_common::{Format, TelemetryPoint, decode_auto};
use zensight_sensor_snmp::config::DeviceConfig;
use zensight_sensor_snmp::mib::MibResolver;
use zensight_sensor_snmp::poller::SnmpPoller;

// ---------------------------------------------------------------------------
// SimMib
// ---------------------------------------------------------------------------

/// Mutable OID→value store served by the in-process agent.
///
/// Clones share the same underlying table, so tests keep a handle and mutate
/// values while the agent serves them.
#[derive(Clone, Default)]
pub struct SimMib {
    table: Arc<RwLock<OidTable<Value>>>,
}

/// Standard OID prefixes used by the builders.
pub const SYSTEM: &str = "1.3.6.1.2.1.1";
pub const IF_TABLE: &str = "1.3.6.1.2.1.2.2.1";
pub const IF_X_TABLE: &str = "1.3.6.1.2.1.31.1.1.1";

pub fn oid(s: &str) -> Oid {
    Oid::parse(s).expect("valid OID literal")
}

fn text(s: &str) -> Value {
    Value::OctetString(Bytes::from(s.to_string()))
}

impl SimMib {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a value.
    pub fn set(&self, oid_str: &str, value: Value) {
        self.table.write().unwrap().insert(oid(oid_str), value);
    }

    pub fn remove(&self, oid_str: &str) {
        self.table.write().unwrap().remove(&oid(oid_str));
    }

    /// Synthetic SNMPv2-MIB system group.
    pub fn with_system_group(self) -> Self {
        self.set(&format!("{SYSTEM}.1.0"), text("zensight sim agent"));
        self.set(
            &format!("{SYSTEM}.2.0"),
            Value::ObjectIdentifier(oid("1.3.6.1.4.1.99999.1.1")),
        );
        self.set(&format!("{SYSTEM}.3.0"), Value::TimeTicks(123_456));
        self.set(&format!("{SYSTEM}.5.0"), text("sim-device"));
        self
    }

    /// Synthetic IF-MIB ifTable + ifXTable with `n` interfaces (indexes 1..=n).
    ///
    /// Interfaces come up admin-up/oper-up at 100 Mb/s with zeroed counters;
    /// mutate via [`set`](Self::set) between polls.
    pub fn with_if_table(self, n: u32) -> Self {
        for i in 1..=n {
            // ifTable columns
            self.set(&format!("{IF_TABLE}.1.{i}"), Value::Integer(i as i32));
            self.set(&format!("{IF_TABLE}.2.{i}"), text(&format!("eth{}", i - 1)));
            self.set(&format!("{IF_TABLE}.3.{i}"), Value::Integer(6)); // ethernetCsmacd
            self.set(&format!("{IF_TABLE}.5.{i}"), Value::Gauge32(100_000_000));
            self.set(&format!("{IF_TABLE}.7.{i}"), Value::Integer(1)); // ifAdminStatus up
            self.set(&format!("{IF_TABLE}.8.{i}"), Value::Integer(1)); // ifOperStatus up
            self.set(&format!("{IF_TABLE}.10.{i}"), Value::Counter32(0)); // ifInOctets
            self.set(&format!("{IF_TABLE}.13.{i}"), Value::Counter32(0)); // ifInDiscards
            self.set(&format!("{IF_TABLE}.14.{i}"), Value::Counter32(0)); // ifInErrors
            self.set(&format!("{IF_TABLE}.16.{i}"), Value::Counter32(0)); // ifOutOctets
            self.set(&format!("{IF_TABLE}.19.{i}"), Value::Counter32(0)); // ifOutDiscards
            self.set(&format!("{IF_TABLE}.20.{i}"), Value::Counter32(0)); // ifOutErrors
            // ifXTable columns
            self.set(
                &format!("{IF_X_TABLE}.1.{i}"),
                text(&format!("eth{}", i - 1)),
            );
            self.set(&format!("{IF_X_TABLE}.6.{i}"), Value::Counter64(0)); // ifHCInOctets
            self.set(&format!("{IF_X_TABLE}.10.{i}"), Value::Counter64(0)); // ifHCOutOctets
            self.set(&format!("{IF_X_TABLE}.15.{i}"), Value::Gauge32(100)); // ifHighSpeed (Mb/s)
            self.set(&format!("{IF_X_TABLE}.18.{i}"), text("uplink"));
        }
        self
    }
}

impl MibHandler for SimMib {
    fn get<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        oid: &'a Oid,
    ) -> BoxFuture<'a, HandlerResult<GetResult>> {
        let result = match self.table.read().unwrap().get(oid) {
            Some(v) => GetResult::Value(v.clone()),
            None => GetResult::NoSuchInstance,
        };
        Box::pin(async move { Ok(result) })
    }

    fn get_next<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        oid: &'a Oid,
    ) -> BoxFuture<'a, HandlerResult<GetNextResult>> {
        let result = match self.table.read().unwrap().get_next(oid) {
            Some((next, v)) => {
                GetNextResult::Value(async_snmp::VarBind::new(next.clone(), v.clone()))
            }
            None => GetNextResult::EndOfMibView,
        };
        Box::pin(async move { Ok(result) })
    }
}

// ---------------------------------------------------------------------------
// SimAgent
// ---------------------------------------------------------------------------

/// An in-process async-snmp agent serving a [`SimMib`] on `127.0.0.1:0`.
pub struct SimAgent {
    agent: Arc<Agent>,
    task: tokio::task::JoinHandle<()>,
}

impl SimAgent {
    /// Start a v2c agent with community `public`.
    pub async fn start(mib: SimMib) -> Self {
        Self::start_with(mib, |b| b.community(b"public")).await
    }

    /// Start with a customized builder (v3 users, other communities, engine id).
    /// The `127.0.0.1:0` bind and the mib-2 handler are pre-wired.
    pub async fn start_with(
        mib: SimMib,
        configure: impl FnOnce(AgentBuilder) -> AgentBuilder,
    ) -> Self {
        let builder = Agent::builder()
            .bind("127.0.0.1:0")
            .handler(oid("1.3.6.1.2.1"), Arc::new(mib));
        let agent = Arc::new(
            configure(builder)
                .build()
                .await
                .expect("failed to start sim agent"),
        );
        let task = tokio::spawn({
            let agent = agent.clone();
            async move {
                let _ = agent.run().await;
            }
        });
        Self { agent, task }
    }

    pub fn addr(&self) -> SocketAddr {
        self.agent.local_addr()
    }

    pub fn shutdown(&self) {
        self.agent.cancel().cancel();
        self.task.abort();
    }
}

impl Drop for SimAgent {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// FlakyProxy
// ---------------------------------------------------------------------------

/// UDP forwarder client → proxy → agent with failure-injection knobs.
///
/// Single-client by design (the poller). Client-bound datagrams can be
/// dropped (`drop_next`) or all traffic suppressed (`blackhole`); the backend
/// address can be swapped to simulate an agent restart on a new port while
/// the client keeps one stable target address.
pub struct FlakyProxy {
    addr: SocketAddr,
    drop_next: Arc<AtomicUsize>,
    blackhole: Arc<AtomicBool>,
    backend: Arc<Mutex<SocketAddr>>,
    forwarded: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl FlakyProxy {
    pub async fn start(backend: SocketAddr) -> Self {
        let front = UdpSocket::bind("127.0.0.1:0").await.expect("bind front");
        let relay = UdpSocket::bind("127.0.0.1:0").await.expect("bind relay");
        let addr = front.local_addr().expect("front addr");

        let drop_next = Arc::new(AtomicUsize::new(0));
        let blackhole = Arc::new(AtomicBool::new(false));
        let backend = Arc::new(Mutex::new(backend));
        let forwarded = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn({
            let drop_next = drop_next.clone();
            let blackhole = blackhole.clone();
            let backend = backend.clone();
            let forwarded = forwarded.clone();
            async move {
                let mut client: Option<SocketAddr> = None;
                let mut fwd_buf = [0u8; 65536];
                let mut back_buf = [0u8; 65536];
                loop {
                    tokio::select! {
                        Ok((len, from)) = front.recv_from(&mut fwd_buf) => {
                            client = Some(from);
                            if blackhole.load(Ordering::SeqCst) {
                                continue;
                            }
                            if drop_next
                                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                                .is_ok()
                            {
                                continue;
                            }
                            let to = *backend.lock().unwrap();
                            forwarded.fetch_add(1, Ordering::SeqCst);
                            let _ = relay.send_to(&fwd_buf[..len], to).await;
                        }
                        Ok((len, _)) = relay.recv_from(&mut back_buf) => {
                            if blackhole.load(Ordering::SeqCst) {
                                continue;
                            }
                            if let Some(to) = client {
                                let _ = front.send_to(&back_buf[..len], to).await;
                            }
                        }
                    }
                }
            }
        });

        Self {
            addr,
            drop_next,
            blackhole,
            backend,
            forwarded,
            task,
        }
    }

    /// Client→agent datagrams forwarded so far (request count).
    pub fn forwarded(&self) -> usize {
        self.forwarded.load(Ordering::SeqCst)
    }

    /// The stable front address to configure as the device address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Drop the next `n` client→agent datagrams (each dropped request costs
    /// the client one timeout+retry).
    pub fn drop_next(&self, n: usize) {
        self.drop_next.store(n, Ordering::SeqCst);
    }

    /// Suppress all traffic in both directions (device unreachable).
    pub fn set_blackhole(&self, on: bool) {
        self.blackhole.store(on, Ordering::SeqCst);
    }

    /// Point the proxy at a new backend (agent restarted on another port).
    pub fn set_backend(&self, backend: SocketAddr) {
        *self.backend.lock().unwrap() = backend;
    }
}

impl Drop for FlakyProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ---------------------------------------------------------------------------
// Poller / bus glue
// ---------------------------------------------------------------------------

/// Standalone Zenoh config: scouting disabled so concurrent test peers don't
/// discover each other (same pattern as sensor-core's alert_reporter tests).
pub fn isolated_zenoh_config() -> zenoh::Config {
    let mut config = zenoh::Config::default();
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .unwrap();
    config
        .insert_json5("scouting/gossip/enabled", "false")
        .unwrap();
    config
}

/// A v2c device config pointing at `addr` (community `public`).
///
/// Timeout/retry default to fast-fail (1 s, no retries) so loss-injection
/// tests stay quick; raise per test where retry behavior is the subject.
pub fn v2c_device(name: &str, addr: SocketAddr) -> DeviceConfig {
    DeviceConfig {
        name: name.to_string(),
        address: addr.to_string(),
        community: "public".to_string(),
        version: zensight_sensor_snmp::config::SnmpVersion::V2c,
        security: None,
        poll_interval_secs: 1,
        timeout_secs: 1,
        retries: 0,
        max_repetitions: 20,
        oids: Vec::new(),
        walks: Vec::new(),
        oid_group: None,
        alerts: None,
    }
}

/// Bus + poller bundle for one simulated device.
pub struct TestRig {
    pub session: Arc<zenoh::Session>,
    pub sub: zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    pub poller: SnmpPoller,
}

/// Lowercase OID→name mappings for everything [`SimMib`] serves, in the same
/// per-config `oid_names` style as `configs/snmp.json5`.
///
/// Chunk-valid names are deliberate: the built-in MIB names (`sysUpTime.0`)
/// violate the key grammar's lowercase chunk rule and trip the registry
/// metric guard in debug builds — tracked as issue #559. Unmapped OIDs fall
/// back to their dotted form, which is a valid chunk already.
pub fn test_oid_names() -> HashMap<String, String> {
    let mut names = HashMap::new();
    let mut add = |oid: String, name: &str| {
        names.insert(oid, name.to_string());
    };
    add(format!("{SYSTEM}.1.0"), "system/descr");
    add(format!("{SYSTEM}.2.0"), "system/object_id");
    add(format!("{SYSTEM}.3.0"), "system/uptime");
    add(format!("{SYSTEM}.5.0"), "system/name");
    for (col, name) in [
        (1, "index"),
        (2, "descr"),
        (3, "type"),
        (5, "speed"),
        (7, "admin_status"),
        (8, "oper_status"),
        (10, "in_octets"),
        (13, "in_discards"),
        (14, "in_errors"),
        (16, "out_octets"),
        (19, "out_discards"),
        (20, "out_errors"),
    ] {
        add(format!("{IF_TABLE}.{col}"), &format!("if/{{index}}/{name}"));
    }
    for (col, name) in [
        (1, "name"),
        (6, "hc_in_octets"),
        (10, "hc_out_octets"),
        (15, "high_speed"),
        (18, "alias"),
    ] {
        add(
            format!("{IF_X_TABLE}.{col}"),
            &format!("ifx/{{index}}/{name}"),
        );
    }
    names
}

/// Build an initialized poller over an isolated Zenoh peer with a telemetry
/// subscriber already declared (lowercase test OID names, JSON serialization).
pub async fn rig(device: DeviceConfig) -> TestRig {
    let session = Arc::new(
        zenoh::open(isolated_zenoh_config())
            .await
            .expect("open zenoh"),
    );
    let sub = session
        .declare_subscriber("v1/*/telemetry/snmp/**")
        .await
        .expect("declare subscriber");
    // Give the subscriber a beat to be routable before the poller publishes.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut resolver = MibResolver::new();
    resolver.add_custom_mappings(&test_oid_names());

    let mut poller = SnmpPoller::new(
        device,
        session.clone(),
        Arc::new(resolver),
        &HashMap::new(),
        Format::Json,
    );
    poller.init().await.expect("poller init");

    TestRig {
        session,
        sub,
        poller,
    }
}

/// [`rig`] plus threshold alerting: a shared `AlertReporter` wired into the
/// poller and a subscriber on the device's alert state keys.
pub struct AlertRig {
    pub rig: TestRig,
    pub alert_sub:
        zenoh::pubsub::Subscriber<zenoh::handlers::FifoChannelHandler<zenoh::sample::Sample>>,
    pub reporter: Arc<zensight_sensor_core::AlertReporter>,
}

/// Build a poller with alerting enabled (`cfg`) over an isolated Zenoh peer.
pub async fn rig_with_alerts(
    device: DeviceConfig,
    cfg: zensight_sensor_snmp::alerts::SnmpAlertsConfig,
) -> AlertRig {
    let device_name = device.name.clone();
    let mut rig = rig(device).await;
    let alert_sub = rig
        .session
        .declare_subscriber("v1/*/state/snmp/alert/*")
        .await
        .expect("declare alert subscriber");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let publisher = zensight_sensor_core::Publisher::new(rig.session.clone(), "snmp", Format::Json);
    let reporter = Arc::new(
        zensight_sensor_core::AlertReporter::new(
            publisher,
            zensight_common::Protocol::Snmp,
            Format::Json,
        )
        .with_debounce(Duration::from_secs(cfg.for_secs)),
    );
    let evaluator =
        zensight_sensor_snmp::alerts::AlertEvaluator::new(device_name, cfg, reporter.clone());
    rig.poller.with_alerts(evaluator);

    AlertRig {
        rig,
        alert_sub,
        reporter,
    }
}

/// Collect alert samples until `idle` elapses with no new one:
/// `(SampleKind, decoded Alert for Puts)`.
pub async fn collect_alerts(
    rig: &AlertRig,
    idle: Duration,
) -> Vec<(zenoh::sample::SampleKind, Option<zensight_common::Alert>)> {
    let mut out = Vec::new();
    while let Ok(Ok(sample)) = tokio::time::timeout(idle, rig.alert_sub.recv_async()).await {
        let alert = match sample.kind() {
            zenoh::sample::SampleKind::Put => {
                Some(decode_auto(&sample.payload().to_bytes()).expect("decode alert"))
            }
            _ => None,
        };
        out.push((sample.kind(), alert));
    }
    out
}

/// Collect telemetry points until `deadline` elapses with no new sample.
/// Returns `metric name → point` (the metric name is the last key chunks after
/// the device name).
pub async fn collect_points(rig: &TestRig, idle: Duration) -> HashMap<String, TelemetryPoint> {
    let mut points = HashMap::new();
    while let Ok(Ok(sample)) = tokio::time::timeout(idle, rig.sub.recv_async()).await {
        let point: TelemetryPoint =
            decode_auto(&sample.payload().to_bytes()).expect("decode telemetry point");
        points.insert(point.metric.clone(), point);
    }
    points
}
