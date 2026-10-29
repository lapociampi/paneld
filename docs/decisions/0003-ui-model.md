---
title: "ADR 0003: UI as a tree of building blocks and components, not templates"
description: "Why the UI is a tree of building blocks and components instead of templates."
date: 2026-10-29
tags: [paneld, adr]
series: paneld
order: 10
---

- Status: Accepted
- Date: 2026-10-29

## Context

The UI must be defined entirely in config, with full freedom over how it looks, and stay checkable.

## Options

- **Text templates (Jinja2 style):** total freedom, but they produce text that must be parsed again (two languages), errors appear only after rendering, logic mixes with markup, and in practice the output is HTML, which rules out a native UI.
- **A fixed widget list** (`gauge: {...}`): short configs, but only the options the author foresaw.
- **A tree of building blocks, components and expressions:** a small set of primitives drawn natively, user-defined components built from them, expressions only inside property values (QML, Flutter, ESPHome LVGL config).

## Decision

The third option. Building blocks: layouts (`row`, `column`, `grid`, `stack`, `scroll`), visuals (`text`, `icon`, `image`, `rect`, `arc`, `line`, `chart`, `qr`), inputs (`slider`, `toggle`, `select`, `keypad`, and `on_tap` / `on_hold` / `on_change` on any node). The built-in widgets are components in a library file in the same language. The only structure-level control flow is `repeat` and `visible`.

## Consequences

- Simple configs stay short; full control by writing new components.
- The whole tree is checked against a schema before drawing, with file and line in errors.
- A truly new primitive is a program change.
