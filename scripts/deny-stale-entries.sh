#!/usr/bin/env bash
# deny-stale-entries.sh — find the entries in deny.toml that no longer match
# anything (#1097).
#
# deny.toml's own comment says the weekly deny-fresh run is what "surfaces a
# stale entry so it cannot rot silently". It could not: that workflow strips
# `ignore = []` and reports live advisories, which shows a human WHICH ignores
# still apply and leaves them to infer the rest by absence.
#
# cargo-deny already answers the question directly. With the ignore list
# INTACT, an entry matching nothing is reported as:
#
#   warning[advisory-not-detected]: advisory was not encountered
#      ┌─ deny.toml:22:6
#      │
#   22 │     "RUSTSEC-2026-0194",
#      │      ━━━━━━━━━━━━━━━━━ no crate matched advisory criteria
#
# — a WARNING, so `cargo deny check` exits 0 and the gate is green. This script
# turns those warnings into the report's failure signal, which is the job the
# weekly run was written to do.
#
# The same shape covers every stale-able list in the file, not just advisories:
# `license-exception-not-encountered` for the two libsystemd exceptions,
# `unmatched-skip` for a [bans] skip, `unmatched-organization`/
# `unmatched-path-source` for [sources].
#
# Exit 1 when something is stale, naming it. Report, not gate: only
# deny-fresh.yml runs this, and on that workflow red means "prune deny.toml",
# never "the tree is broken".
set -uo pipefail

export GIT_TERMINAL_PROMPT=0

log=$(mktemp)
# Findings are irrelevant here — a live advisory is the gate's business, not
# this script's — so the exit code is deliberately ignored and only the
# warnings are read.
cargo deny check advisories licenses bans sources >"$log" 2>&1 || true

STALE_RE='advisory-not-detected|license-exception-not-encountered|unmatched-skip|unmatched-organization|unmatched-path-source|unmatched-bypass|unmatched-workspace-dependency'

if ! grep -qE "$STALE_RE" "$log"; then
    echo "deny-stale-entries: every entry in deny.toml still matches something"
    exit 0
fi

echo "::error::deny.toml carries entries that no longer match anything — prune them"
echo
# Print each warning with the five lines that name the offending entry.
grep -E -A5 "$STALE_RE" "$log"
echo
echo "Each block above is an entry in deny.toml whose reason has expired: the"
echo "advisory no longer reaches this tree, or the crate the exception was"
echo "written for is gone. Delete it, or say in the comment why it is being"
echo "kept. A list made of stale entries protects less than a shorter true one."
exit 1
