---
title: "Architecture"
description: "How the single native process is split into modules and threads."
date: 2026-10-29
updated: 2026-11-06
tags: [paneld, architecture, rust, lvgl]
series: paneld
order: 2
---

v1, before any code. One process ([ADR 0001](decisions/0001-single-process.md)), Rust + LVGL ([ADR 0002](decisions/0002-rust-lvgl.md)).

## Modules

```
config files --> [config] --> [engine] --> resolved tree --> [ui] --> screen
                                ^  |                           |
              [sources] --------+  +--> [runner] <-- actions --+ <-- touch
              [state]  <--------+
```

| Module | Does |
| --- | --- |
| config | Parses the hypr-style files ([Config format](config-format.md), hand-written parser, [ADR 0009](decisions/0009-config-parser.md)) with includes and `$vars`, checks them against the schema, reports errors with file and line, watches for changes. On an error keeps the last good config. |
| expr | Compiles CEL expressions once at load, evaluates them against the current values. |
| runner | Runs commands: argv or `sh -c`, typed inputs as argv items or env vars, clean environment, timeout, output size limit, concurrency cap, secrets resolved and masked in logs ([ADR 0013](decisions/0013-command-runner.md)). |
| sources | Poll, stream and on-demand sources on top of the runner; keeps last value, time, failure and a short history per source. |
| alerts | Maps events to alerts, keeps the active list and history, ack, snooze, reminders. |
| state | One JSON file with acks, snoozes, reminder dates, alert history; written atomically, at most every 10s (SD card) ([ADR 0014](decisions/0014-ctl-protocol-state-file.md)). |
| engine | Expands components and `repeat`, evaluates bindings, produces the resolved tree, and re-evaluates only what depends on a changed value. |
| ui | The only module that knows LVGL: builds and updates LVGL objects from the resolved tree, applies the theme, sends taps and value changes back. Screen sleep, wake, PIN pad and confirmations. |
| ctl | Unix socket server for `paneld ctl`, JSON lines ([ADR 0007](decisions/0007-ctl.md), [ADR 0014](decisions/0014-ctl-protocol-state-file.md)); the same binary is the client. |
| platform | Display and input backends: DRM/KMS (fbdev fallback) plus evdev on the device, SDL on a PC ([ADR 0012](decisions/0012-display-input.md)). |

## Threads

- LVGL is single-threaded: one UI thread owns every LVGL object and runs the LVGL timer loop.
- Everything else (sources, runner, alerts, file watching) runs off the UI thread and sends changes to it through a channel; the UI thread applies them between frames.
- Taps go the other way through a channel to the engine.
- Plain threads and channels, no async runtime; the thread list is in [ADR 0010](decisions/0010-threads.md).

## Updates

As built ([ADR 0024](decisions/0024-in-place-updates.md)):

- A page drawn keeps a node table: per node its LVGL objects and what its expressions read (`sources.x.value`, `vars.y`, `alerts.count`, `now:minute`, a call's state).
- Each pass of the main loop names what changed (a source's fields, vars, `alerts` fields, the clock, call states). An update walks the page and sets up again only the nodes that read one of those and now evaluate otherwise; the rest keep their objects untouched.
- A node that now makes other objects, or a `repeat` whose items changed, is built again where it is (kept items keep their objects); a reload or a theme switch builds the page whole.
- Only what an update changed is redrawn: the layout pass after it redraws what it moved, resized, hid or showed, not what it placed again where it was.
- Pages left are kept, hidden, and updated when they show again: `app:page_cache` (default 3) counts the page on screen, so 2 by default.
- With nothing changing, the program sleeps: no redraws, no polling of the screen.

## Build

- Cargo workspace; LVGL 9 (pinned release) compiled from source by the build script with our own `lv_conf.h`; bindings generated with bindgen in a `lvgl-sys` crate, safe layer in `ui` ([ADR 0011](decisions/0011-lvgl-bindings.md)). libdrm is built and linked statically the same way.
- Cross-compiling with `cargo-zigbuild` (or `cross`) for `aarch64-unknown-linux-musl`, `armv7-unknown-linux-musleabihf`, `x86_64-unknown-linux-musl`.
