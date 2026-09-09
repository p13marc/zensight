# Releasing ZenSight

The procedure is short, but it has two traps that a careless bump walks straight into
(§2). It was reconstructed from commit archaeology during 0.8.0 prep and rewritten for
the Forgejo pipeline during 0.9.0 prep — keep it current.

Versioning is pre-1.0 `0.MINOR.PATCH`: **the minor is the breaking slot.** Any `!` commit
since the last tag means the minor moves.

**There is no 1.0 until the software has been battle-tested by the community.** Not until
fleets outside this project have run it in production, long enough to have found what one
reference fleet cannot. Every other criterion — the feature milestones landed, two
consecutive minors with no breaking change, the sizing measured, the demo runnable by a
stranger — is something this repository can satisfy on its own, which is precisely why none
of them is sufficient. A 1.0 is a promise made *to* other people; it cannot be earned by
talking to yourself. (The full list, and what each surface actually promises today, is
[`docs/COMPATIBILITY.md`](docs/COMPATIBILITY.md) — #943.)

So: **do not schedule a 1.0, and do not milestone one.** The readiness work that has to
happen first is milestoned `0.18.0` (#903), and it is deliberately not called 1.0. Keep
cutting `0.MINOR` releases until the evidence exists, then write the release notes that
say what the evidence was.

CI is **Forgejo Actions** (`.forgejo/workflows/`), releases live on the Forgejo instance
(`https://git.marcpardo.eu/marcpardo/zensight`), GitHub is a passive push mirror. `gh`
does not talk to this forge — use the Actions tab / releases page in the web UI (over the
management VPN) or the Forgejo API.

## 0. Preconditions

```bash
git checkout master && git pull
git status                       # clean
# CI on master must be green at the exact commit you intend to tag:
#   https://git.marcpardo.eu/marcpardo/zensight/actions  (workflow: CI)
```

`ci.yml` also runs on tags (since 0.9.0 prep), so tagging re-runs the suite — but that is
a parallel signal, not a gate: `release.yml` does not wait for it. Tag only a commit whose
master CI already passed.

> **There is no dry run any more.** This paragraph used to say that
> `workflow_dispatch` builds everything and publishes nothing "because every
> push/upload step is gated on the ref being a tag". That gating is gone:
> `release.yml` now resolves `RELEASE_TAG`/`RELEASE_SHA` from a **required**
> `tag` input and every upload uses it unconditionally. Dispatching the
> workflow **publishes the release and pushes the images**. It is the
> re-run-a-failed-release path (see the workflow's own header comment), not a
> rehearsal.
>
> Corrected 2026-08-31, during the 0.12.0/0.13.0 cuts, by reading the workflow:
> there is no `startsWith(github.ref, 'refs/tags/')` left anywhere in it.

## 1. CHANGELOG.md

Rename `## [Unreleased]` → `## [X.Y.Z] - YYYY-MM-DD`, add a fresh empty `[Unreleased]`.

Check it against reality rather than trusting it — the changelog is a **purely human
artifact**: `forgejo-release` does not generate a release body and CI never reads
CHANGELOG.md. Nothing fails if it is wrong.

```bash
git log --oneline <prev-tag>..HEAD | grep '!'   # every breaking change — all must be documented
git log --oneline <prev-tag>..HEAD | wc -l      # scale check
```

Entries written before a later refactor go stale silently — 0.8.0 shipped with a parallax
entry describing a control plane that three commits had since replaced. Re-read the entries
you are *keeping*, not just the ones you are adding.

Also update `flatpak/com.github.p13marc.ZenSight.metainfo.xml`: its `<releases>` block
**is maintained again since 0.9.0** — add a `<release version="X.Y.Z" date="…">` entry
(one summary paragraph + a link to the CHANGELOG anchor).

## 2. Version bump — 3 files + the lock

| File | Note |
|---|---|
| `Cargo.toml` (`[workspace.package] version`) | the 25 normal crates inherit this |
| `zensight-sensor-netlink-ebpf/Cargo.toml` | **hardcodes its version — does not inherit** |
| `zensight-sensor-sysinfo-ebpf/Cargo.toml` | **hardcodes its version — does not inherit** |

> **Trap 1.** The two eBPF crates are the only 2 of the 27 member manifests that do not use
> `version.workspace = true`. They are `publish = false`, but every prior release moved them
> and a mismatch is confusing. Verify with:
> ```bash
> for f in $(find . -name Cargo.toml -not -path './target/*' -mindepth 2); do
>   grep -qE '^version\.workspace = true' "$f" || echo "$f"
> done
> ```
> The `^` is load-bearing. This check shipped for several releases as an unanchored
> `grep -q 'version.workspace = true'`, which also matches `rust-version.workspace = true`
> — present in both hardcoding manifests — so it printed nothing and reported the trap
> closed while it stood open. It must print exactly the two eBPF manifests.

Then regenerate the lock:

```bash
cargo check --workspace     # updates Cargo.lock for the 27 workspace crates
```

> **Trap 2.** **Never `sed` `Cargo.lock`.** Several unrelated third-party crates
> (`tower-http`, `sigma-rust`, `radium`, `nanorand`, `pem-rfc7468`, `jpeg-encoder`) have
> coincidentally sat on the same version as the workspace. A blind substitution corrupts them.

Also: `docs/design/*.md` mention old versions as *historical prose* ("implemented in 0.7.0").
Do not bump those.

## 3. Land it

One commit on a release branch, mirroring 0.6.x:

```bash
git checkout -b release/X.Y.Z
git commit -am "chore(release): X.Y.Z"
# open the PR on Forgejo (or push and merge fast-forward), merge once CI is green
```

Expected diff: `CHANGELOG.md`, `Cargo.toml`, `Cargo.lock`, the two `-ebpf/Cargo.toml`,
`flatpak/…metainfo.xml`.

## 4. Tag

```bash
git checkout master && git pull
git tag -a X.Y.Z -m "ZenSight X.Y.Z — <theme>"
git push origin X.Y.Z
```


> **The release now WAITS for CI on the tag's own commit (#1095).** `release.yml`
> opens with a `gate` job that polls `/commits/<sha>/status` until every one of
> `ci.yml`'s jobs has reported success, and every other job needs it. Before
> this, images were built, smoke-tested and **pushed** while the suite might
> still be running or red — `ci.yml`'s own header calls the tag run a "parallel
> signal, not a gate". Expect the release run to sit in `gate` for as long as
> the suite takes; it prints `combined state=… over n/6 status(es)` each minute
> so you can see it waiting rather than hung. It refuses on a red suite and
> times out after 80 minutes with instructions.

> **`:latest` only moves for the newest release (#1095).** Re-dispatching an
> older tag builds and attaches everything as before but leaves `:latest`
> where it is, saying so in the log. Every quadlet in `packaging/` pulls
> `:latest`, so the old behaviour meant a re-run of 0.11.0 rolled the whole
> fleet back. The newest tag is read from the API, not from the job's shallow
> clone, and an unreadable tag list leaves `:latest` alone rather than guessing.

> **The tag takes no `v` prefix.** `release.yml` triggers on `[0-9]+.[0-9]+.[0-9]+`;
> `v0.8.0` matches nothing and silently does nothing. Tags are **annotated** (`-a`), message
> `ZenSight <version>[ — <theme>]`.

> **Give the runner time before concluding the tag did nothing.** There is one
> runner and jobs queue serially across every open PR, so the tag's own runs can
> take a while to appear — during the 0.13.0 cut they were mistaken for runs that
> were never created, and the release was then dispatched by hand for no reason.
> Confirm with a *large* listing rather than the top of the newest-first page:
>
> ```bash
> curl -H "Authorization: token $TOKEN" \
>   "https://git.marcpardo.eu/api/v1/repos/marcpardo/zensight/actions/tasks?limit=200" \
>   | jq -r '.workflow_runs[] | select(.head_branch=="X.Y.Z") | "\(.name) \(.status)"'
> ```
>
> A completed tag push produces the whole CI suite **plus** `release`, `images`,
> `flatpak` and `checksums`, all with `event: push` on the tag's own ref.

## 5. Watch and finish

Watch the run in the Actions tab. `release.yml` produces, all amd64-only:

- **source tarball** + `SHA256SUMS` (release assets);
- **`zensight-<ver>-linux-amd64.tar.gz`**: all 18 binaries (14 sensors, 2 exporters,
  correlator, historian) with an internal `SHA256SUMS`, the `packaging/systemd/` units, and the
  example configs — the native-install path. There is no separate `.tar.gz.sha256`
  asset; the `checksums` job publishes one release-wide `SHA256SUMS`;
- **19 container images** at `git.marcpardo.eu/marcpardo/<name>:{<ver>,latest}`:
  `zensight-sensor-{logs,sysinfo,snmp,gnmi,modbus,netflow,netlink,netring,systemd,hostspec,pve,container,probe,parallax}`,
  `zensight-exporter-{prometheus,otel}`, `zensight-correlator`, `zensight-historian`, and the all-in-one
  `zensight-sensors` bundle (the six host sensors; parallax stays out of it on purpose —
  see the 0.14.0 changelog);
- **flatpak**: an unsigned `zensight-<ver>.flatpak` bundle on the release, plus a
  force-push of the OSTree export to the repo's `flatpak-export` branch — vm-edge's
  `deploy-flatpak.timer` picks that up within ~5 min, GPG-signs it, and publishes to
  `https://flatpak.marcpardo.eu`.

There is **no crates.io publish** and **no deb/rpm** anywhere in the pipeline (deb/rpm
retired with the GitHub pipeline).

`forgejo-release` does not auto-generate a release body — write it by hand from the
CHANGELOG entry, pointing at the migration notes on a breaking release.

Post-release spot checks:

```bash
podman pull git.marcpardo.eu/marcpardo/zensight-sensors:<ver>   # pulls + runs --help
curl -LO https://git.marcpardo.eu/marcpardo/zensight/releases/download/<ver>/zensight-<ver>-linux-amd64.tar.gz
flatpak remote-ls marcpardo | grep ZenSight                      # after ~5 min
```

## Migration: re-keying the alert state on upgrade (#737)

**Applies to the release that carries #736** (the normative RFC 11 §3.1
`alert_key`), and to any future release whose CHANGELOG says the alert-key
derivation moved. Nothing else in this file is version-specific; this section
is, deliberately.

**Why there is anything to do.** Alerts are **LWW state** at
`<base>/v1/<origin>/state/<producer>/alert/<alert_key>` — the key *is* the
identity. Change the derivation and every alert that was firing at the moment
of the upgrade is stranded: the upgraded producer publishes its `Resolved` and
its `Delete` tombstone on the **new** key, so the old key is never written
again. If a Zenoh storage is holding that key, it holds it forever — a phantom
alert in every GUI that seeds from the storage, with nothing logged anywhere
to explain it.

**Since #882, the producers do this for themselves.** On startup every sensor
GETs its own `state/<producer>/alert/*` selector and takes ownership of what it
finds: it adopts what it can still claim, and tombstones what it cannot — a
document whose key does not match the `alert_key` its own payload derives (this
migration, exactly), a `Resolved` whose tombstone was lost, and a rule the build
no longer has. So the ordinary rollout clears the phantoms as each producer
restarts, and it logs a line saying how many.

The manual sweep below remains the answer for what a running producer cannot
reach: **keys whose producer will never start again** — a sensor retired from
the fleet, or one whose `origin` changed. Run it after the rollout, on whatever
`zenctl` still finds.

**Who needs this.**

| Deployment | Sweep needed? |
|---|---|
| `just run`, `just demo-*`, the e2e suites | **No.** No storage, no persistence — state lives only in the publishers, which restart. |
| A fleet with **no** Zenoh storage on `v1/*/state/**` | **No.** Same reason: the only copy of an alert is its live publisher. |
| A fleet with a storage on `v1/*/state/**` (`configs/router-*.json5`) whose producers all come back | **Rarely.** Each producer clears its own on restart; sweep only to verify. |
| The same, with a producer that is being **retired** | **Yes.** Nothing will ever reclaim its keys. |

**Order matters: sweep *after* every publisher is upgraded.** A single sensor
still on the old build re-publishes its firing alerts on old-shaped keys within
its next evaluation cycle, so a sweep run mid-rollout deletes keys that
immediately come back — and now you cannot tell a leftover from a live one.
Upgrade the whole fleet, confirm no old-version producer remains
(`zenctl node list`, or the GUI's fleet view, which flags version skew), and
only then sweep.

**The sweep is GET-then-delete, one concrete key at a time.** RFC 04 §1.2
refuses a wildcard delete as an operator act, and for a good reason: `delete
v1/*/state/*/alert/*` cannot distinguish a stranded old key from an alert that
fired one second ago, and there is no undo. So:

1. **Enumerate.** GET the selector `v1/*/state/*/alert/*` and collect the
   concrete key of every reply. A storage answers one reply per stored key.
2. **Delete each concrete key**, individually — the key the GET replied on,
   verbatim, never a pattern built from it.

**Use `zenctl`, not a one-shot binary.** `zenctl`'s `Publication::retire`
already implements exactly this, and its `check_retire` already refuses the
unsafe shapes (a wildcard in the key to delete, a selector that is not a state
selector). Adding a sweep tool to this tree would be a second, less careful
implementation of a destructive operation that already exists — and it would
be dead code the moment the migration is over.

**Verify.** After the sweep, GET `v1/*/state/*/alert/*` again and check that
every remaining key is one a currently-running producer will claim: cross-check
against the fleet's live firing set (the same selector answered by the
producers themselves, which is what a GUI seeds from). A key no live producer
answers is a leftover.

## Notes

- **Every sensor crate is packaged since #512** (parallax was the last holdout). If you
  add a sensor crate, add it to `release.yml`'s **three lists**
  (the `-p` build list, the staging `cp` loop, the image loop), to
  `docker/Dockerfile.sensors-runtime`'s COPY list if it belongs in the bundle, and to
  `packaging/systemd/` (with `ExecStart=/usr/bin/…` like the others). Nothing
  asserts these stay in lockstep any more (the old `rust.yml` sensor-count guard died with
  the GitHub pipeline) — check by hand.
- The `images` job runs inside `rust:1.98-bookworm` **on purpose**: the binaries must link
  against the same glibc (2.36) as the `debian:bookworm-slim` runtime base. Don't "simplify"
  it back to building on the act ubuntu-24.04 image (glibc 2.39) — that's a load-time
  failure shipped to every host, and the in-image smoke step will catch it.
- rustc is pinned to **1.98** (root `rust-toolchain.toml`, ci.yml, the images container) in
  lockstep with the whole cluster (see myserver docs) — bump everywhere together.
