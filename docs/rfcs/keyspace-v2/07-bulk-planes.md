# 07 — Bulk Planes: `@media` and `@blob`

**Status: Draft** · normative chapter

Two kinds of traffic must never meet a wildcard: frame-rate opaque bytes
(video, imagery) and bulk transfers (files, directory trees, chunks). Both
get verbatim planes in the class position, so no data selector — not even a
per-origin firehose `…/h-xxx/**` — can ever pull them by accident
(design property D2, [03-grammar.md §4](03-grammar.md)).

---

## 1. `@media` — live opaque streams

```
<base>/@v1/<origin>/@media/<producer>/<stream>/video/<codec>/<profile>
<base>/@v1/<origin>/@media/<producer>/<stream>/preview/<format>
```

Example: `zensight/@v1/h-3fa9c2d41b7e/@media/parallax/cam0/video/h264/main`,
`…/@media/parallax/cam0/preview/jpeg`.

Rules:

- **Payload is raw encoded bytes** (access units, JPEG frames) with the
  container/codec declared via the middleware `Encoding`
  (`video/h264`, `image/jpeg`) — never a telemetry envelope, never fed to a
  telemetry decoder.
- **Frame metadata rides the attachment**, as a compact binary document
  (reference: CBOR `FrameMeta` — keyframe flag, pts/dts/duration,
  sequence, dimensions). Keys stay stable; per-frame data never touches
  them ([03-grammar.md §2](03-grammar.md)).
- **QoS: best-effort · drop · interactive-high** — a stale frame is
  worthless and the encoder must never block ([04-planes.md §3](04-planes.md)).
  Plain declared publisher; explicitly **never** an AdvancedPublisher —
  no cache, no miss detection, no heartbeat (recovering a superseded frame
  is anti-useful; [04-planes.md §3.1](04-planes.md)).
- **Keyframe-on-subscribe**: the publisher SHOULD watch subscriber matching
  (matching listener) and force a keyframe when a viewer arrives, and the
  keyframe flag MUST be a byte-level promise — a fresh decoder can start at
  any sample whose attachment says keyframe (parameter sets inline or
  prepended). Note the matching listener signals only the
  no-viewers ↔ some-viewers *edge*: an Nth viewer joining beside a current
  one produces no event and obtains its immediate keyframe via
  `@rpc/<producer>/stream/keyframe`
  ([05-control-rpc.md §5](05-control-rpc.md)) instead of waiting out a GOP.
- **Viewer selectors stay single-stream**: exact key for previews;
  `…/<stream>/video/<codec>/*` for video (one `*` over the
  publisher-configured profile chunk, which the viewer cannot know).
  Matching is intersection-based, so the publisher's matching listener
  fires for the wildcard subscriber.
- **Stream control is `@rpc`**, stream status/catalogue is `state`
  ([05-control-rpc.md §5](05-control-rpc.md)); stream *stats*
  (fps/kbps/drops/viewers) are ordinary `telemetry` under
  `telemetry/<producer>/<stream>/stats/…` — charts light up for free.
  `@media` carries pixels and nothing else.

## 2. `@blob` — bulk and content-addressed transfer

```
<base>/@v1/<origin>/@blob/artifact/<id>/**            Tier-1: manifest + chunks of one named blob
<base>/@v1/<origin>/@blob/tree/<id>                   Tier-2: directory-tree index (depth-first entry list)
<base>/@v1/<origin>/@blob/store/<algo>/<hash>         Tier-2: content-addressed chunk (immutable)
```

The chunk after `@blob` is a reserved **tier token** (`artifact` | `tree` |
`store`), not a producer chunk ([03-grammar.md §1.5](03-grammar.md)) —
content-addressed data has no owning component. All three tiers are
**queryables** served by the origin (pull-only — a consumer that never asks
never pays a byte), fronted by a resumable client (reference: `zenoh-blob`
— manifest + ranged chunk GETs, hash verification, resume by have-set).

- **Tier-1 (`artifact/<id>`)**: whole-file delivery of a one-off artifact
  (debug bundle, pcap). The `<id>` is the ULID minted by the RPC that
  created it ([05-control-rpc.md §3](05-control-rpc.md)); the id is
  per-artifact, which is acceptable *here* because blob keys are
  short-lived queryable endpoints, not published state.
- **Tier-2 (`tree/<id>` + `store/<algo>/<hash>`)**: content-addressed
  directory trees. The client GETs the index, diffs the needed hashes
  against its local content store, fetches only missing chunks
  (re-hashing on receipt), reconstructs, verifies the root. Resume *is*
  "which hashes I already have" — it survives reconnect and restart with
  no session state.
- **Chunks are immutable ⇒ cacheable fleet-wide.** `store/<algo>/<hash>`
  replies are valid from *any* holder. The normative dedup point is a
  **router-hosted content store**: chunks and indexes MAY be PUT into a
  router storage on the `store/**` selector (the sanctioned exemption from
  the declared-publisher rule, [04-planes.md §3](04-planes.md)) so a
  producer publishes once and exits, and the fleet fetches the router copy.
  A wildcard-origin fan-out (`GET <base>/@v1/*/@blob/store/sha256/<hash>`)
  is legal but MUST NOT be the default fetch path: every holder ships the
  full chunk (Zenoh cannot cancel remote replies in flight), so N holders
  cost N× the bytes — amplification on exactly the links this plane
  promises to spare. If used at all, wildcard fan-out is for *probing*
  (manifest/existence checks with tiny replies), followed by a fetch from
  one chosen origin's literal key.
- **QoS: bulk yields — a client obligation.** Zenoh replies inherit the
  *query's* QoS (server-side reply-QoS setters are no-ops), so it is the
  `@blob` caller that MUST issue its GETs at data-low priority; that is
  what keeps a transfer from starving telemetry or an alert on a
  constrained link ([04-planes.md §3](04-planes.md)).

## 3. Why planes and not payloads

The alternative — riding bulk/media on the data classes with a "big"
payload type — fails all three constraints these planes exist for:

- **Selector safety**: `…/h-xxx/**` (a UI's per-host subscription) must be
  affordable on a constrained link; one camera behind it must not turn it
  into a video feed. Verbatim chunks make reaching a *placed* frame
  impossible for any data selector — and registry review is what
  guarantees frames are placed here (the theorem/precondition split of
  design property D2, [03-grammar.md §4](03-grammar.md)).
- **Storage safety**: class-driven storage selectors
  ([04-planes.md §4](04-planes.md)) must never ingest frames or chunks into
  a time-series backend by accident.
- **Different delivery physics**: media wants newest-only/drop; blobs want
  pull/resume/verify. Neither is pub/sub state or telemetry; forcing them
  into data classes would corrupt the class semantics that everything else
  relies on.
