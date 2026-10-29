---
title: "ADR 0011: Own thin LVGL bindings"
description: "Why paneld generates its own bindings to LVGL 9 instead of using an existing crate."
date: 2026-10-29
tags: [paneld, adr, lvgl, rust]
series: paneld
order: 18
---

- Status: Accepted
- Date: 2026-10-29

## Context

The program uses LVGL 9 ([ADR 0002](0002-rust-lvgl.md)): about 20 widgets, styles, fonts (including TinyTTF), the display and input drivers, and the SDL driver on a PC. Only the `ui` module talks to LVGL.

## Options

- **An existing crate (`lvgl`, lvgl-rs):** safe wrappers, but it follows LVGL 8 and wraps a different, larger surface than we need; upgrading LVGL would wait on the crate.
- **Own bindings:** bindgen generates raw `unsafe` FFI from the LVGL headers at build time; a thin safe layer covers only what the `ui` module uses.

## Decision

Own bindings, in a `lvgl-sys` crate inside the workspace.

- The build script compiles a pinned LVGL 9.x release (vendored as a git submodule or a checked-in archive) with our own `lv_conf.h`: only the widgets, drivers and features we use.
- bindgen runs on the same headers with an allowlist (`lv_.*` functions and types we use), so the generated file stays small.
- The safe layer lives in the `ui` module: object handles that cannot outlive their LVGL object, styles, event callbacks routed to Rust closures. No other module sees `unsafe`.
- Cross-compiling: bindgen and the C build use the target's sysroot (zig cc or the cross image).

## Consequences

- LVGL upgrades are in our hands: bump the pinned release, regenerate, fix what changed.
- We maintain the safe layer, but only for the parts we use.
- `lv_conf.h` is part of the design: turning a feature on (a widget, a decoder, a driver) is a deliberate change that costs binary size.
