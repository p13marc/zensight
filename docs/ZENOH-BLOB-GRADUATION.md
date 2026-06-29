# Graduating `zenoh-blob` to its own repo + crates.io

`zenoh-blob` (epic #193, issue #202) was incubated inside the ZenSight monorepo
but designed from day one to be **generic** — it has zero `zensight-*`
dependencies and no path dependencies, so it can be lifted out and published as a
standalone crate the wider Zenoh community can use. This guide covers that move.

The mechanical, history-preserving extraction is scripted. The irreversible,
account-bound steps — creating the GitHub repo, a crates.io token, and the actual
`cargo publish` — are **yours to run**; this guide tells you exactly what to do.

## 0. Pre-flight (already done in-tree)

These are in place so extraction is turnkey:

- **Clean boundary** — `zenoh-blob` depends only on published crates (`zenoh`,
  `tokio`, `serde`, `serde_json`, `ciborium`, `thiserror`, `sha2`, `fastcdc`),
  never on a ZenSight crate or a `path = ` dependency. (A CI guard could pin this;
  today it's upheld by review.)
- **Publish metadata** — `zenoh-blob/Cargo.toml` carries `description`, `readme`,
  `keywords`, `categories`, `rust-version`.
- **`README.md` + `LICENSE`** live in the crate directory (carried by extraction),
  with acknowledgements to casync / desync / zenoh-fs / sendit / FastCDC.
- **Standalone manifest** — `zenoh-blob/Cargo.standalone.toml` pins the same
  dependency versions as a self-contained workspace root; the extraction script
  renames it to `Cargo.toml`.
- **Standalone CI** — `zenoh-blob/.github/workflows/ci.yml` (inert in the monorepo;
  becomes the repo's CI once extracted) runs build/test/fmt/clippy + a
  `cargo publish --dry-run`.

## 1. Extract (scripted)

```bash
scripts/extract-zenoh-blob.sh ../zenoh-blob
```

This clones a scratch copy, rewrites history to just the `zenoh-blob/` subtree
(preferring `git-filter-repo`, falling back to `git subtree split`), swaps the
standalone manifest in, and runs build + test + `cargo publish --dry-run` to
confirm the result is publishable. The output repo lands at `../zenoh-blob`.

> Needs **either** `git-filter-repo` (Fedora: `sudo dnf install git-filter-repo`,
> or `pip install git-filter-repo`) **or** `git subtree` (ships in git's contrib;
> Fedora: `sudo dnf install git-subtree`). The script picks whichever is present
> and errors with install hints if neither is.

## 2. Create the GitHub repo and push  *(you)*

```bash
gh repo create p13marc/zenoh-blob --public --description "Resumable chunked blob & directory transfer over Zenoh"
cd ../zenoh-blob
git remote add origin git@github.com:p13marc/zenoh-blob.git
git push -u origin HEAD:main
```

If you change the repo URL, update `repository = ` in the crate's `Cargo.toml`.

## 3. Publish to crates.io  *(you)*

```bash
# One-time: create a token at https://crates.io/settings/tokens
cargo login            # paste the token
cargo publish --dry-run   # final check (also run in CI)
cargo publish             # the irreversible step — a version is forever
```

`zenoh-blob` is currently versioned `0.1.0` in the standalone manifest — an honest
first public release. Bump it there for subsequent releases (and tag the repo).

## 4. Switch ZenSight to the published crate

Back in the ZenSight monorepo, replace the path dependency with the published one
and drop the in-tree crate:

```toml
# Cargo.toml (workspace)
# - members: remove "zenoh-blob"
# [workspace.dependencies]
zenoh-blob = "0.1"     # was: { path = "zenoh-blob" }
```

```bash
git rm -r zenoh-blob
cargo update -p zenoh-blob
cargo test --workspace --locked
```

The adapter code (`zensight-common`, `zensight-sensor-core`, the frontend) is
**unchanged** — it already imports `zenoh_blob::…` by crate name, so only the
dependency source changes.

## Notes

- **Versioning.** The monorepo crate inherits the workspace `0.6.x` version; the
  standalone crate restarts at `0.1.0` (its real public history). Don't carry the
  ZenSight version across — they are different release lines.
- **Licensing.** Currently MIT (matching ZenSight). If you want the broader Zenoh
  ecosystem's dual `MIT OR Apache-2.0`, add an `Apache-2.0` license file and set
  `license = "MIT OR Apache-2.0"` in the standalone manifest before publishing.
- **Attribution.** The `README.md` Acknowledgements section credits the prior art
  the design draws on; keep it.
