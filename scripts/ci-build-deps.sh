#!/usr/bin/env bash
# ci-build-deps.sh — the system packages a ZenSight build needs (#1099).
#
# ONE LIST, because there were six copies of it (ci.yml's five jobs and
# features-ebpf.yml) plus a seventh in release.yml that DISAGREED with all of
# them — and a disagreement between two lists that both pass is the shape of a
# build that works by luck.
#
# Why each package:
#
#   protobuf-compiler  gnmi's `protoc` build step
#   libsystemd-dev     the systemd sensor's D-Bus/journal linkage
#   libudev-dev        parallax's camera hotplug (#410). Build-time only:
#                      libudev1 is required by libapt-pkg and util-linux, so
#                      every Debian base image already has the runtime half
#   libclang-dev       bindgen, for `v4l2-sys-mit`'s build script, which
#                      generates its bindings UNCONDITIONALLY — reached from
#                      zensight-sensor-parallax via parallax-pipeline -> v4l
#   pkg-config         the `pkg-config` crate, used by the -sys crates above.
#                      Preinstalled on the ubuntu-24.04 runner and NOT in
#                      rust:*-bookworm, which is exactly why it belongs in a
#                      shared list rather than in whichever job noticed
#
# release.yml also installed `libssl-dev` and `libpcap-dev`. NEITHER IS USED:
# `Cargo.lock` contains no `openssl-sys` and no libpcap binding of any kind —
# netring's `pcap` feature is `pcap-file`, which is pure Rust. They are dropped
# here rather than copied into a seventh place. Conversely release.yml did NOT
# install `libclang-dev`, which parallax genuinely needs; its builds have been
# relying on `rust:1.98-bookworm` happening to carry libclang. That is now
# declared instead of assumed.
#
# `buildah` and `jq` stay in release.yml: they are that job's tooling, not a
# dependency of the build.
#
# Callers run this with or without sudo depending on where they are — the
# ubuntu-24.04 runner needs it, a root container does not.
set -euo pipefail

PACKAGES=(
    protobuf-compiler
    libsystemd-dev
    libclang-dev
    libudev-dev
    pkg-config
)

sudo_if_needed() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo "$@"
    fi
}

sudo_if_needed apt-get update
sudo_if_needed apt-get install -y "${PACKAGES[@]}"
