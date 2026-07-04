//! Capture-tap index derivation (#333 review fix, F1).
//!
//! The on-demand capture tap's reload index must reflect whether the
//! capture-focus subscription was *actually registered*, not merely
//! `capture_focus.enabled`: if focus is enabled but its `base_expr` fails to
//! parse, focus is skipped and the tap lands at index 0, not 1. A stale index
//! would make `set_packet_filter` narrow the wrong subscription at capture time.

use zensight_sensor_netring::capture::CaptureTap;
use zensight_sensor_netring::config::NetringSensorConfig;
use zensight_sensor_netring::monitor;

fn build_tap_index(capture_focus: &str) -> Option<usize> {
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/passive_dns.pcap"
    );
    let cfg: NetringSensorConfig = json5::from_str(&format!(
        r#"{{
            netring: {{
                source: "cap-idx-test",
                pcap: "{fixture}",
                capture_focus: {capture_focus},
                capture: {{ on_demand: {{ enabled: true }} }},
            }},
        }}"#
    ))
    .expect("test config parses");
    let (_mon, _channels, keepalive, _handle, tap_index) =
        monitor::build(&cfg.netring, CaptureTap::default()).expect("monitor builds");
    drop(keepalive);
    tap_index
}

#[tokio::test]
async fn tap_index_follows_actual_focus_registration() {
    // Focus disabled → tap owns index 0.
    assert_eq!(
        build_tap_index(r#"{ enabled: false }"#),
        Some(0),
        "no focus sub ⇒ tap at index 0"
    );

    // Focus enabled with a VALID base_expr → focus registers at 0, tap at 1.
    assert_eq!(
        build_tap_index(r#"{ enabled: true, base_expr: "udp" }"#),
        Some(1),
        "registered focus ⇒ tap at index 1"
    );

    // Focus enabled but base_expr is INVALID → focus is NOT registered, so the
    // tap still lands at index 0 (the bug fixed by F1 would have reported 1).
    assert_eq!(
        build_tap_index(r#"{ enabled: true, base_expr: "!!bogus!!" }"#),
        Some(0),
        "un-registered (parse-failed) focus ⇒ tap at index 0, not 1"
    );
}
