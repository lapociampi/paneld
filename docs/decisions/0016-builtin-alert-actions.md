---
title: "ADR 0016: Built-in actions for acknowledge and snooze"
description: "How a config acknowledges or snoozes an alert, and what the panel shows by default."
date: 2026-11-03
updated: 2026-11-03
tags: [paneld, adr, alerts]
series: paneld
order: 23
---

- Status: Accepted
- Date: 2026-11-03

## Context

The panel must handle acknowledge (the alert stays listed, the banner goes) and snooze (1 day, 1 week) ([Requirements](../requirements.md), section 7), and keep both across restarts ([ADR 0014](0014-ctl-protocol-state-file.md)). Every other effect in a config is an action, but these change the panel's own alert list, which no command can reach ([ADR 0005](0005-commands-only.md)).

## Decision

- Two **built-in actions**, called like any other action:

  | Action | Params | Does |
  | --- | --- | --- |
  | `alert_ack` | `id: string` | Marks the alert acknowledged: it stays in `alerts.active`, leaves `alerts.shown` |
  | `alert_snooze` | `id: string`, `for: duration` (default: the first of `alerts:snooze`) | Hides it from `alerts.shown` until `for` has passed |
  | `reminder_done` | `id: string` (reminder name or `reminder:<name>`) | Records today as done for a due reminder ([Schema](../schema.md), section 10) and resolves its alert |

  `id` is the alert's full id (`alerts.active[i].id`). A config cannot define an action with these names.
- An alert that resolves and fires again starts fresh. A repeat event for a firing alert keeps its ack and snooze.
- Expressions gain `alerts.shown` (active, not acknowledged, not snoozed; worst first) and `alerts.snooze` (the `alerts:snooze` choices as written).
- **Library items** written in the config language ship with the program and are merged at load, so `paneld check` sees them too: the built-in action declarations, the page `shell` component (banner from `alerts.shown`) and a page `alerts` listing every alert with Acknowledge and Snooze buttons. A config or theme replaces the component or the page by defining its own.
- `paneld ctl alert ack <id>`, `alert snooze <id> [duration]` and `reminder done <name>` do the same from a script.

## Consequences

- No new syntax: ack and snooze are action calls, so a theme or page can put them anywhere (a button on a tile, a hold on the banner).
- The default panel handles alerts with no config at all, and stays replaceable.
- Built-in names are reserved; a config that already used `alert_ack`, `alert_snooze` or `reminder_done` for its own action must rename it. So are the alert rule names the panel raises itself: `ctl`, `reminder`, `source_down`.
