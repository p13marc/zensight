//! Demo mode simulation engine.
//!
//! Provides realistic, time-varying telemetry data for demonstrating
//! ZenSight without actual sensors or Zenoh connections.

use std::collections::HashMap;
use std::f64::consts::PI;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use zensight_common::{
    Alert, AlertKind, AlertSeverity, DeviceLiveness, DeviceStatus, HealthSnapshot, HealthStatus,
    Protocol, TelemetryPoint, TelemetryValue,
};

/// Demo simulation state.
///
/// Maintains stateful counters and generates realistic time-varying metrics.
pub struct DemoSimulator {
    /// Random number generator.
    rng: SmallRng,
    /// Simulation tick counter.
    tick: u64,
    /// Per-device counter values (for monotonically increasing counters).
    counters: HashMap<String, u64>,
    /// Base values for gauges (to add variation around).
    base_values: HashMap<String, f64>,
    /// Scheduled events (tick -> event type).
    events: Vec<ScheduledEvent>,
    /// Currently active anomalies.
    active_anomalies: Vec<Anomaly>,
    /// Start time for uptime calculation.
    start_tick: u64,
    /// Metrics published counter per sensor.
    metrics_published: HashMap<String, u64>,
    /// Errors per sensor in the last "hour" (scaled for demo).
    errors_per_sensor: HashMap<String, u64>,
    /// Currently-firing sensor alerts, keyed by `Alert::alert_key()`. Used to
    /// emit `Resolved` transitions when an anomaly clears.
    firing_alerts: HashMap<String, zensight_common::Alert>,
}

/// A scheduled event that affects the simulation.
#[derive(Debug, Clone)]
struct ScheduledEvent {
    /// Tick when this event triggers.
    tick: u64,
    /// Type of event.
    event_type: EventType,
}

/// Types of events that can occur.
#[derive(Debug, Clone)]
enum EventType {
    /// CPU spike on a server.
    CpuSpike { server: String, intensity: f64 },
    /// Memory leak simulation.
    MemoryLeak { server: String },
    /// Network traffic burst.
    TrafficBurst { device: String, interface: u32 },
    /// Interface goes down.
    InterfaceDown { device: String, interface: u32 },
    /// Interface comes back up.
    InterfaceUp { device: String, interface: u32 },
    /// Disk filling up.
    DiskFilling { server: String },
    /// Temperature spike on PLC.
    TemperatureSpike { plc: String },
    /// Error log burst.
    ErrorBurst { server: String },
    /// Port scan observed by the netring probe (netring anomaly).
    PortScan { src_ip: String },
    /// Periodic C2 beaconing observed by the netring probe (netring anomaly).
    Beaconing { dst_ip: String },
    /// A service stops listening on a netlink host (netlink expectation).
    ServiceDown {
        host: String,
        service: String,
        port: u16,
    },
}

/// An active anomaly affecting values.
#[derive(Debug, Clone)]
struct Anomaly {
    /// When this anomaly started.
    start_tick: u64,
    /// How long it lasts.
    duration_ticks: u64,
    /// Type of anomaly.
    anomaly_type: AnomalyType,
}

#[derive(Debug, Clone)]
enum AnomalyType {
    CpuSpike {
        server: String,
        intensity: f64,
    },
    MemoryLeak {
        server: String,
        rate: f64,
    },
    TrafficBurst {
        device: String,
        interface: u32,
        multiplier: f64,
    },
    InterfaceDown {
        device: String,
        interface: u32,
    },
    DiskFilling {
        server: String,
        rate: f64,
    },
    TemperatureHigh {
        plc: String,
        temp: f64,
    },
    ErrorBurst {
        server: String,
    },
    PortScan {
        src_ip: String,
    },
    Beaconing {
        dst_ip: String,
    },
    ServiceDown {
        host: String,
        service: String,
        port: u16,
    },
}

impl DemoSimulator {
    /// Create a new demo simulator.
    pub fn new() -> Self {
        let mut sim = Self {
            rng: SmallRng::from_os_rng(),
            tick: 0,
            counters: HashMap::new(),
            base_values: HashMap::new(),
            events: Vec::new(),
            active_anomalies: Vec::new(),
            start_tick: 0,
            metrics_published: HashMap::new(),
            errors_per_sensor: HashMap::new(),
            firing_alerts: HashMap::new(),
        };

        // Initialize base values for servers
        sim.init_base_values();

        // Schedule initial events
        sim.schedule_random_events(0, 100);

        sim
    }

    /// Initialize base values for various metrics.
    fn init_base_values(&mut self) {
        // Server CPU baselines (different servers have different loads)
        self.base_values.insert("server01/cpu".to_string(), 35.0);
        self.base_values.insert("server02/cpu".to_string(), 55.0);
        self.base_values.insert("server03/cpu".to_string(), 25.0);
        self.base_values.insert("database01/cpu".to_string(), 45.0);

        // Memory baselines (percentage used)
        self.base_values.insert("server01/memory".to_string(), 60.0);
        self.base_values.insert("server02/memory".to_string(), 75.0);
        self.base_values.insert("server03/memory".to_string(), 40.0);
        self.base_values
            .insert("database01/memory".to_string(), 82.0);

        // Disk usage baselines
        self.base_values.insert("server01/disk".to_string(), 45.0);
        self.base_values.insert("server02/disk".to_string(), 68.0);
        self.base_values.insert("server03/disk".to_string(), 30.0);
        self.base_values.insert("database01/disk".to_string(), 55.0);

        // PLC temperatures
        self.base_values.insert("plc01/temp".to_string(), 42.0);
        self.base_values.insert("plc02/temp".to_string(), 38.0);

        // Network interface status (1.0 = up, 0.0 = down)
        for i in 1..=4 {
            self.base_values
                .insert(format!("router01/if/{}/status", i), 1.0);
            self.base_values
                .insert(format!("switch01/if/{}/status", i), 1.0);
        }
    }

    /// Schedule random events for the future.
    fn schedule_random_events(&mut self, start_tick: u64, range: u64) {
        let servers = ["server01", "server02", "server03", "database01"];
        let network_devices = ["router01", "switch01"];
        let plcs = ["plc01", "plc02"];

        // Schedule 3-6 events in the given range
        let num_events = self.rng.random_range(3..=6);

        for _ in 0..num_events {
            let tick = start_tick + self.rng.random_range(10..range);
            let event_type = match self.rng.random_range(0..13) {
                0..=2 => {
                    // CPU spike (most common)
                    let server = servers[self.rng.random_range(0..servers.len())];
                    EventType::CpuSpike {
                        server: server.to_string(),
                        intensity: self.rng.random_range(75.0..98.0),
                    }
                }
                3 => {
                    // Memory leak
                    let server = servers[self.rng.random_range(0..servers.len())];
                    EventType::MemoryLeak {
                        server: server.to_string(),
                    }
                }
                4..=5 => {
                    // Traffic burst
                    let device = network_devices[self.rng.random_range(0..network_devices.len())];
                    EventType::TrafficBurst {
                        device: device.to_string(),
                        interface: self.rng.random_range(1..=4),
                    }
                }
                6 => {
                    // Interface down
                    let device = network_devices[self.rng.random_range(0..network_devices.len())];
                    let interface = self.rng.random_range(1..=4);
                    // Schedule it to come back up
                    self.events.push(ScheduledEvent {
                        tick: tick + self.rng.random_range(5..20),
                        event_type: EventType::InterfaceUp {
                            device: device.to_string(),
                            interface,
                        },
                    });
                    EventType::InterfaceDown {
                        device: device.to_string(),
                        interface,
                    }
                }
                7 => {
                    // Disk filling
                    let server = servers[self.rng.random_range(0..servers.len())];
                    EventType::DiskFilling {
                        server: server.to_string(),
                    }
                }
                8 => {
                    // Temperature spike
                    let plc = plcs[self.rng.random_range(0..plcs.len())];
                    EventType::TemperatureSpike {
                        plc: plc.to_string(),
                    }
                }
                9 => {
                    // Error burst
                    let server = servers[self.rng.random_range(0..servers.len())];
                    EventType::ErrorBurst {
                        server: server.to_string(),
                    }
                }
                10 => {
                    // Port scan seen by the netring probe
                    let host = self.rng.random_range(20..250);
                    EventType::PortScan {
                        src_ip: format!("198.51.100.{host}"),
                    }
                }
                11 => {
                    // C2 beaconing seen by the netring probe
                    let host = self.rng.random_range(2..250);
                    EventType::Beaconing {
                        dst_ip: format!("203.0.113.{host}"),
                    }
                }
                _ => {
                    // A monitored service stops listening on a host (netlink)
                    let host = servers[self.rng.random_range(0..servers.len())];
                    let (service, port) = [("sshd", 22u16), ("nginx", 443), ("postgres", 5432)]
                        [self.rng.random_range(0..3)];
                    EventType::ServiceDown {
                        host: host.to_string(),
                        service: service.to_string(),
                        port,
                    }
                }
            };

            self.events.push(ScheduledEvent { tick, event_type });
        }

        // Sort by tick
        self.events.sort_by_key(|e| e.tick);
    }

    /// Process events for the current tick.
    fn process_events(&mut self) {
        // Find events that should trigger
        let triggered: Vec<_> = self
            .events
            .iter()
            .filter(|e| e.tick <= self.tick)
            .cloned()
            .collect();

        // Remove triggered events
        self.events.retain(|e| e.tick > self.tick);

        // Process each triggered event
        for event in triggered {
            match event.event_type {
                EventType::CpuSpike { server, intensity } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(5..15),
                        anomaly_type: AnomalyType::CpuSpike { server, intensity },
                    });
                }
                EventType::MemoryLeak { server } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(20..50),
                        anomaly_type: AnomalyType::MemoryLeak {
                            server,
                            rate: self.rng.random_range(0.5..2.0),
                        },
                    });
                }
                EventType::TrafficBurst { device, interface } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(3..10),
                        anomaly_type: AnomalyType::TrafficBurst {
                            device,
                            interface,
                            multiplier: self.rng.random_range(5.0..20.0),
                        },
                    });
                }
                EventType::InterfaceDown { device, interface } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: 999999, // Until InterfaceUp
                        anomaly_type: AnomalyType::InterfaceDown { device, interface },
                    });
                }
                EventType::InterfaceUp { device, interface } => {
                    // Remove the InterfaceDown anomaly
                    self.active_anomalies.retain(|a| {
                        !matches!(&a.anomaly_type, AnomalyType::InterfaceDown { device: d, interface: i }
                            if d == &device && i == &interface)
                    });
                }
                EventType::DiskFilling { server } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(30..60),
                        anomaly_type: AnomalyType::DiskFilling {
                            server,
                            rate: self.rng.random_range(0.3..1.0),
                        },
                    });
                }
                EventType::TemperatureSpike { plc } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(8..20),
                        anomaly_type: AnomalyType::TemperatureHigh {
                            plc,
                            temp: self.rng.random_range(65.0..85.0),
                        },
                    });
                }
                EventType::ErrorBurst { server } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(5..15),
                        anomaly_type: AnomalyType::ErrorBurst { server },
                    });
                }
                EventType::PortScan { src_ip } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(6..16),
                        anomaly_type: AnomalyType::PortScan { src_ip },
                    });
                }
                EventType::Beaconing { dst_ip } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(15..40),
                        anomaly_type: AnomalyType::Beaconing { dst_ip },
                    });
                }
                EventType::ServiceDown {
                    host,
                    service,
                    port,
                } => {
                    self.active_anomalies.push(Anomaly {
                        start_tick: self.tick,
                        duration_ticks: self.rng.random_range(8..25),
                        anomaly_type: AnomalyType::ServiceDown {
                            host,
                            service,
                            port,
                        },
                    });
                }
            }
        }

        // Remove expired anomalies
        self.active_anomalies
            .retain(|a| self.tick < a.start_tick + a.duration_ticks);

        // Schedule more events if we're running low
        if self.events.len() < 3 {
            self.schedule_random_events(self.tick, 100);
        }
    }

    /// Get or initialize a counter value.
    fn get_counter(&mut self, key: &str, initial: u64) -> u64 {
        *self.counters.entry(key.to_string()).or_insert(initial)
    }

    /// Increment a counter and return the new value.
    fn increment_counter(&mut self, key: &str, amount: u64) -> u64 {
        let counter = self.counters.entry(key.to_string()).or_insert(0);
        *counter = counter.saturating_add(amount);
        *counter
    }

    /// Increment a counter by a random delta in `[lo, hi)` and return the new
    /// value. Computes the delta into a local first to avoid borrowing `self`
    /// twice in one call expression.
    fn random_bump(&mut self, key: &str, lo: u64, hi: u64) -> u64 {
        let delta = if hi > lo {
            self.rng.random_range(lo..hi)
        } else {
            lo
        };
        self.increment_counter(key, delta)
    }

    /// Get a base value with time-varying oscillation.
    fn oscillating_value(&mut self, key: &str, default: f64, amplitude: f64) -> f64 {
        let base = *self.base_values.get(key).unwrap_or(&default);
        let phase = self.rng.random_range(0.0..PI);
        let oscillation = amplitude * (self.tick as f64 * 0.1 + phase).sin();
        let noise = self.rng.random_range(-amplitude * 0.3..amplitude * 0.3);
        (base + oscillation + noise).clamp(0.0, 100.0)
    }

    /// Generate a tick of telemetry data.
    pub fn tick(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        self.tick += 1;
        self.process_events();

        let mut points = Vec::new();

        // Generate all device telemetry
        points.extend(self.generate_servers(timestamp));
        points.extend(self.generate_network_devices(timestamp));
        points.extend(self.generate_plcs(timestamp));
        points.extend(self.generate_syslog(timestamp));
        points.extend(self.generate_netlink(timestamp));
        points.extend(self.generate_netring(timestamp));
        points.extend(self.generate_netflow(timestamp));
        points.extend(self.generate_gnmi(timestamp));

        points
    }

    /// Generate server (sysinfo) telemetry.
    fn generate_servers(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();
        let servers = ["server01", "server02", "server03", "database01"];

        for server in servers {
            // System uptime and boot time
            let uptime_secs = self.tick * 5 + 86400; // Base uptime + simulated running time
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "system/uptime",
                TelemetryValue::Counter(uptime_secs),
                timestamp,
            ));

            let boot_time = timestamp / 1000 - uptime_secs as i64;
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "system/boot_time",
                TelemetryValue::Counter(boot_time as u64),
                timestamp,
            ));

            // Load averages (with period labels)
            let load1 = self.oscillating_value(&format!("{}/load1", server), 1.5, 0.5);
            let load5 = self.oscillating_value(&format!("{}/load5", server), 1.2, 0.3);
            let load15 = self.oscillating_value(&format!("{}/load15", server), 1.0, 0.2);

            points.push(self.make_point_with_labels(
                Protocol::Sysinfo,
                server,
                "system/load",
                TelemetryValue::Gauge(load1.max(0.0)),
                timestamp,
                vec![("period".to_string(), "1m".to_string())],
            ));
            points.push(self.make_point_with_labels(
                Protocol::Sysinfo,
                server,
                "system/load",
                TelemetryValue::Gauge(load5.max(0.0)),
                timestamp,
                vec![("period".to_string(), "5m".to_string())],
            ));
            points.push(self.make_point_with_labels(
                Protocol::Sysinfo,
                server,
                "system/load",
                TelemetryValue::Gauge(load15.max(0.0)),
                timestamp,
                vec![("period".to_string(), "15m".to_string())],
            ));

            // CPU usage
            let mut cpu = self.oscillating_value(&format!("{}/cpu", server), 40.0, 10.0);

            // Check for CPU spike anomaly
            for anomaly in &self.active_anomalies {
                if let AnomalyType::CpuSpike {
                    server: s,
                    intensity,
                } = &anomaly.anomaly_type
                    && s == server
                {
                    // Spike with some variation
                    cpu = *intensity + self.rng.random_range(-5.0..5.0);
                }
            }

            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "cpu/usage",
                TelemetryValue::Gauge(cpu.clamp(0.0, 100.0)),
                timestamp,
            ));

            // Per-core CPU (4 cores) with frequency
            let base_freq = 3200.0; // 3.2 GHz base
            for core in 0..4 {
                let core_cpu = cpu + self.rng.random_range(-15.0..15.0);
                points.push(self.make_point(
                    Protocol::Sysinfo,
                    server,
                    &format!("cpu/{}/usage", core),
                    TelemetryValue::Gauge(core_cpu.clamp(0.0, 100.0)),
                    timestamp,
                ));

                // CPU frequency varies with load
                let freq =
                    base_freq + (core_cpu / 100.0) * 800.0 + self.rng.random_range(-100.0..100.0);
                points.push(self.make_point(
                    Protocol::Sysinfo,
                    server,
                    &format!("cpu/{}/frequency", core),
                    TelemetryValue::Gauge(freq.clamp(800.0, 4500.0)),
                    timestamp,
                ));
            }

            // Memory usage (using metric names that match the sensor)
            let mut memory_pct = self.oscillating_value(&format!("{}/memory", server), 60.0, 5.0);

            // Check for memory leak anomaly
            for anomaly in &self.active_anomalies {
                if let AnomalyType::MemoryLeak { server: s, rate } = &anomaly.anomaly_type
                    && s == server
                {
                    let elapsed = self.tick - anomaly.start_tick;
                    memory_pct += elapsed as f64 * rate;
                }
            }

            let total_memory = 17_179_869_184u64; // 16 GB
            let used_memory = ((memory_pct.clamp(0.0, 99.0) / 100.0) * total_memory as f64) as u64;
            let available_memory = total_memory - used_memory;

            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/total",
                TelemetryValue::Counter(total_memory),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/used",
                TelemetryValue::Counter(used_memory),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/available",
                TelemetryValue::Counter(available_memory),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/usage_percent",
                TelemetryValue::Gauge(memory_pct.clamp(0.0, 99.0)),
                timestamp,
            ));

            // Swap
            let swap_total = 8_589_934_592u64; // 8 GB
            let swap_pct = self.oscillating_value(&format!("{}/swap", server), 10.0, 5.0);
            let swap_used = ((swap_pct.clamp(0.0, 99.0) / 100.0) * swap_total as f64) as u64;

            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/swap_total",
                TelemetryValue::Counter(swap_total),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/swap_used",
                TelemetryValue::Counter(swap_used),
                timestamp,
            ));

            // Disk usage (using metric names that match the sensor: disk/{mount}/used, disk/{mount}/total)
            let mut disk_pct = self.oscillating_value(&format!("{}/disk", server), 50.0, 2.0);

            // Check for disk filling anomaly
            for anomaly in &self.active_anomalies {
                if let AnomalyType::DiskFilling { server: s, rate } = &anomaly.anomaly_type
                    && s == server
                {
                    let elapsed = self.tick - anomaly.start_tick;
                    disk_pct += elapsed as f64 * rate;
                }
            }

            let total_disk = 536_870_912_000u64; // 500 GB
            let used_disk = ((disk_pct.clamp(0.0, 99.0) / 100.0) * total_disk as f64) as u64;
            let available_disk = total_disk - used_disk;

            // Root partition
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "disk/_/total",
                TelemetryValue::Counter(total_disk),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "disk/_/used",
                TelemetryValue::Counter(used_disk),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "disk/_/available",
                TelemetryValue::Counter(available_disk),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "disk/_/usage_percent",
                TelemetryValue::Gauge(disk_pct.clamp(0.0, 99.0)),
                timestamp,
            ));

            // Network I/O with rates
            let rx_rate_val = self.rng.random_range(100_000.0..5_000_000.0);
            let tx_rate_val = self.rng.random_range(50_000.0..2_000_000.0);

            let rx = self.increment_counter(&format!("{}/eth0/rx", server), rx_rate_val as u64);
            let tx = self.increment_counter(&format!("{}/eth0/tx", server), tx_rate_val as u64);

            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "network/eth0/rx_bytes",
                TelemetryValue::Counter(rx),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "network/eth0/tx_bytes",
                TelemetryValue::Counter(tx),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "network/eth0/rx_rate",
                TelemetryValue::Gauge(rx_rate_val),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "network/eth0/tx_rate",
                TelemetryValue::Gauge(tx_rate_val),
                timestamp,
            ));

            // Top processes (simulated)
            let process_names = ["systemd", "postgres", "nginx", "java", "python3"];
            for (rank, name) in process_names.iter().enumerate() {
                let proc_cpu = self.rng.random_range(0.5..15.0) / (rank as f64 + 1.0);
                let proc_mem =
                    self.rng.random_range(50_000_000u64..500_000_000u64) / (rank as u64 + 1);

                points.push(self.make_point_with_labels(
                    Protocol::Sysinfo,
                    server,
                    &format!("process/{}/cpu", rank + 1),
                    TelemetryValue::Gauge(proc_cpu),
                    timestamp,
                    vec![
                        ("name".to_string(), name.to_string()),
                        ("pid".to_string(), (1000 + rank * 100).to_string()),
                        ("rank".to_string(), (rank + 1).to_string()),
                    ],
                ));
                points.push(self.make_point_with_labels(
                    Protocol::Sysinfo,
                    server,
                    &format!("process/{}/memory", rank + 1),
                    TelemetryValue::Counter(proc_mem),
                    timestamp,
                    vec![
                        ("name".to_string(), name.to_string()),
                        ("pid".to_string(), (1000 + rank * 100).to_string()),
                        ("rank".to_string(), (rank + 1).to_string()),
                    ],
                ));
            }
        }

        points
    }

    /// Generate network device (SNMP) telemetry.
    fn generate_network_devices(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();

        // Router
        let router = "router01";
        let uptime = self.tick * 100 + 8640000; // centiseconds (base 1 day uptime)
        points.push(self.make_point(
            Protocol::Snmp,
            router,
            "system/sysUpTime",
            TelemetryValue::Counter(uptime),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            router,
            "system/sysName",
            TelemetryValue::Text(router.to_string()),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            router,
            "system/sysDescr",
            TelemetryValue::Text("Cisco IOS XE Software, ASR1002-X, Version 17.3.4a".to_string()),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            router,
            "system/sysContact",
            TelemetryValue::Text("netops@example.com".to_string()),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            router,
            "system/sysLocation",
            TelemetryValue::Text("DC1 Rack 42".to_string()),
            timestamp,
        ));

        // Router CPU and memory
        let router_cpu = self.oscillating_value(&format!("{}/cpu", router), 25.0, 10.0);
        points.push(self.make_point(
            Protocol::Snmp,
            router,
            "host/hrProcessorLoad",
            TelemetryValue::Gauge(router_cpu.clamp(0.0, 100.0)),
            timestamp,
        ));

        let mem_total = 8_589_934_592u64; // 8 GB
        let mem_pct = self.oscillating_value(&format!("{}/mem", router), 45.0, 5.0);
        let mem_used = ((mem_pct.clamp(0.0, 99.0) / 100.0) * mem_total as f64) as u64;
        points.push(self.make_point(
            Protocol::Snmp,
            router,
            "host/hrStorageSize",
            TelemetryValue::Counter(mem_total),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            router,
            "host/hrStorageUsed",
            TelemetryValue::Counter(mem_used),
            timestamp,
        ));

        // Router interface names
        let router_ifaces = [
            "GigabitEthernet0/0/0",
            "GigabitEthernet0/0/1",
            "GigabitEthernet0/0/2",
            "TenGigabitEthernet0/1/0",
        ];

        // Router interfaces
        for iface in 1..=4 {
            let is_down = self.active_anomalies.iter().any(|a| {
                matches!(&a.anomaly_type, AnomalyType::InterfaceDown { device, interface }
                    if device == router && *interface == iface)
            });

            let status = if is_down { 2.0 } else { 1.0 }; // 1=up, 2=down

            // Traffic multiplier for bursts
            let mut traffic_mult = 1.0;
            for anomaly in &self.active_anomalies {
                if let AnomalyType::TrafficBurst {
                    device,
                    interface,
                    multiplier,
                } = &anomaly.anomaly_type
                    && device == router
                    && *interface == iface
                {
                    traffic_mult = *multiplier;
                }
            }

            let base_in = if is_down {
                0
            } else {
                (self.rng.random_range(1_000_000u64..10_000_000u64) as f64 * traffic_mult) as u64
            };
            let base_out = if is_down {
                0
            } else {
                (self.rng.random_range(500_000u64..5_000_000u64) as f64 * traffic_mult) as u64
            };

            let in_octets = self.increment_counter(&format!("{}/if/{}/in", router, iface), base_in);
            let out_octets =
                self.increment_counter(&format!("{}/if/{}/out", router, iface), base_out);

            // Interface name and description
            let iface_name = router_ifaces.get(iface as usize - 1).unwrap_or(&"Unknown");
            points.push(self.make_point(
                Protocol::Snmp,
                router,
                &format!("if/{}/ifName", iface),
                TelemetryValue::Text(iface_name.to_string()),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Snmp,
                router,
                &format!("if/{}/ifDescr", iface),
                TelemetryValue::Text(format!("{} - Uplink {}", iface_name, iface)),
                timestamp,
            ));

            points.push(self.make_point(
                Protocol::Snmp,
                router,
                &format!("if/{}/ifOperStatus", iface),
                TelemetryValue::Gauge(status),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Snmp,
                router,
                &format!("if/{}/ifAdminStatus", iface),
                TelemetryValue::Gauge(1.0), // Admin up
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Snmp,
                router,
                &format!("if/{}/ifInOctets", iface),
                TelemetryValue::Counter(in_octets),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Snmp,
                router,
                &format!("if/{}/ifOutOctets", iface),
                TelemetryValue::Counter(out_octets),
                timestamp,
            ));

            // Error counters (occasional errors)
            if self.rng.random_range(0..20) == 0 {
                let error_amount = self.rng.random_range(1..5);
                let errors = self
                    .increment_counter(&format!("{}/if/{}/errors", router, iface), error_amount);
                points.push(self.make_point(
                    Protocol::Snmp,
                    router,
                    &format!("if/{}/ifInErrors", iface),
                    TelemetryValue::Counter(errors),
                    timestamp,
                ));
            }
        }

        // Switch (similar but more ports)
        let switch = "switch01";
        points.push(self.make_point(
            Protocol::Snmp,
            switch,
            "system/sysUpTime",
            TelemetryValue::Counter(uptime + 50000),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            switch,
            "system/sysName",
            TelemetryValue::Text(switch.to_string()),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            switch,
            "system/sysDescr",
            TelemetryValue::Text("Cisco Catalyst 9300-48P, Version 17.6.3".to_string()),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            switch,
            "system/sysContact",
            TelemetryValue::Text("netops@example.com".to_string()),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            switch,
            "system/sysLocation",
            TelemetryValue::Text("DC1 Rack 43".to_string()),
            timestamp,
        ));

        // Switch CPU and memory
        let switch_cpu = self.oscillating_value(&format!("{}/cpu", switch), 15.0, 5.0);
        points.push(self.make_point(
            Protocol::Snmp,
            switch,
            "host/hrProcessorLoad",
            TelemetryValue::Gauge(switch_cpu.clamp(0.0, 100.0)),
            timestamp,
        ));

        let switch_mem_total = 4_294_967_296u64; // 4 GB
        let switch_mem_pct = self.oscillating_value(&format!("{}/mem", switch), 35.0, 5.0);
        let switch_mem_used =
            ((switch_mem_pct.clamp(0.0, 99.0) / 100.0) * switch_mem_total as f64) as u64;
        points.push(self.make_point(
            Protocol::Snmp,
            switch,
            "host/hrStorageSize",
            TelemetryValue::Counter(switch_mem_total),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Snmp,
            switch,
            "host/hrStorageUsed",
            TelemetryValue::Counter(switch_mem_used),
            timestamp,
        ));

        for port in 1..=8 {
            let is_down = self.active_anomalies.iter().any(|a| {
                matches!(&a.anomaly_type, AnomalyType::InterfaceDown { device, interface }
                    if device == switch && *interface == port)
            });

            let status = if is_down { 2.0 } else { 1.0 };

            let in_rate = self.rng.random_range(100_000u64..2_000_000u64);
            let out_rate = self.rng.random_range(50_000u64..1_000_000u64);

            let in_octets = if is_down {
                self.get_counter(&format!("{}/if/{}/in", switch, port), 0)
            } else {
                self.increment_counter(&format!("{}/if/{}/in", switch, port), in_rate)
            };

            let out_octets = if is_down {
                self.get_counter(&format!("{}/if/{}/out", switch, port), 0)
            } else {
                self.increment_counter(&format!("{}/if/{}/out", switch, port), out_rate)
            };

            // Interface name and description
            points.push(self.make_point(
                Protocol::Snmp,
                switch,
                &format!("if/{}/ifName", port),
                TelemetryValue::Text(format!("Gi1/0/{}", port)),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Snmp,
                switch,
                &format!("if/{}/ifDescr", port),
                TelemetryValue::Text(format!("GigabitEthernet1/0/{} - Access Port", port)),
                timestamp,
            ));

            points.push(self.make_point(
                Protocol::Snmp,
                switch,
                &format!("if/{}/ifOperStatus", port),
                TelemetryValue::Gauge(status),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Snmp,
                switch,
                &format!("if/{}/ifAdminStatus", port),
                TelemetryValue::Gauge(1.0),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Snmp,
                switch,
                &format!("if/{}/ifInOctets", port),
                TelemetryValue::Counter(in_octets),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Snmp,
                switch,
                &format!("if/{}/ifOutOctets", port),
                TelemetryValue::Counter(out_octets),
                timestamp,
            ));
        }

        points
    }

    /// Generate PLC (Modbus) telemetry.
    fn generate_plcs(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();
        let plcs = ["plc01", "plc02"];

        for plc in plcs {
            // Temperature sensor
            let mut temp = self.oscillating_value(&format!("{}/temp", plc), 40.0, 3.0);

            // Check for temperature spike
            for anomaly in &self.active_anomalies {
                if let AnomalyType::TemperatureHigh { plc: p, temp: t } = &anomaly.anomaly_type
                    && p == plc
                {
                    temp = *t + self.rng.random_range(-2.0..2.0);
                }
            }

            points.push(self.make_point(
                Protocol::Modbus,
                plc,
                "holding/0",
                TelemetryValue::Gauge(temp),
                timestamp,
            ));

            // Pressure sensor
            let pressure = 100.0 + self.rng.random_range(-5.0..5.0);
            points.push(self.make_point(
                Protocol::Modbus,
                plc,
                "holding/1",
                TelemetryValue::Gauge(pressure),
                timestamp,
            ));

            // Speed/RPM
            let rpm = 1500.0 + self.rng.random_range(-50.0..50.0);
            points.push(self.make_point(
                Protocol::Modbus,
                plc,
                "holding/2",
                TelemetryValue::Gauge(rpm),
                timestamp,
            ));

            // Coils (on/off states)
            points.push(self.make_point(
                Protocol::Modbus,
                plc,
                "coil/0",
                TelemetryValue::Boolean(true), // Motor running
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Modbus,
                plc,
                "coil/1",
                TelemetryValue::Boolean(self.tick % 10 < 5), // Alternating valve
                timestamp,
            ));

            // Production counter
            let prod_rate = self.rng.random_range(1..3);
            let production = self.increment_counter(&format!("{}/production", plc), prod_rate);
            points.push(self.make_point(
                Protocol::Modbus,
                plc,
                "input/0",
                TelemetryValue::Counter(production),
                timestamp,
            ));
        }

        points
    }

    /// Generate syslog messages.
    /// Uses message/{id} format with severity, facility, and app_name labels.
    fn generate_syslog(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();

        // Helper to create a log line matching the real sensor's contract (#104):
        // a per-line event keyed `events/<uid>` (unique, so every line survives
        // instead of last-writer-wins), with facility/severity plus the OTel logs
        // data model carried in labels and the message text as the value.
        let make_syslog = |id: u64,
                           server: &str,
                           severity: u64,
                           facility: &str,
                           app_name: &str,
                           msg: &str,
                           ts: i64| {
            let severity_name = match severity {
                0 => "emerg",
                1 => "alert",
                2 => "crit",
                3 => "err",
                4 => "warning",
                5 => "notice",
                6 => "info",
                _ => "debug",
            };
            let (sev_num, sev_text) = match severity {
                0 => (24, "FATAL"),
                1 => (23, "FATAL"),
                2 => (22, "FATAL"),
                3 => (17, "ERROR"),
                4 => (13, "WARN"),
                5 => (10, "INFO"),
                6 => (9, "INFO"),
                _ => (5, "DEBUG"),
            };
            // Mirror the sensor's `<timestamp_ms><seq>` uid shape (#104).
            let uid = format!("{:013}{:012}", ts.max(0), id);
            TelemetryPoint {
                timestamp: ts,
                source: server.to_string(),
                protocol: Protocol::Logs,
                metric: format!("events/{uid}"),
                value: TelemetryValue::Text(msg.to_string()),
                labels: [
                    ("severity".to_string(), severity_name.to_string()),
                    ("facility".to_string(), facility.to_string()),
                    ("app".to_string(), app_name.to_string()),
                    ("severity_number".to_string(), sev_num.to_string()),
                    ("severity_text".to_string(), sev_text.to_string()),
                    ("log.record.uid".to_string(), uid.clone()),
                ]
                .into_iter()
                .collect(),
            }
        };

        let base_id = self.tick * 100; // Unique ID base per tick

        // Normal operation logs (frequent)
        if self.rng.random_range(0..3) == 0 {
            // severity 6 = Informational
            let info_messages = [
                (
                    "server01",
                    "auth",
                    "sshd",
                    "Accepted publickey for admin from 10.0.0.50",
                ),
                ("server01", "auth", "sshd", "session opened for user admin"),
                ("server02", "cron", "crond", "CMD: /usr/local/bin/backup.sh"),
                (
                    "server02",
                    "cron",
                    "crond",
                    "CRON job completed successfully",
                ),
                (
                    "database01",
                    "daemon",
                    "postgres",
                    "checkpoint complete: wrote 1234 buffers",
                ),
                (
                    "database01",
                    "daemon",
                    "postgres",
                    "automatic vacuum of table public.events",
                ),
                (
                    "server03",
                    "daemon",
                    "nginx",
                    "10.0.0.100 GET /api/health 200 0.003s",
                ),
                (
                    "server03",
                    "daemon",
                    "nginx",
                    "10.0.0.101 POST /api/data 201 0.015s",
                ),
                (
                    "server01",
                    "daemon",
                    "systemd",
                    "Started Daily apt download activities",
                ),
                (
                    "server02",
                    "daemon",
                    "docker",
                    "Container web-app started successfully",
                ),
            ];

            let idx = self.rng.random_range(0..info_messages.len());
            let (server, facility, app, msg) = info_messages[idx];
            points.push(make_syslog(
                base_id, server, 6, facility, app, msg, timestamp,
            ));
        }

        // Notice logs (less frequent)
        if self.rng.random_range(0..8) == 0 {
            // severity 5 = Notice
            let notice_messages = [
                (
                    "server01",
                    "auth",
                    "sudo",
                    "admin : TTY=pts/0 ; PWD=/home/admin ; COMMAND=/bin/systemctl restart nginx",
                ),
                (
                    "database01",
                    "daemon",
                    "postgres",
                    "received fast shutdown request",
                ),
                ("server03", "daemon", "nginx", "signal process started"),
                ("server02", "kern", "kernel", "eth0: link becomes ready"),
            ];

            let idx = self.rng.random_range(0..notice_messages.len());
            let (server, facility, app, msg) = notice_messages[idx];
            points.push(make_syslog(
                base_id + 1,
                server,
                5,
                facility,
                app,
                msg,
                timestamp,
            ));
        }

        // Warning logs (occasional)
        if self.rng.random_range(0..12) == 0 {
            // severity 4 = Warning
            let warnings = [
                (
                    "server01",
                    "kern",
                    "kernel",
                    "possible SYN flooding on port 443. Sending cookies",
                ),
                (
                    "server02",
                    "daemon",
                    "docker",
                    "Container memory usage exceeds 80%",
                ),
                (
                    "database01",
                    "daemon",
                    "postgres",
                    "checkpoints are occurring too frequently (10 second intervals)",
                ),
                (
                    "server03",
                    "daemon",
                    "nginx",
                    "upstream server temporarily disabled",
                ),
                (
                    "router01",
                    "daemon",
                    "bgpd",
                    "neighbor 10.0.0.1 state changed from Established to Idle",
                ),
            ];

            let idx = self.rng.random_range(0..warnings.len());
            let (server, facility, app, msg) = warnings[idx];
            points.push(make_syslog(
                base_id + 2,
                server,
                4,
                facility,
                app,
                msg,
                timestamp,
            ));
        }

        // Error logs during anomalies
        for (i, anomaly) in self.active_anomalies.iter().enumerate() {
            if self.rng.random_range(0..3) == 0 {
                let msg_id = base_id + 10 + i as u64;
                match &anomaly.anomaly_type {
                    AnomalyType::CpuSpike { server, intensity } => {
                        // severity 3 = Error
                        let msg = format!(
                            "Process java consuming {:.0}% CPU, system unresponsive",
                            intensity
                        );
                        points.push(make_syslog(
                            msg_id, server, 3, "daemon", "monit", &msg, timestamp,
                        ));
                    }
                    AnomalyType::MemoryLeak { server, .. } => {
                        // severity 4 = Warning
                        let msg = "Memory pressure increasing, swap usage at 85%";
                        points.push(make_syslog(
                            msg_id, server, 4, "kern", "kernel", msg, timestamp,
                        ));
                    }
                    AnomalyType::InterfaceDown { device, interface } => {
                        // severity 3 = Error
                        let msg = format!("interface {} link down, carrier lost", interface);
                        points.push(make_syslog(
                            msg_id, device, 3, "kern", "kernel", &msg, timestamp,
                        ));
                    }
                    AnomalyType::TemperatureHigh { plc, temp } => {
                        // severity 2 = Critical
                        let msg = format!(
                            "CRITICAL: Temperature sensor reads {:.1}C, threshold 60C exceeded",
                            temp
                        );
                        points.push(make_syslog(
                            msg_id,
                            plc,
                            2,
                            "daemon",
                            "plc-monitor",
                            &msg,
                            timestamp,
                        ));
                    }
                    AnomalyType::ErrorBurst { server } => {
                        // severity 3 = Error
                        let error_messages = [
                            (
                                "daemon",
                                "nginx",
                                "upstream connection refused: 10.0.0.200:8080",
                            ),
                            (
                                "daemon",
                                "postgres",
                                "FATAL: too many connections for role \"app\"",
                            ),
                            (
                                "kern",
                                "kernel",
                                "EXT4-fs error: I/O error writing to journal",
                            ),
                            (
                                "daemon",
                                "sshd",
                                "error: maximum authentication attempts exceeded",
                            ),
                            (
                                "daemon",
                                "docker",
                                "OOM killer terminated container web-app",
                            ),
                            ("daemon", "nginx", "worker process exited on signal 9"),
                            ("daemon", "redis", "Background save error: fork failed"),
                        ];
                        let idx = self.tick as usize % error_messages.len();
                        let (facility, app, msg) = error_messages[idx];
                        points.push(make_syslog(
                            msg_id, server, 3, facility, app, msg, timestamp,
                        ));
                    }
                    _ => {}
                }
            }
        }

        points
    }

    /// Generate netlink (Linux kernel networking) telemetry for the demo hosts.
    ///
    /// Mirrors the real `zensight-sensor-netlink` contract: per-interface
    /// counters, TCP socket-state gauges, route/neighbor inventory and the
    /// rolled-up diagnostics, all under `Protocol::Netlink` with the host as the
    /// `source`.
    fn generate_netlink(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();
        let hosts = ["server01", "server02", "server03", "database01"];

        for host in hosts {
            // Is one of this host's services currently "down"?
            let service_down = self.active_anomalies.iter().any(|a| {
                matches!(&a.anomaly_type, AnomalyType::ServiceDown { host: h, .. } if h == host)
            });

            // Per-interface counters (eth0 carries traffic, lo is loopback).
            for (iface, ifindex) in [("eth0", 2u64), ("lo", 1u64)] {
                let scale = if iface == "eth0" { 1 } else { 4 };
                for (stat, lo, hi) in [
                    ("rx_bytes", 40_000u64, 400_000u64),
                    ("tx_bytes", 30_000, 300_000),
                    ("rx_packets", 200, 2_000),
                    ("tx_packets", 150, 1_500),
                ] {
                    let delta = self.rng.random_range(lo..hi) / scale;
                    let v = self.increment_counter(&format!("nl/{host}/{iface}/{stat}"), delta);
                    points.push(self.make_point_with_labels(
                        Protocol::Netlink,
                        host,
                        &format!("iface/{iface}/{stat}"),
                        TelemetryValue::Counter(v),
                        timestamp,
                        vec![("ifindex".to_string(), ifindex.to_string())],
                    ));
                }
                // Errors / drops accrue slowly.
                for stat in ["rx_errors", "tx_errors", "rx_dropped", "tx_dropped"] {
                    let delta = if self.rng.random_range(0..20) == 0 {
                        1
                    } else {
                        0
                    };
                    let v = self.increment_counter(&format!("nl/{host}/{iface}/{stat}"), delta);
                    points.push(self.make_point_with_labels(
                        Protocol::Netlink,
                        host,
                        &format!("iface/{iface}/{stat}"),
                        TelemetryValue::Counter(v),
                        timestamp,
                        vec![("ifindex".to_string(), ifindex.to_string())],
                    ));
                }
                points.push(self.make_point_with_labels(
                    Protocol::Netlink,
                    host,
                    &format!("iface/{iface}/oper_state"),
                    TelemetryValue::Text("up".to_string()),
                    timestamp,
                    vec![("ifindex".to_string(), ifindex.to_string())],
                ));
                points.push(self.make_point_with_labels(
                    Protocol::Netlink,
                    host,
                    &format!("iface/{iface}/up"),
                    TelemetryValue::Boolean(true),
                    timestamp,
                    vec![("ifindex".to_string(), ifindex.to_string())],
                ));
                points.push(self.make_point_with_labels(
                    Protocol::Netlink,
                    host,
                    &format!("iface/{iface}/mtu"),
                    TelemetryValue::Gauge(if iface == "lo" { 65536.0 } else { 1500.0 }),
                    timestamp,
                    vec![("ifindex".to_string(), ifindex.to_string())],
                ));
            }

            // TCP socket-state gauges.
            let established = self.oscillating_value(&format!("{host}/nl/estab"), 120.0, 30.0);
            let listen = if service_down { 11.0 } else { 12.0 };
            for (stat, value) in [
                ("established", established.max(0.0)),
                ("listen", listen),
                (
                    "time_wait",
                    self.oscillating_value(&format!("{host}/nl/tw"), 40.0, 15.0)
                        .max(0.0),
                ),
                ("syn_sent", self.rng.random_range(0.0..4.0)),
                ("close_wait", self.rng.random_range(0.0..6.0)),
            ] {
                points.push(self.make_point(
                    Protocol::Netlink,
                    host,
                    &format!("sockets/tcp/{stat}"),
                    TelemetryValue::Gauge(value),
                    timestamp,
                ));
            }
            let retrans = self.random_bump(&format!("nl/{host}/retrans"), 0, 3);
            points.push(self.make_point(
                Protocol::Netlink,
                host,
                "sockets/tcp/retransmits_total",
                TelemetryValue::Counter(retrans),
                timestamp,
            ));
            for (stat, base, amp) in [
                ("rtt_p50_us", 800.0_f64, 300.0_f64),
                ("rtt_p95_us", 4500.0, 1500.0),
                ("max_rtt_us", 12000.0, 4000.0),
            ] {
                let v = (base + self.rng.random_range(-amp..amp)).max(1.0);
                points.push(self.make_point(
                    Protocol::Netlink,
                    host,
                    &format!("sockets/tcp/{stat}"),
                    TelemetryValue::Gauge(v),
                    timestamp,
                ));
            }

            // Route inventory.
            points.push(self.make_point(
                Protocol::Netlink,
                host,
                "routes/ipv4_count",
                TelemetryValue::Gauge(14.0),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Netlink,
                host,
                "routes/ipv6_count",
                TelemetryValue::Gauge(6.0),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Netlink,
                host,
                "routes/total",
                TelemetryValue::Gauge(20.0),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Netlink,
                host,
                "routes/default_v4_present",
                TelemetryValue::Boolean(true),
                timestamp,
            ));

            // Neighbor (ARP/NDP) table.
            let reachable = self
                .oscillating_value(&format!("{host}/nl/nbr"), 18.0, 6.0)
                .max(0.0);
            for (state, value) in [
                ("reachable", reachable),
                ("stale", self.rng.random_range(0.0..5.0)),
                (
                    "failed",
                    if self.rng.random_range(0..10) == 0 {
                        1.0
                    } else {
                        0.0
                    },
                ),
            ] {
                points.push(self.make_point(
                    Protocol::Netlink,
                    host,
                    &format!("neighbors/by_state/{state}"),
                    TelemetryValue::Gauge(value),
                    timestamp,
                ));
            }
            points.push(self.make_point(
                Protocol::Netlink,
                host,
                "neighbors/total",
                TelemetryValue::Gauge(reachable + 4.0),
                timestamp,
            ));

            // Rolled-up diagnostics (the netlink view's health summary).
            let warnings = if service_down { 1.0 } else { 0.0 };
            for (sev, value) in [
                ("info", self.rng.random_range(0.0..2.0)),
                ("warning", warnings),
                ("error", 0.0),
                ("critical", 0.0),
            ] {
                points.push(self.make_point(
                    Protocol::Netlink,
                    host,
                    &format!("diagnostics/issues/{sev}"),
                    TelemetryValue::Gauge(value),
                    timestamp,
                ));
            }
            points.push(self.make_point(
                Protocol::Netlink,
                host,
                "diagnostics/issues/total",
                TelemetryValue::Gauge(warnings),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Netlink,
                host,
                "diagnostics/bottleneck_score",
                TelemetryValue::Gauge(if service_down { 0.4 } else { 0.05 }),
                timestamp,
            ));

            // Real-time RTNETLINK event counters.
            for (family, action) in [
                ("link", "changed"),
                ("neighbor", "added"),
                ("route", "added"),
            ] {
                let delta = if self.rng.random_range(0..8) == 0 {
                    1
                } else {
                    0
                };
                let v = self.increment_counter(&format!("nl/{host}/ev/{family}/{action}"), delta);
                points.push(self.make_point(
                    Protocol::Netlink,
                    host,
                    &format!("events/{family}/{action}_total"),
                    TelemetryValue::Counter(v),
                    timestamp,
                ));
            }
        }

        points
    }

    /// Generate netring (passive flow monitor) telemetry. One probe aggregates
    /// flow/L4/TCP/bandwidth/DNS/HTTP/TLS rollups for the whole segment, matching
    /// the real `zensight-sensor-netring` contract.
    fn generate_netring(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();
        let probe = "netprobe01";

        let scanning = self
            .active_anomalies
            .iter()
            .any(|a| matches!(&a.anomaly_type, AnomalyType::PortScan { .. }));

        // Flow lifecycle + volume.
        let started = self.random_bump("nr/flow_started", 20, 120);
        let ended = self.random_bump("nr/flow_ended", 18, 115);
        let active = self
            .oscillating_value("nr/flow_active", 240.0, 60.0)
            .max(0.0);
        let bytes = self.random_bump("nr/flow_bytes", 2_000_000, 20_000_000);
        let packets = self.random_bump("nr/flow_packets", 4_000, 40_000);
        let retransmits = self.random_bump("nr/flow_retx", 0, 50);
        for (metric, value) in [
            ("flow/started_total", started),
            ("flow/ended_total", ended),
            ("flow/bytes_total", bytes),
            ("flow/packets_total", packets),
            ("flow/retransmits_total", retransmits),
        ] {
            points.push(self.make_point(
                Protocol::Netring,
                probe,
                metric,
                TelemetryValue::Counter(value),
                timestamp,
            ));
        }
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "flow/active",
            TelemetryValue::Gauge(active),
            timestamp,
        ));
        let dur_p50 = self.rng.random_range(40.0..120.0);
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "flow/duration_p50_ms",
            TelemetryValue::Gauge(dur_p50),
            timestamp,
        ));
        let dur_p95 = self.rng.random_range(400.0..1800.0);
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "flow/duration_p95_ms",
            TelemetryValue::Gauge(dur_p95),
            timestamp,
        ));

        // Per-L4 breakdown.
        for (l4, bytes_lo, bytes_hi, flows_lo, flows_hi) in [
            ("tcp", 1_500_000u64, 12_000_000u64, 15u64, 90u64),
            ("udp", 200_000, 3_000_000, 8, 40),
            ("icmp", 1_000, 40_000, 0, 6),
        ] {
            let b = self.random_bump(&format!("nr/l4/{l4}/bytes"), bytes_lo, bytes_hi);
            let f = self.random_bump(&format!("nr/l4/{l4}/flows"), flows_lo, flows_hi + 1);
            points.push(self.make_point(
                Protocol::Netring,
                probe,
                &format!("flow/by_l4/{l4}/bytes_total"),
                TelemetryValue::Counter(b),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Netring,
                probe,
                &format!("flow/by_l4/{l4}/flows_total"),
                TelemetryValue::Counter(f),
                timestamp,
            ));
        }

        // TCP teardown counters (refused spikes during a scan).
        for (metric, lo, hi) in [
            ("tcp/closed_fin_total", 10u64, 60u64),
            ("tcp/closed_rst_total", 2, 20),
            ("tcp/resets_total", 1, 15),
        ] {
            let v = self.random_bump(&format!("nr/{metric}"), lo, hi + 1);
            points.push(self.make_point(
                Protocol::Netring,
                probe,
                metric,
                TelemetryValue::Counter(v),
                timestamp,
            ));
        }
        let refused_delta = if scanning {
            self.rng.random_range(20..80)
        } else {
            self.rng.random_range(0..4)
        };
        let refused = self.increment_counter("nr/tcp/refused", refused_delta);
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "tcp/refused_total",
            TelemetryValue::Counter(refused),
            timestamp,
        ));

        // Per-application bandwidth.
        for (app, base, amp) in [
            ("https", 6_000_000.0_f64, 2_000_000.0_f64),
            ("dns", 80_000.0, 40_000.0),
            ("ssh", 200_000.0, 120_000.0),
            ("quic", 3_000_000.0, 1_500_000.0),
        ] {
            let v = (base + self.rng.random_range(-amp..amp)).max(0.0);
            points.push(self.make_point_with_labels(
                Protocol::Netring,
                probe,
                &format!("bandwidth/{app}/bytes_per_sec"),
                TelemetryValue::Gauge(v),
                timestamp,
                vec![("app".to_string(), app.to_string())],
            ));
        }

        // TLS fingerprinting.
        let handshakes = self.random_bump("nr/tls/hs", 5, 40);
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "tls/handshakes_total",
            TelemetryValue::Counter(handshakes),
            timestamp,
        ));
        let tls_fps = self.rng.random_range(8.0..24.0);
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "tls/distinct_fingerprints",
            TelemetryValue::Gauge(tls_fps),
            timestamp,
        ));

        // DNS RED.
        let dns_q = self.random_bump("nr/dns/q", 30, 200);
        let dns_un = self.random_bump("nr/dns/un", 0, 6);
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "dns/queries_total",
            TelemetryValue::Counter(dns_q),
            timestamp,
        ));
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "dns/unanswered_total",
            TelemetryValue::Counter(dns_un),
            timestamp,
        ));
        let dns_rtt = self.rng.random_range(2.0..30.0);
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "dns/query_rtt_p50_ms",
            TelemetryValue::Gauge(dns_rtt),
            timestamp,
        ));

        // HTTP RED (cleartext).
        let http_req = self.random_bump("nr/http/req", 10, 80);
        let http_2xx = self.random_bump("nr/http/2xx", 8, 70);
        let http_4xx = self.random_bump("nr/http/4xx", 0, 8);
        let http_5xx = self.random_bump("nr/http/5xx", 0, 3);
        for (metric, value) in [
            ("http/requests_total", http_req),
            ("http/status_2xx_total", http_2xx),
            ("http/status_4xx_total", http_4xx),
            ("http/status_5xx_total", http_5xx),
        ] {
            points.push(self.make_point(
                Protocol::Netring,
                probe,
                metric,
                TelemetryValue::Counter(value),
                timestamp,
            ));
        }
        let http_lat = self.rng.random_range(5.0..60.0);
        points.push(self.make_point(
            Protocol::Netring,
            probe,
            "http/latency_p50_ms",
            TelemetryValue::Gauge(http_lat),
            timestamp,
        ));

        points
    }

    /// Generate NetFlow telemetry: one byte-counter series per
    /// `{src}/{dst}/{proto}` flow, sourced from a flow exporter.
    fn generate_netflow(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();
        let exporter = "edge-fw";
        let exporter_ip = "10.0.0.1";
        let flows = [
            ("10.0.0.50", "93.184.216.34", "tcp"),
            ("10.0.0.51", "8.8.8.8", "udp"),
            ("10.0.0.52", "10.0.0.20", "tcp"),
            ("10.0.0.53", "151.101.1.69", "tcp"),
        ];

        for (src, dst, proto) in flows {
            let delta = self.rng.random_range(50_000..5_000_000);
            let v = self.increment_counter(&format!("nf/{src}/{dst}/{proto}"), delta);
            points.push(self.make_point_with_labels(
                Protocol::Netflow,
                exporter,
                &format!("{src}/{dst}/{proto}"),
                TelemetryValue::Counter(v),
                timestamp,
                vec![
                    ("version".to_string(), "v9".to_string()),
                    ("exporter_ip".to_string(), exporter_ip.to_string()),
                    ("protocol".to_string(), proto.to_string()),
                ],
            ));
        }

        points
    }

    /// Generate gNMI telemetry: streamed YANG paths from network targets.
    fn generate_gnmi(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();
        let targets = ["router01", "switch01"];

        for target in targets {
            for iface in ["eth0", "eth1"] {
                let in_delta = self.rng.random_range(100_000..2_000_000);
                let out_delta = self.rng.random_range(80_000..1_600_000);
                let in_octets =
                    self.increment_counter(&format!("gnmi/{target}/{iface}/in"), in_delta);
                let out_octets =
                    self.increment_counter(&format!("gnmi/{target}/{iface}/out"), out_delta);
                points.push(self.make_point(
                    Protocol::Gnmi,
                    target,
                    &format!("interfaces/interface[name={iface}]/state/counters/in-octets"),
                    TelemetryValue::Counter(in_octets),
                    timestamp,
                ));
                points.push(self.make_point(
                    Protocol::Gnmi,
                    target,
                    &format!("interfaces/interface[name={iface}]/state/counters/out-octets"),
                    TelemetryValue::Counter(out_octets),
                    timestamp,
                ));
                points.push(self.make_point(
                    Protocol::Gnmi,
                    target,
                    &format!("interfaces/interface[name={iface}]/state/oper-status"),
                    TelemetryValue::Text("UP".to_string()),
                    timestamp,
                ));
            }
        }

        points
    }

    /// Derive the current set of sensor-emitted [`Alert`]s from active
    /// anomalies, emitting `Firing` for live ones and a single `Resolved`
    /// transition for any that just cleared (so the UI auto-clears them).
    pub fn generate_alerts(&mut self) -> Vec<Alert> {
        let mut desired: HashMap<String, Alert> = HashMap::new();
        for anomaly in &self.active_anomalies {
            if let Some(alert) = Self::anomaly_to_alert(&anomaly.anomaly_type) {
                desired.insert(alert.alert_key(), alert);
            }
        }

        let mut out = Vec::new();
        // Resolved transitions: previously firing, no longer desired.
        for (key, alert) in &self.firing_alerts {
            if !desired.contains_key(key) {
                out.push(alert.clone().resolved());
            }
        }
        // Firing (insert/update is idempotent on the receiver side).
        for alert in desired.values() {
            out.push(alert.clone());
        }
        self.firing_alerts = desired;
        out
    }

    /// Map an anomaly to the sensor alert a real sensor would raise for it, or
    /// `None` for anomalies that don't surface on the `@/alerts` channel.
    fn anomaly_to_alert(anomaly: &AnomalyType) -> Option<Alert> {
        match anomaly {
            AnomalyType::PortScan { src_ip } => Some(
                Alert::new(
                    "netprobe01",
                    Protocol::Netring,
                    AlertKind::Anomaly,
                    "PortScanTRW",
                    AlertSeverity::Warning,
                    format!("Port scan from {src_ip} (many ports, one source)"),
                )
                .with_label("src", src_ip.clone())
                .with_label("dst", "10.0.0.0/24")
                .with_label("proto", "tcp"),
            ),
            AnomalyType::Beaconing { dst_ip } => Some(
                Alert::new(
                    "netprobe01",
                    Protocol::Netring,
                    AlertKind::Anomaly,
                    "BeaconCv",
                    AlertSeverity::Warning,
                    format!("Periodic beaconing to {dst_ip} (possible C2)"),
                )
                .with_label("dst", dst_ip.clone())
                .with_label("proto", "tcp"),
            ),
            AnomalyType::ServiceDown {
                host,
                service,
                port,
            } => Some(
                Alert::new(
                    host.clone(),
                    Protocol::Netlink,
                    AlertKind::Expectation,
                    format!("socket:{service}"),
                    AlertSeverity::Warning,
                    format!("Expected {service} listening on :{port}, none found"),
                )
                .with_label("expected", "listen")
                .with_label("actual", "absent")
                .with_label("port", port.to_string()),
            ),
            _ => None,
        }
    }

    /// Helper to create a telemetry point.
    fn make_point(
        &self,
        protocol: Protocol,
        source: &str,
        metric: &str,
        value: TelemetryValue,
        timestamp: i64,
    ) -> TelemetryPoint {
        TelemetryPoint {
            timestamp,
            source: source.to_string(),
            protocol,
            metric: metric.to_string(),
            value,
            labels: HashMap::new(),
        }
    }

    /// Helper to create a telemetry point with labels.
    fn make_point_with_labels(
        &self,
        protocol: Protocol,
        source: &str,
        metric: &str,
        value: TelemetryValue,
        timestamp: i64,
        labels: Vec<(String, String)>,
    ) -> TelemetryPoint {
        TelemetryPoint {
            timestamp,
            source: source.to_string(),
            protocol,
            metric: metric.to_string(),
            value,
            labels: labels.into_iter().collect(),
        }
    }

    /// Generate sensor health snapshots.
    pub fn generate_health_snapshots(&mut self) -> Vec<HealthSnapshot> {
        let uptime = self.tick - self.start_tick;

        // Define sensors and their device counts
        let sensors = [
            ("sysinfo", 4u64), // 4 servers
            ("snmp", 2u64),    // router + switch
            ("modbus", 2u64),  // 2 PLCs
            ("logs", 4u64),    // Same servers that generate logs
            ("netlink", 4u64), // Linux hosts (kernel networking)
            ("netring", 1u64), // 1 passive flow probe
            ("netflow", 1u64), // 1 flow exporter
            ("gnmi", 2u64),    // router + switch (streamed telemetry)
        ];

        sensors
            .iter()
            .map(|(name, device_count)| {
                // Count devices with active anomalies
                let devices_with_issues = self.count_devices_with_issues(name);
                let devices_failed = devices_with_issues.min(*device_count);
                let devices_responding = device_count.saturating_sub(devices_failed);

                // Determine overall status
                let status = if devices_failed == 0 {
                    HealthStatus::Healthy
                } else if devices_responding > 0 {
                    HealthStatus::Degraded
                } else {
                    HealthStatus::Error
                };

                let metrics = *self.metrics_published.get(*name).unwrap_or(&0);
                let errors = *self.errors_per_sensor.get(*name).unwrap_or(&0);

                HealthSnapshot {
                    sensor: name.to_string(),
                    status,
                    uptime_secs: uptime, // Each tick is ~0.6s in demo, but we use tick count
                    devices_total: *device_count,
                    devices_responding,
                    devices_failed,
                    last_poll_duration_ms: self.rng.random_range(50..200),
                    errors_last_hour: errors,
                    metrics_published: metrics,
                }
            })
            .collect()
    }

    /// Count devices with active issues for a sensor.
    fn count_devices_with_issues(&self, sensor: &str) -> u64 {
        let mut count = 0u64;

        for anomaly in &self.active_anomalies {
            let device_affected = match &anomaly.anomaly_type {
                AnomalyType::CpuSpike { server, .. } => {
                    sensor == "sysinfo" && self.is_server(server)
                }
                AnomalyType::MemoryLeak { server, .. } => {
                    sensor == "sysinfo" && self.is_server(server)
                }
                AnomalyType::DiskFilling { server, .. } => {
                    sensor == "sysinfo" && self.is_server(server)
                }
                AnomalyType::InterfaceDown { device, .. } => {
                    sensor == "snmp" && self.is_network_device(device)
                }
                AnomalyType::TrafficBurst { device, .. } => {
                    sensor == "snmp" && self.is_network_device(device)
                }
                AnomalyType::TemperatureHigh { plc, .. } => sensor == "modbus" && self.is_plc(plc),
                AnomalyType::ErrorBurst { server } => sensor == "logs" && self.is_server(server),
                // Port scans / beacons are observed by the netring probe; a
                // downed service is a netlink expectation violation.
                AnomalyType::PortScan { .. } | AnomalyType::Beaconing { .. } => sensor == "netring",
                AnomalyType::ServiceDown { .. } => sensor == "netlink",
            };

            if device_affected {
                count += 1;
            }
        }

        count
    }

    fn is_server(&self, name: &str) -> bool {
        matches!(name, "server01" | "server02" | "server03" | "database01")
    }

    fn is_network_device(&self, name: &str) -> bool {
        matches!(name, "router01" | "switch01")
    }

    fn is_plc(&self, name: &str) -> bool {
        matches!(name, "plc01" | "plc02")
    }

    /// Generate device liveness updates based on active anomalies.
    pub fn generate_liveness_updates(&self) -> Vec<(String, DeviceLiveness)> {
        let mut updates = Vec::new();

        // Define all devices per protocol
        let devices: Vec<(&str, &[&str])> = vec![
            (
                "sysinfo",
                &["server01", "server02", "server03", "database01"],
            ),
            ("snmp", &["router01", "switch01"]),
            ("modbus", &["plc01", "plc02"]),
            (
                "netlink",
                &["server01", "server02", "server03", "database01"],
            ),
            ("netring", &["netprobe01"]),
            ("netflow", &["edge-fw"]),
            ("gnmi", &["router01", "switch01"]),
        ];

        for (protocol, device_list) in devices {
            for device in device_list.iter() {
                let (status, failures, error) = self.get_device_status(device);

                updates.push((
                    protocol.to_string(),
                    DeviceLiveness {
                        device: device.to_string(),
                        status,
                        last_seen: 0, // Will be set by caller with actual timestamp
                        consecutive_failures: failures,
                        last_error: error,
                    },
                ));
            }
        }

        updates
    }

    /// Get the status of a specific device based on active anomalies.
    fn get_device_status(&self, device: &str) -> (DeviceStatus, u32, Option<String>) {
        // Check for severe anomalies (interface down = offline)
        for anomaly in &self.active_anomalies {
            if let AnomalyType::InterfaceDown {
                device: d,
                interface,
            } = &anomaly.anomaly_type
                && d == device
            {
                return (
                    DeviceStatus::Offline,
                    3,
                    Some(format!("Interface {} is down", interface)),
                );
            }
        }

        // Check for degrading anomalies
        for anomaly in &self.active_anomalies {
            let (is_affected, error_msg) = match &anomaly.anomaly_type {
                AnomalyType::CpuSpike {
                    server, intensity, ..
                } if server == device => (true, Some(format!("High CPU usage: {:.0}%", intensity))),
                AnomalyType::MemoryLeak { server, .. } if server == device => {
                    (true, Some("Memory leak detected".to_string()))
                }
                AnomalyType::DiskFilling { server, .. } if server == device => {
                    (true, Some("Disk space critically low".to_string()))
                }
                AnomalyType::TemperatureHigh { plc, temp, .. } if plc == device => {
                    (true, Some(format!("Temperature alarm: {:.1}°C", temp)))
                }
                AnomalyType::TrafficBurst { device: d, .. } if d == device => {
                    (true, Some("Traffic burst detected".to_string()))
                }
                AnomalyType::ErrorBurst { server } if server == device => {
                    (true, Some("Error burst detected".to_string()))
                }
                AnomalyType::ServiceDown { host, service, .. } if host == device => {
                    (true, Some(format!("{service} not listening")))
                }
                _ => (false, None),
            };

            if is_affected {
                return (DeviceStatus::Degraded, 1, error_msg);
            }
        }

        // No anomalies affecting this device
        (DeviceStatus::Online, 0, None)
    }

    /// Record metrics published for a sensor.
    pub fn record_metrics(&mut self, sensor: &str, count: u64) {
        *self
            .metrics_published
            .entry(sensor.to_string())
            .or_insert(0) += count;
    }

    /// Record an error for a sensor.
    pub fn record_error(&mut self, sensor: &str) {
        *self
            .errors_per_sensor
            .entry(sensor.to_string())
            .or_insert(0) += 1;
    }
}

impl Default for DemoSimulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Get default alert rules for demo mode.
///
/// These rules are designed to trigger alerts during the demo simulation.
pub fn demo_alert_rules() -> Vec<crate::view::alerts::AlertRule> {
    use crate::view::alerts::{AlertRule, ComparisonOp, Severity};

    vec![
        // CPU alerts (Warning severity)
        {
            let mut rule = AlertRule::new(1, "High CPU Usage", "cpu/usage");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 80.0;
            rule.severity = Severity::Warning;
            rule.protocol = Some(Protocol::Sysinfo);
            rule
        },
        // Memory alerts (Warning severity)
        {
            let mut rule = AlertRule::new(2, "High Memory Usage", "memory/usage_percent");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 85.0;
            rule.severity = Severity::Warning;
            rule.protocol = Some(Protocol::Sysinfo);
            rule
        },
        // Disk alerts (Critical severity)
        {
            let mut rule = AlertRule::new(3, "Disk Space Critical", "disk/root/usage_percent");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 90.0;
            rule.severity = Severity::Critical;
            rule.protocol = Some(Protocol::Sysinfo);
            rule
        },
        // Network interface down (Critical severity)
        {
            let mut rule = AlertRule::new(4, "Interface Down", "ifOperStatus");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 1.5; // 2 = down
            rule.severity = Severity::Critical;
            rule.protocol = Some(Protocol::Snmp);
            rule
        },
        // Temperature alerts (Warning severity)
        {
            let mut rule = AlertRule::new(5, "High Temperature", "holding/0");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 60.0;
            rule.severity = Severity::Warning;
            rule.protocol = Some(Protocol::Modbus);
            rule
        },
        // Interface errors (Info severity)
        {
            let mut rule = AlertRule::new(6, "Interface Errors", "ifInErrors");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 0.0;
            rule.severity = Severity::Info;
            rule.protocol = Some(Protocol::Snmp);
            rule
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_demo_simulator_generates_data() {
        let mut sim = DemoSimulator::new();
        let timestamp = 1700000000000;

        let points = sim.tick(timestamp);
        assert!(!points.is_empty());

        // Check we have multiple protocols, including the network sensors.
        let protocols: std::collections::HashSet<_> = points.iter().map(|p| p.protocol).collect();
        assert!(protocols.contains(&Protocol::Sysinfo));
        assert!(protocols.contains(&Protocol::Snmp));
        assert!(protocols.contains(&Protocol::Modbus));
        assert!(protocols.contains(&Protocol::Netlink));
        assert!(protocols.contains(&Protocol::Netring));
        assert!(protocols.contains(&Protocol::Netflow));
        assert!(protocols.contains(&Protocol::Gnmi));
    }

    #[test]
    fn test_demo_health_covers_all_sensors() {
        let mut sim = DemoSimulator::new();
        let names: std::collections::HashSet<_> = sim
            .generate_health_snapshots()
            .into_iter()
            .map(|s| s.sensor)
            .collect();
        for sensor in [
            "sysinfo", "snmp", "modbus", "logs", "netlink", "netring", "netflow", "gnmi",
        ] {
            assert!(names.contains(sensor), "missing health for {sensor}");
        }
    }

    #[test]
    fn test_anomaly_to_alert_mapping() {
        let scan = DemoSimulator::anomaly_to_alert(&AnomalyType::PortScan {
            src_ip: "198.51.100.7".to_string(),
        })
        .expect("port scan should map to an alert");
        assert_eq!(scan.protocol, Protocol::Netring);
        assert_eq!(scan.kind, AlertKind::Anomaly);
        assert_eq!(
            scan.labels.get("src").map(String::as_str),
            Some("198.51.100.7")
        );

        let down = DemoSimulator::anomaly_to_alert(&AnomalyType::ServiceDown {
            host: "server01".to_string(),
            service: "sshd".to_string(),
            port: 22,
        })
        .expect("service down should map to an alert");
        assert_eq!(down.protocol, Protocol::Netlink);
        assert_eq!(down.kind, AlertKind::Expectation);

        // CPU spikes are local-rule territory, not a sensor alert.
        assert!(
            DemoSimulator::anomaly_to_alert(&AnomalyType::CpuSpike {
                server: "server01".to_string(),
                intensity: 95.0,
            })
            .is_none()
        );
    }

    #[test]
    fn test_generate_alerts_firing_then_resolved() {
        let mut sim = DemoSimulator::new();
        // Start clean so scheduled anomalies don't interfere.
        sim.active_anomalies.clear();
        sim.active_anomalies.push(Anomaly {
            start_tick: 0,
            duration_ticks: 999,
            anomaly_type: AnomalyType::Beaconing {
                dst_ip: "203.0.113.9".to_string(),
            },
        });

        let firing = sim.generate_alerts();
        assert_eq!(firing.len(), 1);
        assert!(firing[0].is_firing());
        assert_eq!(firing[0].rule, "BeaconCv");

        // Anomaly clears -> a single Resolved transition is emitted.
        sim.active_anomalies.clear();
        let resolved = sim.generate_alerts();
        assert_eq!(resolved.len(), 1);
        assert!(!resolved[0].is_firing());

        // Nothing left to report.
        assert!(sim.generate_alerts().is_empty());
    }

    #[test]
    fn test_demo_syslog_emits_per_line_events() {
        let mut sim = DemoSimulator::new();
        // Log lines are probabilistic per tick; sample enough ticks.
        let mut saw_syslog = false;
        for i in 0..200 {
            for p in sim.tick(1700000000000 + i * 600) {
                if p.protocol == Protocol::Logs {
                    saw_syslog = true;
                    // Per-line event key (#104): `events/<uid>`, value is text.
                    assert!(
                        p.metric.starts_with("events/"),
                        "metric {} is not a per-line event",
                        p.metric
                    );
                    assert!(matches!(p.value, TelemetryValue::Text(_)));
                    // Facility/severity now travel in labels, with OTel fields.
                    let sev = p.labels.get("severity").expect("severity label");
                    assert!(
                        matches!(
                            sev.as_str(),
                            "emerg"
                                | "alert"
                                | "crit"
                                | "err"
                                | "warning"
                                | "notice"
                                | "info"
                                | "debug"
                        ),
                        "unexpected severity slug {sev}"
                    );
                    assert!(p.labels.contains_key("facility"));
                    assert!(p.labels.contains_key("severity_number"));
                    assert_eq!(
                        p.labels.get("log.record.uid"),
                        Some(&p.metric["events/".len()..].to_string())
                    );
                }
            }
        }
        assert!(saw_syslog, "no syslog telemetry generated in 200 ticks");
    }

    #[test]
    fn test_demo_simulator_values_change() {
        let mut sim = DemoSimulator::new();
        let timestamp = 1700000000000;

        let points1 = sim.tick(timestamp);
        let points2 = sim.tick(timestamp + 1000);

        // Counter values should increase
        let counter1: u64 = points1
            .iter()
            .find(|p| p.metric == "network/eth0/rx_bytes" && p.source == "server01")
            .and_then(|p| match &p.value {
                TelemetryValue::Counter(v) => Some(*v),
                _ => None,
            })
            .unwrap_or(0);

        let counter2: u64 = points2
            .iter()
            .find(|p| p.metric == "network/eth0/rx_bytes" && p.source == "server01")
            .and_then(|p| match &p.value {
                TelemetryValue::Counter(v) => Some(*v),
                _ => None,
            })
            .unwrap_or(0);

        assert!(counter2 > counter1);
    }

    #[test]
    fn test_demo_alert_rules() {
        let rules = demo_alert_rules();
        assert!(!rules.is_empty());
        assert!(rules.iter().any(|r| r.name.contains("CPU")));
        assert!(rules.iter().any(|r| r.name.contains("Memory")));
        assert!(rules.iter().any(|r| r.name.contains("Temperature")));
    }
}
