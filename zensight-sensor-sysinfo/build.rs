//! Compiles the eBPF program crate to bytecode — but ONLY under `--features
//! ebpf`. On a default/stable build this is a no-op, so the workspace build
//! never invokes `aya-build` / `bpf-linker` or needs nightly. (#99)
//!
//! Build scripts don't see `cfg(feature = ...)`; Cargo exposes enabled features
//! as `CARGO_FEATURE_<NAME>` env vars instead, which is what we gate on.

fn main() {
    // Re-run if the gate changes.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EBPF");

    #[cfg(feature = "ebpf")]
    build_ebpf();
}

#[cfg(feature = "ebpf")]
fn build_ebpf() {
    use aya_build::{Package, Toolchain};

    // aya-build resolves the program crate by `--package`, so it must be a
    // workspace member (it is — see root Cargo.toml). It compiles to
    // bpfel-unknown-none and drops the object in OUT_DIR/<name> for
    // include_bytes_aligned!. The build uses the `nightly` toolchain via
    // `rustup run` (needs rust-src + bpf-linker installed).
    aya_build::build_ebpf(
        [Package {
            name: "zensight-sensor-sysinfo-ebpf",
            root_dir: concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../zensight-sensor-sysinfo-ebpf"
            ),
            no_default_features: false,
            features: &[],
        }],
        Toolchain::default(),
    )
    .expect("build eBPF program crate");
}
