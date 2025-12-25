//! Demo mode simulation engine.
//!
//! Provides realistic, time-varying telemetry data for demonstrating
//! ZenSight without actual bridges or Zenoh connections.

use std::collections::HashMap;
use std::f64::consts::PI;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use zensight_common::{Protocol, TelemetryPoint, TelemetryValue};

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
            // CPU usage
            let mut cpu = self.oscillating_value(&format!("{}/cpu", server), 40.0, 10.0);

            // Check for CPU spike anomaly
            for anomaly in &self.active_anomalies {
                if let AnomalyType::CpuSpike {
                    server: s,
                    intensity,
                } = &anomaly.anomaly_type
                {
                    if s == server {
                        // Spike with some variation
                        cpu = *intensity + self.rng.random_range(-5.0..5.0);
                    }
                }
            }

            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "cpu/usage",
                TelemetryValue::Gauge(cpu.clamp(0.0, 100.0)),
                timestamp,
            ));

            // Per-core CPU (4 cores)
            for core in 0..4 {
                let core_cpu = cpu + self.rng.random_range(-15.0..15.0);
                points.push(self.make_point(
                    Protocol::Sysinfo,
                    server,
                    &format!("cpu/{}/usage", core),
                    TelemetryValue::Gauge(core_cpu.clamp(0.0, 100.0)),
                    timestamp,
                ));
            }

            // Memory usage
            let mut memory_pct = self.oscillating_value(&format!("{}/memory", server), 60.0, 5.0);

            // Check for memory leak anomaly
            for anomaly in &self.active_anomalies {
                if let AnomalyType::MemoryLeak { server: s, rate } = &anomaly.anomaly_type {
                    if s == server {
                        let elapsed = self.tick - anomaly.start_tick;
                        memory_pct += elapsed as f64 * rate;
                    }
                }
            }

            let total_memory = 17_179_869_184.0_f64; // 16 GB
            let used_memory = (memory_pct.clamp(0.0, 99.0) / 100.0) * total_memory;

            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/used_bytes",
                TelemetryValue::Gauge(used_memory),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/total_bytes",
                TelemetryValue::Gauge(total_memory),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "memory/usage_percent",
                TelemetryValue::Gauge(memory_pct.clamp(0.0, 99.0)),
                timestamp,
            ));

            // Disk usage
            let mut disk_pct = self.oscillating_value(&format!("{}/disk", server), 50.0, 2.0);

            // Check for disk filling anomaly
            for anomaly in &self.active_anomalies {
                if let AnomalyType::DiskFilling { server: s, rate } = &anomaly.anomaly_type {
                    if s == server {
                        let elapsed = self.tick - anomaly.start_tick;
                        disk_pct += elapsed as f64 * rate;
                    }
                }
            }

            let total_disk = 536_870_912_000.0_f64; // 500 GB
            let used_disk = (disk_pct.clamp(0.0, 99.0) / 100.0) * total_disk;

            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "disk/root/used_bytes",
                TelemetryValue::Gauge(used_disk),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "disk/root/total_bytes",
                TelemetryValue::Gauge(total_disk),
                timestamp,
            ));
            points.push(self.make_point(
                Protocol::Sysinfo,
                server,
                "disk/root/usage_percent",
                TelemetryValue::Gauge(disk_pct.clamp(0.0, 99.0)),
                timestamp,
            ));

            // Network I/O
            let rx_rate = self.rng.random_range(100_000u64..5_000_000u64);
            let tx_rate = self.rng.random_range(50_000u64..2_000_000u64);

            let rx = self.increment_counter(&format!("{}/eth0/rx", server), rx_rate);
            let tx = self.increment_counter(&format!("{}/eth0/tx", server), tx_rate);

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
                {
                    if device == router && *interface == iface {
                        traffic_mult = *multiplier;
                    }
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
                if let AnomalyType::TemperatureHigh { plc: p, temp: t } = &anomaly.anomaly_type {
                    if p == plc {
                        temp = *t + self.rng.random_range(-2.0..2.0);
                    }
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
    use crate::view::alerts::{AlertRule, ComparisonOp};

    vec![
        // CPU alerts
        {
            let mut rule = AlertRule::new(1, "High CPU Usage", "cpu/usage");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 80.0;
            rule.protocol = Some(Protocol::Sysinfo);
            rule
        },
        // Memory alerts
        {
            let mut rule = AlertRule::new(2, "High Memory Usage", "memory/usage_percent");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 85.0;
            rule.protocol = Some(Protocol::Sysinfo);
            rule
        },
        // Disk alerts
        {
            let mut rule = AlertRule::new(3, "Disk Space Critical", "disk/root/usage_percent");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 90.0;
            rule.protocol = Some(Protocol::Sysinfo);
            rule
        },
        // Network interface down
        {
            let mut rule = AlertRule::new(4, "Interface Down", "ifOperStatus");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 1.5; // 2 = down
            rule.protocol = Some(Protocol::Snmp);
            rule
        },
        // Temperature alerts
        {
            let mut rule = AlertRule::new(5, "High Temperature", "holding/0");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 60.0;
            rule.protocol = Some(Protocol::Modbus);
            rule
        },
        // Interface errors
        {
            let mut rule = AlertRule::new(6, "Interface Errors", "ifInErrors");
            rule.operator = ComparisonOp::GreaterThan;
            rule.threshold = 0.0;
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
