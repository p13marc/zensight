#!/usr/bin/env bash
# gen-configs.sh — generate run configs for one of two profiles.
#
# The single definition of BOTH profiles:
#
#   --profile demo-max    (default; what `just configure` uses) transforms the
#     committed example configs (configs/*.json5) into run configs with the
#     opt-in collectors, anomaly/security detectors and on-demand artifacts
#     (report / directory snapshot / pcap capture) turned ON, so the whole
#     feature surface is visible in the GUI. Bits gated on a non-default build
#     feature (ja4plus / lateral / sigma / snmp / ipfix) and privileged systemd
#     unit control (`actions`) stay off.
#
#   --profile production  (what the sensors container defaults to) keeps every
#     anomaly/security detector and analytics alert at its shipped default
#     (off): no netring detector suite (beaconing/RITA, DNS tunnelling, NOD,
#     DGA, exfil, encrypted-DNS bypass, connection floods), no log
#     error-budget burn alerts, no durable log store on the (tmpfs) run dir.
#     Telemetry stays rich — the L7 collectors, sysinfo opt-in collectors and
#     hardware thermal alert, and the actionable systemd ops alerts remain on,
#     as do the on-demand debug reports. Chosen for real deployments where the
#     detector suite's false positives are noise (2026-07 fleet experience).
#
# A sed can only flip a key that is really in configs/*.json5 — a key that is
# merely absent takes the Rust `#[serde(default)]` silently, and nothing here
# sets `deny_unknown_fields`. So every flag this script flips is spelled out in
# the committed example (and pinned by a `shipped_config_spells_out_*` test in
# the owning crate). Add the key there first, then the sed here.
#
# Used by BOTH `just configure` (local runs) and the all-in-one sensors
# container image (docker/entrypoint-sensors.sh) — change the profile here,
# never in two places.
#
# Usage:
#   gen-configs.sh --iface IFACE --outdir DIR --configs-dir DIR \
#                  [--profile demo-max|production] [--snapshot-dir PATH] \
#                  [--pcap-dir PATH] [--ebpf]
#
#   --profile      demo-max (default) or production — see header
#   --iface        interface netring captures on
#   --outdir       where the generated *.json5 land (created if missing)
#   --configs-dir  the committed example configs (repo configs/ or the image's copy)
#   --snapshot-dir optional: enable sysinfo's Tier-2 directory snapshot of this
#                  path (exposed in the GUI as "Download docs"). Omitted in the
#                  container, where the repo's docs/ doesn't exist.
#   --pcap-dir     optional: arm netring's triggered capture-to-disk into this
#                  path — an anomaly fires and the GUI can download a pcap of the
#                  lead-up — and expose the finished files for Tier-2 pull.
#                  Omitted in the container, where the 32 MiB pre-trigger ring
#                  stays resident for no demo gain.
#   --ebpf         optional: turn on sysinfo's eBPF histograms (runqlat +
#                  biolatency on @rpc/sysinfo/latency). Only meaningful for a
#                  binary built `--features ebpf` that holds CAP_BPF/CAP_PERFMON,
#                  so `just configure` passes it only when it built one. Never
#                  passed by the container: a rootless podman userns cannot load
#                  BPF at all, and claiming `ebpf: true` there would be a lie.

set -euo pipefail

iface="" outdir="" configs_dir="" snapshot_dir="" pcap_dir="" ebpf=0
profile="demo-max"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile)      profile="$2"; shift 2 ;;
        --iface)        iface="$2"; shift 2 ;;
        --outdir)       outdir="$2"; shift 2 ;;
        --configs-dir)  configs_dir="$2"; shift 2 ;;
        --snapshot-dir) snapshot_dir="$2"; shift 2 ;;
        --pcap-dir)     pcap_dir="$2"; shift 2 ;;
        --ebpf)         ebpf=1; shift ;;
        *) echo "gen-configs.sh: unknown argument '$1'" >&2; exit 64 ;;
    esac
done
if [[ -z "$iface" || -z "$outdir" || -z "$configs_dir" ]]; then
    echo "Usage: gen-configs.sh --iface IFACE --outdir DIR --configs-dir DIR \
[--profile demo-max|production] [--snapshot-dir PATH] [--pcap-dir PATH] [--ebpf]" >&2
    exit 64
fi
case "$profile" in demo-max|production) ;; *)
    echo "gen-configs.sh: unknown --profile '$profile' (demo-max|production)" >&2; exit 64 ;;
esac

mkdir -p "$outdir"

# netring: point capture at the chosen interface + light up the L7 collectors
# (QUIC/SSH/encrypted-DNS), IP reassembly, the extra anomaly detectors, the
# on-demand pcap capture (@/artifact, needs CAP_NET_RAW) and the debug report.
# Both profiles: capture iface, the L7 telemetry collectors (QUIC/SSH/
# encrypted-DNS — enrichment, no alerts), IP reassembly and the on-demand
# debug report. demo-max additionally lights the whole detector suite up.
netring_seds=(
    -e "s#interfaces: \[[^]]*\]#interfaces: [\"$iface\"]#"
    -e 's/quic: false/quic: true/'
    -e 's/ssh: false/ssh: true/'
    -e 's/encrypted_dns: false/encrypted_dns: true/'
    -e 's/ip_reassembly: false/ip_reassembly: true/'
    -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/'
    -e '/^      on_demand:/,/enabled:/ s/enabled: false/enabled: true/'
)
if [[ "$profile" == "production" ]]; then
    # The one detector the committed example ships ON: TRW port-scan. It
    # false-fired on the first fleet deployment (VPN/monitoring traffic looks
    # scan-shaped) — production turns it off with the rest of the suite.
    netring_seds+=(
    -e 's/port_scan: true/port_scan: false/'
    )
fi
if [[ "$profile" == "demo-max" ]]; then
    netring_seds+=(
    -e 's/encrypted_dns_bypass: false/encrypted_dns_bypass: true/'
    -e 's/rita_beacon_fqdn: false/rita_beacon_fqdn: true/'
    -e 's/data_exfil: false/data_exfil: true/'
    # The rest of the detector suite: beaconing/C2 (both the coarse CV detector
    # and RITA's robust Bowley+MAD one), DNS tunnelling, newly-observed domains,
    # connection floods and DGA scoring. Their inputs (collect.dns, collect.flows,
    # names) are already on above, so these cost no extra capture.
    # Scoped to the anomalies block and line-anchored, so `rita_beacon` cannot
    # also rewrite the `rita_beacon_fqdn` flipped above.
    -e '/^    anomalies: \{/,/^    \},/ s/^( *)(beaconing|rita_beacon|dns_tunnel|nod|connection_flood|dga): false/\1\2: true/'
    # Arm the hot-swappable IOC channel (#328) even though the demo ships no
    # indicators — otherwise @rpc/netring/threat_intel/set has nothing to fill.
    -e '/^    threat: \{/,/^    \},/ s/^( *)reload: false/\1reload: true/'
    )
fi
if [[ -n "$pcap_dir" ]]; then
    # Triggered capture-to-disk: keep a pre-trigger ring, and when an anomaly
    # fires write the lead-up to a pcap the GUI can download. Also expose the
    # finished files as a Tier-2 snapshot dir (the use documented in
    # configs/netring.json5's artifacts block). netring's disk.rs does its own
    # create_dir_all, so don't mkdir it here.
    # BOTH seds MUST stay scoped: an unscoped `dir: null` would rewrite
    # threat.sigma's, and `mode: "off"` would reach zenoh's `mode: "peer"`.
    netring_seds+=(
        -e '/^      to_disk: \{/,/^      \},/ { s/mode: "off"/mode: "triggered"/; s#dir: null#dir: "'"$pcap_dir"'"#; }'
        # The pcap dir is append-mostly — exactly the sanctioned case for
        # `incremental` (build_tree_from parent reuse); the durable store
        # lands beside the pcaps so repeated demo snapshots dedup and survive
        # a sensor restart.
        -e '/snapshot: \{/,/dirs: \[/ { s/enabled: false/enabled: true/; s#dirs: \[#dirs: [ { name: "pcaps", path: "'"$pcap_dir"'", incremental: true },# }'
        -e 's#// state_dir: "/var/lib/zensight-netring/artifacts",#state_dir: "'"$pcap_dir"'-state",#'
    )
fi
sed -E "${netring_seds[@]}" "$configs_dir/netring.json5" > "$outdir/netring.json5"

# netlink: baseline is already broad; just add the on-demand debug report.
# production also drops the `demo-expected-service` sentinel expectation —
# it is DESIGNED to always fire (a listener that deliberately isn't there,
# so `just run` demos the alert pipeline); on a real fleet it is a permanent
# false positive on every host. The `no-telnet` forbid rule stays (it cannot
# false-fire on a clean host).
# NOTE xfrm stays OFF. It is off in the committed config precisely because
# nlink's XFRM dump trips a ratelimited kernel warning on every poll and that is
# noisy *in this demo* (#242, nlink#160) — a sed here re-enabling it silently
# undid that. Re-enable in configs/netlink.json5 when nlink ships the fix, in one
# place, not two.
netlink_seds=(
    -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/'
)
if [[ "$profile" == "production" ]]; then
    netlink_seds+=(
    -e '/\/\/ Demo: a service that should be listening/,/demo-expected-service/d'
    )
fi
sed -E "${netlink_seds[@]}" "$configs_dir/netlink.json5" > "$outdir/netlink.json5"

# logs: journald ingestion is already on; both profiles add the on-demand debug
# report. demo-max additionally lights up the alerting analytics over the
# (default-on) Drain3 template miner — SLO error-budget burn alerting — and the
# epic #542 additions: the durable per-line store (#544, days of queryable
# history + server-side search depth) and the log-bundle export artifact
# (#555). The store lives under the run dir so it's self-contained and cleaned
# with it. production keeps all of those at their shipped default (off): the
# budget alerts are the false-positive-prone bit, and the store on a tmpfs run
# dir is RAM.
# Scoped ranges are mandatory: a bare `enabled: false` would hit artifacts.report.
logs_seds=(
    -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/'
)
if [[ "$profile" == "demo-max" ]]; then
    logs_seds+=(
    -e '/^    error_budget: \{/,/^    \},/ s/^( *)enabled: false/\1enabled: true/'
    -e '/^    store: \{/,/^    \},/ s/^( *)enabled: false/\1enabled: true/'
    -e '/^    store: \{/,/^    \},/ s#path: null#path: "'"$outdir"'/logs-store.redb"#'
    -e '/^    logbundle: \{/,/^    \},/ s/^( *)enabled: false/\1enabled: true/'
    )
fi
sed -E "${logs_seds[@]}" "$configs_dir/logs.json5" > "$outdir/logs.json5"

# sysinfo: the opt-in collectors (hwmon temps + fans, cgroup-v2 saturation, TCP
# states, top processes), the thermal alert that grades against the trip points
# those temps carry, the on-demand debug report, plus (when --snapshot-dir is
# given) a Tier-2 directory snapshot of that path.
# Scoped seds so each block's own `enabled` flips, not the alert rules'.
#
# Some boards expose one physical EC through two hwmon drivers, publishing every
# temperature and fan twice: Dell's modern `dell_ddv` (labelled "CPU Fan",
# "Ambient", ...) and the legacy `dell_smm` (identical readings, unlabelled).
# Drop the unlabelled twin, but ONLY when the labelled one is present — a
# pre-2022 Dell exposes just `dell_smm`, and excluding it there would delete all
# its fan and EC-temperature data.
exclude_chips="[]"
if grep -qxs dell_ddv /sys/class/hwmon/*/name; then
    exclude_chips='["dell_smm"]'
fi

sysinfo_seds=(
    -e '/^    report:/,/enabled:/ s/enabled: false/enabled: true/'
    # Line-anchored inside the collect block, so `processes` cannot also rewrite
    # `top_processes: 10` or the sibling `processes: {` argv-scrub block.
    -e '/^    collect: \{/,/^    \},/ s/^( *)(processes|temperatures|tcp_states|cgroups|power): false/\1\2: true/'
    # The only alert rule that ships off: it needs the critical trip points that
    # collect.temperatures (just flipped) provides. Single-line, so the literal
    # prefix cannot reach any other rule's `enabled:`.
    -e 's/thermal: \{ enabled: false/thermal: { enabled: true/'
    -e "s#exclude_chips: \[\]#exclude_chips: $exclude_chips#"
)
if [[ "$ebpf" == 1 ]]; then
    # runqlat + biolatency histograms on @rpc/sysinfo/latency (never streamed).
    # The binary is a no-op for this unless built --features ebpf AND holding
    # CAP_BPF/CAP_PERFMON/CAP_DAC_READ_SEARCH, on a host whose
    # perf_event_paranoid is <= 2 (#683).
    #
    # `just configure` passes --ebpf on the strength of *toolchain detection*
    # alone (justfile's `ebpf_on`) — it knows nothing about whether the caps
    # were granted, and said otherwise here until #685. Turning the flag on
    # without them yields `available: false` and one warning, which is the
    # designed-for outcome and not a lie in the config: `collect.ebpf` is a
    # request, and the sensor reports honestly when it cannot serve it.
    # `just sysinfo` now depends on `_sysinfo-caps`, so the two travel together
    # on the demo path.
    sysinfo_seds+=(-e '/^    collect: \{/,/^    \},/ s/^( *)ebpf: false/\1ebpf: true/')
fi
if [[ -n "$snapshot_dir" ]]; then
    sysinfo_seds+=(
        -e '/snapshot: \{/,/dirs: \[/ { s/enabled: false/enabled: true/; s#dirs: \[#dirs: [ { name: "docs", path: "'"$snapshot_dir"'" },# }'
    )
fi
sed -E "${sysinfo_seds[@]}" "$configs_dir/sysinfo.json5" > "$outdir/sysinfo.json5"

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

notes=""
[[ "$ebpf" == 1 ]]              && notes+=" +ebpf(sysinfo)"
[[ -n "$pcap_dir" ]]            && notes+=" pcap='$pcap_dir'"
[[ -n "$snapshot_dir" ]]        && notes+=" snapshot='$snapshot_dir'"
[[ "$exclude_chips" != "[]" ]]  && notes+=" hwmon-exclude=$exclude_chips"
detectors="detectors on"
[[ "$profile" == "production" ]] && detectors="detectors OFF"
echo "Configured ($profile): netring iface='$iface' (L7 on, $detectors), netlink, logs=journald, sysinfo=+thermal/fans/cgroups, systemd=full, parallax=test-pattern, correlator$notes  (configs in $outdir/)"
