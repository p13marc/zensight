# zensight-sensor-core

The shared framework every ZenSight protocol sensor is built on. A sensor crate
(`zensight-sensor-snmp`, `-netlink`, `-netring`, …) supplies the protocol-specific
polling/parsing; this crate supplies everything else: the lifecycle runner, the
Zenoh publishers with the right QoS, host identity, health and alert reporting,
liveness, and the on-demand artifact channel.

## What it gives a sensor

- **`SensorRunner`** — config load, logging, Zenoh connect, task spawning,
  graceful shutdown (SIGINT/SIGTERM), and periodic health + identity/evidence
  publication.
- **`Publisher` / registries** — declare-once Zenoh publishers: zenoh-ext
  *advanced* publishers for telemetry (cache + late-joiner recovery) and plain
  declared publishers for the `@/` control plane, each with the correct
  `QosClass`.
- **`HostIdentity` / `SharedIdentity`** — a stable, non-reversible `host_id`
  (`sha256(machine-id + salt)`) plus boot id, hostname, IPs, MACs, container and
  cloud facts.
- **`HealthReporter` / `AlertReporter`** — rolling-window health snapshots and the
  firing/resolved alert lifecycle with debounce + reconcile.
- **`ArtifactChannel` / `ArtifactProducer`** — on-demand large-data transfer
  (debug report, directory snapshot, packet capture) over `zenoh-blob`.
- **`procutil` / `scrub`** — `(pid, start_time)` process identity and an argv
  secret scrubber.

## Quick start

This is a library crate — sensors depend on it and provide their own `main`:

```toml
[dependencies]
zensight-sensor-core = { path = "../zensight-sensor-core" }
```

```rust
use zensight_sensor_core::{SensorArgs, SensorConfig, SensorRunner};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = SensorArgs::parse("mysensor.json5");
    let config = MySensorConfig::load(&args.config)?;

    let mut runner = SensorRunner::new("mysensor", config)
        .await?
        .with_identity("host01");

    let publisher = runner.publisher();
    runner.spawn(async move { /* poll + publisher.publish(...) */ });

    runner.run().await   // runs until Ctrl+C / SIGTERM
}
```

Run the tests:

```bash
cargo test -p zensight-sensor-core
```

## Documentation

- [Framework](docs/framework.md) — runner, publishers, identity, health, alerts,
  liveness, procutil, scrub, and the declare-all discipline.
- [Artifacts](docs/artifacts.md) — the `ArtifactChannel` + `ArtifactProducer`
  contract for on-demand large data.

The wire types this framework publishes (`TelemetryPoint`, `Alert`, `HostEvidence`,
`ArtifactRequest`, `QosClass`, …) live in and are documented under
[`zensight-common`](../zensight-common/README.md). The full key contract is
[`../docs/KEYSPACE.md`](../docs/KEYSPACE.md).

## License

MIT OR Apache-2.0
