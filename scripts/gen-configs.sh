#!/usr/bin/env bash
# gen-configs.sh — generate the *demo-max* run configs.
#
# The single definition of the "demo-max" profile: it transforms the committed
# example configs (configs/*.json5) into run configs with the opt-in
# collectors, anomaly detectors and on-demand artifacts (report / directory
# snapshot / pcap capture) turned ON, so the whole feature surface is visible
# in the GUI. Build-feature-gated bits (ja4plus / lateral / sigma / snmp /
# ebpf) and privileged systemd unit control (`actions`) stay off.
#
# Used by BOTH `just configure` (local runs) and the all-in-one sensors
# container image (docker/entrypoint-sensors.sh) — change the profile here,
# never in two places.
#
# Usage:
#   gen-configs.sh --iface IFACE --outdir DIR --configs-dir DIR [--snapshot-dir PATH]
#
#   --iface        interface netring captures on
#   --outdir       where the generated *.json5 land (created if missing)
#   --configs-dir  the committed example configs (repo configs/ or the image's copy)
#   --snapshot-dir optional: enable sysinfo's Tier-2 directory snapshot of this
#                  path (exposed in the GUI as "Download docs"). Omitted in the
#                  container, where the repo's docs/ doesn't exist.

set -euo pipefail

iface="" outdir="" configs_dir="" snapshot_dir=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --iface)        iface="$2"; shift 2 ;;
        --outdir)       outdir="$2"; shift 2 ;;
        --configs-dir)  configs_dir="$2"; shift 2 ;;
        --snapshot-dir) snapshot_dir="$2"; shift 2 ;;
        *) echo "gen-configs.sh: unknown argument '$1'" >&2; exit 64 ;;
    esac
done
if [[ -z "$iface" || -z "$outdir" || -z "$configs_dir" ]]; then
    echo "Usage: gen-configs.sh --iface IFACE --outdir DIR --configs-dir DIR [--snapshot-dir PATH]" >&2
    exit 64
fi

mkdir -p "$outdir"

# netring: point capture at the chosen interface + light up the L7 collectors
# (QUIC/SSH/encrypted-DNS), IP reassembly, the extra anomaly detectors, the
# on-demand pcap capture (@/artifact, needs CAP_NET_RAW) and the debug report.
sed -E \
    -e "s#interfaces: \[[^]]*\]#interfaces: [\"$iface\"]#" \
    -e 's/quic: false/quic: true/' \
    -e 's/ssh: false/ssh: true/' \
    -e 's/encrypted_dns: false/encrypted_dns: true/' \
    -e 's/ip_reassembly: false/ip_reassembly: true/' \
    -e 's/encrypted_dns_bypass: false/encrypted_dns_bypass: true/' \
    -e 's/rita_beacon_fqdn: false/rita_beacon_fqdn: true/' \
    -e 's/data_exfil: false/data_exfil: true/' \
    -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/' \
    -e '/^      on_demand:/,/enabled:/ s/enabled: false/enabled: true/' \
    "$configs_dir/netring.json5" > "$outdir/netring.json5"

# netlink: baseline is already broad; add the IPsec/XFRM collector (needs
# CAP_NET_ADMIN) + the on-demand debug report.
sed -E \
    -e 's/xfrm: false/xfrm: true/' \
    -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/' \
    "$configs_dir/netlink.json5" > "$outdir/netlink.json5"

# logs: journald ingestion is already on; enable the on-demand debug report.
sed -E \
    -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/' \
    "$configs_dir/logs.json5" > "$outdir/logs.json5"

# sysinfo: the on-demand debug report, plus (when --snapshot-dir is given) a
# Tier-2 directory snapshot of that path. Scoped seds so each block's own
# `enabled` flips, not the alert rules'.
if [[ -n "$snapshot_dir" ]]; then
    sed -E \
        -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/' \
        -e '/snapshot: \{/,/dirs: \[/ { s/enabled: false/enabled: true/; s#dirs: \[#dirs: [ { name: "docs", path: "'"$snapshot_dir"'" },# }' \
        "$configs_dir/sysinfo.json5" > "$outdir/sysinfo.json5"
else
    sed -E \
        -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/' \
        "$configs_dir/sysinfo.json5" > "$outdir/sysinfo.json5"
fi

# parallax: the committed config already streams the synthetic test pattern
# (test0) on any machine — camera or not — and advertises local /dev/video*
# cameras in the catalogue; demo-max just adds the on-demand debug report.
sed -E \
    -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/' \
    "$configs_dir/parallax.json5" > "$outdir/parallax.json5"

# correlator: fuses the sensors' identity evidence into one HostEntity per
# host. Machine-agnostic; the example config already has every merge rule on.
cp -f "$configs_dir/correlator.json5" "$outdir/correlator.json5"

# systemd: generate a demo config with (nearly) everything on. NOTE the
# watchlist is deliberately a *curated* set, not `*.service` — the Units /
# Timers / Sockets / cgroup tabs all populate from the on-demand @/query/*
# channels regardless of the watchlist, so a broad watch just streams a lot of
# per-unit telemetry every tick for no UI gain (and, stacked on the other
# maxed-out sensors, can starve the desktop). `actions` (gated start/stop/
# restart) stays OFF — it mutates real units and is privileged.
cat > "$outdir/systemd.json5" <<'JSON5'
{
  zenoh: { mode: "peer", serialization: "json" },
  // On-demand redacted debug bundle (Sensors → report) — safe to enable.
  report: { enabled: true, max_bytes: 67108864, cooldown_secs: 30, ttl_secs: 600, chunk_size: 524288 },
  systemd: {
    poll_interval_secs: 15,
    // Curated per-unit stream (timers + sockets + a few high-value services).
    // The full inventory is still browsable via the on-demand query tabs.
    watch_units: ["*.timer", "*.socket", "sshd.service", "NetworkManager.service",
                  "systemd-journald.service", "systemd-logind.service",
                  "dbus-broker.service", "polkit.service", "user@*.service"],
    watch_max: 50,
    ip_io_accounting: true,       // per-unit IP + disk IO byte counters
    events_capacity: 512,         // control-plane event ring (@/query/events)
    alerts: {
      enabled: true,
      for_secs: 15,
      unit_failed: true,
      system_degraded: true,
      restart_storm_threshold: 3,
      restart_storm_window_secs: 300,
      unit_mem_ceiling_bytes: 0,  // 0 = unit-mem rule off (avoids demo noise)
      timer_overdue_grace_secs: 300,
    },
    cgroup: { root: "system.slice", max_depth: 6, max_children: 64, max_pids: 32 },
    // Sentinel: default.target must be active and nothing may be failed — the
    // latter fires a real, actionable alert iff the host has a failed unit.
    expectations: {
      eval_interval_secs: 15,
      for_secs: 15,
      targets_active: [{ target: "default.target" }],
      forbid_failed: true,
    },
    // actions: { enabled: false }  // gated service control — off for the demo.
    collect: { list_units: true, boot: true, mounts: true, journal: true },
  },
  logging: { level: "info" },
}
JSON5

echo "Configured (demo-max): netring iface='$iface' (L7+capture on), netlink (+xfrm), logs=journald, sysinfo${snapshot_dir:+ snapshot='$snapshot_dir'}, systemd=full, parallax=test-pattern, correlator  (configs in $outdir/)"
