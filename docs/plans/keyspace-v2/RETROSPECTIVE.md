# Epic #453 retrospective — `epic/453-keyspace-v2` vs `master`

*Written 2026-07-13, after the branch went green and survived a live `just run`.
Nothing here is merged; this is the document to read before deciding to merge.*

> **Revision 2** — every claim below, and in particular every proposed RFC amendment, has been
> fact-checked against the ratified chapter text and against Zenoh's documented semantics
> (method in §8). **Three claims in revision 1 did not survive**: one proposed amendment was
> already normatively covered by the RFC, one contradicted it, and one excused a real
> conformance gap with a mis-citation. They are corrected in place and called out, not quietly
> deleted — the corrections are the most useful part of this document.

---

## 1. Verdict up front

**Following the RFC was worth it, and the branch is in better shape than master** —
not because the keys look nicer, but because three specific properties of the
convention turned into working machinery: the bus is *provably* cut over, consumers
stopped filtering client-side, and every sensor now describes its own surface.

It cost more than a rename. 20 commits, 289 files, **+9,987 / −5,519 lines**, one
deliberate wire break, and one live regression that took the entire GUI drill-down
surface down until it was found. None of that was wasted, but it is the honest price.

The branch is green with **zero crate exclusions** (`cargo test --workspace`,
`clippy --all-targets -D warnings`, `fmt`, the feature matrix, `just build`), and an
isolated 5-sensor + correlator smoke proves the legacy bus is silent while every v1
plane answers — including a full parallax open → JPEG frame → close round-trip.

Merge is gated on judgement calls, not correctness (§6).

---

## 2. What changed: branch vs master

### The shape of the diff

| Area | Files | What happened |
|---|---|---|
| `zensight-keyspace/` (new) | 25 | ~4,200 lines: grammar core, origin minting, slug/QoS types, **11 registry TOMLs** (10 producers + `@catalog`), `build.rs` codegen + lints-as-build-errors, guard tests |
| `zensight/` (GUI) | 33 | every subscription, GET and command re-keyed; new source→origin map |
| `zensight-sensor-core/` | 24 | publishes under the v1 grammar; `key_prefix` → `producer()`; `@rpc` queryables; artifact/alert planes |
| `zensight-common/` | 19 | keyexpr/command builders rewritten on `V1Context`; new fleet-vs-origin helpers; `v1_probe` example |
| 10 sensor crates | ~110 | control plane → `@rpc`, alerts → LWW state, evidence → v1, configs lose `key_prefix` |
| `zensight-correlator/` | 15 | becomes the **`@catalog` service**: entity seed, alias records, `introspect` |
| exporters + rerun | 23 | consume class selectors instead of filtering the firehose |
| `configs/` | 18 | `key_prefix` gone; router storages re-expressed as v1 selectors |
| `docs/` + per-crate docs | 41+ | rewritten around v1; `KEYSPACE.md` is now the deployed-profile summary |
| `.github/workflows/` | 1 | new **legacy-literal guard**, put-ban extended |

### The keys themselves

| | master | branch |
|---|---|---|
| telemetry | `zensight/sysinfo/toolbx/cpu/usage` | `zensight/@v1/h-9706b31ddad3/telemetry/sysinfo/cpu/usage` |
| health | `zensight/sysinfo/@/health` | `zensight/@v1/h-…/state/sysinfo/health` |
| alerts | one blob on `@/alerts` | `…/state/netlink/alert/<16hex>` — one LWW doc per alert key |
| commands | put on `zensight/<p>/@/command` | GET `…/@rpc/netlink/sockets`, write `…/@rpc/netlink/expectations/set` |
| identity | `_meta/evidence/**` | `…/state/<producer>/evidence/**` → `@catalog/state/entity/<id>` |
| media | ad-hoc `zensight/parallax/…` | `…/@media/parallax/<stream>/preview/jpeg` |

### What was *deleted* (this is the part that matters)

- `KeyExprBuilder`, `parse_key_expr`, `sensor_control_prefix`, and six `all_*_wildcard`
  helpers — `zensight-common/src/keyexpr.rs` went from **32 public functions to 25**, and
  the survivors are all typed.
- The `@/status` document plane (the health doc absorbed the running flag).
- The device-liveness *document* plane — it turned out to have **zero publishers**;
  liveliness tokens were always the real presence path.
- The `key_prefix` config knob, in every sensor config. Producers are now *named*
  (`SensorConfig::producer()`), never prefixed. This is the breaking config change.

---

## 3. Was following the RFC a good thing?

Yes — and here is the evidence, rather than the vibe.

**D1 (`@v1` is a verbatim chunk, so `**` never crosses it) made the cutover provable.**
This sounded like a pedantic grammar rule when it was written. In practice it is the
single most valuable line in the RFC: it let me write
`zensight-sensor-core/tests/cutover_e2e.rs`, which stands up an isolated pair, subscribes
to the *entire legacy bus* (`zensight/**`), and asserts it stays **silent** while v1
traffic flows. A migration that can assert "the old world emits nothing" is a migration
you can actually finish. Without D1 the legacy wildcard would have swallowed the new keys
and the test would be impossible to write.

**Class selectors retired client-side filtering.** On master the exporters subscribed to
the firehose and threw away everything that wasn't telemetry, with an `is_telemetry_key`
predicate carrying the knowledge. On the branch they subscribe to
`zensight/@v1/*/telemetry/**` and the router does the work. Same for the GUI's state vs
telemetry paths. That is real bandwidth on a constrained link, not just tidiness.

**The population-budget rule caught a real design bug before it shipped.** netflow was
about to key telemetry per flow-pair — unbounded key population. The rule forbade it, so
the branch has `zensight-sensor-netflow/src/rollup.rs`: bounded per-exporter counters on
the bus, and the per-flow detail served on demand from a ring buffer via `@rpc/netflow/flows`.
That is strictly the better design and I would not have arrived at it without the rule.

**The registry + `introspect` gave every sensor a self-describing surface — but only for
state and control, and nothing calls it yet.** 11 TOMLs generate typed builders and parsers,
lint failures are *build* failures, and each sensor serves its own compiled registry slice at
`…/@rpc/<producer>/introspect`. Nothing on master offers this. Two honest qualifications the
fact-check forced on me:

- **Telemetry is not actually described.** Six producers register their whole metric tree as a
  single catch-all `{metric...}` subject (`registry/{sysinfo,netlink,netring,systemd,logs,parallax}.toml:15`).
  So the lint "every published key is buildable from a registry entry" is **vacuous** for
  telemetry — anything is buildable — and `introspect` tells a consumer nothing about which
  metrics a producer emits. It was a conscious shortcut (the TOML comment says typed refinement
  is additive); it is undischarged debt (#468).
- **`introspect` has no consumer.** All 10 sensors and `@catalog` serve it. The GUI, the
  exporters and rerun call it **zero times** (#469). We built the capability the RFC asks for
  and never used it.

The claim that survives is narrower than the one I made in revision 1: the registry made the
*control* surface typed, honest and machine-readable. It has not yet done that for telemetry,
and no consumer has yet cashed the cheque.

**And that is the pattern, not the exception — see §6.5.** The most uncomfortable thing the
fact-check turned up is that we *built* several of the RFC's capabilities and then never used
them. That is the difference between paying for a convention and benefiting from one.

**The `@rpc` plane collapsed three ad-hoc mechanisms into one.** Commands were puts,
queries were queryables, status was a document. Now: reads are GETs, writes are GETs with
a payload on `…/set`, failures are `reply_err` with namespaced error names, and "alive ⇒
callable" is guaranteed by declaring queryables before the liveliness token.

**Where it was neutral or negative:** the grammar is more verbose on the wire (an extra
`@v1` chunk and a 14-char origin on every key — measurable on a constrained link, though
Zenoh's key-expression interning largely absorbs it), and the migration cost real
calendar time for zero new user-facing features. If you had asked me "is a keyspace
rewrite the highest-value thing to do this week", the honest answer is that it is an
investment, not a feature — but it is one that keeps paying, and the longer it was
deferred the more consumers would have to be rewritten.

---

## 4. Pain points

These are the things that actually hurt. Each one is a candidate RFC amendment (§5).

### 4.1 Local-origin vs fleet-origin builders — hit three separate times

`command_key()` / `query_key()` / `status_key()` / `artifact_blob_prefix()` all derive
**the calling process's own origin**. That is exactly right for a sensor *serving* its
own queryable, and exactly wrong for the GUI *calling* someone else's. The GUI got it
wrong three times in three different commits (`a9a0969`, `feeabd2`, and again inside
`9742de4`), each time producing keys that pointed at the GUI's own host.

Fixed by splitting the API into caller-side helpers — `fleet_rpc_key` / `fleet_command_key`
(`*` origin, `QueryTarget::All`) and `origin_rpc_key(origin, …)` (one host) — but the
type system never stopped me. A `LocalOrigin` vs `RemoteOrigin` distinction would have.

### 4.2 Payload `source` vs key `origin` — the outage

**This is the big one.** The RFC makes every key origin-scoped (`h-<12hex>`), but the
payloads carry a *human* identity (`source` = hostname, e.g. `toolbx`). The GUI's device
map is keyed by `source`, because that is what it shows the user. When the drill-down
fetches became origin-scoped, they were built from the *hostname* — so the GUI cheerfully
GET `zensight/@v1/toolbx/@rpc/sysinfo/processes`, where no queryable has ever lived.

Result: **every drill-down in the product died at once** — Process Explorer, all of
netlink's tabs, all of netring's, systemd's, parallax — while streamed telemetry kept
working perfectly, which made it look like a sensor problem. The user found it, not me.

The bridge existed on the wire the whole time (`HealthSnapshot.host_id` *is* the origin
id, RFC 06 §1), but **the RFC never says that a consumer needs to build that bridge, or
how.** The fix (`9742de4`) is a `source → origin` map in the GUI fed by health docs,
`SensorInfo`, and catalog entities, with a fleet-selector fallback before the map fills.
That is a load-bearing consumer-side pattern and it is nowhere in the convention.

### 4.3 Fleet-selector smoke tests cannot catch broken origin paths

My isolated smoke was green while the product was completely broken, because it probed
`zensight/@v1/*/@rpc/…` — the `*` matches *any* origin, so a caller with a garbage origin
concept still gets replies. The smoke now has a **GUI-shaped phase**: it builds the same
health-fed source→origin map and then requires *origin-scoped* replies (plus the full
parallax tile lifecycle). Lesson: a test that uses a wildcard where the product uses a
concrete value is testing a different program.

### 4.4 The registry must not lie

`introspect` serves the *compiled* registry slice. So any TOML entry describing a surface
the build doesn't actually serve is a lie shipped to every consumer. An audit found seven:
netring `capture/trigger` + `capture/{ulid}`, parallax `stream/open|close|keyframe`,
catalog `link`/`unlink`, a phantom `artifact/{kind}` subject, proxy device-liveness, a
snmp alert. And it omitted five surfaces that *were* served. All reconciled in `004ec24`,
but nothing structurally prevents the drift — the registry is checked against the grammar,
not against the code.

### 4.5 Zenoh operational facts — one real RFC gap, and two things I simply hadn't read

Revision 1 filed all three of these as "facts the RFC doesn't mention". **Two of them the RFC
mentions, normatively, as MUSTs.** That correction matters more than the original claim did.

- **Gossip ≠ multicast — a genuine gap.** My first isolated run disabled *scouting* wholesale
  and the entity seed came back empty: spoke→spoke evidence cannot traverse a gossip-less hub
  peer. The `ZENSIGHT_ZENOH_SCOUTING` knob now disables **multicast only**. The words
  `scout` / `gossip` / `multicast` appear **zero times across all 13 RFC chapters** — 09 §0's
  entire treatment of connectivity is the comment `// mode/connect/listen as the deployment
  requires`. This one is real, and it is amendment D.

- **Reply on the concrete key** and **fan-in needs query target All** — *not* gaps. Both are
  already in **05 §2.1, a section titled "Fan-in call discipline (normative)"**, as bolded
  MUSTs, with exactly the right reasoning:

  > **Target.** Fan-in GETs MUST set query target **All**. The default (`BestMatching`)
  > short-circuits to a *single* queryable the moment any matching queryable is declared
  > `complete` — one storage config away from silently collapsing the fleet to one reply. […]
  >
  > **Reply key.** A procedure MUST reply on its **own concrete key** […] Zenoh's default reply
  > consolidation keeps one reply *per reply key* — distinct origins on distinct keys survive
  > it; a fleet replying on the shared wildcard key is consolidated down to one survivor.

  I hit both as bugs, then "rediscovered" the mechanism by reading Zenoh's API docs — and only
  found out during this fact-check that the chapter I was implementing had said it all along.
  **That is a reading failure, not a spec gap**, and it is the more useful lesson: the RFC was
  load-bearing in a place I had skimmed. The one thing worth adding to 05 is ergonomic (a
  copy-pasteable checklist), not normative.

### 4.6 Mechanical churn

Retiring `key_prefix` touched every sensor crate, every config, and every test that pinned
a key. Regenerating the registry changes enum variants, which breaks test pins by design.
~127 files needed nothing but stale-comment updates. This is unavoidable in a keyspace
rewrite, but it means the diff is large and mostly *boring*, which makes the interesting
5% hard to review. A reviewer should read `zensight-keyspace/`, `sensor-core`'s publisher,
and the GUI's origin map, and skim the rest.

### 4.7 A wrong assumption let a non-compiling sensor sit in the branch

I believed snmp/gnmi/systemd couldn't build in this sandbox (missing openssl/protoc/systemd
dev packages) and excluded them. **They all build fine.** systemd therefore went several
commits with a blind-edit compile error (`reply_json` referencing an undefined `key`) — which
is exactly why `just run` didn't compile for the user. Every build since has used the full
workspace with zero exclusions. This is a process failure, not an RFC failure, but it is the
one that damaged trust.

---

## 5. Proposed RFC amendments (v1.1 — all additive, no wire change) → **#467**

**Revision 1 proposed seven. Five survive.** Each was checked against the chapter text; the
verdict column is the honest result, including where I was wrong.

| # | Chapter | Verdict | Change |
|---|---|---|---|
| **A** | **06-identity**, new §"Consumer identity bridge" | **absent — confirmed.** `host_id` appears nowhere in 06. §5.1 "How a UI joins" runs **origin → entity** only; there is no step for "I have a hostname, I need the origin". | State normatively that the payload `host_id` **is** the origin id; that a consumer keying on human identity **MUST** resolve it to an origin before building an origin-scoped key; and give the two sanctioned bridges — a `source → origin` map fed by the health/registration docs, or the `@catalog` entity doc (`entity.origins[]`). **This is the amendment that would have prevented the outage.** |
| **B** | **08-registry**, codegen contract | **absent — confirmed.** 08 §1's contract is *build vs parse*, and builder args are "one argument per `{var}`" — the origin is never mentioned. "remote" appears **0×** in 06/08/11. | Extend the contract to build/parse × **local/remote**, and recommend a **type-level** distinction between an origin you own and one you address, so "I built a key for my own host by accident" is a compile error, not a timeout. |
| **C** | **08-registry** §5/§6 | **weaker than I claimed.** 08 §6 *does* address it — and explicitly declines to close it: a registry↔introspect mismatch is "a **finding**, not an ambiguity". §5's lint is a **SHOULD** and runs one direction only (published ⊆ registry), never registry ⊆ served. | So the amendment is *upgrade §6 to a MUST and add the reverse-direction lint*, not "add something new". Our own registry had **7 advertised-but-unserved surfaces**; `introspect` was shipping lies. |
| **D** | **09-operations**, new §0.1 "Discovery and scouting" | **absent — confirmed.** `scout`/`gossip`/`multicast` = **zero hits across all 13 chapters.** | Multicast scouting and gossip are independent switches. Isolated verification = multicast **off**, gossip **on**. Document the hub-and-spoke failure mode (a gossip-less hub peer silently breaks spoke→spoke discovery). |
| ~~E~~ | ~~05-control-rpc~~ | **FALSE — dropped.** 05 **§2.1 "Fan-in call discipline (normative)"** already mandates both the concrete reply key and query target All, *with* the `BestMatching`/`complete`/consolidation reasoning. | Nothing to add but a copy-pasteable checklist. **Recording this so nobody "fixes" a section that was already right** (see §4.5). |
| **F′** | **07-bulk-planes** | **my proposal contradicted the RFC.** 07 §2 already says a wildcard-origin `@blob` fan-out "is legal but **MUST NOT** be the default fetch path" — N holders ⇒ N× the bytes. | What *is* absent: `*`-origin guidance for **`@media`** (§1 only covers the `*` *profile* chunk), and the general rule — **a publisher MUST always use its concrete origin; a subscriber MAY wildcard a chunk it cannot know**. ⚠️ This has a live consequence: the GUI's `*`-origin media fallback would subscribe to *every* host's stream of that name. Same amplification `@blob` is warned about. |
| **G** | **09-operations** (*not* 11) | **absent — confirmed**, but 11's header explicitly scopes verification out, so it belongs in the operations cookbook. | A cutover is not done until an isolated run shows the retired key family **silent** *and* a **consumer-shaped, concrete-key** probe passes. A `*`-origin probe cannot catch a broken origin path — ours was green while the product was entirely broken. |

Also: **07 §1 has a cross-ref bug** — it cites `05 §5` twice for stream-control RPC, but 05 §5
is "Mapping the incumbent channels"; the normative home is **05 §3** (which 07 §2 cites
correctly).

A and B carry the weight; the rest is hygiene. None change a byte on the wire, so they land as
a doc-only v1.1.

---

## 6. Remaining work — all now tracked

**Nothing here blocks the merge.** Every item was verified in the code before being filed;
nothing is filed on a hunch.

| # | Item | Why it's real |
|---|---|---|
| **#466** | **Set the Zenoh session `namespace`** (p1) | ⚠️ **Correction.** Revision 1 called this "a follow-up; RFC 09 §5 explicitly allows spelling the base as the fallback". **That was a mis-citation** — 09 §5 is *Debugging etiquette* and permits un-namespaced full keys for **debug tools**, not applications. 03-grammar §1.1 makes the namespace the **RECOMMENDED** implementation of the base, and **#465's own scope demanded it**. It is a **conformance gap**, not a sanctioned choice. Also an ingress isolation boundary we're not getting. |
| **#467** | **RFC v1.1 amendments** | §5 above. |
| **#468** | **Telemetry is a catch-all `{metric...}` on 6 producers** | The registry lint is vacuous for telemetry; `introspect` describes none of it. |
| **#469** | **`introspect` has no consumer — plus 8 other orphan procedures** | Served by all 11 producers, called by nobody. Also orphaned: sysinfo `latency`, netring `encrypted_dns`/`ipfix`/`capture_disk`(read), netlink `collection`(+set), systemd `failed`, logs `filter`(read), netflow `flows` — the last of which *we built this epic*. Wire or retire (retirement needs the `[[deprecated]]` ledger, 08 §3). |
| **#470** | **Logs' doubled `telemetry/logs/logs/…` chunk** | ⚠️ **Correction.** Not "a one-line registry fix" — the registry entry is the catch-all, so the fault is in the **sensor's metric names**, and consumers pin the metric name (4 GUI files, 2 test files) while **the exporters derive the Prometheus/OTel series name from it** — stripping the prefix renames exported series and breaks dashboards. The *key* change is non-breaking (all consumers subscribe by class wildcard). |
| **#471** | **Router storage configs unverified** | 5 storages across 3 config files; zero tests, zero CI, zero justfile recipes reference them. `zenoh-blob/tests/storage.rs` deliberately uses a stand-in. GC/lifespan entirely unexercised; the InfluxDB v2 variant is commented out and marked UNVERIFIED. |
| **#472** | **Container image not rebuilt post-cutover** | The branch touched `docker/` once, to delete three `key_prefix:` lines. Dockerfiles and entrypoint unchanged and not broken — but never built or run against v1. |
| **#473** | **`@catalog` `link`/`unlink`** | Designed, unimplemented, honestly absent from the registry. Design constraint: the catalog is a *pure function of live evidence*, so an operator override must itself be published as evidence, not kept in a side table. |
| **#474** | **GUI device map should key on the entity id** | `origins: HashMap<source, origin>` collides on duplicate hostnames. Master had the same collision — but there it only muddled a *display*; here it **misroutes queries**. First thing to break in the container/multi-machine deployment. |

Not filed, but worth one pass: **a full click-through of every GUI view on the live run.** The
drill-down outage proved our headless verification had a blind spot; the branch has closed it
for the paths the probe now covers, and human eyes are cheap insurance for the rest.

**Issues #454–#465 remain open** (per your instruction), each carrying its commit sha; the epic
has the completion summary. They close on merge.

### 6.5 The capabilities we built and never used → **epic #477**

Everything above is debt or verification. This is different, and it is the part I'd fix first.

The migration delivered **zero new user-facing features** — that was the accepted price of an
investment. But the fact-check showed we didn't even collect the investment: **the RFC's own
capabilities are implemented, on the wire, and unconsumed.**

| Capability | Built | Used |
|---|---|---|
| **`introspect`** — fleet capability & version inventory; "who still serves a deprecated subject" (08 §6 sells exactly this) | all 10 sensors + `@catalog` | **nothing calls it** (#469) |
| **The registry's generated `parse` direction** — normative in 08 §1, specified precisely to *"replace positional `split('/')` re-parsing scattered across consumers"* | `parse_subject` is generated | **zero callers; the GUI still hand-parses in 18 sites** (#475) |
| **One-origin drill-down** (`@v1/h-xxx/**`) — "complete data plane of one host", 09 §1. Not expressible on master's keyspace at all. | the grammar makes it expressible; a `scope` config knob exists | **no UI affordance** (#476) |
| **netflow's `flows` ring** — built *this epic*, to replace the per-flow-pair keys the population-budget rule forbade | served | **no GUI tab** (#469) |

`introspect` and the one-origin selector are the two that would visibly change how the product
is used: a fleet capability view answers "who is running what" without SSH, and a *focus this
host* toggle makes a technician's laptop stop paying for the whole fleet's telemetry on a
constrained link — which is the deployment the entire convention was shaped around.

Adopting the parse direction is the other one: we paid the full cost of the registry (11 TOMLs,
codegen, lints, build-time failures) and left the most ergonomic thing it produces on the floor.

**Recommendation: do #477 next, not last.** It is what converts "we rewrote the keyspace" into
something a user can point at.

---

## 7. Recommendation

1. **Review the branch in three passes, not one.** `zensight-keyspace/` (the contract),
   `zensight-sensor-core/` + one sensor (the producer side), `zensight/src/app.rs` +
   one detail view (the consumer side). The remaining ~250 files are mechanical.
2. **Re-run `just run` once more** and click through the views — the drill-down outage
   proves that our headless verification has a blind spot the branch has only just closed.
3. **Merge as one epic PR, then close #454–#465.** The branch is a wire break; there is no
   value in landing it piecemeal, and every intermediate commit already leaves the workspace
   green.
4. **Land RFC amendments A + B (#467) before the next consumer is written** — they are the two
   lessons that cost the most, and they are cheap to write down while they are fresh.
5. **Then #466 (namespace)** — it is the one item that is a *conformance gap* rather than a
   preference, and it was in #465's scope.
6. **Then epic #477 — cash in the convention.** `introspect`, the parse direction, and the
   one-host focus subscription. This is what turns a 15k-line rewrite with no user-visible
   change into something worth having done. Do it before the ordinary debt.
7. **The rest (#468, #470–#474) are ordinary follow-ups.** Don't hold the epic open for them.

---

## 8. How these claims were verified

Revision 1 was written from memory of the run. Revision 2 checked it, and the check changed the
answer — which is the argument for doing it.

- **Every proposed amendment was greped against the ratified chapter text** before being kept.
  Amendment E died because 05 §2.1 already said it (as a MUST). Amendment F was rewritten
  because 07 §2 said the *opposite* of what I proposed. The namespace item in §6 was
  re-classified because I had cited 09 §5 for something it does not say.
- **The Zenoh semantics the amendments rest on were checked against the API docs**, not
  recalled: `QueryTarget::BestMatching` is the default and means *"the nearest complete
  queryable if any, else all matching queryables"* — so target `All` matters precisely when a
  **`complete`** queryable (e.g. a router storage) can shadow the live producers, which is
  exactly the reasoning 05 §2.1 already gives. `ConsolidationMode` defaults to `Auto`→`Latest`,
  which keeps one reply **per reply key** — which is *why* replying on the query's wildcard
  selector silently collapses a fleet to one survivor. Scouting `multicast` and `gossip` are
  independent config switches, which is why disabling "scouting" wholesale broke spoke→spoke
  discovery through our hub.
- **Every remaining-work item was verified in the code** (file:line) before an issue was filed.
  Two claims from revision 1 were wrong and are corrected above (the logs "one-line fix", the
  namespace "sanctioned fallback"); two findings were discovered that the run had never
  surfaced (the `{metric...}` catch-alls, and `introspect` having no consumer at all).

The pattern in all four corrections is the same one that caused the outage: **I trusted my
memory of a contract instead of re-reading it.** That is worth more than any single amendment.
