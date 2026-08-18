# Keyspace helpers

A quick index of the key-expression builders in `keyexpr.rs` (and the `@rpc` /
`@blob` builders in `command.rs`). New code MUST build keys through these helpers
(or `zensight_keyspace::V1Context` on the producer side) rather than ad-hoc
`format!()`. This page is only an index — [`../docs/KEYSPACE.md`](../../docs/KEYSPACE.md)
summarizes the deployed profile and the RFC set in
[`../docs/rfcs/keyspace-v2/`](../../docs/rfcs/keyspace-v2/00-index.md) is the
**normative** contract.

The single root is `KEY_PREFIX` = `zensight`, and everything rides the v1 grammar
`zensight/v1/<origin>/<class>/<producer>/<subject...>` with the data classes
`telemetry` / `state` / `events`. A key chunk starting with `@` is matched
**verbatim** by Zenoh: `*` / `**` do not cross into it, which is how the version
chunk (`v1`), the planes (`@rpc` / `@media` / `@blob`) and the catalog origin
(`@catalog`) stay off every data-class selector.

## Consumer-side class selectors

Fleet-wide selectors (all origins, `keyexpr.rs`):

| Helper | Result |
|--------|--------|
| `all_telemetry_wildcard()` | `zensight/v1/*/telemetry/**` |
| `all_state_wildcard()` | `zensight/v1/*/state/**` (the whole fleet state plane) |
| `all_health_wildcard()` | `zensight/v1/*/state/*/health` |
| `all_errors_wildcard()` | `zensight/v1/*/state/*/errors` |
| `all_liveness_wildcard()` | `zensight/v1/*/state/*/device/*/liveness` |
| `all_sensors_wildcard()` | `zensight/v1/*/state/*/sensor` (registration docs) |
| `all_alerts_wildcard()` | `zensight/v1/*/state/*/alert/*` |
| `all_evidence_wildcard()` | `zensight/v1/*/state/*/evidence/**` |
| `all_name_evidence_wildcard()` | `zensight/v1/*/state/*/evidence/names/*` |

State is its own late-joiner seed: a plain GET on any of these state selectors is
answered storage-shaped (one reply per concrete key) by producer-side queryables
and/or a router latest-value storage.

## `@rpc` — runtime control (request/reply, no publications)

Commands do not exist in v1: writes are GETs on `<topic>/set`, reads on
`<topic>`. Caller-side selectors (`keyexpr.rs`):

| Helper | Result |
|--------|--------|
| `fleet_rpc_key(producer, procedure)` | `zensight/v1/*/@rpc/<producer>/<procedure>` (use query target `All`) |
| `fleet_command_key(producer, topic)` | `zensight/v1/*/@rpc/<producer>/<topic>/set` |
| `origin_rpc_key(&RemoteOrigin, producer, procedure)` | `zensight/v1/<origin>/@rpc/<producer>/<procedure>` (single host) |

**The origin's kind is a type** (#485, RFC 08 §1.1 amendment B). `origin_rpc_key`
takes a parsed [`zenkey::RemoteOrigin`], not a `&str`, because the distinction
between *an origin I own* and *an origin I address* shipped as a bug three
times: the local-host builders below are exactly right for a sensor serving its
own queryable and exactly wrong for the GUI calling someone else's, and the
failure mode — a timeout, at runtime, in one view — is the worst possible way
to find out. Origins parse once where they enter the GUI
(`DeviceId::remote_origin`); a malformed one is `None`, so the caller falls back
to the fleet selector instead of building a key aimed at nobody. (That fallback
used to be silent: the builder hand-spelled a matches-nothing key, which is how
every fixture-built drill-down in the test suite was aimed at nobody without a
single test noticing.)

Topic keys for the **local** host (`command.rs`). Every helper takes the bare
**producer name** (`"netlink"`, `"logs"`, …) — the legacy `zensight/<protocol>` prefix
form is gone with `key_prefix` (#465), and the origin is derived, never passed:

| Helper | Result |
|--------|--------|
| `command_key(producer, topic)` | `zensight/v1/<origin>/@rpc/<producer>/<topic>/set` (write) |
| `status_key(producer, topic)` | `zensight/v1/<origin>/@rpc/<producer>/<topic>` (read) |
| `query_key(producer, topic)` | same as `status_key` — reads are reads (on-demand bulk detail) |
| `artifact_request_key(producer)` | `…/@rpc/<producer>/artifact/request` |
| `artifact_status_key(producer)` | `…/@rpc/<producer>/artifact/status` |
| `artifact_cancel_key(producer)` | `…/@rpc/<producer>/artifact/cancel` (`?id=<ulid>`) |
| `artifact_blob_prefix(producer)` | `zensight/v1/<origin>/@blob/artifact` (Tier-1 delivery) |
| `artifact_store_prefix(producer)` | `zensight/v1/<origin>/@blob/store` (Tier-2 chunks) |
| `artifact_tree_prefix(prefix)` | `zensight/v1/<origin>/@blob/tree` (Tier-2 index) |

Since zenkey 0.4 the `@blob` tiers are also **declared in the registry**
(`[[blob]]` entries, RFC 08 §2 v1.8): every artifact-capable producer's TOML
names the tiers it serves, so `introspect` slices answer "does this producer
serve blobs?" per producer and `zenctl` can see the blob surface without
probing. The generated app-level module is `zensight_common::registry::blob`
(`Tier::{Artifact,Tree,Store}`, typed key builders over
`zenkey::ContentHash`, and `Tier::probe()` → `BlobProbePrefix`, the RFC 07
§2.5 probe form that deliberately does not convert into a fetchable key). The
three prefix helpers above remain the producer-side entry points.

## Producer-side keys — `zensight_keyspace::V1Context`

Producers (sensors) build their own keys through `V1Context` (re-exported as
`zensight_sensor_core::v1`): `from_prefix`, `telemetry_prefix()`
(`zensight/v1/<origin>/telemetry/<producer>`), `state_key(&[…])`, `health_key`,
`errors_key`, `sensor_info_key`, `evidence_self_key`, `evidence_device_key`,
`device_liveness_key`, `alive_key`, `device_alive_key`, `rpc_key(&[…])`,
`media_video_key` / `media_key(&[…])`, and `blob_prefix(tier)`. Registry
violations are build errors (`zensight-common/registry/*.toml`).

Two producer-side evidence builders live in `keyexpr.rs` because non-sensor code
uses them too (both mint the **local** origin):

| Helper | Result |
|--------|--------|
| `host_evidence_key(sensor, device)` | `zensight/v1/<local-origin>/state/<sensor>/evidence/device/<device-slug>` |
| `name_observation_key(sensor, ip_slug)` | `zensight/v1/<local-origin>/state/<sensor>/evidence/names/<ip-slug>` |

## The catalog — `zensight/v1/@catalog/…`

The identity catalog (the correlator service) publishes under the verbatim
`@catalog` origin (`keyexpr.rs`):

| Helper | Result |
|--------|--------|
| `entity_key(entity_id)` | `zensight/v1/@catalog/state/entity/<entity_id>` |
| `all_entity_wildcard()` | `zensight/v1/@catalog/state/entity/*` |
| `entities_query_key()` | `zensight/v1/@catalog/state/entity/*` (the seed IS the state selector) |
| `alias_key(old_id)` | `zensight/v1/@catalog/state/alias/<old-id>` |
| `names_query_key()` | `zensight/v1/@catalog/@rpc/names` (selector `?ip=<addr>`) |
| `pdns_key(ip)` | `zensight/v1/@catalog/state/pdns/<ip-slug>` |
| `all_pdns_wildcard()` | `zensight/v1/@catalog/state/pdns/**` |
| `correlator_alive_key()` | `zensight/v1/@catalog/state/alive` (owner presence) |
| `catalog_claim_key(zid)` | `zensight/v1/@catalog/state/claim/<zid>` (ownership claim) |
| `catalog_claims_wildcard()` | `zensight/v1/@catalog/state/claim/*` (election set) |

Because `@catalog` is a verbatim chunk, the `*`-origin fleet selectors above never
match it — catalog state needs its own subscriber/storage.

## `@media` — opaque live video

| Helper | Result |
|--------|--------|
| `media_video_key(&origin, stream, codec, tier)` | `zensight/v1/<origin>/@media/parallax/<stream>/video/<codec>/<tier>` |
| `media_preview_key(&origin, stream)` | `zensight/v1/<origin>/@media/parallax/<stream>/preview/jpeg` |

Media samples are opaque encoded access units (no `TelemetryPoint`/`Format`
envelope); stream *control* rides the `@rpc` plane with the `stream` /
`stream/set` / `streams` procedures.

**The origin is a type, and there is no wildcard (#649.)** Both take a parsed
`zenkey::RemoteOrigin`, like `origin_rpc_key` above — but here the reason is
not only the own-vs-other confusion the `@rpc` fix was about. On a bulk plane a
`*` origin is a *cost* bug: RFC 07 §3 says a consumer that cannot name an
origin still MUST NOT fan out across origins on `@media` or `@blob`, because
every matching holder ships the full payload and Zenoh cannot cancel remote
replies in flight. One camera named `cam0` per host meant N× the video on one
viewer's wire. `zenkey-build` emits no wildcard selector for this plane at all,
for the same reason, and the string spelling these helpers used to accept is
gone rather than typed.

**They are parallax-only, and that is in the signature.** `parallax.toml` is
the registry's only `[[media]]` declarer, so `protocol` had exactly one legal
production value and is gone too. A producer that publishes media without a
registry entry uses `Publisher::raw_media_publisher`, which takes any key
string by design — there is no builder for such a key, and there should not be
one until the producer declares its `[[media]]`.

The third parameter of `media_video_key` is a **tier** — a bandwidth rung of
the ladder, subscribed to exactly (keyspace v1.3). It is not an H.264 coding
profile, which is what this table used to call it.

## See also

- [`../docs/KEYSPACE.md`](../../docs/KEYSPACE.md) — deployed profile;
  [`../docs/rfcs/keyspace-v2/`](../../docs/rfcs/keyspace-v2/00-index.md) — normative spec.
- [Data model](data-model.md) and [Identity, evidence & entities](identity-evidence.md).
