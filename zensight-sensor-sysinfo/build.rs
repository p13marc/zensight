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

    const ROOT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../zensight-sensor-sysinfo-ebpf"
    );
    let toolchain = pinned_toolchain(ROOT);

    // aya-build resolves the program crate by `--package`, so it must be a
    // workspace member (it is — see root Cargo.toml). It compiles to
    // bpfel-unknown-none and drops the object in OUT_DIR/<name> for
    // include_bytes_aligned! (needs rust-src + bpf-linker installed).
    //
    // The toolchain is read from the program crate's own `rust-toolchain.toml`
    // (#1094), because `aya-build` shells out to `rustup run <toolchain>` and
    // that **bypasses `rust-toolchain.toml` entirely**. `Toolchain::default()`
    // is the literal string "nightly", so the pin next door was decorative on
    // this path: the object was built by whatever nightly the machine happened
    // to have, which on a developer box is often one that predates the
    // workspace's own `rust-version` and fails with a message about the *ebpf*
    // crates rather than about the toolchain.
    aya_build::build_ebpf(
        [Package {
            name: "zensight-sensor-sysinfo-ebpf",
            root_dir: ROOT,
            no_default_features: false,
            features: &[],
        }],
        Toolchain::Custom(&toolchain),
    )
    .expect("build eBPF program crate");
}

/// The channel pinned by `<program crate>/rust-toolchain.toml`.
///
/// One fact in one file: rustup honours that file for a direct `cargo` in that
/// directory, and this makes `aya-build`'s `rustup run` honour it too, so the
/// two cannot disagree (#1094).
#[cfg(feature = "ebpf")]
fn pinned_toolchain(root_dir: &str) -> String {
    let path = std::path::Path::new(root_dir).join("rust-toolchain.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("channel"))
        .filter_map(|l| l.split('"').nth(1))
        .next()
        .unwrap_or_else(|| panic!("no `channel = \"…\"` in {}", path.display()))
        .to_string()
}
