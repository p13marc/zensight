#!/usr/bin/env bash
# check-doc-links.sh — every relative link in a Markdown file points at a file
# that exists (#1158).
#
# There were 19 broken ones, and they had one shape between them: something
# MOVED and the links did not. Six pointed at `docs/rfcs/keyspace-v2/` and
# eight at `rfcs/keyspace-v2/`, both of which left for the zenkey repo with the
# RFC extraction; two at `zensight-keyspace/registry/`, which is now
# `zensight-common/registry/`; one at a file that moved into `docs/design/` and
# was lowercased on the way.
#
# NO NETWORK. External URLs are deliberately not checked: a link checker that
# fetches is a gate that fails when someone else's site is down, and this runs
# in `ci.yml`'s lint job beside the grep guards. `lychee --offline` would do
# the same job and need installing; this needs nothing.
#
# Anchors (`#section`) are stripped rather than verified — checking those needs
# a heading model per file, and a link to the right FILE with a stale anchor
# still lands the reader somewhere useful.
set -euo pipefail

cd "$(dirname "$0")/.."

python3 - "$@" <<'PY'
import pathlib, re, sys, urllib.parse

SKIP_DIRS = {"target", ".git", "node_modules", "builddir", "export-repo"}

broken = []
checked = 0
for md in sorted(pathlib.Path(".").rglob("*.md")):
    if any(part in SKIP_DIRS for part in md.parts):
        continue
    text = md.read_text(errors="replace")
    for m in re.finditer(r'\[[^\]]*\]\(([^)\s]+)(?:\s+"[^"]*")?\)', text):
        link = m.group(1)
        if link.startswith(("http://", "https://", "mailto:", "#")):
            continue
        target = urllib.parse.unquote(link.split("#", 1)[0])
        if not target:
            continue
        checked += 1
        if not (md.parent / target).resolve().exists():
            line = text[: m.start()].count("\n") + 1
            broken.append(f"{md}:{line}: {link}")

if broken:
    print("::error::Markdown files link to paths that do not exist:")
    for b in broken:
        print(f"  {b}")
    print()
    print("Something moved and its links did not. Fix the link, or delete it if")
    print("the target left the repository (the RFCs live in the zenkey repo now).")
    sys.exit(1)

print(f"check-doc-links: {checked} relative links, all resolve")
PY
