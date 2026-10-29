---
title: "ADR 0002: Rust + LVGL"
description: "Why Rust and LVGL 9, with own thin bindings."
date: 2026-10-29
tags: [paneld, adr]
series: paneld
order: 9
---

- Status: Accepted
- Date: 2026-10-29

## Context

Needs: lightest possible, easy on arm, draws on the screen without a desktop, a UI built at runtime from config files, themes, charts, QR codes, touch.

## Options

- **LVGL (C):** made for embedded touch screens, a few MB of RAM, DRM/fbdev and evdev backends, SDL on a PC. Already has flex and grid layouts, label, image, arc, line, chart, QR code, slider, switch, dropdown, keyboard, and a style system that maps onto themes.
- **Slint (Rust):** declarative and light, but a UI built from config files at runtime would mean generating `.slint` source for its interpreter; GPL or commercial license for embedded use.
- **Flutter (flutter-pi), Qt Quick (eglfs):** easy runtime trees, but ~100MB class.
- Engine language for LVGL: **Rust** (no GC, small static binaries, easy cross-compiling, CEL implementation available) or **Go** (faster to write, but cgo for LVGL complicates cross-compiling, GC uses more memory).

## Decision

Rust for the program, LVGL 9 for drawing, through thin bindings to the ~20 widgets used.

## Consequences

- Static binaries for arm64, armv7 and x86_64 from a PC (`cargo-zigbuild` or `cross`).
- The LVGL C code is built by the build script; unsafe code stays inside the `ui` module.
- Development on a PC in an SDL window, WSLg included.
