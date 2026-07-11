# Deterministic correlated-incident demo (#422)

The evaluation's core storytelling test: can a Rerun recording make a multi-signal network
incident *readable* — cause ramps before effect, event marks the failover, alert brackets
the window — the way an operator would want to replay it?

## 1. The script (`demo/incident.rs`)

A wireless backhaul degrades and fails over. All timestamps derive from one `base_ts`
(`--base-ts` pins it; default = now), so two runs produce **identical `.rrd` timelines**
(pinned by unit test). Continuous series ramp linearly between scripted plateaus; discrete
steps live in `const SCRIPT: &[(u64, Step)]`.

| t (s) | signal | behavior |
|---|---|---|
| 0–10 | all | healthy baseline (RSSI −60 dBm, loss 0 %, RTT 20 ms, retransmits 1/s) |
| 10–20 | `netlink … wifi/wlan0/rssi_dbm` | ramps −60 → **−85 dBm** (cause) |
| 15–22 | `netring … path/gateway/loss_percent` | ramps 0 → **8 %** |
| 18 | `netlink … sockets/tcp/retransmits` (counter) | accelerates 1/s → **50/s** |
| 22–30 | `netring … path/gateway/rtt_ms` | ramps 20 → **400 ms** |
| 28 | `events/route/replace` | **RouteChange**: default via lte0 (was wlan0) |
| 30 | `@/alerts` | **Critical firing** `wan-path-degraded`, `correlation_id=inc-<base_ts>` |
| 50–60 | all | recovery ramps back to baseline |
| 60 | `@/alerts` | **Resolved** (same `alert_key`) |

The `correlation_id` (`inc-<base_ts>`) is stamped on the route-change event *and* both alert
transitions — the cross-lane join a viewer/dataframe query can pull on. A demo `HostEntity`
(`h_deadbeef0042`) binds the netlink/netring/sysinfo sources to one host, so every lane lands
under `hosts/h_deadbeef0042/…` plus `alerts/netlink/wan-path-degraded`.

Causality reads bottom-up in the timeline: RSSI leads loss by 5 s, loss leads retransmits,
retransmits lead RTT, and the alert fires 20 s after the first physical-layer sign — exactly
the "why did this alert fire" story the evaluation wants to scrub through.

## 2. Reproducing the recording

```bash
cargo build -p zensight-rerun --bins

# 1. adapter: lone listener, scouting off, record mode
ZENSIGHT_ZENOH_LISTEN=tcp/127.0.0.1:7449 \
  ./target/debug/zensight-rerun --mode record \
      --rrd-path /tmp/incident.rrd --recording-id incident-demo --isolate &

# 2. deterministic incident, fast-forwarded (~1.3 s wall; domain time is scripted)
./target/debug/zensight-rerun-demo --connect tcp/127.0.0.1:7449 \
      incident --base-ts 1752192000000 --pace-ms 20

# 3. stop + inspect
kill -TERM %1 && sleep 1
ls -l /tmp/incident.rrd && head -c 4 /tmp/incident.rrd   # RRF2

# replay on a GPU box:  rerun /tmp/incident.rrd
```

`--pace-ms 1000` runs it in real time against a live viewer instead.

## 3. Observed (2026-07-11, headless record mode, debug build)

`--base-ts 1752192000000 --pace-ms 20` against a record-mode adapter on an isolated
loopback session:

```text
demo:    points=264 (66 s x 4 series)  events=1  alerts=2   (~1.4 s wall at 20 ms pace)
adapter: metrics=263  events=1  alerts=2  entities=1  sink_errors=0
         # 263 = 264 - 1: the retransmit counter's first sample, absorbed by the rate converter
incident.rrd: 95424 bytes, magic RRF2
```

- Every lane landed under `hosts/h_deadbeef0042/…` (the entity doc was published before the
  series — the `EntityIndex` join worked live), plus `alerts/netlink/wan-path-degraded`
  (+`/state`) and the route-change event on the host's events lane.
- Fast-forwarding at 20 ms pace produced the identical scripted timeline (domain timestamps
  are computed from `base_ts`, never from wall clock) — the whole 65 s incident records in
  under two seconds, which makes CI-able scenario recordings practical.
- Determinism note: two runs with the same `--base-ts` yield identical *timeline content*;
  the files are not byte-identical (unique `RowId`s, `log_time`, store ids).

## 4. Viewer checklist (assess on GPU box)

- [ ] One blueprint-free load shows the four ramps + the event + the alert lanes under one
      host subtree; timeline scrub tells the story without manual view setup.
- [ ] The `alerts/netlink/wan-path-degraded/state` step lane visually brackets t+30..t+60.
- [ ] Filtering the dataframe view on `correlation_id == inc-<base_ts>` pulls exactly the
      route change + two alert transitions.
- [ ] Time-cursor correlation across `hosts/...` and `alerts/...` roots (different subtrees —
      does the shared cursor suffice?).
