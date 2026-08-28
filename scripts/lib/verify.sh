#!/usr/bin/env bash
# verify.sh — the three things every stand-a-deployment-up script got wrong.
#
# Sourced by scripts/{conformance,demo}-verify.sh. Not executable on its own.
#
# WHY THIS EXISTS (#790)
#
# The verify scripts start binaries, wait for a rendezvous, and fail with a
# paragraph explaining Zenoh discovery. That paragraph is true in general and
# was, at least once, entirely irrelevant to what had actually happened: the
# binary was never there. `BIN="${BINDIR:-target/${PROFILE}}"` is repo-relative,
# so anyone with CARGO_TARGET_DIR set — a shared build dir, a worktree that
# builds elsewhere, sccache — builds to one place and is launched from another.
# `cargo build` reports success, so the script's own build step gives no hint.
# The run then waited 40s for a roster that could never populate, blamed
# multicast, and pointed at a log directory its own EXIT trap had just deleted.
#
# Three fixes, and each one is about the same thing: a failure should describe
# itself rather than describe the most common failure.
#
#   require_bins   — fail before starting anything, naming the path we looked at
#   keep_logs_on_failure / logs_note — the evidence outlives the trap
#   still_running / children_died_note — a dead child and an undiscovered one
#                    are different failures and must not render identically

# --- 1. Preflight ----------------------------------------------------------
#
# `cargo build` succeeding tells you a binary exists SOMEWHERE. This tells you
# it exists where we are about to look.
require_bins() {
    local missing=()
    local b
    for b in "$@"; do
        [[ -x "$b" ]] || missing+=("$b")
    done
    if ((${#missing[@]})); then
        printf '\nFAIL: %d binary/binaries missing where this script looks for them:\n' \
            "${#missing[@]}" >&2
        printf '  %s\n' "${missing[@]}" >&2
        cat >&2 <<EOF

  BIN is '${BIN:-?}' (BINDIR=${BINDIR:-unset}, PROFILE=${PROFILE:-unset}).

  If CARGO_TARGET_DIR is set, cargo built somewhere else and this script is
  still looking in the repo's target/. Set BINDIR to the directory that really
  holds the binaries:

      BINDIR="\$CARGO_TARGET_DIR/${PROFILE:-release}" $0

EOF
        exit 1
    fi
}

# --- 2. Evidence that outlives the trap ------------------------------------
#
# The cleanup trap deletes the mktemp -d, which is right on success and wrong
# on failure: the last line of every failure message points into it. Callers
# set KEEP_TMP=1 (via keep_logs_on_failure) before dying, and their cleanup
# honours it.
KEEP_TMP=0

keep_logs_on_failure() { KEEP_TMP=1; }

# The "logs:" footer, with the tail already inlined so it survives even if the
# directory does not — the trick demo-verify.sh already played for one log, now
# available to every failure path.
logs_note() {
    local dir="$1" f
    shift || true
    printf '\n  logs kept in %s:\n' "$dir"
    for f in "$@"; do
        [[ -f "$f" ]] || continue
        printf '\n  --- %s (last 20 lines) ---\n' "$f"
        sed 's/^/  /' <(tail -n 20 "$f")
    done
}

# --- 3. A dead child is not an undiscovered one ----------------------------
#
# The discovery paragraph is the right diagnosis when the processes are alive
# and cannot find each other. It is the wrong one when a process is already
# gone — which is the case the preflight above cannot catch, because a binary
# can exist and still exit on a bad config, a busy port or a missing capability.
still_running() {
    local pid
    for pid in "$@"; do
        [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null && return 0
    done
    return 1
}

# Which of the pids we started are gone. Prints nothing when all are alive.
dead_children() {
    local pid dead=()
    for pid in "$@"; do
        [[ -n "$pid" ]] || continue
        kill -0 "$pid" 2>/dev/null || dead+=("$pid")
    done
    ((${#dead[@]})) && printf '%s\n' "${dead[@]}"
    return 0
}
