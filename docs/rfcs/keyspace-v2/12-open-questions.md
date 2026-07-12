# 12 — Open Questions

**Status: Draft** · each item lists the options and the default the RFC
assumes until decided otherwise. These are the points reviewers should
attack first; everything not listed here is a settled recommendation.

---

## 1. Realm / multi-tenancy placement

**Question.** Should the grammar reserve a tenant/realm position, or is a
deployment prefix (`acme/fleet-a` as `<base>`) sufficient forever?

**Options.** (a) deployment prefix only, as specified
([03-grammar.md §1.1](03-grammar.md)); (b) reserve a fixed chunk between
`<base>` and `@v1` now, empty-tolerated later.

**Default: (a).** A reserved-but-unused chunk costs every key today for a
need that may never come, and the verbatim `@v1` boundary means a future
`@v2` could introduce a realm position without ambiguity anyway. Revisit
only if a real multi-realm deployment materializes.

## 2. Observed-device promotion to origin

**Question.** May a proxied device (a router polled over SNMP) ever appear
in the origin position — e.g. a `d-<hash>` origin published *on its
behalf* by a gateway (Keelson's gateway pattern)?

**Options.** (a) never — devices are subjects, per
[06-identity.md §3](06-identity.md); (b) a registered gateway may mint
device origins, giving per-device ACL and per-device link budgets at the
cost of a weaker origin-equals-trust-boundary invariant.

**Default: (a).** The origin chunk's value comes from meaning exactly one
thing (the machine that runs the publisher). Revisit if a deployment needs
per-device ACL on proxied devices — that is a new trust model and should
arrive as a reviewed amendment, not a drift.

## 3. Durable command delivery

**Question.** RPC is deliberately not durable — an offline host misses the
call and the caller knows ([05-control-rpc.md §3](05-control-rpc.md)). Does
any control path need instructions that survive producer downtime?

**Options.** (a) none needed — keep RPC-only; (b) desired-state
reconciliation: a controller publishes `state/<producer>/desired/<topic>`
(LWW, storage-backed), producers converge on (re)connect.

**Default: (a),** with (b) as the sanctioned escape hatch — it is already
expressible in the grammar with no new mechanism, which is the reason it
does not need to be pre-built. The forbidden third option is durable
pub/sub *commands* (fire-and-forget imperatives with no convergence
semantics); if durability is wanted, it must come as desired state.

## 4. `events` retention and replay

**Question.** The bus contract for `events` is immutability + unique keys
+ a per-subject rate budget ([04-planes.md §1.3](04-planes.md)). What is
the *deployment* contract — how long are events queryable, and via what?

**Options.** (a) storage-only: a retention-windowed time-series storage on
`…/*/events/**`, replay = storage query; (b) additionally require each
producer to serve its recent events over `@rpc` from a bounded ring
(uniform with other detail queries, works with no router storage).

**Default: (a) + (b)** — they are cheap together and cover both
router-full and router-less deployments. The unresolved part is the budget
rule itself: what rate class boundaries the registry should enforce
(per-minute? per-hour?) needs one release of real data before being fixed.

## 5. Short class tokens

**Question.** `telemetry`/`state`/`events` are 5–9 bytes per key. Do they
cost anything real on constrained links, and should the grammar use
`t`/`s`/`e`?

**Position.** Declared publishers intern keys — the cost is
per-declaration and per-selector, not per-sample — so the readable tokens
should be free in steady state. But this is an assumption about wire
behavior, not a measurement.

**Default: full words**, with a measurement task attached: publish a
representative fleet over a bandwidth-shaped link and compare declaration
+ steady-state overhead. The RFC should ratify with the numbers recorded
in this chapter, whichever way they point.

## 6. Alert placement

**Question.** Alerts live at `state/<producer>/alert/<key>`
([04-planes.md §1.2](04-planes.md)). Should they instead be a dedicated
class-level token (`alerts`) beside `telemetry`/`state`/`events`?

**Options.** (a) subject under `state` — alerts are LWW state and inherit
all state machinery (seed-by-GET, latest-value storage, tombstones);
(b) own class — buys a shorter fleet selector
(`…/*/alerts/**` vs `…/*/state/*/alert/*`) and a coarser ACL boundary
("this client may read alerts but no other state").

**Default: (a).** The selector difference is cosmetic and the ACL case is
speculative; a fourth data class dilutes the "class = update semantics"
rule, since alerts *are* state semantically. Revisit only if a real
consumer needs alerts-without-state read permission.
