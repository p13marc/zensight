#!/usr/bin/env bash
# Extract the in-tree `zenoh-blob/` crate into a standalone repository with its
# history preserved, ready to push to a new GitHub repo and publish to crates.io.
#
# This is the mechanical half of graduating zenoh-blob (#202). The remaining steps
# — creating the GitHub repo, a crates.io account + API token, and running the
# actual `cargo publish` — are yours; see docs/ZENOH-BLOB-GRADUATION.md.
#
# Usage:
#   scripts/extract-zenoh-blob.sh [OUTPUT_DIR]
# Default OUTPUT_DIR: ../zenoh-blob
#
# Prefers `git-filter-repo` (https://github.com/newren/git-filter-repo; Fedora:
# `sudo dnf install git-filter-repo`, or `pip install git-filter-repo`) and falls
# back to the built-in `git subtree split` when it's not installed. Both preserve
# the subdirectory's history rooted at the new repo's top level.
set -euo pipefail

SUBDIR="zenoh-blob"
OUT_DIR="${1:-../zenoh-blob}"

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if [[ -e "$OUT_DIR" ]]; then
  echo "error: output path '$OUT_DIR' already exists; remove it or pass another." >&2
  exit 1
fi

echo "==> Cloning a scratch copy of the monorepo"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
git clone --no-local "$repo_root" "$tmp/clone"

# Produce a clean repo at $tmp/out whose entire history is just $SUBDIR/, rooted
# at the top level. Two routes; both leave the result on branch `main`.
if command -v git-filter-repo >/dev/null 2>&1; then
  echo "==> Rewriting history to just $SUBDIR/ (git-filter-repo)"
  git -C "$tmp/clone" filter-repo --force --subdirectory-filter "$SUBDIR"
  mv "$tmp/clone" "$tmp/out"
  git -C "$tmp/out" branch -M main
elif git subtree -h >/dev/null 2>&1; then
  echo "==> git-filter-repo not found; using 'git subtree split' fallback"
  split_sha="$(git -C "$tmp/clone" subtree split --prefix "$SUBDIR" HEAD)"
  git init -q "$tmp/out"
  git -C "$tmp/out" fetch -q "$tmp/clone" "$split_sha"
  git -C "$tmp/out" checkout -q -b main FETCH_HEAD
else
  echo "error: need 'git-filter-repo' or 'git subtree' to rewrite history; neither found." >&2
  echo "  install one of:" >&2
  echo "    git-filter-repo : pip install git-filter-repo   (or: sudo dnf install git-filter-repo)" >&2
  echo "    git subtree     : ships in git contrib          (Fedora: sudo dnf install git-subtree)" >&2
  exit 1
fi

cd "$tmp/out"
echo "==> Swapping in the standalone manifest"
if [[ -f Cargo.standalone.toml ]]; then
  # Overwrite the workspace-inheriting manifest with the standalone one.
  git mv -f Cargo.standalone.toml Cargo.toml
  git commit -q -m "chore: standalone manifest (extracted from ZenSight monorepo)"
fi

echo "==> Sanity build + test + publish dry-run"
cargo build --all-targets
cargo test
cargo publish --dry-run --allow-dirty

cd "$repo_root"
mv "$tmp/out" "$OUT_DIR"
trap - EXIT
rm -rf "$tmp"

cat <<EOF

==> Done. Standalone repo at: $OUT_DIR

Next (manual) steps — see docs/ZENOH-BLOB-GRADUATION.md:
  1. Create the GitHub repo (e.g. gh repo create p13marc/zenoh-blob --public).
  2. cd $OUT_DIR && git remote add origin git@github.com:p13marc/zenoh-blob.git && git push -u origin HEAD:main
  3. Create a crates.io API token, then: cargo login && cargo publish
  4. Switch ZenSight to the published crate (see the guide).
EOF
