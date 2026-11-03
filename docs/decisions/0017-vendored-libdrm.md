---
title: "ADR 0017: libdrm built from a vendored copy"
description: "How the DRM/KMS backend gets libdrm for a static Pi build."
date: 2026-11-03
tags: [paneld, adr, build]
series: paneld
order: 24
---

- Status: Accepted. Completes the build side of [ADR 0012](0012-display-input.md).
- Date: 2026-11-03

## Context

The DRM/KMS backend ([ADR 0012](0012-display-input.md)) needs libdrm. The Pi build is one static musl binary made with `cargo zigbuild`, and the zig toolchain carries no libdrm. A cross-sysroot from a Pi image would work but is heavy to keep and differs per machine.

## Decision

- libdrm is a git submodule at `vendor/libdrm`, pinned to a release tag (`libdrm-2.4.134`), next to `vendor/lvgl`.
- With the `drm` feature, `lvgl-sys/build.rs` compiles libdrm's core (`xf86drm.c`, `xf86drmMode.c`, `xf86drmHash.c`, `xf86drmRandom.c`, `xf86drmSL.c`) with `cc`, like LVGL. It writes the `config.h` meson would write for Linux and generates the format modifier table that libdrm's `gen_table_fourcc.py` makes, in Rust; no meson or python at build time.
- `PANELD_LIBDRM_SRC` points the build at another checkout. Without either, the system's libdrm is used (pkg-config), as before.

## Consequences

- `cargo zigbuild ... --features device` gives one static binary (about 2.2 MB, about 80 KB of it libdrm); nothing to install on the Pi.
- Updating libdrm is a submodule bump; the generated table follows the new `drm_fourcc.h` by itself. A libdrm release that adds a core source file needs a line in `build.rs`.
- Fresh clones need `git submodule update --init --recursive`, as for LVGL.
