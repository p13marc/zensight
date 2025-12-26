//! Demo mode simulation engine.
//!
//! Provides realistic, time-varying telemetry data for demonstrating
//! ZenSight without actual bridges or Zenoh connections.

use std::collections::HashMap;
use std::f64::consts::PI;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use zensight_common::{
    DeviceLiveness, DeviceStatus, HealthSnapshot, Protocol, TelemetryPoint, TelemetryValue,
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
    /// Metrics published counter per bridge.
    metrics_published: HashMap<String, u64>,
    /// Errors per bridge in the last "hour" (scaled for demo).
    errors_per_bridge: HashMap<String, u64>,
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
            errors_per_bridge: HashMap::new(),
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
            let event_type = match self.rng.random_range(0..10) {
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
                _ => {
                    // Error burst
                    let server = servers[self.rng.random_range(0..servers.len())];
                    EventType::ErrorBurst {
                        server: server.to_string(),
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

            // Memory usage (using metric names that match the bridge)
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

            // Disk usage (using metric names that match the bridge: disk/{mount}/used, disk/{mount}/total)
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
        let uptime = self.tick * 100; // centiseconds
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
    fn generate_syslog(&mut self, timestamp: i64) -> Vec<TelemetryPoint> {
        let mut points = Vec::new();

        // Normal operation logs (occasional)
        if self.rng.random_range(0..5) == 0 {
            let messages = [
                ("server01", "auth/info", "User admin logged in successfully"),
                ("server02", "cron/info", "CRON job completed: backup.sh"),
                ("database01", "daemon/info", "Database checkpoint completed"),
                ("server03", "kernel/info", "Network interface eth0 link up"),
            ];

            let (server, facility, msg) = messages[self.rng.random_range(0..messages.len())];
            points.push(self.make_point(
                Protocol::Syslog,
                server,
                facility,
                TelemetryValue::Text(msg.to_string()),
                timestamp,
            ));
        }

        // Warning logs (less frequent)
        if self.rng.random_range(0..15) == 0 {
            let warnings = [
                ("server01", "kernel/warning", "High memory usage detected"),
                (
                    "server02",
                    "daemon/warning",
                    "Connection pool nearly exhausted",
                ),
                ("database01", "daemon/warning", "Slow query detected: 2.3s"),
                ("router01", "daemon/warning", "BGP neighbor flapping"),
            ];

            let (server, facility, msg) = warnings[self.rng.random_range(0..warnings.len())];
            points.push(self.make_point(
                Protocol::Syslog,
                server,
                facility,
                TelemetryValue::Text(msg.to_string()),
                timestamp,
            ));
        }

        // Error logs during anomalies
        for anomaly in &self.active_anomalies {
            if self.rng.random_range(0..3) == 0 {
                match &anomaly.anomaly_type {
                    AnomalyType::CpuSpike { server, .. } => {
                        points.push(self.make_point(
                            Protocol::Syslog,
                            server,
                            "daemon/error",
                            TelemetryValue::Text("Process consuming excessive CPU".to_string()),
                            timestamp,
                        ));
                    }
                    AnomalyType::MemoryLeak { server, .. } => {
                        points.push(self.make_point(
                            Protocol::Syslog,
                            server,
                            "kernel/warning",
                            TelemetryValue::Text("Memory pressure increasing".to_string()),
                            timestamp,
                        ));
                    }
                    AnomalyType::InterfaceDown { device, interface } => {
                        points.push(self.make_point(
                            Protocol::Syslog,
                            device,
                            "kernel/error",
                            TelemetryValue::Text(format!("Interface {} link down", interface)),
                            timestamp,
                        ));
                    }
                    AnomalyType::TemperatureHigh { plc, temp } => {
                        points.push(self.make_point(
                            Protocol::Syslog,
                            plc,
                            "daemon/error",
                            TelemetryValue::Text(format!(
                                "Temperature alarm: {:.1}C exceeds threshold",
                                temp
                            )),
                            timestamp,
                        ));
                    }
                    AnomalyType::ErrorBurst { server } => {
                        let error_messages = [
                            "Connection refused: upstream server unreachable",
                            "Database query timeout after 30s",
                            "Failed to write to disk: I/O error",
                            "SSL handshake failed: certificate expired",
                            "Out of file descriptors",
                            "Service health check failed",
                            "Request queue overflow, dropping requests",
                        ];
                        let msg = error_messages[self.tick as usize % error_messages.len()];
                        points.push(self.make_point(
                            Protocol::Syslog,
                            server,
                            "daemon/error",
                            TelemetryValue::Text(msg.to_string()),
                            timestamp,
                        ));
                    }
                    _ => {}
                }
            }
        }

        points
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

    /// Generate bridge health snapshots.
    pub fn generate_health_snapshots(&mut self) -> Vec<HealthSnapshot> {
        let uptime = self.tick - self.start_tick;

        // Define bridges and their device counts
        let bridges = [
            ("sysinfo", 4u64), // 4 servers
            ("snmp", 2u64),    // router + switch
            ("modbus", 2u64),  // 2 PLCs
            ("syslog", 4u64),  // Same servers that generate syslog
        ];

        bridges
            .iter()
            .map(|(name, device_count)| {
                // Count devices with active anomalies
                let devices_with_issues = self.count_devices_with_issues(name);
                let devices_failed = devices_with_issues.min(*device_count);
                let devices_responding = device_count.saturating_sub(devices_failed);

                // Determine overall status
                let status = if devices_failed == 0 {
                    "healthy"
                } else if devices_responding > 0 {
                    "degraded"
                } else {
                    "error"
                };

                let metrics = *self.metrics_published.get(*name).unwrap_or(&0);
                let errors = *self.errors_per_bridge.get(*name).unwrap_or(&0);

                HealthSnapshot {
                    bridge: name.to_string(),
                    status: status.to_string(),
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

    /// Count devices with active issues for a bridge.
    fn count_devices_with_issues(&self, bridge: &str) -> u64 {
        let mut count = 0u64;

        for anomaly in &self.active_anomalies {
            let device_affected = match &anomaly.anomaly_type {
                AnomalyType::CpuSpike { server, .. } => {
                    bridge == "sysinfo" && self.is_server(server)
                }
                AnomalyType::MemoryLeak { server, .. } => {
                    bridge == "sysinfo" && self.is_server(server)
                }
                AnomalyType::DiskFilling { server, .. } => {
                    bridge == "sysinfo" && self.is_server(server)
                }
                AnomalyType::InterfaceDown { device, .. } => {
                    bridge == "snmp" && self.is_network_device(device)
                }
                AnomalyType::TrafficBurst { device, .. } => {
                    bridge == "snmp" && self.is_network_device(device)
                }
                AnomalyType::TemperatureHigh { plc, .. } => bridge == "modbus" && self.is_plc(plc),
                AnomalyType::ErrorBurst { server } => bridge == "syslog" && self.is_server(server),
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
            {
                if d == device {
                    return (
                        DeviceStatus::Offline,
                        3,
                        Some(format!("Interface {} is down", interface)),
                    );
                }
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
                _ => (false, None),
            };

            if is_affected {
                return (DeviceStatus::Degraded, 1, error_msg);
            }
        }

        // No anomalies affecting this device
        (DeviceStatus::Online, 0, None)
    }

    /// Record metrics published for a bridge.
    pub fn record_metrics(&mut self, bridge: &str, count: u64) {
        *self
            .metrics_published
            .entry(bridge.to_string())
            .or_insert(0) += count;
    }

    /// Record an error for a bridge.
    pub fn record_error(&mut self, bridge: &str) {
        *self
            .errors_per_bridge
            .entry(bridge.to_string())
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

        // Check we have multiple protocols
        let protocols: std::collections::HashSet<_> = points.iter().map(|p| p.protocol).collect();
        assert!(protocols.contains(&Protocol::Sysinfo));
        assert!(protocols.contains(&Protocol::Snmp));
        assert!(protocols.contains(&Protocol::Modbus));
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
