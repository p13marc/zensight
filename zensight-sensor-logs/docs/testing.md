# Testing the logs sensor

Alongside the inline unit tests (parser, framing, multiline, cursor, SLO math,
novelty, templates), the crate has an **integration harness** (#548) at
`tests/` that exercises the real socket→intake→ring→query paths end-to-end
over localhost, with no external services and no root.

## Harness (`tests/harness/mod.rs`)

- `isolated_config()` — a Zenoh config with multicast+gossip scouting off, so a
  test never joins a live fleet on the same host (RFC 09 §0.1).
- `RigBuilder::{udp,tcp,unix}(...)` → `LogRig` — starts `receiver::start_listeners`
  for one listener, spawns the intake bridge (filter → uid →
  `to_telemetry_point` → `LogRecord::from_point` → `query::push`, faithful to
  `main.rs`) and the `@rpc/logs/events` queryable on an in-process session,
  under a unique per-rig producer prefix so parallel rigs don't share keys.
  `.no_drain()` parks the intake so the channel back-pressures (drop accounting);
  `.overflow(...)` selects the policy; `.channel_capacity(n)` sizes the intake
  queue and `.collapse(window)` enables ingest repeat collapse (#546).
- `LogRig::events(params)` / `events_until(min, deadline)` — GET the events
  queryable; `serve_filters()` + `set_filter(cmd)` drive the live
  `filter`/`filter/set` path.
- Senders: `send_udp`, `send_tcp_octet` (RFC 6587 octet-counting),
  `tcp_connect`+`send_line` (LF framing, connection held open for idle-flush),
  `send_unix`.

## Coverage (`tests/e2e.rs`)

UDP/TCP-octet/Unix round-trips; `events` `since`/`max` selectors; backpressure
(a parked channel under `DropNewest` counts drops); a larger `channel_capacity`
measurably reducing drops for the same burst (#546); ingest repeat collapse
folding a burst into one `repeat_count` record (#546); the multiline `select!`
idle-flush arm; and the dynamic-filter live path (`filter/set` narrows the ring).

The harness is the named safety net for the durable-store (#544), TLS (#550),
and file-source (#549) work — each adds its scenarios here.

Note: `journald` live reads are not exercised in CI (they need journal access);
that path stays `#[ignore]`d / env-gated. A joined multiline record over a
`<PRI>` stream currently fails to re-parse (#584) — the idle-flush test asserts
a single buffered line pending that fix.
