#!/usr/bin/env bash
# packaging-check.sh — the two unit forms say the same thing, or this fails (#1092).
#
# ZenSight ships every producer twice: a native `.service` in packaging/systemd/
# and a Quadlet `.container` in packaging/quadlet/. Nothing compared them, and
# they had drifted in three separate ways at once:
#
#   * netring's quadlet granted NET_ADMIN that its unit deliberately withholds;
#   * netlink's quadlet omitted CAP_BPF/CAP_PERFMON, so eBPF was silently
#     unavailable in the container form only;
#   * logs' quadlet declared nothing where its unit grants CAP_NET_BIND_SERVICE,
#     and worked only because podman's DEFAULT capability set happens to include
#     it — which is the deeper version of the same bug: without
#     `DropCapability=ALL` a quadlet that declares no capability is not
#     equivalent to `CapabilityBoundingSet=`, it is the eleven-capability
#     default.
#
# And `docs/ops/SIZING.md` states an invariant — "set the budget BELOW
# `MemoryMax` so the ladder gets to act first" — that zero shipped units
# satisfied, because the four units with a `MemoryMax` were exactly the four
# that could not declare a budget.
#
# This runs in ci.yml's `lint` job beside the grep guards. It reads files only:
# no podman, no systemd, no network.
#
#   scripts/packaging-check.sh            # check
#   scripts/packaging-check.sh --table    # print the README table to stdout
set -euo pipefail

cd "$(dirname "$0")/.."

SVC_DIR=packaging/systemd
QUAD_DIR=packaging/quadlet
CFG_DIR=configs

fail=0
problem() { printf '  ✗ %s\n' "$1" >&2; fail=1; }

# Which config file a unit reads, when it is not <producer>.json5. The logs
# sensor ships two; the unit names the journald one.
config_for() {
    local unit=$1
    local name=${unit#zensight-sensor-}
    name=${name#zensight-}
    case "$name" in
        exporter-otel) echo "otel-exporter" ;;
        exporter-prometheus) echo "prometheus-exporter" ;;
        *) echo "$name" ;;
    esac
}

# `AmbientCapabilities=CAP_NET_RAW CAP_IPC_LOCK` -> `IPC_LOCK NET_RAW`.
svc_caps() {
    # `|| true` on every grep in this file: `set -o pipefail` is on, and a unit
    # that grants no capability is the NORMAL case, not an error.
    { grep -h '^AmbientCapabilities=' "$1" 2>/dev/null || true; } \
        | sed 's/^AmbientCapabilities=//' \
        | tr ' ' '\n' | sed 's/^CAP_//' | { grep -v '^$' || true; } | sort | tr '\n' ' ' | sed 's/ $//'
}

# `AddCapability=NET_RAW IPC_LOCK` -> `IPC_LOCK NET_RAW`.
quad_caps() {
    { grep -h '^AddCapability=' "$1" 2>/dev/null || true; } \
        | sed 's/^AddCapability=//' \
        | tr ' ' '\n' | sed 's/^CAP_//' | { grep -v '^$' || true; } | sort | tr '\n' ' ' | sed 's/ $//'
}

mem_of() { { grep -h '^MemoryMax=' "$1" 2>/dev/null || true; } | tail -1 | sed 's/^MemoryMax=//'; }

# 512M -> 512. The units only ever use M, and a G would silently compare wrong,
# so anything else is an error rather than a best guess.
mem_mib() {
    local v=$1
    case "$v" in
        *M) echo "${v%M}" ;;
        "") echo "" ;;
        *) echo "BAD" ;;
    esac
}

# resources.budget_rss_mb, read with the same tolerance json5 gives it: the key
# is on its own line inside a `resources: {` block.
budget_of() {
    local f=$1
    [ -f "$f" ] || { echo ""; return; }
    awk '
        /^  resources: \{/ { inblock = 1; next }
        inblock && /^  \}/  { inblock = 0 }
        inblock && /^ *budget_rss_mb: *[0-9]+ *,/ {
            gsub(/[^0-9]/, ""); print; exit
        }
    ' "$f"
}

# Units with no quadlet, and the reason. #1093 empties this list; an entry that
# is no longer true is itself a failure, so it cannot rot.
declare -A NO_QUADLET=()

rows=()

for svc in "$SVC_DIR"/*.service; do
    unit=$(basename "$svc" .service)
    quad="$QUAD_DIR/$unit.container"
    cfg="$CFG_DIR/$(config_for "$unit").json5"

    svc_c=$(svc_caps "$svc")
    svc_m=$(mem_of "$svc")
    bud=$(budget_of "$cfg")

    # 1. Every unit has a cgroup backstop.
    if [ -z "$svc_m" ]; then
        problem "$unit: the .service has no MemoryMax"
    elif [ "$(mem_mib "$svc_m")" = "BAD" ]; then
        problem "$unit: .service MemoryMax=$svc_m is not in M — this check compares MiB"
    fi

    # 2. The budget is below the backstop, so the ladder acts before the kernel.
    if [ -n "$bud" ] && [ -n "$svc_m" ] && [ "$(mem_mib "$svc_m")" != "BAD" ]; then
        if [ "$bud" -ge "$(mem_mib "$svc_m")" ]; then
            problem "$unit: budget_rss_mb=$bud is not below MemoryMax=$svc_m (docs/ops/SIZING.md)"
        fi
    fi

    if [ ! -f "$quad" ]; then
        if [ -n "${NO_QUADLET[$unit]+x}" ]; then
            rows+=("$unit|$svc_c|(none: ${NO_QUADLET[$unit]})|$svc_m|—|${bud:-—}")
        else
            problem "$unit: no quadlet in $QUAD_DIR, and no stated reason (#1093)"
        fi
        continue
    fi

    quad_c=$(quad_caps "$quad")
    quad_m=$(mem_of "$quad")

    # 3. A quadlet that does not drop the default set is not equivalent to
    #    `CapabilityBoundingSet=`, whatever its AddCapability line says.
    if ! grep -q '^DropCapability=ALL$' "$quad"; then
        problem "$unit: the quadlet has no DropCapability=ALL, so it keeps podman's default capability set — its .service does not"
    fi

    # 4. The two forms grant the same capabilities.
    if [ "$svc_c" != "$quad_c" ]; then
        problem "$unit: capabilities disagree — .service grants '${svc_c:-none}', quadlet grants '${quad_c:-none}'"
    fi

    # 5. …and the same backstop.
    if [ -z "$quad_m" ]; then
        problem "$unit: the quadlet has no MemoryMax"
    elif [ "$quad_m" != "$svc_m" ]; then
        problem "$unit: MemoryMax disagrees — .service $svc_m, quadlet $quad_m"
    fi

    rows+=("$unit|$svc_c|$quad_c|$svc_m|$quad_m|${bud:-—}")
done

# 6. A quadlet with no unit is drift in the other direction.
for quad in "$QUAD_DIR"/*.container; do
    unit=$(basename "$quad" .container)
    [ -f "$SVC_DIR/$unit.service" ] || problem "$unit: a quadlet with no .service twin"
done

if [ "${1:-}" = "--table" ]; then
    echo "| Unit | Capabilities | \`MemoryMax\` | \`budget_rss_mb\` |"
    echo "|---|---|---:|---:|"
    for r in "${rows[@]}"; do
        IFS='|' read -r u sc qc sm qm b <<<"$r"
        caps=${sc:-—}
        echo "| \`$u\` | ${caps// /, } | ${sm:-—} | $b |"
    done
    exit 0
fi

if [ "$fail" -ne 0 ]; then
    echo "packaging-check: the two unit forms disagree — see above" >&2
    exit 1
fi
echo "packaging-check: ${#rows[@]} units, both forms agree on capabilities and MemoryMax, every budget below its backstop"
