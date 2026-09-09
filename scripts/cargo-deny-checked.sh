#!/usr/bin/env bash
# cargo-deny-checked.sh — `cargo deny check`, with a fetch failure told apart
# from a finding (#950, #1097).
#
# Three distinct failures wear the same red X and only one of them is about
# this repository, so this wrapper separates them:
#
#   1. THE FETCH WAS REFUSED. github.com rate-limits an anonymous git-HTTPS
#      clone of the (public) RustSec advisory database from this egress IP; the
#      symptom is `fatal: could not read Username for 'https://github.com'` —
#      a credential prompt for a public repo. Re-check against the cached
#      database and pass with a warning. Safe, because `--offline` with NO
#      cached database exits 1 ("failed to get 'FETCH_HEAD' metadata"); it does
#      not report `advisories ok` over an empty directory, so the fallback
#      cannot green a run that checked nothing.
#   2. A WEDGED CLONE poisons the lane's cargo home until something deletes it.
#      Reset both cache spellings and retry once. `advisory-dbs` (plural) is
#      what cargo-deny 0.20.2 writes; `advisory-db` is the pre-0.19 spelling.
#   3. A REAL FINDING. Fail, and leave the cache alone — `cache-on-failure`
#      saves after the step, so deleting the database here would hand the next
#      run an empty cache and put it straight back into the clone that started
#      all this.
#
# Lifted out of ci.yml's deny job by #1097 so `deny-fresh.yml` gets the same
# apparatus. That workflow had NONE of it — no cache, no GIT_TERMINAL_PROMPT,
# no triage — which made the weekly report the run most likely to go red for a
# reason unrelated to the tree, and, being "a REPORT, not a gate", the one
# nobody would investigate.
#
# Forgejo Actions here has no local composite actions — every `uses:` in
# .forgejo/workflows/ is an absolute URL and there is no .forgejo/actions/ —
# so this is a script both workflows call, which is the repo's existing shape
# for shared CI logic (demo-verify.sh, conformance-verify.sh).
#
# Usage: scripts/cargo-deny-checked.sh [check args...]   (default: all four)
set -uo pipefail

# A credential challenge must fail immediately and say so, not block on a
# terminal that is not there.
export GIT_TERMINAL_PROMPT=0

args=("$@")
[ ${#args[@]} -eq 0 ] && args=(advisories licenses bans sources)

cargo deny --version

log=$(mktemp)
if cargo deny check "${args[@]}" 2>&1 | tee "$log"; then exit 0; fi

if grep -q "failed to fetch advisory database" "$log"; then
    echo "::warning::advisory-db fetch refused by github.com (#950); re-checking against the cached database"
    if cargo deny --offline check "${args[@]}"; then
        echo "::warning::checked against the CACHED advisory database, not a freshly fetched one"
        exit 0
    fi
    echo "::error::advisory database is neither fetchable nor cached; nothing was checked"
    exit 1
fi

if grep -qE "expected flush after ref listing|failed to get 'FETCH_HEAD'|failed to open advisory database" "$log"; then
    echo "::warning::the lane's advisory-db clone is wedged; resetting it and retrying once"
    rm -rf ~/.cargo/advisory-db ~/.cargo/advisory-dbs \
           "${CARGO_HOME:-$HOME/.cargo}/advisory-db" "${CARGO_HOME:-$HOME/.cargo}/advisory-dbs"
    cargo deny check "${args[@]}"
    exit $?
fi

echo "::error::cargo deny reported a finding; see the output above"
exit 1
