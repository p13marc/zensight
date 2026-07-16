# Releasing ZenSight

The procedure is short, but it has two traps that a careless bump walks straight into
(§2). It was reconstructed from commit archaeology during 0.8.0 prep — keep it current.

Versioning is pre-1.0 `0.MINOR.PATCH`: **the minor is the breaking slot.** Any `!` commit
since the last tag means the minor moves.

## 0. Preconditions

```bash
git checkout master && git pull
gh run list --workflow=rust.yml --branch=master --limit=1   # must be success
gh pr list --state open                                      # nothing in flight
```

**`rust.yml` does not run on tags.** Tagging executes zero tests — master must already be
green, at the exact commit you intend to tag.

## 1. CHANGELOG.md

Rename `## [Unreleased]` → `## [X.Y.Z] - YYYY-MM-DD`, add a fresh empty `[Unreleased]`.

Check it against reality rather than trusting it — the changelog is a **purely human
artifact**: `softprops/action-gh-release` runs with `generate_release_notes: true`, so the
GitHub Release body is auto-generated from commits and **CI never reads CHANGELOG.md**.
Nothing fails if it is wrong.

```bash
git log --oneline <prev-tag>..HEAD | grep '!'   # every breaking change — all must be documented
git log --oneline <prev-tag>..HEAD | wc -l      # scale check
```

Entries written before a later refactor go stale silently — 0.8.0 shipped with a parallax
entry describing a control plane that three commits had since replaced. Re-read the entries
you are *keeping*, not just the ones you are adding.

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
Do not bump those. `flatpak/…metainfo.xml` has a `<releases>` block that has been stale since
0.1.0; it is not wired to anything.

## 3. Land it

One commit on a release branch, mirroring 0.6.x:

```bash
git checkout -b release/X.Y.Z
git commit -am "chore(release): X.Y.Z"
gh pr create --base master --title "chore(release): X.Y.Z"
# merge once green
```

Expected diff: `CHANGELOG.md`, `Cargo.toml`, `Cargo.lock`, the two `-ebpf/Cargo.toml`.

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

```bash
gh run watch
```

`release.yml` produces `.deb` (ubuntu 22.04/24.04 × amd64/arm64), `.rpm` (fedora41 × 2 arch),
a Flatpak bundle, and GHCR images (9 sensors + 2 exporters + the `zensight-sensors` bundle),
attaching deb/rpm/flatpak to a GitHub Release. There is **no crates.io publish** anywhere in
the pipeline.

The auto-generated Release body is a commit list. Edit it to point at the CHANGELOG entry —
especially the migration notes, on a breaking release.

## Notes

- **Not every workspace member is packaged.** As of 0.8.0, `zensight-sensor-parallax` ships
  in no artifact (see #512). If you add a sensor crate, add it to `release.yml`'s three `-p`
  lists, the sensor-image matrix, `packaging/systemd/`, and `docker/Dockerfile.sensors` —
  and note `rust.yml` asserts the Dockerfile's sensor count, so it moves in lockstep.
- **`workflow_dispatch` is a dry run.** Use it to exercise the full matrix (both arches)
  before tagging when packaging changed. The `pull_request` trigger only validates one
  distro on amd64, so it will not catch an arm64 cross-build failure.
