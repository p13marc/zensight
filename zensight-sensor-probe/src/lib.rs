//! Outside-in synthetic probes (#820).
//!
//! Everything else ZenSight measures is *inside*. Nothing checked that the
//! thing works from **outside** — that the site answers, that the certificate
//! is valid, that the name resolves, that the port is reachable from where a
//! user actually is.
//!
//! # What that cost, twice
//!
//! **The `/etc/hosts` hairpin, 2026-08-20 → 2026-08-28.** A reboot dropped a
//! hosts entry, so a guest resolved `git.marcpardo.eu` to the public IP, which
//! a guest cannot reach because the edge DNAT matches the external interface
//! only. cosign and Renovate both broke. The diagnosis took **eight days** and
//! eventually hinged on someone noticing that failing runs took 2m16s — a 20 s
//! connect timeout — and that the forge's router log showed zero requests. A
//! guest-side probe of that URL would have said "timeout, 20 s" within one
//! interval, on day one. Instead the failures were first attributed to expired
//! tokens and two issues were filed on that theory.
//!
//! **TLS.** The only outside-in check the fleet had was a shell script probing
//! seven vhosts by SNI on a timer.
//!
//! # Three things this sensor is careful about
//!
//! - **A timeout is its own outcome**, never a failure with different text. A
//!   hang means packets are going somewhere that never answers; a refusal
//!   means the service said no. Those are different diagnoses, and the
//!   distinction is what took eight days to rediscover by hand.
//! - **The vantage point rides on every result and every alert.** The same
//!   target checked from the edge, from a guest and from a workstation gives
//!   three different, equally true answers. Two hosts disagreeing is not a
//!   contradiction — it *is* the finding, and it is precisely the shape of the
//!   hairpin.
//! - **An absent verdict is not a negative one.** A PEM on disk has no chain
//!   to validate; a check that did not run is not evidence about its target.
//!
//! # The one non-network check
//!
//! **Local certificate files.** Read a PEM off disk and publish its
//! `notAfter`, through the same parser the socket path uses, so both produce
//! identical documents. It retires the monthly cron that warns when a ZenSight
//! *mesh* certificate is within 60 days of expiry, and removes the oddity of a
//! supervision system needing an external timer to watch its own certificates.
//!
//! # What it does not do
//!
//! **A probe running on the server cannot tell you the server is
//! unreachable.** This is not a replacement for external outage monitoring,
//! and a deployment that treats it as one has a blind spot exactly where it
//! thinks it has coverage.
//!
//! It is also a **client only**: it makes the requests an operator configured
//! and nothing else. No listeners, no action surface.

pub mod alerts;
pub mod check;
pub mod config;
pub mod poller;
mod telemetry_guard;
pub mod tls;
