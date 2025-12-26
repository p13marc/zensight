# Specialized Views Plan

## Status: COMPLETED

All phases of this plan have been implemented successfully.

## Overview

This plan proposes adding protocol-specific specialized views to ZenSight that provide optimized visualizations and interactions for each telemetry type. Instead of a generic device detail view, each protocol gets a tailored interface that highlights the most relevant metrics and provides domain-appropriate visualizations.

---

## Implementation Status

| Phase | Status | Notes |
|-------|--------|-------|
| Phase 1: Infrastructure | DONE | `specialized/mod.rs` with view selection logic |
| Phase 2: Sysinfo View | DONE | CPU/Memory/Disk gauges, network stats |
| Phase 3: SNMP View | DONE | Interface table, system info, metrics |
| Phase 4: Syslog View | DONE | Severity distribution, log stream |
| Phase 5: Modbus View | DONE | Register tables, boolean LEDs |
| Phase 6: NetFlow View | DONE | Top talkers, protocol distribution, flow table |
| Phase 7: gNMI View | DONE | Path browser, subscriptions |

### Components Created

| Component | Status | Location |
|-----------|--------|----------|
| Gauge | DONE | `components/gauge.rs` |
| ProgressBar | DONE | `components/progress_bar.rs` |
| StatusLed | DONE | `components/status_led.rs` |
| Sparkline | DONE | `components/sparkline.rs` |

### Test Coverage

- 72 unit tests passing
- 14 UI tests (including 5 specialized view tests)

---

## Previous State

Previously, ZenSight had a single generic `DeviceDetailState` view that displays:
- List of metrics with values
- Time-series chart for selected metric
- Metric filtering

This works but doesn't leverage protocol-specific knowledge to provide better UX.

---

## Proposed Specialized Views

### 1. SNMP Network Device View

**Target**: Routers, switches, firewalls

**Layout**:
```
+------------------------------------------+
| Device: router01        Status: Healthy  |
| sysName: core-router    Uptime: 45d 3h   |
+------------------------------------------+
| Interface Table                          |
| +--------------------------------------+ |
| | Port | Name  | Status | In    | Out  | |
| | 1    | Gi0/1 | UP     | 1.2G  | 800M | |
| | 2    | Gi0/2 | DOWN   | 0     | 0    | |
| | 3    | Gi0/3 | UP     | 500M  | 1.1G | |
| +--------------------------------------+ |
+------------------------------------------+
| Interface Bandwidth Chart (multi-line)   |
| [=== Gi0/1 In ===] [--- Gi0/2 Out ---]  |
+------------------------------------------+
| System Metrics: CPU | Memory | Temp      |
+------------------------------------------+
```

**Key Features**:
- Interface table with status indicators (UP/DOWN/ADMIN DOWN)
- Sparkline bandwidth charts per interface
- Error/discard counters with thresholds
- Multi-interface comparison chart
- SNMP trap history panel

**Files to create**:
- `zensight/src/view/specialized/mod.rs`
- `zensight/src/view/specialized/snmp.rs`

---

### 2. Sysinfo Host View

**Target**: Servers, workstations monitored via sysinfo bridge

**Layout**:
```
+------------------------------------------+
| Host: server01          OS: Linux 6.x   |
| Uptime: 15d 7h 23m      Load: 2.4 1.8   |
+------------------------------------------+
| CPU Usage        | Memory Usage          |
| [####----] 45%   | [######--] 75%        |
| 8 cores @ 3.2GHz | 16GB / 32GB           |
+------------------------------------------+
| Per-Core Usage                           |
| Core 0: [###] Core 1: [##] Core 2: [#]  |
+------------------------------------------+
| Disk Usage                               |
| /     : [########--] 80% (400G/500G)    |
| /home : [####------] 40% (200G/500G)    |
+------------------------------------------+
| Network I/O                              |
| eth0: rx 1.2 MB/s  tx 500 KB/s          |
+------------------------------------------+
| Top Processes (optional)                 |
+------------------------------------------+
```

**Key Features**:
- Gauge visualizations for CPU/Memory/Disk
- Per-core CPU breakdown
- Disk usage bars with thresholds (warning at 80%, critical at 90%)
- Network throughput sparklines
- Temperature monitoring (if available)

**Files to create**:
- `zensight/src/view/specialized/sysinfo.rs`

---

### 3. Syslog Event View

**Target**: Log aggregation from syslog bridge

**Layout**:
```
+------------------------------------------+
| Host: server01     Messages: 1,234 today |
+------------------------------------------+
| Severity Distribution (last hour)        |
| EMERG: 0 | CRIT: 2 | ERR: 15 | WARN: 45 |
+------------------------------------------+
| Facility Filter: [All v] Severity: [>=WARN v] |
| Search: [_______________] [x] Regex      |
+------------------------------------------+
| Log Stream                               |
| 14:32:01 ERR  sshd   Failed login root  |
| 14:31:45 WARN nginx  High latency 2.3s  |
| 14:31:30 INFO systemd Service started   |
| 14:31:15 ERR  sshd   Failed login admin |
| ...                                      |
+------------------------------------------+
| [Pause] [Export CSV] [Clear Filters]    |
+------------------------------------------+
```

**Key Features**:
- Real-time log streaming with auto-scroll
- Severity-based color coding
- Facility and severity filtering
- Full-text search with regex support
- Log rate histogram (messages per minute)
- Export to CSV/JSON

**Files to create**:
- `zensight/src/view/specialized/syslog.rs`

---

### 4. Modbus PLC View

**Target**: Industrial PLCs and devices via Modbus bridge

**Layout**:
```
+------------------------------------------+
| Device: plc01        Protocol: Modbus TCP|
| Address: 192.168.1.100:502   Unit ID: 1 |
+------------------------------------------+
| Register Map                             |
| +--------------------------------------+ |
| | Addr  | Name         | Value | Type  | |
| | 40001 | Temperature  | 72.5  | Float | |
| | 40003 | Pressure     | 14.7  | Float | |
| | 40005 | Motor Status | 1     | Bool  | |
| | 40006 | RPM          | 1750  | Int16 | |
| +--------------------------------------+ |
+------------------------------------------+
| Live Gauges                              |
| Temp: [|||||||---] 72.5°F               |
| Pressure: [|||||-----] 14.7 PSI         |
+------------------------------------------+
| Trend Chart: Temperature (last 1h)      |
+------------------------------------------+
```

**Key Features**:
- Register table with address, name, value, type
- Live gauge widgets for analog values
- Boolean status indicators (LEDs)
- Register write capability (if supported)
- Alarm thresholds on registers

**Files to create**:
- `zensight/src/view/specialized/modbus.rs`

---

### 5. NetFlow Traffic View

**Target**: Network flow analysis from NetFlow/IPFIX bridge

**Layout**:
```
+------------------------------------------+
| Exporter: router01    Flows: 12,456/min |
+------------------------------------------+
| Top Talkers (by bytes)                   |
| 1. 10.0.1.5 -> 8.8.8.8      : 1.2 GB   |
| 2. 10.0.1.10 -> 172.16.0.1  : 800 MB   |
| 3. 10.0.2.15 -> 10.0.1.5    : 500 MB   |
+------------------------------------------+
| Protocol Distribution                    |
| [TCP 65%] [UDP 30%] [ICMP 3%] [Other 2%]|
+------------------------------------------+
| Port Distribution                        |
| 443: 45% | 80: 20% | 22: 10% | 53: 8%  |
+------------------------------------------+
| Traffic Timeline (bytes/sec)             |
| [=========== chart ===========]          |
+------------------------------------------+
| Flow Table (filterable)                  |
| Src IP | Dst IP | Proto | Bytes | Pkts  |
+------------------------------------------+
```

**Key Features**:
- Top talkers (by bytes, packets, flows)
- Protocol/port distribution pie charts
- Traffic timeline
- Flow table with filtering
- Geo-IP visualization (future)

**Files to create**:
- `zensight/src/view/specialized/netflow.rs`

---

### 6. gNMI Streaming Telemetry View

**Target**: Modern network devices with gNMI support

**Layout**:
```
+------------------------------------------+
| Device: spine01      Model: Arista 7050 |
| gNMI Target: 10.0.0.1:6030              |
+------------------------------------------+
| Active Subscriptions                     |
| /interfaces/interface[*]/state          |
| /system/cpu/state                        |
| /network-instances/network-instance[*]  |
+------------------------------------------+
| Path Browser                             |
| > /interfaces                            |
|   > /interface[name=Ethernet1]          |
|     > /state                             |
|       - admin-status: UP                 |
|       - oper-status: UP                  |
|       - counters/in-octets: 1234567     |
+------------------------------------------+
| Selected Path Chart                      |
+------------------------------------------+
```

**Key Features**:
- Hierarchical path browser (tree view)
- Active subscription list
- Path-based navigation
- OpenConfig schema awareness
- Multi-path comparison

**Files to create**:
- `zensight/src/view/specialized/gnmi.rs`

---

## Implementation Architecture

### View Selection Logic

```rust
// zensight/src/view/specialized/mod.rs

pub mod snmp;
pub mod sysinfo;
pub mod syslog;
pub mod modbus;
pub mod netflow;
pub mod gnmi;

use zensight_common::Protocol;

/// Select the appropriate specialized view based on protocol
pub fn specialized_view<'a>(
    device_id: &'a DeviceId,
    state: &'a DeviceDetailState,
) -> Element<'a, Message> {
    match device_id.protocol {
        Protocol::Snmp => snmp::snmp_device_view(state),
        Protocol::Sysinfo => sysinfo::sysinfo_host_view(state),
        Protocol::Syslog => syslog::syslog_event_view(state),
        Protocol::Modbus => modbus::modbus_plc_view(state),
        Protocol::Netflow => netflow::netflow_traffic_view(state),
        Protocol::Gnmi => gnmi::gnmi_streaming_view(state),
    }
}
```

### State Extensions

Each specialized view may need additional state beyond `DeviceDetailState`:

```rust
// Example: Syslog-specific state
pub struct SyslogViewState {
    /// Base device state
    pub base: DeviceDetailState,
    /// Severity filter
    pub min_severity: Severity,
    /// Facility filter
    pub facility_filter: Option<Facility>,
    /// Search query
    pub search: String,
    /// Use regex for search
    pub use_regex: bool,
    /// Auto-scroll enabled
    pub auto_scroll: bool,
    /// Paused (stop receiving new logs)
    pub paused: bool,
}
```

### New Messages

```rust
// Additional messages for specialized views
pub enum Message {
    // ... existing messages ...
    
    // Syslog view
    SetSyslogSeverityFilter(Severity),
    SetSyslogFacilityFilter(Option<Facility>),
    SetSyslogSearch(String),
    ToggleSyslogRegex,
    ToggleSyslogAutoScroll,
    ToggleSyslogPause,
    ExportSyslogCsv,
    
    // SNMP view
    SelectInterface(u32),
    CompareInterfaces(Vec<u32>),
    
    // Modbus view
    WriteRegister(u16, ModbusValue),
    
    // NetFlow view
    SetFlowFilter(FlowFilter),
    SetTimeRange(Duration),
}
```

---

## Implementation Plan (COMPLETED)

### Phase 1: Infrastructure - DONE
1. Created `zensight/src/view/specialized/mod.rs` with view selection logic
2. Updated `device_view` to delegate to specialized views
3. Generic fallback for unknown protocols (e.g., OPC-UA)

### Phase 2: Sysinfo View - DONE
1. Implemented gauge widgets for CPU/Memory/Disk
2. Added per-core CPU breakdown
3. Added disk usage progress bars with thresholds
4. Added network interface stats with status LEDs

### Phase 3: SNMP View - DONE
1. Implemented interface table component with status indicators
2. Added system info section (sysDescr, sysUpTime, sysContact, sysLocation)
3. Added system metrics section (CPU, Memory, Temperature)
4. Parse interface metrics from if/<index>/<metric> patterns

### Phase 4: Syslog View - DONE
1. Implemented log stream with severity-based coloring
2. Added severity distribution summary
3. Sorted messages by timestamp (newest first)
4. Limit to 100 most recent messages for performance

### Phase 5: Modbus View - DONE
1. Implemented register table grouped by type (Coil, Discrete, Holding, Input)
2. Added boolean LED indicators for coils/discrete inputs
3. Added register tables for holding/input registers with address, name, value, unit

### Phase 6: NetFlow View - DONE
1. Implemented top talkers table (by bytes)
2. Added protocol distribution visualization with colored bars
3. Added flow table with filtering (source, dest, ports, protocol, bytes, packets)
4. Added traffic summary (total bytes, packets, unique sources/destinations)

### Phase 7: gNMI View - DONE
1. Implemented path browser with indentation by depth
2. Added subscription inference from metric path prefixes
3. Added device info section from OpenConfig paths

---

## UI Components Created

| Component | Description | Used By | Status |
|-----------|-------------|---------|--------|
| `Gauge` | Linear gauge with threshold colors | Sysinfo, SNMP | DONE |
| `StatusLed` | Boolean indicator (green/red/yellow/gray) | SNMP, Modbus, Sysinfo | DONE |
| `Sparkline` | Mini inline chart using canvas | Available for future use | DONE |
| `ProgressBar` | Percentage bar with thresholds | Sysinfo disk/memory | DONE |

### Not Implemented (Not Needed)

| Component | Description | Notes |
|-----------|-------------|-------|
| `DataTable` | Sortable, filterable table | Used inline row/column layouts instead |
| `TreeView` | Hierarchical expandable tree | Used indented flat list for gNMI |
| `LogStream` | Virtual-scrolling log viewer | Used scrollable with limit (100 messages) |
| `PieChart` | Distribution visualization | Used colored bar segments for NetFlow |

---

## File Structure

```
zensight/src/view/
├── mod.rs
├── dashboard.rs
├── device.rs              # Generic fallback view
├── specialized/
│   ├── mod.rs             # View selection logic
│   ├── snmp.rs            # SNMP network device view
│   ├── sysinfo.rs         # Host monitoring view
│   ├── syslog.rs          # Log event view
│   ├── modbus.rs          # PLC/industrial view
│   ├── netflow.rs         # Flow analysis view
│   └── gnmi.rs            # Streaming telemetry view
├── components/
│   ├── mod.rs
│   ├── gauge.rs           # Gauge widget
│   ├── status_led.rs      # Boolean indicator
│   ├── sparkline.rs       # Mini chart
│   ├── data_table.rs      # Generic table
│   ├── tree_view.rs       # Hierarchical browser
│   ├── log_stream.rs      # Log viewer
│   └── progress_bar.rs    # Usage bar
```

---

## Dependencies

No new external dependencies required. All visualizations will be built with Iced primitives (canvas, container, text, etc.).

---

## Testing Strategy (IMPLEMENTED)

1. **Unit tests**: Each component and specialized view has unit tests (72 total)
2. **Mock data**: Using existing mock module to generate test data
3. **UI tests**: Simulator-based tests for each specialized view (14 total, 5 for specialized views)
4. **Integration**: Can test with real bridges when available

---

## Questions for Review (RESOLVED)

1. **Priority order**: Implemented all views in the planned order
2. **Generic fallback**: Yes, generic view remains for unknown protocols (e.g., OPC-UA)
3. **View switching**: Not implemented - specialized views are automatic based on protocol
4. **Component reuse**: Components are in `view/components/` module, can be extracted later if needed
5. **State management**: Uses shared `DeviceDetailState` - specialized views parse protocol-specific data from metrics

---

## Completed Effort

| Phase | Status |
|-------|--------|
| Phase 1: Infrastructure | DONE |
| Phase 2: Sysinfo View | DONE |
| Phase 3: SNMP View | DONE |
| Phase 4: Syslog View | DONE |
| Phase 5: Modbus View | DONE |
| Phase 6: NetFlow View | DONE |
| Phase 7: gNMI View | DONE |

**All phases completed.**
