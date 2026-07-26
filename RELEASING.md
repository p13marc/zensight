# Releasing ZenSight

The procedure is short, but it has two traps that a careless bump walks straight into
(§2). It was reconstructed from commit archaeology during 0.8.0 prep and rewritten for
the Forgejo pipeline during 0.9.0 prep — keep it current.

Versioning is pre-1.0 `0.MINOR.PATCH`: **the minor is the breaking slot.** Any `!` commit
since the last tag means the minor moves.

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

**Dry-run the release pipeline first** when packaging or workflows changed (and always
until the pipeline has a few tagged runs behind it): trigger `release.yml` via
`workflow_dispatch` (Actions tab → Release → Run workflow, on master). It builds
everything — binaries in the `rust:1.97-bookworm` container, all images plus an in-image
smoke test, the tarball, the flatpak — but publishes nothing (every push/upload step is
gated on the ref being a tag).

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
| `Cargo.toml` (`[workspace.package] version`) | the 22 normal crates inherit this |
| `zensight-sensor-netlink-ebpf/Cargo.toml` | **hardcodes its version — does not inherit** |
| `zensight-sensor-sysinfo-ebpf/Cargo.toml` | **hardcodes its version — does not inherit** |

> **Trap 1.** The two eBPF crates are the only 2 of 24 that do not use
> `version.workspace = true`. They are `publish = false`, but every prior release moved them
> and a mismatch is confusing. Verify with:
> ```bash
> for f in $(find . -name Cargo.toml -not -path './target/*' -mindepth 2); do
>   grep -q 'version.workspace = true' "$f" || echo "$f"
> done
> ```

Then regenerate the lock:

```bash
cargo check --workspace     # updates Cargo.lock for the 24 workspace crates
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

> **The tag takes no `v` prefix.** `release.yml` triggers on `[0-9]+.[0-9]+.[0-9]+`;
> `v0.8.0` matches nothing and silently does nothing. Tags are **annotated** (`-a`), message
> `ZenSight <version>[ — <theme>]`.

## 5. Watch and finish

Watch the run in the Actions tab. `release.yml` produces, all amd64-only:

- **source tarball** + `SHA256SUMS` (release assets);
- **`zensight-<ver>-linux-amd64.tar.gz`** (+ `.tar.gz.sha256`): all 12 binaries
  (9 sensors, 2 exporters, correlator), the `packaging/systemd/` units, and the example
  configs — the native-install path;
- **13 container images** at `git.marcpardo.eu/marcpardo/<name>:{<ver>,latest}`:
  `zensight-sensor-{logs,sysinfo,snmp,gnmi,modbus,netflow,netlink,netring,systemd}`,
  `zensight-exporter-{prometheus,otel}`, `zensight-correlator`, and the all-in-one
  `zensight-sensors` bundle;
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

## Notes

- **Not every workspace member is packaged.** `zensight-sensor-parallax` ships in no
  artifact (see #512). If you add a sensor crate, add it to `release.yml`'s **three lists**
  (the `-p` build list, the staging `cp` loop, the image loop), to
  `docker/Dockerfile.sensors-runtime`'s COPY list, and to `packaging/systemd/`. Nothing
  asserts these stay in lockstep any more (the old `rust.yml` sensor-count guard died with
  the GitHub pipeline) — check by hand.
- The `images` job runs inside `rust:1.97-bookworm` **on purpose**: the binaries must link
  against the same glibc (2.36) as the `debian:bookworm-slim` runtime base. Don't "simplify"
  it back to building on the act ubuntu-24.04 image (glibc 2.39) — that's a load-time
  failure shipped to every host, and the in-image smoke step will catch it.
- rustc is pinned to **1.97** (root `rust-toolchain.toml`, ci.yml, the images container) in
  lockstep with the whole cluster (see myserver docs) — bump everywhere together.
