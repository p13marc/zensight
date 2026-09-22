> **Design doc — proposal, not yet implemented.** Written 2026-09-20 against master
> `77d69946` (0.14.0). Backward compatibility is explicitly allowed to break,
> including the wire shape of `TelemetryPoint` and how the GUI registers to data.
> For the as-built GUI see [`zensight/docs/views.md`](../../zensight/docs/views.md);
> for the keyspace contract see [`docs/KEYSPACE.md`](../KEYSPACE.md).
>
> **Tracked as [#1253](https://git.marcpardo.eu/marcpardo/zensight/issues/1253)** (epic, milestone 0.23.0). The only child being worked is the
> system-view test, [#1254](https://git.marcpardo.eu/marcpardo/zensight/issues/1254); the phases (#1255–#1262) are blocked on it.

# Dynamic views: the GUI renders what the bus describes — Analysis, Architecture & Proposal

*Status: proposal for review, revised the same day for embedded scripting
(Rhai) in place of any home-grown DSL. Prompted by: "GUI is too static. I think
we have to think of a plugin system."*

This document measures how static the GUI actually is and where the coupling
lives (§1), inventories what the bus already says about itself that the GUI does
not use (§2), surveys how the industry makes views data-driven (§3), weighs the
five plugin shapes against each other (§4), and proposes an architecture (§5), a
view-definition format with worked examples (§6), a migration (§7) and a phased
plan (§8), then the zenkey dependencies (§11). Sources are in §12.

---

## 0. Executive summary

**The questions, answered up front:**

1. **Is the GUI static? Yes, and measurably — in three layers, not one.** The
   coupling everyone sees is the view layer: `Protocol::*` is matched by hand in
   8 files, 82 of the 434 `Message` variants carry one protocol's data, and 7
   per-protocol `*DetailState` structs sit inside `DeviceDetailState`. But the
   binding layer is **intake**: `refine_key` drops any producer that is not one of
   the 18 compiled `AnySubject` variants, and `Protocol::from_str` is a closed
   `match`. And the deepest layer is **the wire**: `TelemetryPoint` serialises a
   `protocol: Protocol` field in every point, so a sensor the GUI was not compiled
   with does not merely render badly — its documents are **dropped before any view
   sees them**. Adding BMC, PVE and probe views this month each meant editing two
   `match` statements, and #1128's eight unreachable tabs was a hard-coded list
   nobody updated. (§1)

2. **Do we need a plugin system? We need a *plugin surface*, and the right one is
   declarative, not code.** Native `.so` plugins are not realistic against Iced
   (`Element<'a, Message, Theme, Renderer>` has no ABI); WASM plugins are
   feasible but solve the wrong problem first. Zed — the flagship Rust app with
   a WASM+WIT extension system — reached the same conclusion: extensions cannot
   render UI, and the planned path is *"a declarative protocol: the extension
   describes what to render as data, Zed renders it natively."* The industry's
   data-driven dashboards (Netdata, Grafana, Perses, JSON Forms, Home Assistant)
   all separate a **data schema** from a **presentation schema** and generate a
   default presentation when none is given. (§3, §4)

3. **The bus already carries the data schema — we just do not read it.**
   `@rpc/<producer>/introspect` returns a `RegistrySlice` whose every subject
   declares `path` (with `{vars}`), `kind` (gauge/counter/text/bool), `unit`
   (UCUM), `cardinality`, `rate`, `ttl_s`, `description`; `describe` returns
   JSON Schemas for every document and reply type. `docs/KEYSPACE.md` says a
   consumer can go *"wire key → subject → type → schema → value with nothing
   producer-specific compiled in"* — and `zenkey-fleet`, **which the GUI already
   links**, implements exactly that in `SchemaStore` + `decode_sample`. The GUI
   fetches `introspect` fleet-wide today and uses it only for the Fleet view. (§2)

4. **What we do:** (a) make intake producer-agnostic on the `zenkey-fleet` path
   and drop `Protocol` from the wire — one breaking change; (b) derive a
   **family model** from the slice (subjects grouped by their `{vars}` = rows,
   sibling subjects = columns, `kind`/`unit` = formatting) and render a default
   view for *any* producer from it; (c) add a **view definition** —
   `views.toml` beside `registry.toml`, served at `@rpc/<producer>/views` — that
   refines the default the way a JSON-Forms UI schema refines a data schema;
   (d) keep hand-written views as bespoke renderers registered by producer name
   over the same model, and delete the 82 messages, 7 detail states and most of
   the 97 typed reply structs; (e) derive the GUI's **subscription** from the
   visible definition — each panel's `scope` × its fields, statically — so the
   firehose `v1/*/telemetry/**` becomes the fallback, not the default (§5.7).
   **Presentation logic** — a row label, a sort key,
   a visibility — is an embedded, sandboxed **Rhai** snippet beside the document,
   never an expression *in* the document, so the format stays a closed
   vocabulary and no DSL is invented. WASM is not planned. (§5–§8)

5. **Proof of concept is cheap and already half-built.** `generic_device_view`
   exists; the slice is already fetched; the Bus explorer already renders any
   payload as JSON. Phase 0 is: give `generic_device_view` the slice. That alone
   would have rendered bmc, pve, container and probe on the day their sensors
   landed. Before any phase: one **system-view test** (#1254) states the
   requirement in six gates — intake, model, default view, honesty, definition +
   scripts, derived subscription — as a `#[should_panic(expected = "GATE
   1/intake")]` ratchet whose expected string is the epic's status. It is red at
   gate 1 today, by design, and it decides whether the rest is built. (§8)

---

## 1. The problem, measured

"Static" is not one thing. Three layers gate whether a producer can be shown at
all, and they must be fixed bottom-up or the upper fixes are unreachable.

### 1.1 The wire: `Protocol` is inside every payload

```rust
// zensight-common/src/telemetry.rs
pub struct TelemetryPoint {
    pub timestamp: i64,
    pub source: String,
    pub protocol: Protocol,     // ← a closed enum, serialised into every point
    pub metric: String,
    pub value: TelemetryValue,
}
#[serde(rename_all = "lowercase")]
pub enum Protocol { Snmp, Logs, Gnmi, Netflow, Opcua, Modbus, Sysinfo, Netlink,
                    Netring, Systemd, Parallax, Hostspec, Bmc, Pve, Container,
                    Probe, Historian }   // telemetry.rs:166 — 17 variants
```

A sensor this GUI was not compiled with serialises a `protocol` the GUI's serde
cannot deserialise. The point fails to decode and is dropped. This is a wire-level
coupling, and no view-layer change can route around it. The memory note *"several
sensors at once must stack (closed `Protocol` enum)"* is this fact seen from the
build side.

The producer's identity is **already in the key** — chunk 4 of every host-origin
key, `v1/<origin>/<class>/<producer>/…` — so the payload field is redundant with
information every consumer has before it decodes anything.

### 1.2 Intake: an unregistered producer does not exist

```rust
// zensight-common/src/keyexpr.rs:49
/// `None` when the key is not a v1 data key, or when the subject is not
/// registered — "a subject that is not registered does not exist".
pub fn refine_key(key: &str) -> Option<(StructuralKey<'_>, String, AnySubject)> {
    …
    let subject = registry::parse_subject(&name, class, &parsed.subject)?;
```

`AnySubject` has **18 variants, one per compiled producer**. Every `state/**`
document from a producer outside that set returns `None` from `refine_key` and is
dropped in `decode_sample` (`zensight/src/subscription.rs:987`). Telemetry takes an
early return before `refine_key` and *would* land — except for §1.1 — and then
`DeviceId::from_telemetry` (`message.rs:52`) needs `Protocol::from_str`
(`telemetry.rs:272`), the closed match.

That rule — *"a subject that is not registered does not exist"* — is correct for
a **producer**: it must not publish what it did not declare. It is the wrong rule
for a **consumer**: the GUI's job is to show what the fleet publishes, and the
fleet can legitimately contain a producer the GUI has never heard of.

### 1.3 The view layer: hand-written dispatch, per protocol

| Coupling | Count | Where |
|---|---:|---|
| `Protocol::*` matched by hand | 8 files | `overview/mod.rs` (13 sites), `specialized/mod.rs` (8), `icons/mod.rs`, `entity.rs`, per-view files |
| `Message` variants carrying one protocol's data | **82** of 434 | Netring 20, Systemd 17, Snmp 15, Parallax 13, Netlink 5, Netflow 4, Sysinfo 2, Hostspec 2, Syslog 1 |
| Per-protocol detail state inside `DeviceDetailState` | 7 structs | `netlink_detail`, `netring_detail`, `systemd_detail`, `netflow_detail`, `parallax_detail`, `sysinfo_detail`, `snmp_detail` (`device.rs:72–100`) |
| Typed reply structs the GUI decodes | 97 | `zensight-common/src/schema.rs` `.json::<T>` list |
| Per-view `Fetch<T>` request/response message pairs | ~40 | e.g. `FetchSysinfoProcesses` / `SysinfoProcessesReceived` (`message.rs:689`) |
| `app.rs` | 12 251 lines | the `update` loop for all of the above |

Each new sensor with a view touches: `Protocol` (common), `subscription.rs`
(decode arm), `message.rs` (Fetch/Received pair), `app.rs` (update arms + query
builder), `specialized/mod.rs` and `overview/mod.rs` (dispatch), `icons/mod.rs`.
The three views added this month (#1126, #1127, #1128) each did exactly that. And
the symptom that motivated #1128 — `render_protocol_tabs` iterating a frozen array
of nine while `render_protocol_overview` had an arm for every protocol, so eight
arms were unreachable code — is the *shape* of this problem: two lists that must
agree, maintained by hand, with nothing checking.

### 1.4 What is *not* static, and must stay

Three things already work for any producer and are the right kind of generic:

- **Common state families** — `health`, `errors`, `sensor`, `alert`,
  `evidence_*`, `entity`, `alias`, `incident`, `silence`, `edge`, `pdns`
  (`common = "…"` in every registry, 16× each for the first five). The GUI
  decodes these through `CommonState`, not per producer. The Sensors card, the
  Alerts view, the Fleet view and the catalog surfaces are producer-agnostic
  *today* because these families are.
- **The Bus explorer** (`view/explorer/inspector.rs:75`) decodes any payload to
  `serde_json::Value` and renders it. It is the proof that a schema-less generic
  renderer already exists in the binary.
- **`generic_device_view`** (`device.rs:1003`) — header, chart, metrics list,
  from a bare `HashMap<String, TelemetryPoint>`. It renders *something* for any
  device. It has no idea what the metrics are.

The design below is about making the second and third of these **registry-aware**
and making the first the *only* hand-written intake path.

---

## 2. What the bus already says about itself

This is the asset the proposal is built on. Every item below exists on master
today and is exercised by CI (the conformance gate runs `zenkey-fleet`'s judges
against a live fleet).

### 2.1 `introspect` → `RegistrySlice` (RFC 08 §6)

Every producer serves its compiled slice at `@rpc/<producer>/introspect`;
`@catalog` serves its own. The slice is a **runtime type** (`zenkey::RegistrySlice`),
not a compile-time enum, and it carries per subject:

| `SubjectDecl` field | Meaning for a view |
|---|---|
| `path` — e.g. `{chassis}/thermal/{sensor}/celsius` | the family and its **instance identity** (`{vars}`) |
| `class` — telemetry / state / events | streamed value, document, or append-only record |
| `type_name` | which JSON Schema in `describe` describes the payload |
| `kind` — `gauge` / `counter` / `text` / `bool` | absolute vs rate; number vs label |
| `unit` — UCUM: `By`, `W`, `Cel`, `s`, `ms`, `1`, `%` | formatting and axis |
| `cardinality` | how many instances to expect — table vs list vs "top N" |
| `rate`, `ttl_s`, `qos` | freshness expectations; staleness rendering |
| `description` | the tooltip / help text (what OpenMetrics `HELP` is for) |
| `common` | one of the cross-producer families in §1.4 |

and per procedure: `path`, `kind` (read/write), `request`, `reply` (type names),
`idempotent`, `fanout`. The GUI fetches this **fleet-wide already**
(`app.rs:10246`, `declare_repeating` over `*/@rpc/*/introspect`) and uses it for
one thing: the Fleet view's drift diff.

### 2.2 `describe` → `SchemaSet` (RFC 08 §7)

JSON Schemas for every type in `types.toml` — 118 `json-schema` entries — served
by every sensor via its runner and by the catalog via the correlator,
`build_verified` at start-up so a gap aborts rather than serving a partial table.
Every put stamps the sample's `Encoding`, so a consumer resolves the payload
format from metadata before sniffing (#1148).

### 2.3 `zenkey-fleet` already decodes without compiled enums

`docs/KEYSPACE.md` (§"Type table + self-description"):

> A consumer goes wire key → subject → type → schema → value with nothing
> producer-specific compiled in (`zenkey-fleet`'s `SchemaStore`/`decode_sample`).

Verified in the vendored `zenkey-fleet-0.13.0/src/model/decode.rs`:
`SchemaStore::{new, bounded, schema_for, set_for, decode, encode, register_decoder,
forget}` with a bounded per-producer cache, and
`decode_sample(…, slices: Option<&SliceSet>, …) -> DecodedSample { type_name,
rendering, verdict, decode_error }`. The GUI links `zenkey-fleet`
(`zensight/Cargo.toml:20`) for the fleet view. **The producer-agnostic intake the
GUI needs is a dependency it already has.**

### 2.4 The design system is the "host config"

Colours may only be constructed in `theme.rs`/`tokens.rs`/`components/` (CI
guard); the type scale and spacing are tokens with a CI ratchet
(`zensight/docs/design-system.md`). This is precisely the contract Adaptive Cards
call *HostConfig*: the thing that describes a view says **what**, the host owns
**how it looks**. A declarative view that could name a colour or a pixel size would
be a regression; one that names a *role* ("critical", "muted", "emphasis") and
lets the tokens resolve it is what the guard already enforces for Rust code.

### 2.5 Existing data-driven components

`components::limit_table` (#1127) takes `(reading, warning, critical, present)`
rows and holds three honesty rules (absent ≠ 0, unmetered ≠ 0, no limit → no
verdict). `data_table`, `gauge`, `sparkline`, `progress_bar`, `status_led` and the
verdict chip are all data-in, widgets-out. A declarative renderer has most of its
target vocabulary already built.

---

## 3. What the industry does

Every mature system that renders unknown telemetry has arrived at the same
two-layer shape: a **data schema** the source declares, and a **presentation
schema** that is optional and generated when absent.

### 3.1 Netdata — chart templates and autogen

The closest analogue to our registry. A collector's `charts.yaml` declares per
chart: `context`, `family`, `units`, `type` (line/area/stacked/heatmap),
`algorithm` (`absolute` | `incremental`), `priority`, `dimensions`, and
`instances.by_labels` — *"the engine creates one chart instance per unique
combination of the selected … label values."* Two behaviours are directly
reusable:

- **Instance identity from labels.** Our `{vars}` in a subject path are exactly
  Netdata's `by_labels`: `{chassis}/psu/{psu}/input_watts` yields one row per
  `(chassis, psu)`.
- **Autogen for unmatched metrics.** *"When `engine.autogen.enabled: true`,
  unmatched metrics receive automatic charts"* with `algorithm` *"resolved per
  dimension from the matched series kind"* — `incremental` for counters,
  `absolute` for gauges. That is our `kind` field doing the same job. Netdata
  ships every collector with a template *and* renders the ones that lack one.

### 3.2 Grafana — data frames and field config

Every data source is reconciled into a **data frame**: *"a collection of fields
organized as columns. Each field … consists of a collection of values and
metadata, such as units, scaling, and so on."* Panels consume frames, never raw
source data. Presentation is a per-field **config**: unit (from a catalogue, or
custom), min/max, decimals, display name templated from labels
(`${__field.labels.X}`), colour scheme, no-value text; thresholds and value
mappings are separate groups; **overrides** target fields by name, regex, type or
query. Grafana 10+ moved dashboards onto **Scenes**, *"a state system and a
component system in one where you declare the state of the visualizations you
want."* The lesson: one intermediate model, presentation as metadata on it, and
overrides as a matcher + config, not as code.

### 3.3 Perses — dashboards as a `kind`/`spec` document

CNCF's declarative dashboard spec: top-level `panels` (a map), `layouts`
(`kind: "Grid"` with `x/y/width/height`), `variables`, `datasources`, `duration`;
each panel is `kind: "Panel"` with `spec.display`, `spec.plugin.{kind, spec}`
(e.g. `TimeSeriesChart`) and `spec.queries`; panels are referenced by JSON pointer
`$ref: "#/spec/panels/<key>"`. Small, regular, versioned. A good template for a
`views.toml` vocabulary that does not grow into a programming language.

### 3.4 JSON Forms — data schema vs UI schema

*"Data schema and UI schema are maintained as two separate JSON objects."* The UI
schema is `Control`s and `Layout`s (`VerticalLayout`, `HorizontalLayout`, `Group`,
`Categorization`) plus `Rule`s (SHOW/HIDE/ENABLE/DISABLE on a condition); a
`Control`'s `scope` is a JSON pointer into the data schema. And the decisive
property: *"If you provide no UI schema to JSON Forms it'll generate one."* Our
`SchemaSet` is a data schema; `views.toml` is the UI schema; the generated default
is the registry-driven view.

### 3.5 Home Assistant — `device_class`

A small semantic enum on an entity (`temperature`, `power`, `battery`, `motion`,
…) from which the frontend chooses icon, unit, state text ("Open/Closed" rather
than "On/Off") and colour, with an auto-generated dashboard that lists every
entity and community cards (`auto-entities`) that filter on it. Our `kind` + `unit`
+ `common` already carry most of this; a tiny `semantic` hint (see §6.4) closes
the gap without inventing a vocabulary.

### 3.6 Adaptive Cards — a versioned schema and a host config

*"A platform-agnostic, JSON-based UI snippet … the card gets sent to and rendered
by a host application"*, with a **HostConfig** that *"allows the host application
to specify font sizes, colors, spacing"*. The card says what; the host says how
it looks. That is our design-system guard, formalised.

### 3.7 OpenTelemetry & OpenMetrics — units and metadata

OTel metrics use **UCUM**: `By`, `s`, `Cel`, `1` for dimensionless, `{request}`
for counts; instruments are counter / updowncounter / gauge / histogram; *"metrics
that have their units included in OpenTelemetry metadata SHOULD NOT include the
units in the metric name."* OpenMetrics carries `TYPE`, `UNIT`, `HELP` per family
— HELP *"SHOULD be short enough to be used as a tooltip."* Our registry's `unit`
vocabulary (`By W Cel s ms 1 %`) is already UCUM, and `description` is HELP. The
one instrument we lack is **histogram** — open issue #1151 — which a generic view
would want for latency families.

### 3.8 Zed — WASM extensions that cannot draw

Zed's extensions are WASM components with WIT interfaces, sandboxed in wasmtime.
They provide languages, themes, debuggers, MCP servers, slash commands — and
**cannot render UI**: *"extensions run as WASM in a Wasmtime sandbox and have zero
access to GPUI internals — no GPU context, no element tree, no window handle."*
Their stated direction: *"The realistic path isn't 'expose GPUI/OpenGL to
extensions.' It's a declarative protocol: the extension describes what to render
as data, Zed renders it natively through GPUI at full speed."* This is the most
relevant single data point for a Rust GUI: the flagship WASM-extension editor
concluded that the extension surface for UI is a declarative tree, and that WASM
is how you *produce* one, not how you *draw* one.

### 3.9 WASM component model in a Rust host

Sy Brand's walkthrough (WIT world, `wasmtime::component::bindgen!`, host trait
implementations, `add_to_linker`) confirms the ergonomics are good and the sandbox
is real — *"memory sharing is prohibited between host and guest"* — with the
known cost: *"applications with latency constraints that would prefer to pass
shared buffers of data around for plugins to operate on would need to consider
alternative options."* For a view that receives a few hundred metrics per device
this is fine; for the traffic-matrix or a 10 k-row bus-explorer tree it is not.

### 3.10 Embedded scripting — Rhai, Starlark, Rune

When a declarative document needs *some* logic, the alternatives to inventing an
expression language are embedded interpreters designed for hosts. **Rhai** is
built for Rust hosts: values cross the boundary as `Dynamic`, host functions and
getters are registered explicitly, and its stated guarantee is *"Rhai is designed
to not bring down the host system, regardless of what a script may do to it."*
Its safety chapter enumerates the vectors it caps — memory, CPU, time, stack,
overflow — with `Engine::set_max_operations` (one operation ≈ *"one expression
node, loading one variable/constant, one operator call, one iteration of a loop,
or one function call"*) and `Engine::on_progress(|ops| …) -> Option<Dynamic>`,
which terminates the script with `EvalAltResult::ErrorTerminated` when it
returns `Some` — the documented way to enforce a timeout. A default engine has
no file, network or clock access; scripts see only what the host registers.
**Starlark** (Bazel's configuration language) is the principled alternative:
hermetic and deterministic by design, Python-subset syntax, immutable data.
**Rune** is a VM with async and hot reload, heavier than either. All three
produce *values*, not widgets — which is exactly the property the Zed
conclusion (§3.8) calls for.

### 3.11 Declarative engines for Iced

Two exist: **Dampen** (XML → Iced, hot reload, *"NOT ready for production use"*)
and **Glacier UI** (XML → Iced, `.gss` stylesheets, data binding). They prove the
pattern renders fine on Iced. Neither is something to depend on: both are
general-purpose layout languages, and the last thing this GUI wants is a second
styling system beside the tokenised design system. What we need is narrower — a
*telemetry* view vocabulary — and small enough to own.

---

## 4. Options

| | Native `.so` plugin | WASM plugin (draws) | WASM plugin (emits data) | Home-grown expression DSL in the document | **Declarative document + Rhai for logic** |
|---|---|---|---|---|---|
| Feasible on Iced | **No** — no ABI for `Element` | No — same reason | Yes | Yes | **Yes** |
| Unknown producer renders | only if plugin present | only if plugin present | default + plugin | default + doc | **default from slice; refined by document + scripts** |
| Sandbox | none — it is the process | wasmtime | wasmtime | our evaluator (unproven) | **Rhai's documented limits; no ambient I/O** |
| Version coupling | exact Iced + zensight version | WIT version | WIT version | our grammar | **document schema + a registered host API** |
| Testable with `iced_test::simulator` | no | no | yes (tree is data) | yes | **yes; scripts unit-tested on fixture rows** |
| Design-system guard holds | no | no | if the tree names roles | if the grammar has no styles | **yes — scripts return values, the renderer draws** |
| Runs where the GUI runs (flatpak, no toolchain) | needs matching build | needs runtime | needs runtime | yes | **yes — one pure-Rust dependency** |
| Cost to first useful result | high | high | high (needs the tree first) | medium, and it grows | **low — phase 0 is one function; scripts are phase 3** |
| Who maintains the language | — | — | — | **us, forever** | Rhai |
| What Zed did | rejected | rejected | planned | — | the same shape: extension emits data, host draws |

The declarative path is not the compromise option — it is the option every
comparable system converged on, and it is a *prerequisite* for the WASM one: a
WASM view plugin has to emit **some** declarative tree, so that tree has to be
designed first regardless. Designing it as a document that sensors can ship
without WASM gets the payoff years earlier.

**Decision:** declarative, registry-driven views with a small versioned
definition format that carries **no expressions**; presentation logic in
**Rhai** snippets that produce values the renderer consumes (§6.4); hand-written
Rust views retained as bespoke renderers over the same model. **No home-grown
DSL** — the test is *if a construct needs an evaluator, it is out of the
format* — and no WASM: with a sandboxed interpreter for logic and the document
for structure, the component-model host would isolate against a threat
(hostile third-party plugins) this project does not have.

---

## 5. Architecture

### 5.1 Principle

> **The key is the identifier. The registry is the schema. The view definition is
> the presentation. The host owns the look.**

Corollaries that decide the hard cases:

- A producer the GUI has never seen **renders** — a default view from its slice —
  rather than being dropped. Silence is a finding, not a mode.
- Nothing producer-specific is required for a producer to appear. Everything
  producer-specific is *optional* and *additive*: a `views.toml`, a bespoke Rust
  renderer, an icon.
- A view definition may say *what* and *how it relates* (this column is the
  limit for that one); it may not say *how it looks* (no colours, no pixels),
  and it may not **compute**. Presentation logic is a script (§6.4) that returns
  a value; a script may compute a label, a sort key or a visibility, never a
  limit the renderer would colour as the producer's.
- The one hand-written intake path is the common families (§1.4). Everything
  else is decoded by schema.

### 5.2 Layer A — intake becomes producer-agnostic

**Wire (breaking).** `TelemetryPoint` loses `protocol`. The producer is chunk 4 of
the key; every consumer reads it from there (the exporters derive series names
from `point.protocol` today and switch to the key — same information, already in
hand as `Reading.origin`/`subject`). `Protocol` survives in `zensight-common`
only where non-GUI code needs a closed set today (alerts, exporters' naming
tables) and is scheduled for the same treatment in a follow-up; the GUI stops
depending on it entirely.

**`DeviceId`** becomes `{ producer: String, origin: String, source: String }`
(RFC 06 §3: observed devices are subjects, not origins — unchanged). `producer`
is a name, never an enum. Icon lookup is `icons::for_producer(&str)` with a
fallback glyph.

**Decode.** `decode_sample` keeps exactly three shapes of message:

```rust
Message::Telemetry(Reading)                      // unchanged: point + origin + subject tail
Message::Document { origin, producer, subject, type_name, value: serde_json::Value, meta: SampleMeta }
Message::Event    { origin, producer, subject, type_name, value: serde_json::Value }
```

`Document` replaces `SnmpInterfaceTable`, `ParallaxStreamStatus`,
`SnmpDiscoveryReport` and every other per-protocol state message. The decode goes
through `zenkey_fleet::SchemaStore` seeded from `describe` and `SliceSet` from
`introspect` — the path KEYSPACE.md already describes, in the crate the GUI already
links. `refine_key`'s *"does not exist"* rule stays for **producers**; the GUI's
intake uses `parse_key` (structural) and the runtime slice, so an unregistered
producer is a *finding rendered in the UI* ("publishes `foo/bar`, declares no
slice") rather than a dropped sample. The common families keep their typed decode
(`HealthSnapshot`, `Alert`, `HostEntity`, …) because their consumers are
hand-written and that is correct.

**Late joiners.** The slice and schema for a producer are fetched on first sight
of its origin (not at start-up), cached per producer with `SchemaStore::bounded`,
and refreshed when `introspect`'s `version` changes. A sample that arrives before
its slice is held in a small per-producer ring and replayed — the same
late-joiner discipline the tree already applies (#1116, memory: *late-joiner
blindness*).

### 5.3 Layer B — the family model

The model is derived, not written. From a producer's slice:

```
subject paths                              family            vars              fields
{chassis}/thermal/{sensor}/celsius         {chassis}/thermal/{sensor}   chassis,sensor  celsius, upper_warning_c, upper_critical_c
{chassis}/thermal/{sensor}/upper_warning_c
{chassis}/thermal/{sensor}/upper_critical_c
{chassis}/psu/{psu}/input_watts            {chassis}/psu/{psu}          chassis,psu     input_watts, output_watts, capacity_watts, present
{chassis}/reachable                        {chassis}                    chassis         reachable
cluster/quorate                            cluster                      —               quorate, nodes_online, nodes_total, …
```

Rule: **a family is the longest common prefix of paths that share the same
`{vars}`, ending at the last variable; fields are the literal tails.** Instances
are the distinct bindings of the vars seen in live keys (`Subject::vars()` gives
them today). This is Netdata's `by_labels` and Grafana's frame in one derivation,
and it is exactly what the three hand-written views this month did by hand
(`fold()` in `specialized/bmc.rs`, `backup_rows()` in `overview/pve.rs`,
`target_rows()` in `specialized/probe.rs` — each one groups subjects by their vars
into rows and reads siblings as columns).

Each field carries its `SubjectDecl`: `kind` → counters render as rates (Netdata's
`incremental`), gauges absolute, bools as state text, text as labels; `unit` →
formatter and axis; `cardinality` → table when small, "top N + more" when large;
`ttl_s`/`rate` → the staleness rule ("stale after 2× the declared rate" instead of
a per-view constant); `description` → tooltip.

Documents (`class = state`) are modelled by their JSON Schema from `describe`:
objects become fact lists, arrays of objects become tables with the schema's
property names as columns, enums render as chips — the JSON Forms default-UI rule.
Replies from read procedures are the same: `reply` type → schema → table.

History comes from the store as today (`(origin, subject)` series, `SeriesKind`),
so any numeric field gets a sparkline and a chart for free.

### 5.4 Layer C — presentation

**The default renderer** takes a family model and produces, with no definition at
all:

- one **table per family with vars** — rows = instances, columns = fields, sorted
  by instance id, kinds/units formatted, counters as rates, a sparkline per
  numeric cell on hover/expand;
- one **facts panel** for var-less telemetry (`cluster/*`, `system/*`);
- one **document panel** per state family from its schema;
- the **common families** rendered by their existing hand-written views, exactly
  as now.

This is what a new sensor gets on day one. It is what `generic_device_view`
becomes once it reads the slice (§8, phase 0).

**The view definition** (`views.toml`, §6) refines the default: which families
are panels and in what order, titles, which field is graded against which
(`limit` — the `LimitRow` semantics, with the honesty rules), which field is the
row's display name, which fields are hidden or promoted, `top_n`, thresholds that
*the producer* declares (never the GUI), and procedure panels.

**Where definitions come from, in precedence order:**

1. **The producer** — `@rpc/<producer>/views` (a new common read procedure beside
   `introspect`/`describe`, declared in each registry the same way those are, reply
   type `ViewSet`). Shipped as `views.toml` beside `registry.toml` and compiled
   into the binary by `zenkey-build` so it is versioned and linted with the slice
   it describes. A sensor and its view move together.
2. **The GUI's bundled overrides** — for producers whose maintainers have not
   written one, or to patch a bad one without a sensor release.
3. **A bespoke Rust renderer** registered by producer name
   (`registry.bespoke("netring", netring::view)`) — the existing specialized
   views, rewritten to read the family model instead of bespoke messages. It
   *wins* when present, and can *compose* declarative panels inside it (the
   design-system components are the same either way).

**Host owns the look.** A definition names roles, not styles: `severity =
"critical"` resolves through `theme::colors`; `emphasis = true` resolves through
`font::EMPHASIS`. The CI colour guard and type-scale guard apply to the renderer,
which is Rust; the definition has no vocabulary for a pixel or an RGB triple.

### 5.5 Layer D — interaction

- **Read procedures** become one generic call path: `Message::Call { producer,
  procedure, params }` → `Message::Reply { …, value, page: Option<PageSignal> }`,
  rendered by the reply's schema (with `partial`/`next_cursor` from #1147's
  `Page<T>` envelope driving "load more"). `FetchSysinfoProcesses`/
  `SysinfoProcessesReceived` and its ~40 siblings collapse into this.
- **Write procedures** render a form from the `request` type's JSON Schema (JSON
  Forms' whole reason for existing) and submit through the audited seam — the
  request/refusal rendering (#866, #925) is already generic. A definition may
  place a write procedure as an action on a family row (`action = "psu/{psu}/cycle"`),
  gated exactly as today.
- **Pivots** (the #313 identity pivots, alert → logs, unit → systemd) become
  declared `link`s on fields: `link = { view = "logs", filter = { unit = "$unit" } }`
  — Grafana's data links, with the same host-side resolution.

### 5.6 What stays hand-written, deliberately

- The **common families** and the views built on them: Sensors, Alerts, Fleet,
  Incidents, Topology, Security, Expectations, the catalog surfaces. These are
  cross-producer by nature and are the product.
- The **Bus explorer** — it is the escape hatch that shows the truth when a
  definition is wrong.
- **Bespoke renderers** for producers whose view is a product feature:
  netring's traffic matrix and NDR surfaces, parallax's live video, syslog's
  search. They keep their Rust and lose their private message plumbing.
- **The design system.** It becomes *more* load-bearing, not less.

### 5.7 Layer E — the subscription follows the definition

Today the GUI subscribes to the whole telemetry class: `effective_scopes`
(`zensight/src/subscription.rs`) yields `v1/*/telemetry/**` unless the operator
configured a scope, or focus mode (#476) narrowed everything to one origin.
Once a definition says which subjects each panel reads, the GUI knows its data
needs *before* the first sample arrives — and a monitor that fetches everything
to show a tenth of it scales with the fleet, not with the screen.

**Derivation.** For every panel of the visible view: the panel's `scope` with
vars replaced by `*`, joined to each of its `fields` and to each field its Rhai
scripts name — the §6.4 lint already extracts those — with the class taken from
the slice. A pure function `(definitions, slices, current view, focus) →
Vec<String>` beside `effective_scopes`, unit-tested on a fixture producer; its
result feeds `LinkConfig.scope`, whose `Hash` change already restarts the
stream, so the first cut needs no new subscriber plumbing. **No script runs to
decide a subscription**: the set is known at build, so a definition cannot
starve its own view, and the lint can say so.

**Rules.** Focus mode still wins (one origin, everything). An operator-configured
scope still wins (an explicit decision). The derivation replaces only the
empty-scope firehose default. Widening is by navigation: a device's detail
subscribes to that origin's `<producer>/**`, so a subject the slice does not
declare is seen there and rendered as the §5.2 finding; the overview subscribes
to the union of every producer's overview needs. A producer with a slice and no
definition gets `v1/*/telemetry/<producer>/**`; a producer with neither is
discovered through the liveliness subscriber the GUI already holds and gets the
same, with the "no slice" finding. The common families — alerts, entities,
incidents — keep their wildcards; they are nobody's producer.

**What narrowing costs, stated.** An undeclared subject from a *defined*
producer is not fetched on the overview. The GUI's honesty rule is "never drop
what arrives", not "fetch everything"; what a producer publishes beyond its
slice is the Bus explorer's and `zenkey-fleet`'s judges' job (#744). If
navigation churn shows up, the second cut changes the subscriber set without
tearing the session down — the telemetry handles are already a `Vec`.

---

## 6. The view definition — `views.toml` v1 (draft)

Design constraints: TOML (the registry's language, so one toolchain lints both);
`kind`/`spec` regularity (Perses); `scope` by **subject pattern** rather than JSON
pointer, because the family model is keyed by subject; no styling vocabulary;
versioned; a definition that references a subject the slice does not declare is a
build error (the same posture as `types.toml`).

### 6.1 Vocabulary

```toml
[view]
version  = "1"            # of this format
producer = "bmc"
title    = "Out-of-band hardware"
group_by = "{chassis}"    # optional: one card per binding of this var

[[panel]]
kind  = "table" | "facts" | "document" | "chart" | "reply" | "custom"
title = "…"
scope = "<subject pattern up to the last var>"     # the family
# table/facts/chart:
fields   = ["celsius", "upper_warning_c", "upper_critical_c"]   # default: all
label    = "$sensor"                # row display name; default: the vars joined
hide     = ["output_watts"]
top_n    = 20                       # default from cardinality
sort     = { by = "celsius", dir = "desc" }   # a field — or { rhai = "…" } for a key
sparkline = ["celsius"]
[panel.grade]                       # LimitRow semantics; absent ≠ 0, unmetered ≠ 0
reading  = "celsius"
warning  = "upper_warning_c"        # sibling field — the producer's own limit
critical = "upper_critical_c"       # FIELD NAMES ONLY here: no script slot (§6.4)
[panel.stale]                       # override the ttl_s/rate default
after_s = 60
# Presentation logic — Rhai, returning a value (§6.4). No `rule`/`when`
# vocabulary exists: anything conditional is a script, so the format never
# grows an operator.
label  = { rhai = "`${sensor}`" }
show   = { rhai = "row.present != false" }        # bool: hide the row
note   = { rhai = "if !row.reachable { \"last readings — BMC did not answer\" }" }
[panel.format]                      # per-field display value, still typed by decl
duration_secs = { rhai = "fmt_age(row.duration_secs)" }
# document:
schema = "Chassis"                  # type name in describe; default: the subject's type
# reply:
procedure = "chassis/{chassis}"     # read procedure; rendered by its reply schema
page      = true                    # honour Page<T> partial/next_cursor
# links / actions:
[[panel.link]]
field = "$unit"
view  = "systemd"
filter = { unit = "$unit" }
[[panel.action]]                    # write procedure; gated + audited as today
procedure = "outlet/{outlet}/cycle"
label     = "Cycle"
```

`custom` names a bespoke Rust renderer for a family and is how a hand-written
view opts one panel into the declarative layout around it.

### 6.2 Worked example — `bmc`, replacing `specialized/bmc.rs`

```toml
[view]
version = "1"
producer = "bmc"
title = "Out-of-band hardware"
group_by = "{chassis}"

[[panel]]
kind = "facts"
scope = "{chassis}"
fields = ["reachable"]
note = { rhai = "if !row.reachable { \"The BMC did not answer this cycle — readings below are the last it gave\" }" }

[[panel]]
kind = "table"
title = "Temperatures"
scope = "{chassis}/thermal/{sensor}"
label = "$sensor"
[panel.grade]
reading = "celsius"
warning = "upper_warning_c"
critical = "upper_critical_c"

[[panel]]
kind = "table"
title = "Fans"
scope = "{chassis}/fan/{fan}"
# no grade: the BMC publishes no fan threshold, and this file may not invent one

[[panel]]
kind = "table"
title = "Power supplies"
scope = "{chassis}/psu/{psu}"
fields = ["input_watts", "capacity_watts", "present"]
[panel.grade]
reading = "input_watts"
critical = "capacity_watts"
absent   = "present"                # the field whose `false` means "no supply in the bay"
```

Every honesty rule the Rust view encodes (#1127) is expressible: "no fan
threshold" is the *absence* of a `grade`; "absent ≠ 0" is `grade.absent` naming
the presence field; "the limit is the publisher's" is enforced because
`warning`/`critical` may only name **sibling fields**, never literals and never
scripts. The one sentence of logic — the unreachable note — is a Rhai snippet
returning a string or nothing.

### 6.3 Worked example — `pve` backup freshness (`overview/pve.rs`)

```toml
[[panel]]
kind = "table"
title = "Backup freshness"
scope = "guest/{vmid}"            # rows are GUESTS, joined to backup/{vmid} below
join  = "backup/{vmid}"           # left join on the shared var: a guest with no
                                  # backup row is the TOP row, not a missing one
fields = ["running", "backup.age_secs", "backup.ok", "backup.size_change_pct"]
label = "vmid $vmid"
sort = { rhai = "if row.backup == () { -1 } else { -row.backup.age_secs }" }   # never backed up sorts first
label = { rhai = "`vmid ${vmid}`" }
[panel.format]
"backup.age_secs" = { rhai = "if row.backup == () { \"never\" } else { fmt_age(row.backup.age_secs) }" }
[panel.grade]
reading  = "backup.age_secs"
critical = { const = 172800, declared_by = "gui" }   # a literal with provenance: a GUI window, not a producer limit
```

The `join` is the one thing the derivation cannot infer — that two families share
a var by *meaning* — and it is exactly the design decision `backup_rows()` makes in
prose today. The "never backed up sorts first" rule that needed a `missing =
"first"` special case in the closed vocabulary is three tokens of Rhai instead,
and the vocabulary is smaller for it. `declared_by = "gui"` is how a definition
admits a threshold is its own rather than the producer's; the renderer labels it
as such (this is the `EXPIRY_SOON_DAYS` rule from the probe view, made visible) —
it is a **literal with provenance**, not logic, which is why it stays in the
document and not in a script.

### 6.4 Presentation scripts — Rhai

The probe view's `outcome_label()` builds *"timed out after 20.0 s"* from
`timeout` and `duration_ms`; pve's `age_label()` turns seconds into *"3.2d"*;
container's row label is `host/name`. None of these is a threshold, all of them
are presentation, and none fits a closed vocabulary without a `format` construct
with conditionals — which is a DSL by another name. So the document has **no
conditional vocabulary at all**; wherever it needs one, a slot takes
`{ rhai = "…" }`.

**What a script sees.** A fresh `Scope` per evaluation with: `row` (a map of the
family's fields for this instance, siblings from a `join` under their family
name, `()` when absent), the bound vars by name (`chassis`, `vmid`, …), and `decl`
(the slice's `SubjectDecl` for each field: `unit`, `kind`, `description`). Host
functions are registered explicitly and are pure: `fmt_age`, `fmt_bytes`,
`fmt_unit(value, unit)`. `now_ms` is **not** among them — a script must not know
the time; staleness is the renderer's, from `ttl_s`/`rate`.

**What a script returns.** A value the slot's type demands — `label`/`note`/
`format.*` a string or `()`, `show` a bool, `sort` a number or string. The
renderer draws. A script cannot name a colour, a size or a widget, so the
design-system guard holds by construction.

**What a script cannot do.** Produce a limit. `grade.warning`/`critical`/
`absent` accept field names only; there is no script slot, so the renderer never
colours a computed number as the producer's threshold. The doctrine — the limit
is the publisher's — is enforced by the *absence of an API*, not by review.

**Limits, from Rhai's Safety chapter.** `Engine::set_max_operations` (a few
thousand is generous for a row label), `on_progress` returning `Some(())` past a
wall-clock budget so a runaway script terminates with `ErrorTerminated` rather
than stalling a frame, `set_max_call_levels`, string/array/map size caps, and
the `unchecked` feature *not* enabled. No file, network or clock is registered.
A script error renders as the slot's fallback (the field's default display) plus
a visible "view script failed: …" note — a broken view must look broken, never
silently empty.

**Where scripts live and how they are checked.** Inline in the document for
one-liners; a `.rhai` file beside `views.toml` for anything longer, inlined into
the served `ViewSet`. At build, every script is `Engine::compile`d and its AST
walked for variable names, which must be declared fields or vars of the panel's
family — the same "must not lie" posture as the registry lint; that same
extraction is the derived subscription's input (§5.7). At load, compiled
`AST`s are cached per view version. In tests, a script is evaluated against a
fixture row in a plain Rust test, no window needed.

**Why Rhai and not Starlark or a WASM component.** Rhai is built for Rust hosts
(getters, `Dynamic`, serde), one pure-Rust dependency, and its limits are the
documented ones above. Starlark is the principled alternative if hermeticity
must be *provable* rather than arranged (nothing registered → nothing reachable);
because scripts only produce values, the engine is swappable behind the slot
API. A WASM component host isolates memory against hostile code, which is a
threat model this project does not have — sensors are already trusted processes
on the host, and their view scripts ship in the same tree. Keep the option; do
not build it.

### 6.5 The semantic hint

Home Assistant's `device_class` earns its keep with a dozen values. We add one
optional field to the registry, upstream in zenkey (RFC 08 §2, additive):
`semantic = "temperature" | "power" | "bytes" | "duration" | "ratio" | "count" |
"identity" | "state"`. It is derivable from `unit` in most cases and is only
declared where it is not (`ratio` for a `1`-unit gauge that is a fraction, not a
count). The renderer picks the gauge style, the icon and the axis scale from it.
Not required for phase 0–3.

---

## 7. Migration and what breaks

**Breaking (one release, marked `!`):**

- `TelemetryPoint.protocol` removed from the wire. Every sensor and both
  exporters bump together (they already have to, for the closed enum — memory:
  *stacked PRs and the version bump*). Old GUIs decode nothing from new sensors;
  new GUIs decode everything from old sensors (the field is ignored on read for
  one release). Exporters take the producer from the key.
- `DeviceId.protocol: Protocol` → `producer: String`. Every GUI test fixture
  (`DeviceId::fixture(Protocol::X, …)`) becomes `fixture("x", …)`; mechanical.
- The 82 protocol messages, 7 `*DetailState`s and ~40 `Fetch*/*Received` pairs are
  deleted in phase 4. `app.rs` shrinks by the corresponding `update` arms.
  (Done 2026-09-22: #1261's per-producer slices, then #1306's app-wide groups —
  `Message` went from 437 to 178, each view's interactions one typed `Action`
  with a pure `State::update -> Effect`; see `zensight/docs/views.md`,
  "Actions and effects".)
- `specialized/mod.rs` and `overview/mod.rs` lose their `Protocol` matches; the
  tab strip is built from producers seen (#1128 already made it structural).

**Not breaking:** the keyspace, the registries, `introspect`/`describe`, the
store schema, the design system, every hand-written common-family view.

**Registry additions (upstream, additive):** a common `views` read procedure with
reply `ViewSet`; a `ViewSet` entry in `types.toml`; optionally `semantic` on
`SubjectDecl`. `registry.lock` regenerates cleanly (memory: *registry lock needs
the local zenctl*).

---

## 8. Phased plan

Each phase is shippable and leaves the tree no worse than before it.

| Phase | Deliverable | Proves |
|---|---|---|
| **0 — spike** (days) | `generic_device_view` reads the producer's `RegistrySlice` (already fetched) and renders families as tables with units and kinds. No `views.toml`. | A producer with no hand-written view renders something honest. Would have covered bmc/pve/container/probe on day one. |
| **1 — intake** | `Message::Document`/`Event`; decode via `zenkey_fleet::SchemaStore`; `DeviceId.producer: String`; `TelemetryPoint` loses `protocol` (`!`); late-joiner ring for pre-slice samples; unregistered producer → rendered finding. | An unknown producer is *seen*. |
| **2 — model + defaults** | The family derivation (§5.3) as a tested pure function over `RegistrySlice`; default renderers for table/facts/document; common families untouched. | The default view is good enough that a bespoke one is a choice, not a requirement. |
| **3 — definitions + scripts** | `views.toml` v1 with Rhai slots (§6.4), the build lint (document references only declared subjects; every script compiles and names only declared fields), `@rpc/<producer>/views`, GUI precedence (producer → bundled → bespoke). **bmc, pve and probe rewritten declaratively** and their Rust views deleted — the acceptance test is that the simulator tests written for #1126/#1127/#1128 pass unchanged against the declarative renderer. | The format plus a sandboxed value-returning script is expressive enough for real views, and the honesty rules survive. |
| **3b — subscription** | The derived subscription (§5.7): a pure function from the visible definitions to key expressions, feeding `LinkConfig.scope`; the firehose only as the no-definition fallback; focus mode and operator scope unchanged. | The GUI fetches what it shows — bandwidth follows the definition, not the fleet size. |
| **4 — consolidation** | Every specialized view reads the family model; generic `Call`/`Reply` replaces the Fetch pairs; write forms from request schemas; delete the 82 messages and 7 detail states. | `app.rs` is a router again. |
| **5 — not planned** | WASM view plugins. Retained as an option only if hostile third-party plugins ever become a requirement; the document + Rhai covers every in-tree case. | — |

Phase 0 is the whole argument in miniature and costs a day. Phase 1 is the one
that needs the release choreography. Phases 2–3 are where the value is. Phase 4 is
debt repayment.

---

## 9. Risks and mitigations

- **Performance of a `Value`-based model.** Documents and replies are small and
  infrequent; the hot path — telemetry points — keeps `TelemetryPoint` typed and
  the store untouched. The family derivation runs once per slice version, not per
  sample. The Bus explorer already renders 10 k-row trees from `Value` (#1124
  fixed its re-flattening); the same discipline applies.
- **Definition drift.** A `views.toml` referencing a subject the slice does not
  declare fails the build (lint) and is reported at runtime (the conformance
  judges' "registry must not lie" posture, extended to views). The GUI's bundled
  override for a producer is diffed against what the producer serves, exactly as
  the Fleet view diffs slices today.
- **Expressiveness ceiling.** Some views will not fit v1 (the traffic matrix,
  live video). They stay bespoke (`kind = "custom"`), which is a feature: the
  format should stay small. The pressure valve for "almost fits" is phase 5, not
  a bigger format.
- **Testing.** Declarative panels are rendered by one Rust renderer, so
  `iced_test::simulator` tests target it; the definitions themselves get
  snapshot tests (definition + fixture model → rendered rows). The three views
  rewritten in phase 3 carry their existing simulator tests as the regression
  suite.
- **`Protocol` outside the GUI.** Alerts, exporters and the correlator still use
  the enum. This proposal removes it from the *wire* and the *GUI*; a follow-up
  retires it from `zensight-common` once every consumer keys on the producer
  name. Not a blocker: `Protocol::from_str` on the producer chunk bridges for
  as long as needed.
- **Script performance.** Rhai is an AST interpreter — its own benchmark is ~1 M
  simple iterations in 0.14 s. A row label over a few hundred rows at the GUI's
  refresh cadence is far inside that; the traffic matrix and the 10 k-row
  explorer tree are not, and stay bespoke Rust. Compile once per view version,
  evaluate with a fresh `Scope`, and `on_progress` caps the worst case.
- **Script determinism.** Not formally hermetic like Starlark, but with no clock,
  I/O or randomness registered it is deterministic in practice, and the build
  lint rejects an unknown identifier. If provable hermeticity is ever required,
  the slot API is engine-agnostic.
- **Scope creep into a dashboard builder.** Non-goal (§10). Grafana exists and the
  exporters feed it; this is about the GUI showing the bus it watches.

---

## 10. Non-goals and open questions

**Non-goals:** user-editable layouts in v1; a general styling language; replacing
Grafana; loading third-party code in v1–4; changing the keyspace.

**Open questions for review:**

1. **Format:** follows the registry. zenkey #374 proposes a KDL spelling because
   *the format is a wire contract*; the view document should take whatever the
   registry takes, not choose on its own. This doc uses TOML because the registry
   is TOML today.
2. **Where a definition lives:** compiled into the sensor and served (proposed —
   versioned with the slice) vs a separate artifact the GUI fetches from a
   store. Serving keeps "a sensor and its view move together."
3. **`join` semantics:** the one non-derivable construct. Left join on a shared
   var only (proposed), or a small expression?
4. **Should `Protocol` be deleted from `zensight-common` in the same release,**
   touching alerts/exporters/correlator, or bridged and retired later (proposed)?
5. **Common-procedure declaration:** `views` declared in all 18 registries like
   `introspect`, or does zenkey gain injected common procedures (an RFC 08
   change)? The former needs no upstream work.
6. **Histogram (#1151 / zenkey #459):** the family model wants it for latency
   families; does it land before phase 2?
7. **Rhai vs Starlark:** Rhai for Rust ergonomics (proposed) or Starlark for
   provable hermeticity? Swappable behind the slot API; decide before phase 3.

---

## 11. Dependencies on zenkey

Filed 2026-09-20. Only the first is required.

| zenkey | What | Needed by | Status |
|---|---|---|---|
| [#460](https://git.marcpardo.eu/marcpardo/zenkey/issues/460) | **`RegistrySlice::bind(class, tail) -> Option<Bound>`** — match a live subject tail against declared `path` patterns and return the `{vars}`. Today `subject_target` compares `s.path == path` literally and the generated `parse_metric()` exists only for compiled producers. ~50 lines. | phase 2 (the family model) | **required** — the GUI can carry a private copy until it lands |
| [#461](https://git.marcpardo.eu/marcpardo/zenkey/issues/461) | RFC 08 §2, additive: optional `semantic` on `SubjectDecl` (§6.5) | phase 3+ | optional |
| [#462](https://git.marcpardo.eu/marcpardo/zenkey/issues/462) | RFC 05: `views` as a *common* read procedure with reply `ViewSet`, so `zenctl` can render a producer from the same document; format deferred to #374 | phase 3+ — ZenSight can declare it per-registry meanwhile | optional |
| [#459](https://git.marcpardo.eu/marcpardo/zenkey/issues/459) | `histogram` as a fifth `SubjectKind` (ZenSight #1151) | phase 2 for latency families | already filed |
| [#374](https://git.marcpardo.eu/marcpardo/zenkey/issues/374) | registry KDL spelling — the view document follows it | phase 3 | tracking |

Nothing else in this design touches zenkey: the `views` procedure rows, the
`ViewSet` type and schema, the family derivation, the renderer, the Rhai host API
and the build lint are all ZenSight-side and additive.

## 12. Sources

Tree evidence (master `77d69946`):
`zensight-common/src/telemetry.rs:166,272` · `zensight-common/src/keyexpr.rs:49` ·
`zensight/src/subscription.rs:987–1030` · `zensight/src/message.rs:52,689` ·
`zensight/src/device.rs:72–100,1003` · `zensight/src/app.rs:10246` ·
`zensight/src/view/explorer/inspector.rs:75` · `zensight/src/view/overview/mod.rs`
· `zensight/src/view/specialized/{bmc,probe}.rs`, `overview/pve.rs` ·
`zensight-common/registry/*.toml` (vocabulary counts) · `docs/KEYSPACE.md`
§"Type table + self-description" · vendored `zenkey-0.8.1/src/slice.rs`
(`SubjectDecl`, `ProcedureDecl`) · vendored `zenkey-fleet-0.13.0/src/model/decode.rs`
(`SchemaStore`, `decode_sample`, `DecodedSample`).

External:

- Netdata — [Chart Template Format](https://learn.netdata.cloud/docs/collecting-metrics/chart-template-format), [Netdata Charts](https://learn.netdata.cloud/docs/dashboards-and-charts/charts)
- Grafana — [Data frames](https://grafana.com/developers/plugin-tools/key-concepts/data-frames), [Standard field options](https://grafana.com/docs/grafana/latest/panels-visualizations/configure-standard-options/), [Scenes](https://grafana.com/blog/new-in-grafana-10-grafana-scenes-for-building-dynamic-dashboarding-experiences/), [Dashboard JSON model](https://grafana.com/docs/grafana/latest/visualizations/dashboards/build-dashboards/view-dashboard-json-model/)
- Perses — [Dashboard API](https://perses.dev/perses/docs/api/dashboard/), [perses.dev](https://perses.dev/), [The New Stack on Perses](https://thenewstack.io/perses-closes-the-observability-gap-with-declarative-dashboards/)
- JSON Forms — [UI Schema](https://jsonforms.io/docs/uischema/), [Introducing the UI Schema](https://eclipsesource.com/blogs/2016/12/27/json-forms-day-2-introducing-the-ui-schema/), [Generate UI Schema](https://jsonforms.io/examples/gen-uischema/)
- Home Assistant — [Sensor entity / device_class](https://developers.home-assistant.io/docs/core/entity/sensor/), [Sensor integration](https://www.home-assistant.io/integrations/sensor/), [auto-entities](https://github.com/thomasloven/lovelace-auto-entities)
- Adaptive Cards — [Schema Explorer](https://adaptivecards.io/explorer/), [Schema and object model](https://deepwiki.com/microsoft/AdaptiveCards/2.1-schema-and-object-model)
- OpenTelemetry — [Metrics semantic conventions](https://opentelemetry.io/docs/specs/semconv/general/metrics/), [Prometheus & OpenMetrics compatibility](https://opentelemetry.io/docs/specs/otel/compatibility/prometheus_and_openmetrics/); Prometheus — [OpenMetrics 1.0](https://prometheus.io/docs/specs/om/open_metrics_spec/)
- Zed — [Life of a Zed Extension: Rust, WIT, Wasm](https://zed.dev/blog/zed-decoded-extensions), [extensions with custom rendering of documents (discussion #37270)](https://github.com/zed-industries/zed/discussions/37270)
- WASM components — [Building Native Plugin Systems with WebAssembly Components](https://tartanllama.xyz/posts/wasm-plugins/), [Extism](https://github.com/extism/extism)
- Iced — [iced-rs/iced](https://github.com/iced-rs/iced), [Dampen](https://github.com/mattdef/dampen), [Glacier UI](https://github.com/antoniofernandodj/glacier-ui)
- Zenoh — [REST API & admin space](https://zenoh.io/docs/apis/rest/), [Storage manager plugin](https://zenoh.io/docs/manual/plugin-storage-manager/)
- Rhai — [Safety](https://rhai.rs/book/safety/index.html), [Maximum operations](https://rhai.rs/book/safety/max-operations.html), [Track progress / terminate a script](https://rhai.rs/book/safety/progress.html), [Benchmarks](https://rhai.rs/book/about/benchmarks.html), [rhaiscript/rhai](https://github.com/rhaiscript/rhai)
- Starlark — [starlark-lang.org](https://starlark-lang.org/), [bazelbuild/starlark](https://github.com/bazelbuild/starlark); Rune — [rune-rs/rune](https://github.com/rune-rs/rune)
