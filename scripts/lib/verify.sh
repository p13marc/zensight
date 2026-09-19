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
#   stop_children  — a child that ignores SIGTERM costs seconds, not a job
#                    timeout (#1211)

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

# --- 4. A child that will not stop must not cost a whole CI lane -----------
#
# Every one of these scripts ended its EXIT trap with `kill $pid` then
# `wait $pid`, and `wait` on a process that does not die is unbounded. On
# 2026-09-09 and again on 2026-09-19 that turned a demo-verify failure at the
# four-minute mark into a **45-minute** demo-smoke job, three times over, on a
# two-lane runner — the script had already printed its diagnosis and was then
# held by one child until the job timeout killed the container.
#
# The child in question was a wedged historian (#1211), which is a bug in its
# own right. But "a monitoring harness can be held open forever by the thing it
# is monitoring" is a defect of the harness, and no fix to one producer closes
# it: TERM, give it a moment, then KILL.
#
# The grace is per-round rather than per-process, so N children cost one wait,
# not N.
stop_children() {
    local grace="${STOP_GRACE_SECS:-5}" pid i
    for pid in "$@"; do
        [[ -n "$pid" ]] && kill -TERM "$pid" 2>/dev/null || true
    done
    for ((i = 0; i < grace * 10; i++)); do
        still_running "$@" || break
        sleep 0.1
    done
    for pid in "$@"; do
        if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
            printf 'warn: pid %s ignored SIGTERM for %ss; killing\n' "$pid" "$grace" >&2
            kill -KILL "$pid" 2>/dev/null || true
        fi
    done
    # Now `wait` is bounded: everything above is either gone or SIGKILLed.
    for pid in "$@"; do
        [[ -n "$pid" ]] && wait "$pid" 2>/dev/null || true
    done
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
