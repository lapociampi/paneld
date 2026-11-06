---
title: "Decisions"
description: "The design decisions behind paneld, one record each."
date: 2026-10-29
updated: 2026-11-06
tags: [paneld, adr]
series: paneld
order: 7
---

| # | Decision |
| --- | --- |
| [0001](0001-single-process.md) | One process, no browser |
| [0002](0002-rust-lvgl.md) | Rust + LVGL |
| [0003](0003-ui-model.md) | UI as a tree of building blocks and components, not templates |
| [0004](0004-cel.md) | CEL for expressions |
| [0005](0005-commands-only.md) | Commands are the only integration |
| [0006](0006-config-format.md) | Hypr-style config format |
| [0007](0007-ctl.md) | Control CLI over a local socket |
| [0008](0008-themes.md) | Themes: folders with tokens, styles, components and motion |
| [0009](0009-config-parser.md) | Hand-written config parser |
| [0010](0010-threads.md) | Plain threads and channels, no async runtime |
| [0011](0011-lvgl-bindings.md) | Own thin LVGL bindings |
| [0012](0012-display-input.md) | DRM/KMS display and evdev touch |
| [0013](0013-command-runner.md) | Command runner limits, environment and retries |
| [0014](0014-ctl-protocol-state-file.md) | JSON lines for ctl, one JSON file for state |
| [0015](0015-fonts.md) | Built-in fonts |
| [0016](0016-builtin-alert-actions.md) | Built-in actions for acknowledge and snooze |
| [0017](0017-vendored-libdrm.md) | libdrm built from a vendored copy |
| [0018](0018-remotes.md) | Remotes |
| [0019](0019-param-patterns.md) | regex-lite for param patterns |
| [0020](0020-musl-static-link.md) | Static musl builds link Rust's musl with rust-lld |
| [0021](0021-theme-folders.md) | Theme folders are found by name beside the config |
| [0022](0022-backgrounds-shadows.md) | Backgrounds are a color, a gradient or a fitted image; shadows are capped |
| [0023](0023-motion.md) | Motion runs once, from what was shown, and never while asleep |
| [0024](0024-in-place-updates.md) | Pages update in place; recent pages are kept (proposed) |
