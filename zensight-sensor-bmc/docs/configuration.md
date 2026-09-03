# Configuration

JSON5, loaded with `--config`. See [`configs/bmc.json5`](../../configs/bmc.json5)
for a worked file; a test loads it, so it cannot rot.

## `bmc`

| Field | Type | Notes |
|---|---|---|
| `source` | string? | The reporting host. Defaults to this machine's hostname — **never** an endpoint address (#885). |
| `interval_secs` | u64 | Sweep cadence, default 60. Floor 10 s, and the floor cannot be configured away. |
| `timeout_secs` | u64 | Per request, default 10. Must be shorter than the interval. |
| `max_concurrent` | usize | In-flight requests across all endpoints, default 4. |
| `evidence` | bool | Publish a third-party identity claim per chassis, default true. |
| `endpoints` | array | See below. An empty list is a no-op, not an error. |
| `alerts` | object | See [`assertions.md`](assertions.md). |

## `endpoints[]`

| Field | Type | Notes |
|---|---|---|
| `name` | string | The key chunk and the alert label. Two endpoints sharing one is refused: they would silently overwrite each other's series. |
| `address` | string | `host` or `host:port`. Not a URL — the scheme is the transport's. |
| `transport` | enum | `redfish` (default) or `ipmi`. |
| `username` / `password` | string | The password goes through the `secret` indirection (`${ENV}` / `file:`) and is resolved **before** the runner starts, so a missing secret fails at start rather than as a 401 every minute. |
| `interval_secs` / `timeout_secs` | u64? | Per-chassis overrides. |
| `ca_file` | string? | PEM bundle for the CA that signed this BMC's certificate. |
| `insecure` | bool | Turn verification off for this endpoint. |
| `enabled` | bool | Park a chassis without deleting its configuration. |

## The TLS decision, stated plainly

A BMC ships a self-signed certificate out of the factory. Refusing to run
against one would push operators to a worse workaround, so both escape hatches
exist — and they are **not** equivalent:

- **`ca_file`** is the right one. Point it at the CA that signed the BMC's
  certificate and verification stays on.
- **`insecure`** is the honest-but-loud one. The endpoint is not
  authenticated, and the sensor says so in a warning at **every** boot, naming
  the chassis and the address. It is per endpoint and never implied.

Setting both is refused at startup: `insecure` turns verification off
entirely, so the CA would never be consulted, and the consequence of that
contradiction is invisible at runtime.

A pinned certificate fingerprint is **not** implemented. reqwest 0.13 does not
expose a custom verifier without dropping to `hyper` + `tokio-rustls`, and
`ca_file` meets the requirement; saying so here is better than half-building it.

## Startup refusals

`validate()` reports **every** problem at once — reporting the first and
stopping means an operator fixes one line, runs it again, and finds the next.
It refuses:

- a duplicate or empty endpoint name, a missing address or username;
- `timeout >= interval` for any endpoint, naming what to change ("raise the
  interval or lower the timeout"). A poll that can outlive its own tick is
  queued, not bounded;
- an `ipmi` endpoint, naming the build flag **and** naming Redfish as the
  working alternative. A transport this build cannot speak is refused here,
  not discovered later as an endpoint that is permanently down — a check that
  did not run is not evidence about the target;
- `ca_file` together with `insecure`.

A **disabled** endpoint is skipped rather than validated, which is what makes
`enabled: false` a way to park a chassis rather than delete it.

## The `ipmi` feature

Off by default, the same rule as probe's `icmp` and sysinfo's `nvml`. It is
currently a **flag without a client**: the config shape, the startup refusal
and the CI leg exist so a protocol client lands into a slot that is already
shaped and type-checked. Either way an `ipmi` endpoint is refused at startup
with a message that says which of the two situations you are in.
