---
title: "Requirements"
description: "What paneld is, what it does and what it deliberately leaves out."
date: 2026-10-29
tags: [paneld, requirements]
series: paneld
order: 1
---

v1. The exact keys are in [Schema](schema.md); the decisions behind them in [Decisions](decisions/index.md). Points marked `TBD` are still open.

## 1. Principle

The program is a generic engine: it draws widgets, runs commands and keeps a little state. Everything specific lives in config files:

- which pages and widgets exist and how they look;
- where every value comes from;
- what every button does;
- what counts as an alert.

A "Shut down" button runs the shutdown command written in the config. An alert pop-up appears because a command in the config produced it (e.g. `curl` reading an ntfy stream). A different setup means different config files and the same binary.

**Boundary test:** if a feature needs the program to know a service name, an API path or a protocol, it belongs in the config. The only thing the program can do with the outside world is run a command and read its output.

## 2. UI model

### Why not templates or a fixed widget list

- Text templates (Jinja2 style) produce text, not a UI: two languages, errors only after rendering, logic mixed into markup, and in practice HTML output, which rules out a native UI ([ADR 0003](decisions/0003-ui-model.md)).
- A fixed widget list (`gauge: {...}`) limits customization to the options the author foresaw.

### Building blocks

A tree of a small, fixed set of building blocks drawn natively, written in the config format ([Config format](config-format.md), [ADR 0006](decisions/0006-config-format.md)):

- layout: `row`, `column`, `grid`, `stack`, `scroll`, `slot` (where a component puts its children);
- visual: `text`, `icon`, `image`, `rect`, `arc`, `bar`, `line`, `chart`, `qr`, `spinner`;
- input: any node can take `on_tap` and `on_hold`; plus `button` (shows its action running, done or failed), and `slider`, `toggle`, `select`, `keypad` for values, which take `on_change`.

Every property is stylable: size, padding, colors (from the theme), font, radius, border, opacity.

The building blocks map one to one onto LVGL objects ([ADR 0002](decisions/0002-rust-lvgl.md)), and the drawing code is kept apart from the engine so another renderer stays possible. No raw markup escape hatch.

### Components

- Named, parameterized pieces of UI built from building blocks or other components, with declared parameters.
- The built-in widgets (gauge, stat tile, button with status, alert card) are components in a library file written in the same language. Users copy and change them, or write their own, without changing the program.
- A new building block (a truly new drawing primitive) is a program change; that is the trade for portability and validation.

### Expressions

- `{{ ... }}` only in property values, never as structure. E.g. `color = {{ temp > 70 ? theme.critical : theme.ok }}`.
- The only control flow in the tree: `repeat <name> in {{ list }} { }` and `visible = {{ }}`.
- The same expression language is used for alert mapping and conditions: CEL ([ADR 0004](decisions/0004-cel.md)).

### Validation

- The whole tree is checked against a schema before anything is drawn; component parameters are checked too.
- Errors report file and line. On an error the program keeps the last good config and shows the error on screen.

### Sketch

See the example in [Config format](config-format.md).

## 3. Themes

Details: [Themes](themes.md) ([ADR 0008](decisions/0008-themes.md)).

- A theme is a folder: tokens (colors, fonts, sizes, spacing, radius, shadows, background image), styles per building block, state and class, replacement components (so it can change the layout), motion, and `extends` for variants.
- Components use the theme's tokens (`theme.critical`), not raw values, so a theme restyles the whole UI.
- Fonts: five built-in families (Roboto, Open Sans, Noto Sans, Montserrat, Lato) and any TTF from the theme, rendered at runtime ([ADR 0015](decisions/0015-fonts.md)).
- Motion: `full | reduced | none`, default `reduced`.
- One theme is active; a night theme takes over in the screen's night hours. Every switch is validated first.
- The program ships a light and a dark theme.

## 4. Commands

Every integration is a command: mostly `curl`, also `ssh`, `wakeonlan`, scripts.

- **Form:** an argv list run directly with no shell, or explicitly a shell string (`sh -c`) when pipes are needed.
- **Inputs:** values from the UI are typed and validated (number range, one of a list, pattern) and reach the command as separate argv items or environment variables, never pasted into a shell string.
- **Result:** exit code 0 is success, anything else a failure with stderr shown. stdout is the value: text, number or JSON; a field is picked with a path, anything harder is done by `jq` inside the command.
- **Limits:** a timeout per command (default 10 s), then killed. An action runs once at a time; taps while it runs are ignored.
- **Privileges:** commands run as the program's own unprivileged user. Anything privileged goes through sudo rules or ssh forced commands set up outside the program.
- **Environment:** a clean one built for each command, run from the config folder; output size limited; at most a few commands at once ([ADR 0013](decisions/0013-command-runner.md)).
- **Secrets:** `secret` items that read an environment variable or a file, never written in the config; usable only inside `run` and `env`; masked in logs.

## 5. Sources

Sources are commands. Components and alerts bind to a source and optionally a field of it.

| Kind | Runs | Example |
| --- | --- | --- |
| poll | every N seconds, and when its page opens | a temperature, a speed, a list of VMs |
| stream | long-running, restarted if it exits; each stdout line is one event | `curl -sN <ntfy>/topic/json`, `journalctl -f -o json` |
| on demand | when a button or page asks | a connectivity test, a QR code |

- Per source the program keeps the last value, its time and whether the last run failed. Stale and failed values are shown as such.
- A short in-memory history of polled values for small charts (e.g. the last hour). Longer history comes from a source returning a series. No database.

- A source can poll always or only while a page using it is on screen (`active`).
- The value is shaped by a `parse` step (text, number, JSON, lines) and a `value` expression; no separate transforms.
- Sources are named items: any page, component or alert can use the same one.

- A failing poll source retries less and less often; a stream that exits restarts with backoff ([ADR 0013](decisions/0013-command-runner.md)).

## 6. Actions

- A button runs an action: optionally a question for missing inputs (select, slider, keypad, toggle), the PIN and a confirmation, then the command. It shows running, done or failed, then sets vars, goes to a page or refreshes the sources the config names.
- Actions take typed parameters, so one action serves every item of a list (`on_tap vm_stop { vm = {{ item.id }} }`).
- Chaining: `then` runs another action on success, `else` on failure.
- Inputs never hold their own value: a toggle or slider shows its `value` expression (a source or a var) and runs `on_change` when touched. No optimistic UI: the display follows the real state after the action.

`TBD`: action menus (several actions per list item).

## 7. Alerts and reminders

- The config maps events from any source to an alert: id, state (firing or resolved), severity (critical, warning, info), title, text, link.
- A poll source can raise an alert from a condition on its value.
- The program keeps the active list and the history, and handles acknowledge (stays listed, no banner) and snooze (1 day, 1 week).
- Severity: critical shows a banner and wakes the screen at night; warning shows a banner; info and reminders show a badge.
- Reminders are scheduled alerts in the config (id, interval, text, link). Done records the date and schedules the next one.
- A link can be shown as a QR code (e.g. a runbook).

- Alerts have instance ids, so one rule can track many alerts and a repeated event updates the same alert.
- History is kept for a configured time (`alerts { history = 30d }`).
- No sound in v1.

- A source feeding alerts that is down raises its own warning alert, so a silent stream is noticed ([ADR 0013](decisions/0013-command-runner.md)).

## 8. Screen

- Idle timer, night hours, wake on touch or on a critical alert.
- Screen on, off and brightness are commands in the config (the backlight path differs per screen).
- The first tap on a sleeping screen only wakes it.

## 9. Security

- PIN for the actions the config marks, stored as a hash from a secret; stays unlocked for a short time after entry.
- Confirmation for actions the config marks.
- Log of every action run: time, action, inputs (secrets masked), result, duration.

## 10. State and config

- Local state: a small file with acks, snoozes, reminder dates and alert history; survives restarts.
- Config: hypr-style files joined by `include` ([Config format](config-format.md)), schema-checked, reloaded on change.

## 11. Control

- `paneld check` validates a config offline; `paneld ctl` drives the running program over a Unix socket: reload, eval an expression, set vars, go to a page, fire or resolve an alert, status ([ADR 0007](decisions/0007-ctl.md)).

## 12. Not in the program

- Built-in integrations or plugins for specific services.
- A GUI editor: the config is text in a repo.
- Users and accounts.
- Metrics storage, and sending notifications to phones (the config can publish with a command).
- Automation rules ("when X, run Y"): only polls, streams and reminders run on their own.
- A remote view (phone, desktop) in v1.

## 13. Runtime and platform

- As light as possible: one native process ([ADR 0001](decisions/0001-single-process.md)), Rust + LVGL ([ADR 0002](decisions/0002-rust-lvgl.md)). Aim: a binary of a few MB, tens of MB of RAM, near zero CPU when idle.
- First target: Raspberry Pi 3B+ (arm64, Raspberry Pi OS Lite, no desktop), 800x480 DSI touchscreen, 1GB RAM shared with other services. The Pi 4 and 5 use the same drivers.
- Builds for arm64, armv7 and x86_64 as static binaries, cross-compiled from a PC.
- Draws directly on the screen (DRM/KMS or framebuffer), touch from the input devices; no X, Wayland or browser.
- Starts at boot as a systemd service; recovers by itself after a crash.
- Runs on a PC for development in an SDL window.
- Packaging: the binary and a systemd unit. A Docker image is deferred ([Open questions](open-questions.md)).
