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
# Requires `git-filter-repo` (https://github.com/newren/git-filter-repo). On
# Fedora: `sudo dnf install git-filter-repo`; or `pip install git-filter-repo`.
set -euo pipefail

SUBDIR="zenoh-blob"
OUT_DIR="${1:-../zenoh-blob}"

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if ! command -v git-filter-repo >/dev/null 2>&1; then
  echo "error: git-filter-repo not found. Install it (see header) and retry." >&2
  exit 1
fi

if [[ -e "$OUT_DIR" ]]; then
  echo "error: output path '$OUT_DIR' already exists; remove it or pass another." >&2
  exit 1
fi

echo "==> Cloning a scratch copy of the monorepo"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
git clone --no-local "$repo_root" "$tmp/clone"
cd "$tmp/clone"

echo "==> Rewriting history to just $SUBDIR/ (as the new repo root)"
git filter-repo --force --subdirectory-filter "$SUBDIR"

echo "==> Swapping in the standalone manifest"
if [[ -f Cargo.standalone.toml ]]; then
  git mv Cargo.standalone.toml Cargo.toml
  git commit -m "chore: standalone manifest (extracted from ZenSight monorepo)"
fi

echo "==> Sanity build + test + publish dry-run"
cargo build --all-targets --locked
cargo test --locked
cargo publish --dry-run

cd "$repo_root"
mv "$tmp/clone" "$OUT_DIR"
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
