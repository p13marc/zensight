# zenoh-bridge-modbus

Modbus bridge for the ZenSight observability platform. Polls Modbus devices and publishes register values to Zenoh.

## Features

- **Modbus TCP** - Ethernet-connected devices
- **Modbus RTU** - Serial (RS-485/RS-232) connected devices
- **All Register Types** - Coils, discrete inputs, holding registers, input registers
- **Data Type Decoding** - u16, i16, u32, i32, f32, f64 with byte ordering
- **Multi-device** - Poll multiple devices from single bridge instance

## Installation

```bash
cargo build -p zenoh-bridge-modbus --release
```

## Usage

```bash
# Run with configuration file
zenoh-bridge-modbus --config configs/modbus.json5

# Run with custom config path
zenoh-bridge-modbus --config /etc/zensight/modbus.json5
```

## Configuration

Create a JSON5 configuration file:

```json5
{
  // Zenoh connection
  zenoh: {
    mode: "peer",
    connect: [],
    listen: [],
  },

  // Serialization format
  serialization: "json",  // or "cbor"

  // Modbus settings
  modbus: {
    key_prefix: "zensight/modbus",

    // Devices to poll
    devices: [
      {
        name: "plc01",
        connection: {
          type: "tcp",
          host: "192.168.1.10",
          port: 502,
        },
        unit_id: 1,
        poll_interval_secs: 10,
        registers: [
          {
            type: "holding",
            address: 0,
            count: 2,
            name: "temperature",
            data_type: "f32",
          },
          {
            type: "holding",
            address: 2,
            count: 2,
            name: "pressure",
            data_type: "f32",
          },
          {
            type: "coil",
            address: 0,
            count: 1,
            name: "pump_running",
          },
          {
            type: "input",
            address: 100,
            count: 1,
            name: "flow_rate",
            data_type: "u16",
          },
        ],
      },
    ],
  },

  // Logging
  logging: {
    level: "info",
  },
}
```

### Modbus TCP Configuration

```json5
{
  devices: [
    {
      name: "plc01",
      connection: {
        type: "tcp",
        host: "192.168.1.10",
        port: 502,
      },
      unit_id: 1,
      poll_interval_secs: 10,
      registers: [ /* ... */ ],
    },
  ],
}
```

### Modbus RTU Configuration

```json5
{
  devices: [
    {
      name: "sensor01",
      connection: {
        type: "rtu",
        port: "/dev/ttyUSB0",
        baud_rate: 9600,
        data_bits: 8,
        parity: "none",     // "none", "even", "odd"
        stop_bits: 1,
      },
      unit_id: 1,
      poll_interval_secs: 5,
      registers: [ /* ... */ ],
    },
  ],
}
```

## Register Types

| Type | Function Code | Description |
|------|---------------|-------------|
| `coil` | FC 01 | Read/write discrete outputs |
| `discrete` | FC 02 | Read-only discrete inputs |
| `holding` | FC 03 | Read/write 16-bit registers |
| `input` | FC 04 | Read-only 16-bit registers |

## Data Types

| Type | Registers | Description |
|------|-----------|-------------|
| `bool` | 1 (coil) | Boolean value |
| `u16` | 1 | Unsigned 16-bit integer |
| `i16` | 1 | Signed 16-bit integer |
| `u32` | 2 | Unsigned 32-bit integer |
| `i32` | 2 | Signed 32-bit integer |
| `f32` | 2 | 32-bit floating point |
| `f64` | 4 | 64-bit floating point |

### Byte Ordering

For multi-register types, specify byte order:

```json5
{
  type: "holding",
  address: 0,
  count: 2,
  name: "value",
  data_type: "f32",
  byte_order: "big_endian",  // or "little_endian" (default: big_endian)
  word_order: "big_endian",  // or "little_endian" (default: big_endian)
}
```

## Key Expressions

Published telemetry uses the format:

```
zensight/modbus/<device>/<register_type>/<name>
```

Examples:
- `zensight/modbus/plc01/holding/temperature`
- `zensight/modbus/plc01/holding/pressure`
- `zensight/modbus/plc01/coil/pump_running`
- `zensight/modbus/sensor01/input/flow_rate`

## Telemetry Format

```json
{
  "timestamp": 1703500800000,
  "source": "plc01",
  "protocol": "modbus",
  "metric": "holding/temperature",
  "value": 45.7,
  "labels": {
    "register_type": "holding",
    "address": "0",
    "unit_id": "1",
    "data_type": "f32"
  }
}
```

## Register Configuration Examples

### Temperature Sensor (f32)

```json5
{
  type: "holding",
  address: 0,
  count: 2,
  name: "temperature",
  data_type: "f32",
}
```

### Discrete Input (bool)

```json5
{
  type: "discrete",
  address: 0,
  count: 1,
  name: "door_open",
}
```

### Counter (u32)

```json5
{
  type: "input",
  address: 100,
  count: 2,
  name: "total_count",
  data_type: "u32",
}
```

### Multiple Coils

```json5
{
  type: "coil",
  address: 0,
  count: 8,
  name: "outputs",
  // Returns array of boolean values
}
```

## Architecture

```
zenoh-bridge-modbus/
├── src/
│   ├── main.rs      # Entry point, CLI, orchestration
│   ├── config.rs    # Configuration structs
│   ├── poller.rs    # Per-device polling task
│   ├── client.rs    # Modbus TCP/RTU client
│   └── decode.rs    # Register value decoding
└── Cargo.toml
```

## Testing

```bash
# Run all tests (11 total)
cargo test -p zenoh-bridge-modbus

# Run decoder tests only
cargo test -p zenoh-bridge-modbus decode

# Run with verbose output
cargo test -p zenoh-bridge-modbus -- --nocapture
```

### Test Categories

| Category | Tests | Description |
|----------|-------|-------------|
| Config | 4 | Configuration validation |
| Decode | 5 | Data type decoding |
| Integration | 2 | End-to-end polling |

### Testing with Modbus Simulator

Use `diagslave` or `ModRSsim2` for testing:

```bash
# Install diagslave
apt install diagslave

# Run TCP slave on port 502
diagslave -m tcp -p 502

# Set register values
# (Use Modbus client tool like mbpoll)
mbpoll -t 4:float -r 0 -a 1 192.168.1.10 45.7
```

### Testing with mbpoll

```bash
# Read holding registers
mbpoll -t 4 -r 0 -c 10 -a 1 192.168.1.10

# Write holding register
mbpoll -t 4 -r 0 -a 1 192.168.1.10 1234

# Read coils
mbpoll -t 0 -r 0 -c 8 -a 1 192.168.1.10
```

## PLC/Device Setup

### Siemens S7 (via Modbus TCP)

Configure Modbus TCP server block in TIA Portal:
1. Add MB_SERVER block
2. Configure IP address and port 502
3. Map data blocks to Modbus addresses

### Allen-Bradley

Use ProSoft MVI modules or native Modbus support:
1. Configure Modbus mapping table
2. Set up Ethernet/IP to Modbus gateway
3. Map PLC tags to registers

### Generic RTU Device

Connect via RS-485:
1. Wire A+/B- to RS-485 converter
2. Configure baud rate, parity, stop bits
3. Verify unit ID matches device DIP switches

## Troubleshooting

### Connection Timeout

```
Error: Connection timeout to 192.168.1.10:502
```

- Verify IP address and port
- Check firewall rules
- Ensure device supports Modbus TCP

### Invalid Response

```
Error: Invalid CRC in Modbus RTU response
```

- Check serial settings (baud, parity, stop bits)
- Verify cable wiring (A+/B-)
- Check unit ID

### Register Out of Range

```
Error: Illegal data address
```

- Verify register address exists on device
- Check register type (holding vs input)
- Confirm count doesn't exceed available registers

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio-modbus` | Modbus TCP/RTU client |
| `zensight-common` | Shared data model |
| `zenoh` | Pub/sub messaging |
| `tokio` | Async runtime |
| `tokio-serial` | Serial port for RTU |
| `clap` | CLI argument parsing |

## License

MIT OR Apache-2.0
