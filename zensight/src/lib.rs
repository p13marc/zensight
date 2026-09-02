//! ZenSight - Observability frontend for Zenoh telemetry.
//!
//! This library exposes the core components for testing.

pub mod app;
pub mod demo;
pub mod entity;
pub mod history;
pub mod message;
pub mod mock;
pub mod replay;
pub mod subscription;
pub mod view;

// Re-export commonly used types
pub use app::ZenSight;
pub use message::{DeviceId, Message};

/// Force this test binary's wgpu onto the GL backend before main (#687, #829).
///
/// Every `iced_test::simulator` stands up a real wgpu device. wgpu defaults to
/// Vulkan, which a GPU-less host resolves to lavapipe — and many test threads
/// creating and destroying Vulkan instances concurrently segfault inside the
/// loader, with no output, before libtest prints a result line. Measured on
/// this crate: `--lib` 3 crashes / 10 runs by default, 0 / 10 under
/// `WGPU_BACKEND=gl` (`docs/testing.md`, "If the `zensight` tests segfault").
///
/// Setting the variable here — not in `.cargo/config.toml`, whose `[env]`
/// applies to `cargo run` and would downgrade the real GUI's renderer — makes
/// a plain `cargo test -p zensight` safe on any host. An explicit
/// `WGPU_BACKEND` from the caller still wins. The same guard sits at the top
/// of `tests/ui_tests.rs`; each test binary needs its own.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn force_gl_backend_for_tests() {
    if std::env::var_os("WGPU_BACKEND").is_none() {
        // SAFETY: `ctor` runs before `main`, while the process is still
        // single-threaded — no concurrent environment reader can exist yet,
        // which is exactly the condition edition-2024 `set_var` asks for.
        unsafe { std::env::set_var("WGPU_BACKEND", "gl") };
    }
}
