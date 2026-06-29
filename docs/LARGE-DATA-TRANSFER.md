# ZenSight — Large-Data Transfer (debug reports) & a zenoh-fs evaluation

> Status: **proposal / decision memo** · June 2026 · reviewer: @p13marc
>
> Questions from the maintainer: *"I found [`kydos/zenoh-fs`](https://github.com/kydos/zenoh-fs) and
> [`dad-io/sendit`](https://github.com/dad-io/sendit). Could we use one to add the ability to download
> large data (e.g. a debug report) to ZenSight? They don't look like maintained libraries — should I fork
> one, or build a new one?"*

## TL;DR — recommendation

**Don't adopt or fork zenoh-fs *or* sendit.** Build it yourself, in two tiers:

- **Tier 1 — single blob** (the debug-report download): a small chunked-transfer over a Zenoh queryable
  (§5), ~few hundred LOC, stable APIs, with progress, SHA-256 integrity, range-resume, bounded memory, authz.
- **Tier 2 — the "rsync-like" ask** (whole **directories** + truly **interruptible/resumable** across
  connection loss *and* process restart + dedup): generalize Tier 1 into a **content-addressed chunk store
  + tree index** — the [**casync**](https://github.com/systemd/casync) model ("git × rsync"). Chunks are
  named by their content hash and served by a `@/store/<algo>/<hash>` queryable; an **index** describes the
  tree; the client fetches only the chunk hashes it's **missing**. Resume = "the set of chunks I don't have
  yet" (survives reconnect *and* restart because progress is just which content-hashes are on disk); dedup
  across files/versions/reports is free; a router-side **storage backend** can cache chunks for the whole
  fleet. **Don't implement rsync's rolling-checksum delta** — content-defined chunking (FastCDC) over a
  content-addressed store subsumes it without the byte-shift problem.

Borrow *ideas* from the prior art — a manifest, **metadata-in-key** (sendit), content-addressing + delay
tolerance (zenoh-fs), the index/chunk-store split (casync) — not their code or their models.

**Why not zenoh-fs:** a 2★ personal *reference* project (Zenoh's creator A. Corsaro), architected as a
**delay-tolerant distributed filesystem** (a `zfsd` daemon that watches directories and replicates files
through a Zenoh **storage**), depending on Zenoh's **`internal` / `internal_config`** unstable features,
**unpublished on crates.io**, low-activity (last commit 2026-01-22). Wrong model for on-demand download.

**Why not sendit:** the closer of the two in spirit (drag-and-drop **file transfer over Zenoh**, Rust,
**actively maintained** — commit 2026-06-12) — but it is a **GUI application, not a library**: a single
egui/eframe binary whose transfer logic is wired through a 69 KB app-coupled `zenoh_worker.rs`. Its model
is **broadcast pub/sub** (drop a file → publish chunks to subscribed peers, AirDrop-style) with
**store-backed reassembly**, not the **authorized point-to-point request/response** ZenSight needs. It
also has design choices to *improve on*, not copy: **64 MB chunks** (kills progress granularity and bounded
memory) and **no content-hash integrity** (only "all indices present + total length matches"). You can't
depend on it (not a crate), and extracting its protocol is about as much work as writing the ~300 LOC you
actually want.

**Net:** two independent Zenoh file-transfer projects exist, both **apps/experiments**, **neither a reusable
library** — which is itself the signal: the right move is to write the small piece you need (and optionally
package it as the `zenoh-blob` crate the ecosystem is missing).

**Why build:** Zenoh already gives you everything a request/response bulk download needs — a queryable
can return **many replies** to one query, the **reliable** transport handles retransmission, and
**`CongestionControl::Block`** gives you backpressure so you never drop a chunk. The "hard parts"
zenoh-fs solves (a DFS, eventual consistency, multi-replica reconciliation) are *not* part of this use case.

---

## 1. The actual use case & requirements

A **debug report** ("sosreport for ZenSight"): on demand, a chosen sensor/host packages a bundle —
recent telemetry from the local redb store, sensor configs, the last N MB of logs, optionally a short
pcap from netring, the sensor's own health — into a `.tar.zst`, and the GUI downloads and saves it.

Requirements that drive the design:

| # | Requirement | Implication |
|---|-------------|-------------|
| R1 | **On-demand, point-to-point** (this GUI ← that sensor) | request/response, not broadcast or FS sync |
| R2 | **Large** (1 MB – ~100 MB; pcap can be big) | chunking; never hold the whole blob in RAM on either side |
| R3 | **Progress** in the GUI | client must see chunk N-of-M as it arrives |
| R4 | **Integrity** | per-blob SHA-256 + per-chunk length; verify on completion |
| R5 | **Resume** (flaky links, GUI restart) | content-addressed chunks + "give me from chunk K" |
| R6 | **Backpressure / no drops** | reliable channel + `CongestionControl::Block` |
| R7 | **Authz / safety** | only an operator-initiated request triggers report generation; bounded size; rate-limited |
| R8 | **Fits ZenSight** | reuse `@/query`-style queryables, the `Fetch<T>` client pattern, `spawn_blocking`, the redb store |
| **R9** | **Directory trees**, not just one file (the "rsync-like" ask) | a **tree index** (paths + mode/mtime + chunk refs) + per-file content; download a whole directory in one operation |
| **R10** | **Interruptible / resumable** across **connection loss *and* process restart** | progress must be **persistent on disk** — "which chunks do I already have" — not in-memory; a restart resumes, doesn't restart |
| **R11** | **Dedup** (nice-to-have; big win for repeated reports / re-sync) | content-address chunks by hash → identical chunks across files/versions transfer once |
| **R12** | **Pause / resume** (user-initiated) | stop fetching, **keep** partial on disk + persist resume-state; resume = the normal resume path (survives a GUI restart while paused) |
| **R13** | **Cancel** (user-initiated) | stop + **discard** partial; optionally tell the source to free its temp artifact early |

R9–R11 are the **Tier 2** requirements; R1–R8 are satisfied by Tier 1 alone. R12–R13 are **client-side**
controls that apply to **both** tiers (see §5.9) and reuse the resume machinery. All are met by the same
content-addressed design (§5.7) — Tier 1 is just the degenerate case (a one-entry "tree", chunks ordered).

Non-requirements (still — which is why a full DFS like zenoh-fs is overkill): no multi-writer replication,
no eventual consistency across nodes, no **always-on** background directory sync (ours is **on-demand,
pull-only**: the GUI asks; the sensor serves), no shared mutable file namespace.

---

## 2. Prior art — zenoh-fs and sendit

### 2a. zenoh-fs — a delay-tolerant distributed filesystem

**What it is** (from its README + source): *"A Zenoh-based delay-tolerant distributed file system
supporting extremely large data files."* Components:

- `zfs` — core lib: fragment a file into **32 KB** chunks, reassemble, track pending fragments.
- `zfsd` — a **daemon** that watches `upload`/`download` directories and drives transfers.
- `zut` / `zet` — CLI **up**load / **get** utilities; files live in / are fetched from a Zenoh **storage**.

**The facts (GitHub API, June 2026):**

| Signal | Value | Read |
|--------|-------|------|
| `zenoh` dependency | **`1.7.2`**, features `["internal", "internal_config"]` | recent & compatible, **but on unstable internal APIs** |
| Last commit | **2026-01-22** (`pushed_at` 2026-02-02) | low activity, ~5 months stale |
| Stars / forks / open issues | **2 / 0 / 0** | personal/experimental; no community |
| License | **Apache-2.0** | permissive — forking is legally fine |
| Published on crates.io? | **No** (`version = "0.1.0"`, members `zfs`,`zfsd`) | git/vendor dep only |
| Archived? | No | but not a maintained product |

**Assessment.** This is a high-quality *demonstrator* of delay-tolerant transfer over Zenoh, written by
the person who knows Zenoh best — worth **reading** for ideas (the fragment/manifest/resume shape). It is
**not** a library you should build a product feature on: the daemon+storage+directory model is the wrong
shape for an on-demand download, the `internal`/`internal_config` feature flags pull in Zenoh APIs that
are explicitly unstable across minor versions (a standing maintenance tax), and there is no community or
release cadence to lean on.

### 2b. sendit (`send-it`) — a drag-and-drop transfer *app*

**What it is** (README + `src/`): *"A drag-and-drop file transfer tool over Zenoh networks."* An
**egui/eframe desktop application** (single `[[bin]] send-it`): drop a file on the window → it's published
to auto-discovered peers (peer mode, multicast, port 7447); files from other peers appear in a topic tree;
chunked files show a progress bar before export. `src/`: `app.rs`, `events.rs`, `types.rs`, `ui/`,
`transfer.rs` (the chunk protocol), and a **69 KB `zenoh_worker.rs`** (the app-coupled Zenoh glue).

**The protocol** (`transfer.rs`, 549 LOC): each chunk is a separate PUT whose **metadata lives in the key**
— `{topic}/__chunk/{total_size}/{total_chunks}/{chunk_index}`. Reassembly scans the local store for all
chunks of a topic, groups by `(total_size, total_chunks)`, and succeeds when indices `0..total_chunks` are
all present and the reassembled length equals `total_size`. `CHUNK_SIZE = 64 MB`; single-payload cap ~4 GB.
**No content hash** (integrity = "all indices present + length matches"); **no range-based resume**
(reassembly is store-backed — a peer collects chunks as they arrive / via a queryable over stored data);
topic key is derived from the file's parent dir + filename.

**The facts (GitHub API, June 2026):**

| Signal | Value | Read |
|--------|-------|------|
| `zenoh` dependency | **`1.0`**, features `["unstable"]` | recent; **`unstable`** (better than zenoh-fs's `internal`, still not stable) |
| Last commit | **2026-06-12** | **actively maintained** |
| Stars / forks / issues | **5 / 0 / 0** | personal/early; no community yet |
| License | **Apache-2.0** | permissive |
| Shape | egui **binary** `send-it` 0.9.1, **not on crates.io as a lib** | an app, not a dependency |
| Model | **broadcast pub/sub** (publish to peers) + store-backed reassembly | not authorized point-to-point request/response |

**Assessment.** The closer of the two to the use case (real file transfer over Zenoh, actively maintained,
on `unstable` not `internal`) and the **better reference** — the **metadata-in-key** chunk scheme is elegant
and worth borrowing. But it is an **application, not a library**: the transfer logic is coupled to the egui
app via `zenoh_worker.rs`, so "using" it means extracting and rewriting `transfer.rs` onto ZenSight's own
session — about as much work as writing the ~300 LOC you want, minus the parts that don't fit. Its model
(broadcast to all peers, store-backed) and its choices (64 MB chunks → coarse progress + 64 MB live in RAM
per chunk; no content hash) are wrong for an authorized, bounded-memory, integrity-checked report download.

### 2c. Side-by-side

| | **zenoh-fs** | **sendit** | **ZenSight needs** |
|---|---|---|---|
| Form | lib + `zfsd` daemon + CLIs | egui **app** (1 bin) | embedded capability in Iced app + sensor crates |
| Model | delay-tolerant **DFS** (dir sync via storage) | **broadcast** pub/sub to peers + store | **authorized point-to-point request/response** |
| Zenoh features | `internal` + `internal_config` (unstable) | `unstable` | **stable only** (matches the rest of ZenSight) |
| Chunk size | 32 KB | **64 MB** | 256 KB–1 MB (progress + bounded RAM) |
| Integrity | (fragment bookkeeping) | length + index completeness, **no hash** | **SHA-256** per blob |
| Resume | pending-fragment retry | store-backed | **range `?from=K`** |
| Reusable as a dep? | git-dep only, unpublished | no (app) | n/a |
| Maintained? | low (Jan 2026) | **active (Jun 2026)** | — |
| Verdict | read for ideas | **best reference**; borrow metadata-in-key | **build (Option C)** |

---

## 3. The Zenoh-native toolbox (what you actually build on)

Zenoh 1.x already provides the primitives — you don't need a file-system layer on top:

- **Queryables return *multiple* replies to one query.** A single `session.get(selector)` yields a stream
  of `Reply`s; the queryable side calls `query.reply(key, payload)` as many times as it wants before the
  query is dropped. → A "download" is one query that streams a **manifest reply + N chunk replies**, and the
  client gets progress for free (it sees replies arrive). ([Zenoh 1.0 concepts](https://zenoh.io/docs/migration_1.0/concepts/))
- **Reliability is a transport property.** A *reliable* channel between routers handles retransmission and
  ordered delivery, and **fragmentation is automatic** at the wire level. (Caveat: the **LowLatency**
  transport does *not* support fragmentation — ZenSight uses the normal transport, so this is fine.)
  ([Zenoh reliability & congestion control](https://zenoh.io/blog/2021-06-14-zenoh-reliability/))
- **Congestion control is yours to choose.** `CongestionControl::Block` makes a publication/reply *block*
  until the reliability queue drains instead of dropping — exactly the backpressure you want for bulk
  (R6). `Drop` (the pub/sub default) would silently lose chunks. ([reliability blog](https://zenoh.io/blog/2021-06-14-zenoh-reliability/))
- **`ZBytes`** is the 1.x payload type; chunk payloads are just `ZBytes` (a `&[u8]`/`Vec<u8>`).
- **Storage backends** (filesystem / RocksDB / S3 via the storage-manager plugin) exist if you ever want
  reports *cached/served by a router* rather than generated by the sensor — useful later, not needed now.
  ([storage-manager](https://zenoh.io/docs/manual/plugin-storage-manager/), [filesystem backend](https://github.com/eclipse-zenoh/zenoh-backend-filesystem), [S3 backend](https://zenoh.io/blog/2023-07-17-s3-backend/))
- **Shared memory** gives zero-copy for *same-host* large payloads — irrelevant for a remote GUI download,
  but worth knowing it exists. ([Zenoh 1.6 Imoogi](https://zenoh.io/blog/2025-10-20-zenoh-imoogi/))

ZenSight already uses the building block: every sensor serves on-demand detail via `@/query/<topic>`
queryables (`fetch_records` on the GUI side decodes a single JSON reply). The blob transfer is the **same
pattern, generalized to many binary replies**.

---

## 4. Options compared

| Option | What it is | Pros | Cons | Verdict |
|--------|-----------|------|------|---------|
| **A. Depend on a prior project** (zenoh-fs / sendit) | git-dep zenoh-fs (+ `zfsd` + storage), or try to reuse sendit | reuses existing code | neither is a consumable library — zenoh-fs is a daemon/DFS on `internal` APIs; sendit is an **egui app** (no lib crate). Wrong models (DFS / broadcast), extra moving parts | ❌ No |
| **B. Fork a prior project** | fork zenoh-fs, or extract sendit's `transfer.rs` | keeps the fragment/manifest logic (zenoh-fs) or metadata-in-key (sendit) | you'd rewrite most of it to a point-to-point request/response model on stable APIs, and you'd own a fork; ≈ the same effort as building clean | ❌ No |
| **C. Build a purpose-built `blob`/report transfer** (recommended) | a small chunked-transfer over a queryable, in-repo | stable APIs only, fits `@/query` + `Fetch<T>` + `command` patterns, progress/integrity/resume by design, bounded memory, no new daemon | you write it (~few hundred LOC) | ✅ **Yes** |
| **D. Out-of-band HTTP** | sensor exposes a one-shot HTTP endpoint; Zenoh carries a URL | simple to stream | second transport; breaks Zenoh's NAT/firewall/discovery guarantees (sensors may not be HTTP-reachable from the GUI); auth/TLS to re-solve | ⚠️ Only if a sensor is already HTTP-reachable |
| **E. Zenoh storage backend** | sensor PUTs **content-addressed chunks** into a router-hosted FS/RocksDB/S3 storage; GUIs GET by hash | the natural **Tier-2 chunk store**: router caches/serves, dedup fleet-wide, survives sensor restart | needs a router + storage volume deployed; operational weight | 🔭 **Pairs with Tier 2** (§5.7) — the growth path for directory sync |

**Recommendation: Option C**, with **E** as a later enhancement if you want reports cached centrally, and
**D** explicitly rejected unless you already have an HTTP path to sensors.

---

## 5. Recommended design — a `@/report` chunked blob transfer

A thin, self-contained capability that drops into the existing architecture. Two new building blocks:
a **command** to generate a report and a **queryable** to stream it.

### 5.1 Keyspace (consistent with `docs/KEYSPACE.md`)

```
# 1) Request generation (operator-initiated, authz point) — command channel pattern (like netlink @/commands)
zensight/<protocol>/@/report/request        # PUT a ReportRequest{ id, kind, options }
zensight/<protocol>/@/report/status         # queryable: ReportStatus{ id, state, manifest? }

# 2) Stream the bytes — queryable returning a manifest reply + N chunk replies
zensight/<protocol>/@/report/<id>            # GET ?from=<k>  -> Manifest + chunks[k..]
```

`<protocol>/<source>` identifies which sensor/host produces the report (the GUI already knows the host).

### 5.2 Wire types (`zensight-common`, serde → JSON/CBOR like everything else; chunk payloads are raw bytes)

```rust
struct ReportRequest { id: Ulid, kind: ReportKind, opts: ReportOptions }   // kind: DebugBundle | Pcap | Logs…
struct Manifest {
    id: Ulid, filename: String, total_len: u64, chunk_size: u32,
    chunk_count: u32, sha256: [u8;32], created_ms: i64,
}
enum ReportState { Generating, Ready, Failed(String), Expired }
```

A **chunk reply** is a `Sample` whose key carries the chunk index and whose `ZBytes` payload is the chunk
bytes; the **manifest reply** is the JSON `Manifest` (distinguished by an encoding/attachment marker). One
query → one manifest + the requested chunks.

> **Borrow from sendit: metadata-in-key.** Instead of (or alongside) packing `seq` into the payload, encode
> chunk metadata *in the key expression* — `…/report/<id>/__chunk/<total_len>/<chunk_count>/<index>` — as
> sendit does (`{topic}/__chunk/{total_size}/{total_chunks}/{index}`). The index/total are then visible to
> Zenoh routing/selectors and trivially parseable by the client (no payload framing), which also makes the
> range-resume selector natural. **Keep the SHA-256 in the manifest** (sendit omits a content hash — its
> integrity check is only "all indices present + length matches"; we want a real hash, R4). Use **256 KB–1 MB
> chunks**, not sendit's 64 MB, so progress is fine-grained and per-chunk memory stays small (R2/R3).

### 5.3 Flow

1. **GUI → request.** Operator clicks "Download debug report" on a host → GUI PUTs `ReportRequest` to
   `…/@/report/request`.
2. **Sensor generates off-thread.** The sensor builds the bundle in a `spawn_blocking` task (tar+zstd of
   configs + a redb store export + recent logs + optional pcap), writes it to a **temp file**, computes
   SHA-256 and the chunk count, and registers a `Manifest` keyed by `id` (kept for a TTL, e.g. 10 min,
   then the temp file is deleted → `Expired`). Status is observable via `…/@/report/status`.
3. **GUI → download.** GUI `get`s `…/@/report/<id>?from=0`. The queryable handler streams the manifest
   reply, then reads the temp file chunk-by-chunk (`chunk_size`, e.g. **256 KB–1 MB**), `query.reply(...)`
   each with `CongestionControl::Block` (backpressure, R6). Memory stays O(chunk_size) on both ends.
4. **GUI reassembles** to a temp file, updates a progress bar from `seq / chunk_count`, verifies length +
   SHA-256 (R4), then prompts a native save dialog (ties into the existing export work, issue #37).
5. **Resume (R5).** On a dropped transfer or GUI restart, re-`get` with `?from=<next_missing>`; chunks are
   content-addressed by `(id, seq)` so already-received chunks are skipped. The sensor serves any range
   from the still-cached temp file until TTL.

### 5.4 Safety / authz (R7)

- Only the **request command** triggers generation — never a passive subscription. Validate `kind`/`opts`.
- **Bound** the bundle (max size, max pcap seconds, max log MB) in sensor config; refuse/trim beyond.
- **Rate-limit** report generation per sensor (1 in flight, cooldown). Report generation runs in
  `spawn_blocking` so it never stalls the capture/poll path.
- Redact: the debug bundle should exclude secrets (filter config values, no raw credentials).

### 5.5 Frontend (`Fetch<T>`-adjacent)

A `BlobFetch` state machine mirroring the existing `Fetch<T>`, with explicit pause/cancel (§5.9):

```
Idle → Requesting → Generating(progress?) → Downloading{got,total}
                                              ├─(pause)→  Paused{got,total}  ─(resume)→ Downloading
                                              ├─(cancel)→ Cancelled  (discard .part)
                                              └─(done)→   Verifying → Saved(path) | Failed
```

Reuses the `query_*` plumbing the redesign report proposes to unify (#126/#127). A progress bar with a
**Pause/Resume** toggle + a **Cancel** button; a "Save as…" dialog on completion. Paused/partial transfers
are persisted (a `.part` + a tiny resume-state sidecar), so a "Downloads" list can offer **resume after a
GUI restart**.

### 5.6 Effort

- `zensight-common`: types + a `report_key()` helper — **S**.
- `zensight-sensor-core`: a reusable `report` module (generate→tempfile→manifest→chunked queryable),
  so every sensor gets it for free — **M**.
- Per-sensor `kind` content (what goes in the bundle) — **S** each.
- Frontend `BlobFetch` + progress UI + save dialog — **M** (shares plumbing with #37/#127).

Total ≈ a focused **M**. No new runtime dependency, no daemon, no storage volume.

### 5.7 Tier 2 — directories + interruptible/resumable sync (content-addressed, casync-style)

For the **rsync-like** ask (R9–R11: pull a whole **directory tree**; survive connection loss *and* restart;
dedup), generalize Tier 1 from "ordered chunks of one blob" to a **content-addressed chunk store + a tree
index** — the [casync](https://github.com/systemd/casync) model (chunk store named by content hash + a chunk
**index**; [desync](https://github.com/folbricht/desync) is a Go reimplementation). Mapped onto Zenoh:

**Keyspace**
```
zensight/<proto>/@/store/<algo>/<hex-hash>   # queryable: GET a single chunk by its content hash.
                                             #   Immutable + content-addressed ⇒ safely cacheable;
                                             #   a router storage backend can cache it fleet-wide.
zensight/<proto>/@/tree/<snapshot-id>        # queryable: GET the index (the serialized tree manifest).
```

**The index** (JSON/CBOR; or a casync-style `catar` tree serialization + a `caidx` chunk list): a Merkle-y
description of the tree —
```rust
struct TreeIndex {
    id: Ulid, root_hash: [u8;32], algo: HashAlgo, chunk_size_policy: ChunkPolicy,
    entries: Vec<Entry>,            // depth-first
}
enum Entry { Dir { path, mode, mtime },
             File { path, mode, mtime, size, chunks: Vec<ChunkRef> },   // ordered content hashes
             Symlink { path, target } }
struct ChunkRef { hash: [u8;32], len: u32 }
```

**Client algorithm (stateless server, persistent client — this is what makes it interruptible):**
1. GET `@/tree/<id>` → the index. Compute `needed = ∪ entries.chunks.hash`.
2. `missing = needed − have` where `have` = hashes already in the **local on-disk content store** (from a
   prior interrupted run *or* an earlier report — this is the dedup + resume substrate).
3. Fetch each `missing` hash via `@/store/<algo>/<hash>`, **bounded in-flight** (e.g. 8 concurrent),
   `CongestionControl::Block`. **Verify each chunk by re-hashing on receipt** → corruption is impossible;
   write it atomically into the content store.
4. Reconstruct files from their ordered `chunks`, set `mode`/`mtime`, recreate dirs/symlinks.
5. Verify the tree `root_hash` (Merkle) end-to-end.

**Why this nails R9–R11**
- **R9 directories:** the index *is* the tree; one operation pulls the whole thing.
- **R10 interruptible across reconnect *and* restart:** progress is *not* a cursor — it's simply *which
  content hashes are on disk*. A dropped connection or a killed GUI loses nothing; on resume you recompute
  `missing` and continue. The server keeps **no per-client state** (every `@/store/<hash>` GET is
  stateless and idempotent) — the [zsync](http://zsync.moria.org.uk/) property, which is exactly why
  content-addressing beats a byte-offset cursor for unreliable links.
- **R11 dedup:** identical chunks (same hash) transfer once; retain the local store and a *re-download / new
  report version* only pulls the chunks that actually changed — the casync win.

**Chunking choice.** Start with **fixed-size** content-addressed chunks (256 KB–1 MB): simplest, already
gives resume + identical-chunk dedup. Adopt **content-defined chunking ([FastCDC](https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia),
gear-hash)** later if cross-*version* dedup matters — CDC fixes rsync's "byte-shift problem" (insert one
byte ⇒ every fixed block downstream changes) and is ~30× faster than rsync's rolling adler-32. **Do not
implement rsync's rolling-checksum delta:** it's designed for bidirectional in-place file updates, suffers
the byte-shift problem, and is subsumed by CDC + a content-addressed store for a pull-only download.

**Where the chunk store lives** — two options, pick per deployment:
- **Sensor-side temp store** (default): the sensor content-chunks the report/dir into a TTL'd local store
  and serves `@/store/<hash>`. Simple; chunks vanish after TTL.
- **Router-side Zenoh storage backend** (Option E becomes genuinely valuable here): the sensor `PUT`s chunks
  into a router-hosted filesystem/RocksDB/S3 storage keyed by hash; any GUI GETs them, they're cached
  fleet-wide, survive sensor restart, and dedup across *all* sensors. This is the natural growth path.

**Relationship to Tier 1.** Tier 1 (§5.1–5.6) is the degenerate case (one file, ordered chunks, no store).
Ship Tier 1 first for the debug report; grow into Tier 2 by adding the index + the content-addressed
`@/store` queryable when an actual *directory/dataset* pull is needed (e.g. a captured pcap set, a `/etc`
snapshot, a model/data bundle). **zenoh-fs is the closest prior art to Tier 2** (content-addressed +
delay-tolerant fragments) — read it here for ideas, but it's still a daemon/DFS, so extract, don't depend.

**Effort:** Tier 2 ≈ **L** (content store + index + reconstruct + resume; +FastCDC is a further increment).

### 5.8 Does resume work for a *single large* file? Yes — and it matters most there

A large file (a multi-GB pcap, a big bundle) is just a **long chunk list**, so resume is **per-chunk, not
per-file**: on reconnect you re-fetch only the chunks you don't already have — never the whole file. This is
the case where resume earns its keep (you don't want to restart a 5 GB transfer from zero). Two rules make
it robust:

1. **Persist partial progress to disk, not memory.** Write each chunk straight to a `.part` file (Tier 1) or
   the content store (Tier 2) *as it arrives*, and record what's done. Then a dropped connection **or a GUI
   process restart** both resume:
   - **Tier 2** gets this for free — progress *is* "which content-hashes are on disk"; recompute `missing`
     and continue.
   - **Tier 1** keeps a `.part` file + derives the resume point from its length (`from = len/chunk_size`,
     re-verifying the last chunk), or a small sidecar bitmap for out-of-order/parallel fetch.
   The hard rule (R2): **never `read_to_end` a multi-GB file** on either side — the sensor streams chunks
   from disk; the client writes chunks to disk. Bounded RAM = O(chunk_size × in-flight), independent of file
   size. Writing-as-you-go is *also* what enables restart-resume — the two requirements are the same rule.
2. **The source must still serve the missing chunks after the gap.** Resume window = how long the data stays
   available at the source: a sensor-side **TTL'd** temp report (short window — fine for an interactive
   download) vs a **content-addressed store / router storage backend** (long window, survives sensor restart,
   fleet-wide). For large, long-lived artifacts, prefer **Tier 2 + a storage backend**.

**Integrity scales:** each chunk is verified by re-hashing on receipt, so a bad/partial chunk is detected and
re-fetched (not silently corrupting a 5 GB output); the manifest SHA-256 / tree Merkle root verifies the
whole assembly at the end. **No Zenoh size ceiling:** nothing is ever sent as one payload — the per-chunk
payload stays 256 KB–1 MB regardless of total file size, and Zenoh's reliable transport handles
wire-level fragmentation under that.

> Caveat to design for: if a chunk's source *changes* between the interruption and the resume (e.g. a sensor
> regenerated the report), Tier 1's `?from=K` could splice mismatched halves. Guard it by binding resume to
> the **manifest id + SHA-256** (Tier 1) or by content-addressing (Tier 2, where a changed byte simply means
> different chunk hashes and the stale ones are skipped) — i.e. **content-addressing makes large-file resume
> correct by construction**, which is another reason Tier 2 is the better home for big artifacts.

### 5.9 Pause & cancel (R12 / R13)

Because the design is already chunk-granular and resumable, pause/cancel are mostly **client state + a prompt
stop** — not new protocol:

- **Pause = an intentional interruption that *keeps* state.** Stop issuing new chunk fetches, **keep** the
  `.part` / content-store chunks, and persist a tiny resume-state (manifest id + SHA-256 + chunks done).
  Resume = the normal resume path (§5.8), so a paused transfer **survives a GUI restart** and can be resumed
  from a Downloads list.
  - *Tier 1:* the clean pause is to **drop the in-flight `get`** — *not* merely stop reading, which with
    `CongestionControl::Block` would stall the stream and tie up the sensor. Resume issues a fresh
    `…/report/<id>?from=K`.
  - *Tier 2:* just stop the fetch loop; the content store persists; resume recomputes `missing`.
- **Cancel = stop + *discard*.** Stop fetching, delete the partial, go to `Cancelled`. Optionally PUT
  `…/@/report/<id>/cancel` so the **sensor frees its temp artifact immediately** instead of waiting for the
  TTL (matters for big reports / scarce disk). Cancel is best-effort on the source; the TTL is the backstop.

**Server cooperation — the one real design point.** The producer must stop **promptly** when the client goes
away or cancels (don't keep streaming a 5 GB file to a gone GUI):
- *Tier 1 (queryable):* between chunk replies, check the **query's liveness** (a dropped `get` finalizes the
  query → `query.reply()` errors) and/or a per-transfer **cancel token** (set by the `…/cancel` PUT); stop on
  either. This is *why* Tier 1 streams **lazily** — read+reply one chunk at a time — rather than enqueuing the
  whole file.
- *Tier 2 (content-addressed):* trivial — the server is **stateless**; the client just stops GETting
  `@/store/<hash>` chunks. Nothing to cancel server-side; orphaned chunks expire by TTL / are GC'd.

So pause/resume and cancel **reuse the resume + TTL machinery**; the only additions are (a) a clean
prompt-stop of the in-flight stream, (b) persist-on-pause vs delete-on-cancel, and (c) the optional
`…/cancel` hint. **Effort: S** on top of Tier 1/2.

---

## 6. Where should it live? — repo & packaging strategy

**Short answer: split it — a generic, reusable transfer crate + a thin ZenSight adapter — and *incubate it
in-tree first, graduate it to its own repo later*.**

### Does it have a life outside ZenSight? Yes.

The protocol (resumable, content-addressed chunk/tree transfer over Zenoh) is **not ZenSight-specific** —
it's the library the Zenoh ecosystem is missing (twice-confirmed: zenoh-fs and sendit are both
apps/experiments, neither a consumable library). Plausible external users: robotics/edge **data offload**
(rosbag/pcap/model pulls), **OTA / image distribution** (the casync use case), **log/artifact shipping**,
any Zenoh app that needs "download this big thing, resumably." That external value is the argument for a
**standalone, generic crate** rather than burying the logic inside ZenSight.

### The layered split (regardless of repo layout)

- **`zenoh-blob` / `zenoh-transfer`** — *generic*, zero ZenSight dependency: the protocol (manifest +
  ordered chunks for Tier 1; `@/store/<hash>` + tree index for Tier 2), `BlobServer` / `BlobClient`,
  resume/pause/cancel, progress events, pluggable hash + chunker (fixed → FastCDC), its **own** configurable
  key prefix and its **own** serde. Apache-2.0/MIT, publishable to crates.io. Grows `zenoh-blob` (Tier 1) →
  `zenoh-casync` semantics (Tier 2).
- **ZenSight adapter** (`zensight-sensor-core::report` + the `BlobFetch` GUI state) — *ZenSight-specific*
  glue: maps the generic transfer onto the `zensight/<proto>/@/report` keyspace, the operator command/authz
  channel, what goes *in* a debug bundle, the redb store as the client content-store, and the Iced UI
  (progress bar, pause/resume/cancel, Downloads list). **No protocol logic here** — just wiring.

This boundary is the win: the generic crate is reusable and open-sourceable; the ZenSight-flavoured
keyspace/auth/UI stays in ZenSight.

### Repo layout — incubate, then graduate

| Option | When | Trade-off |
|---|---|---|
| **Integrate directly** (a module in `zensight-sensor-core`, no separate crate) | only if you're sure it'll never be reused & you want minimum ceremony | fastest, but couples protocol to ZenSight types → not reusable, hard to extract later |
| **Incubate in-tree** (a `zenoh-blob` **workspace crate** in the ZenSight monorepo, generic API, + the adapter) ✅ | **now** | one repo / one CI / atomic changes during the churny early phase, *and* a clean generic boundary so extraction is trivial later |
| **Separate repo from day 1** (own repo + crates.io + ZenSight depends on it) | once the API is stable / you commit to open-sourcing | reusable + publishable, but cross-repo churn + release dance while the API is still moving |

**Recommended: incubate in-tree as a generic workspace crate now** (keep it strictly free of ZenSight
types), then **`git filter-repo`/subtree-extract it to its own repo + publish to crates.io** once the API
settles and you decide to open-source. Best of both: fast iteration today, reuse + community later, clean
separation throughout. (Avoid "integrate directly" unless you're certain it's ZenSight-only — re-extracting
ZenSight types out of the protocol later is the expensive path.)

### On the prior art

**Extract ideas, don't fork.** Borrow (both Apache-2.0 — attribute): zenoh-fs's manifest/fragment/resume
bookkeeping + content-addressing, **sendit's metadata-in-key**, casync's index/chunk-store split — but skip
zenoh-fs's daemon/DFS surface and sendit's egui coupling + broadcast model, and add the SHA-256 sendit
lacks. Forking a whole project only makes sense for a *different* goal (zenoh-fs → a delay-tolerant
directory-sync DFS; sendit → a standalone drag-and-drop app) — neither is this use case.

---

## 7. Risks & watch-items

- **Queryable long-lived replies:** ensure the sensor streams replies promptly and respects the query's
  lifetime; very slow generation should return `Generating` on the *status* channel and let the client
  poll/re-`get`, rather than holding one query open for minutes.
- **Backpressure correctness:** use `CongestionControl::Block` on chunk replies; verify under a constrained
  link that the sensor blocks (doesn't buffer unboundedly) — the whole point of streaming from a temp file.
- **TTL vs resume:** the resume window equals the temp-file TTL; document it and surface `Expired` clearly.
- **Routed vs peer:** confirm behavior both peer-to-peer (loopback `just run`) and via a router; reliable
  channels are per-hop, so a long route just means more retransmit points — fine, but test it.
- **Memory:** never `read_to_end` the report on either side; stream by chunk (the one rule that keeps a
  100 MB pcab from OOMing a sensor).

---

## 8. Decision

> **Build Option C, in two tiers.**
> - **Now — Tier 1** (§5.1–5.6): a purpose-built `@/report` chunked blob transfer in `zensight-sensor-core`
>   + a `BlobFetch` client; stable queryable multi-reply + `CongestionControl::Block`, **256 KB–1 MB chunks**,
>   **metadata-in-key** (from sendit) + a **SHA-256 manifest** (which sendit lacks). Ships the debug-report
>   download.
> - **When directory/dataset pull is actually needed — Tier 2** (§5.7): the **content-addressed chunk store
>   + tree index** (casync model over Zenoh) for whole directories, true reconnect-*and*-restart resume, and
>   dedup; back it with a router **storage backend** (Option E) for fleet-wide caching. **Don't** implement
>   rsync's rolling-checksum delta — content-defined chunking (FastCDC) subsumes it.
>
>
> **Packaging (§6):** build it as a **generic `zenoh-blob` workspace crate** (zero ZenSight types) + a thin
> **ZenSight adapter** (keyspace/auth/UI). **Incubate in-tree now**, then **graduate it to its own repo +
> crates.io** once the API stabilizes — it has real reuse value outside ZenSight (edge data offload, OTA,
> artifact shipping), and the gap is confirmed (lots of apps, no library).
>
> **Pause / cancel (§5.9):** ~free given resume — pause = keep partial + persist resume-state (resumable
> across restart); cancel = discard + optional `…/cancel` hint to free the source's temp file; the producer
> stops promptly via query-liveness/cancel-token (Tier 1) or statelessly (Tier 2). Effort **S**.
>
> **Do not depend on or fork zenoh-fs or sendit** — read them (and casync/desync/zsync) for ideas, attribute
> the Apache-2.0 designs.

If you agree, I can file this as an issue under the redesign epic (#94, Wave-1/2) and sketch the
`zensight-common` types + the `zensight-sensor-core::report` module + the `BlobFetch` state machine.

---

## Sources

- zenoh-fs: [repo](https://github.com/kydos/zenoh-fs) (facts via GitHub API: zenoh `1.7.2`+`internal`, last commit 2026-01-22, 2★, Apache-2.0, unpublished)
- sendit / `send-it`: [repo](https://github.com/dad-io/sendit) (facts via GitHub API: zenoh `1.0`+`unstable`, last commit 2026-06-12, 5★, Apache-2.0, egui **app** not a lib; `transfer.rs` metadata-in-key chunk protocol, 64 MB chunks, no content hash, broadcast/store-backed model)
- Zenoh concepts / 1.0 migration: [migration_1.0/concepts](https://zenoh.io/docs/migration_1.0/concepts/) · [Firesong 1.0.0](https://zenoh.io/blog/2024-10-21-zenoh-firesong/)
- Reliability & congestion control (Block/Drop/Block-first; reliable channel; fragmentation): [Zenoh reliability blog](https://zenoh.io/blog/2021-06-14-zenoh-reliability/)
- Storage backends: [storage-manager plugin](https://zenoh.io/docs/manual/plugin-storage-manager/) · [filesystem backend](https://github.com/eclipse-zenoh/zenoh-backend-filesystem) · [RocksDB backend](https://github.com/eclipse-zenoh/zenoh-backend-rocksdb) · [S3 backend](https://zenoh.io/blog/2023-07-17-s3-backend/)
- Shared memory / large payloads: [Zenoh 1.6 Imoogi](https://zenoh.io/blog/2025-10-20-zenoh-imoogi/) · [Zenoh-Pico fragmented-packet perf](https://zenoh.io/blog/2025-04-09-zenoh-pico-performance/)
- Zenoh home / repo: [zenoh.io](https://zenoh.io/) · [eclipse-zenoh/zenoh](https://github.com/eclipse-zenoh/zenoh)
- **rsync-like / directory + resumable (Tier 2):** [casync (Poettering)](https://github.com/systemd/casync) · ["casync — distributing filesystem images" (0pointer)](https://0pointer.net/blog/casync-a-tool-for-distributing-file-system-images.html) · [LWN: distributing images with casync](https://lwn.net/Articles/726625/) · [desync (Go reimpl.)](https://github.com/folbricht/desync) · [zsync (stateless resumable download)](http://zsync.moria.org.uk/) · [FastCDC (USENIX ATC '16)](https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia) · [rolling hash / rsync algorithm](https://en.wikipedia.org/wiki/Rolling_hash) · [Intro to Content-Defined Chunking](https://blog.gopheracademy.com/advent-2018/split-data-with-cdc/)
