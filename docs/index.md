---
title: "paneld"
description: "A generic touch panel that draws widgets and runs commands, with everything specific in config files."
date: 2026-10-29
tags: [paneld]
series: paneld
order: 0
updated: 2026-11-05
---

paneld is a generic touch panel: "Grafana for actions". The program draws widgets, runs commands and keeps a little state. Everything specific lives in config files: the pages and how they look, where every value comes from, what every button does, what counts as an alert. It knows no service: every integration is a command, mostly `curl`.

Status: v0.1. The daemon runs on desktop (SDL) and on a Raspberry Pi (DRM/KMS), and every block, key and function in [Schema](schema.md) is implemented. The repo's `README.md` shows the examples and how to build.

## Design in short

- **One native process, written in Rust, drawing with LVGL.** No browser and no desktop: it draws directly on the screen and reads touch from the input devices. On a PC it opens in an SDL window for development.
- **Light and arm-friendly:** a static binary of a few MB, tens of MB of RAM, cross-compiled for arm64 and armv7.
- **UI as a tree of building blocks** (layouts, text, arcs, charts, buttons...), reusable components defined in the config, and CEL expressions inside property values.
- **Config in a hypr-style format** (`key = value`, blocks, `$vars`, includes), reloaded on save; `paneld check` and `paneld ctl` for checking and driving it.
- **Commands are the only integration.** Sources and actions are commands; alerts, reminders and the screen's sleep are generic features driven by config.

## Pages

| Page | Contents |
| --- | --- |
| [Requirements](requirements.md) | Scope, boundaries and requirements |
| [Architecture](architecture.md) | How the program is put together |
| [Config format](config-format.md) | The config file syntax |
| [Schema](schema.md) | Everything that can be written in the config |
| [Themes](themes.md) | What a theme can change |
| [Open questions](open-questions.md) | Decisions still to make |
| [Decisions](decisions/index.md) | Design decisions (ADRs) |

## First target

The touchscreen of a home server rack: Raspberry Pi 3B+ (arm64, 1GB RAM shared with other services), Waveshare 5" DSI touchscreen, 800x480, Raspberry Pi OS Lite with no desktop.
