#!/usr/bin/env bash
# demo-actions.sh — install (or remove) the one unit `just actions=1 run` is
# allowed to control, plus the scoped polkit rule that lets it (#866).
#
# WHY THIS EXISTS. The systemd sensor's gated service-control surface —
# allowlist matching, the arm/confirm/cancel flow, the in-flight lock, the
# audit ring on @rpc/systemd/actions, and c620838's refuse-don't-hide contract
# — shipped with NO run path at all. `actions.enabled` has been false in every
# generated config since the block existed, so the whole feature was
# permanently un-demonstrable: correct end to end, and never once watched
# working. That is the same blind spot #845 closed for the exporters.
#
# Keeping it off BY DEFAULT is right: it mutates real units and needs
# authorization. Having no opt-in lever at all was the gap.
#
# WHAT THIS INSTALLS, and its blast radius:
#
#   /etc/systemd/system/zensight-demo.service
#       A purpose-made unit that does nothing but sleep, under DynamicUser
#       with no network and a read-only filesystem. Starting, stopping and
#       restarting it is observable and harmless. The demo therefore never
#       touches a unit anything depends on.
#
#   /etc/polkit-1/rules.d/49-zensight-demo.rules
#       Grants org.freedesktop.systemd1.manage-units to ONE user for ONE
#       unit. Not a blanket grant: the rule checks both the action id and
#       action.lookup("unit"), so it cannot authorize anything else, and it
#       carries no manage-unit-files or reload-daemon permission.
#
# Both need root, which is the honest cost of demonstrating a privileged
# surface; the script asks for it explicitly rather than hiding a sudo inside
# a build recipe. `remove` deletes both and reloads, so the machine goes back
# to exactly where it was.
#
# Usage:
#   scripts/demo-actions.sh install [user]     (default: $SUDO_USER, else $USER)
#   scripts/demo-actions.sh remove
#   scripts/demo-actions.sh status
set -euo pipefail

UNIT="zensight-demo.service"
UNIT_PATH="/etc/systemd/system/$UNIT"
RULE_PATH="/etc/polkit-1/rules.d/49-zensight-demo.rules"

die() { echo "error: $*" >&2; exit 1; }

need_root() {
    [[ "$(id -u)" == "0" ]] || die "'$1' needs root — re-run as: sudo $0 $1"
}

cmd="${1:-}"
case "$cmd" in
install)
    need_root install
    who="${2:-${SUDO_USER:-$USER}}"
    id "$who" >/dev/null 2>&1 || die "no such user: $who"
    echo "==> installing $UNIT (controllable by '$who' only)"
    cat > "$UNIT_PATH" <<UNITFILE
[Unit]
Description=ZenSight demo unit — exists only to be started and stopped from the GUI (#866)
Documentation=https://git.marcpardo.eu/marcpardo/zensight/issues/866

[Service]
# Does nothing, owns nothing, reaches nothing. The point is that starting and
# stopping it is observable in the GUI and harmless to the machine.
Type=exec
ExecStart=/bin/sleep infinity
DynamicUser=yes
PrivateNetwork=yes
ProtectSystem=strict
ProtectHome=yes
NoNewPrivileges=yes
CapabilityBoundingSet=
RestrictAddressFamilies=
SystemCallFilter=@system-service

[Install]
WantedBy=multi-user.target
UNITFILE
    # The polkit rule is deliberately narrow: one action, one unit, one user.
    # `unit` is the only lookup that matters here — manage-unit-files and
    # reload-daemon are NOT granted, so the demo cannot enable anything at
    # boot or reload the manager.
    cat > "$RULE_PATH" <<RULEFILE
// ZenSight demo (#866): let one user start/stop/restart ONE inert unit, so the
// gated service-control surface can actually be demonstrated. Installed by
// scripts/demo-actions.sh; remove with: sudo scripts/demo-actions.sh remove
polkit.addRule(function (action, subject) {
    if (action.id == "org.freedesktop.systemd1.manage-units" &&
        action.lookup("unit") == "$UNIT" &&
        subject.user == "$who") {
        return polkit.Result.YES;
    }
});
RULEFILE
    chmod 0644 "$UNIT_PATH" "$RULE_PATH"
    systemctl daemon-reload
    echo "==> installed:"
    echo "    $UNIT_PATH"
    echo "    $RULE_PATH   (manage-units on $UNIT, for $who, and nothing else)"
    echo
    echo "Now run the demo with service control armed:"
    echo "    just actions=1 run"
    echo "then open the systemd device → Units tab, filter for 'zensight-demo',"
    echo "and start/stop/restart it. The Actions tab holds the audit trail."
    echo
    echo "Undo everything with:  sudo $0 remove"
    ;;
remove)
    need_root remove
    echo "==> stopping and removing $UNIT"
    systemctl stop "$UNIT" 2>/dev/null || true
    systemctl disable "$UNIT" 2>/dev/null || true
    rm -f "$UNIT_PATH" "$RULE_PATH"
    systemctl daemon-reload
    systemctl reset-failed "$UNIT" 2>/dev/null || true
    echo "==> removed. The machine is back where it was."
    ;;
status)
    if [[ -f "$UNIT_PATH" ]]; then echo "unit:   installed ($UNIT_PATH)"; else echo "unit:   absent"; fi
    if [[ -f "$RULE_PATH" ]]; then echo "polkit: installed ($RULE_PATH)"; else echo "polkit: absent"; fi
    systemctl is-active "$UNIT" 2>/dev/null | sed 's/^/state:  /' || true
    ;;
*)
    sed -n '2,40p' "$0"
    exit 2
    ;;
esac
