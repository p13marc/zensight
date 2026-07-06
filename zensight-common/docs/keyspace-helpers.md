# Keyspace helpers

A quick index of the key-expression builders in `keyexpr.rs` (and a few in
`command.rs`). New code MUST build keys through these helpers rather than ad-hoc
`format!()`. This page is only an index — [`../docs/KEYSPACE.md`](../../docs/KEYSPACE.md)
is the **authoritative** contract for what each key means and how the wildcards
interact.

The single root is `KEY_PREFIX` = `zensight`. A key chunk starting with `@` is
matched **verbatim** by Zenoh: `*` / `**` do not cross into it, which is how the
control plane (`@/`), the media plane (`@media`) and the passive-DNS tier
(`@pdns`) stay off the telemetry firehose.

## Telemetry — `zensight/<protocol>/<source>/<metric>`

Use `KeyExprBuilder::new(protocol)` (or `with_prefix`):

| Method | Result |
|--------|--------|
| `.build(source, metric)` | `zensight/<proto>/<source>/<metric>` |
| `.source_wildcard(source)` | `zensight/<proto>/<source>/**` |
| `.protocol_wildcard()` | `zensight/<proto>/**` |
| `.status_key()` | `zensight/<proto>/@/status` |
| `.alert_key_expr(alert_key)` | `zensight/<proto>/@/alerts/<alert_key>` |
| `all_telemetry_wildcard()` | `zensight/**` |
| `parse_key_expr(key)` | `ParsedKeyExpr { protocol, source, metric }` (or `ParseError`) |

## Per-sensor control plane — `zensight/<protocol>/@/…`

Health/liveness/error wildcards (`keyexpr.rs`):

| Helper | Result |
|--------|--------|
| `all_health_wildcard()` | `zensight/*/@/health` |
| `all_liveness_wildcard()` | `zensight/*/@/devices/*/liveness` |
| `all_errors_wildcard()` | `zensight/*/@/errors` |
| `all_alerts_wildcard()` | `zensight/*/@/alerts/*` |

Command / status / query + artifact channels (`command.rs`), where `prefix` is
`zensight/<protocol>`:

| Helper | Result |
|--------|--------|
| `command_key(prefix, topic)` | `<prefix>/@/commands/<topic>` |
| `status_key(prefix, topic)` | `<prefix>/@/status/<topic>` |
| `query_key(prefix, topic)` | `<prefix>/@/query/<topic>` |
| `artifact_request_key(prefix)` | `<prefix>/@/artifact/request` |
| `artifact_status_key(prefix)` | `<prefix>/@/artifact/status` |
| `artifact_cancel_key(prefix)` | `<prefix>/@/artifact/cancel` |
| `artifact_blob_prefix(prefix)` | `<prefix>/@/artifact/blob` (Tier-1 delivery) |
| `artifact_store_prefix(prefix)` | `<prefix>/@/store` (Tier-2 chunks) |
| `artifact_tree_prefix(prefix)` | `<prefix>/@/tree` (Tier-2 index) |

## Cross-sensor metadata — `zensight/_meta/…`

| Helper | Result |
|--------|--------|
| `sensor_info_key(name, source)` | `zensight/_meta/sensors/<name>/<source>` |
| `all_sensors_wildcard()` | `zensight/_meta/sensors/**` |
| `host_evidence_key(sensor, source)` | `zensight/_meta/evidence/host/<sensor>/<source>` |
| `name_observation_key(sensor, ip-slug)` | `zensight/_meta/evidence/names/<sensor>/<ip-slug>` |
| `all_evidence_wildcard()` | `zensight/_meta/evidence/**` |
| `all_name_evidence_wildcard()` | `zensight/_meta/evidence/names/**` |
| `entity_key(entity_id)` | `zensight/_meta/entity/host/<entity_id>` |
| `all_entity_wildcard()` | `zensight/_meta/entity/**` |
| `entities_query_key()` | `zensight/_meta/query/entities` |
| `names_query_key()` | `zensight/_meta/query/names` (selector `?ip=<addr>`) |
| `correlator_alive_key()` | `zensight/_meta/correlator/@/alive` (single-writer guard) |

## `@`-verbatim planes — media & passive DNS

| Helper | Result |
|--------|--------|
| `media_video_key(proto, source, stream, codec, profile)` | `zensight/<proto>/<source>/@media/<stream>/video/<codec>/<profile>` |
| `media_preview_key(proto, source, stream)` | `zensight/<proto>/<source>/@media/<stream>/preview/jpeg` |
| `pdns_key(ip)` | `zensight/@pdns/<ip-slug>` |
| `all_pdns_wildcard()` | `zensight/@pdns/**` |

Media samples are opaque encoded access units (no `TelemetryPoint`/`Format`
envelope); their stream control rides the ordinary `@/` command/query/status
channels with topics `stream` / `streams`.

## See also

- [`../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) — authoritative keyspace contract.
- [Data model](data-model.md) and [Identity, evidence & entities](identity-evidence.md).
