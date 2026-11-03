---
title: "Schema"
description: "Every item, category, building block and property the config can hold."
date: 2026-10-29
updated: 2026-11-03
tags: [paneld, config, reference]
series: paneld
order: 4
---

v1. What can be written in the config: every item, category, building block and property. Syntax: [Config format](config-format.md). Themes: [Themes](themes.md).

Notation: `name: type = default`. A property marked `expr` can be a `{{ }}` expression that updates live; the others are read once at load.

## 1. Top level

| Item or category | Form | Section |
| --- | --- | --- |
| `include` | `include = <path>` | [Config format](config-format.md) |
| `$vars` | `$name = <value>` | [Config format](config-format.md) |
| `secret` | `secret <name> { }` | [2](#2-secrets) |
| `var` | `var <name>: <type> = <default>` | [3](#3-vars) |
| `source` | `source <name> { }` | [4](#4-sources) |
| `action` | `action <name> { }` | [5](#5-actions) |
| `component` | `component <name> { }` | [7](#7-components) |
| `page` | `page <name> { }` | [8](#8-pages-and-building-blocks) |
| `alert` | `alert <name> { }` | [9](#9-alerts) |
| `reminder` | `reminder <name> { }` | [10](#10-reminders) |
| `theme` | `theme <name> { }` | [11](#11-themes) |
| `screen` | `screen { }` | [12](#12-screen) |
| `display` | `display { }` | [13](#13-display) |
| `security` | `security { }` | [14](#14-security) |
| `app` | `app { }` | [15](#15-app) |

Names: `[a-z][a-z0-9_]*`, one namespace per item kind. A component may not take the name of a building block.

## 2. Secrets

```
secret adguard { env = ADGUARD_AUTH }
secret pve     { file = /run/secrets/pve-token }
```

| Key | Type | Notes |
| --- | --- | --- |
| `env` | string | Environment variable name |
| `file` | path | File content, trailing newline stripped |

Exactly one of the two. Read at load and on reload. In expressions: `secrets.<name>`, only allowed inside `run` and `env` of sources and actions; masked in logs and `ctl` output.

## 3. Vars

UI state that actions can change (`set`), typed so expressions check at load.

```
var vm: string = ""
var tab: int = 0
```

In expressions: `vars.<name>`. Kept in memory; reset on restart.

## 4. Sources

```
source cpu_temp {
    poll    = 30s
    run     = cat /sys/class/thermal/thermal_zone0/temp
    parse   = number
    value   = {{ out / 1000.0 }}
    history = 1h
}
```

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `run` | command | required | |
| `poll` | duration | | Run every interval. Without `poll` or `stream`: on demand |
| `stream` | bool | false | Long-running; each stdout line is one event; restarted with backoff when it exits |
| `parse` | `text`, `number`, `json`, `lines` | `text` | How stdout becomes `out` (for a stream: each line) |
| `value` | expr | `out` | Turns `out` into the value |
| `timeout` | duration | `app:timeout` | Not for streams |
| `history` | duration | none | Keep past values in memory for charts |
| `active` | `always`, `visible` | `always` | `visible`: poll only while a page using it is on screen (never for sources used by alerts) |
| `env { }` | block | | `KEY = value` pairs passed to the command |
| `cwd` | path | config folder | Working directory ([ADR 0013](decisions/0013-command-runner.md)) |

In expressions, `sources.<name>`:

| Field | Type |
| --- | --- |
| `value` | the value's type |
| `ok` | bool, last run succeeded |
| `error` | string, stderr or the reason |
| `at` | timestamp of the last value |
| `age` | duration since `at` |
| `running` | bool |
| `history` | list of `{at, value}` |

`ctl` and the `refresh` action step run a source now.

## 5. Actions

```
action vm_stop {
    param vm: string
    pin     = true
    confirm = Stop VM {{ vm }}?
    run     = curl -sf -X POST $pve/api2/json/nodes/pve/qemu/{{ vm }}/status/stop
    refresh = vms
    done    = Stopped
}

action pause_adblock {
    param minutes: int { ask = select; options = [5, 30]; label = Minutes }
    run = curl -sf -u {{ secrets.adguard }} -d '{"enabled":false,"duration":{{ minutes * 60000 }}}' \
          http://localhost:8080/control/protection
}
```

| Key | Type | Notes |
| --- | --- | --- |
| `param` | repeatable | Typed parameters (section 6); passed by the caller or asked for |
| `pin` | bool | Ask the PIN first (section 14) |
| `confirm` | string, expr | Ask for confirmation with this text |
| `run` | command | Optional: an action may only change the UI |
| `timeout` | duration | |
| `env { }` | block | |
| `cwd` | path | Working directory; default the config folder |
| `set { }` | block | `var = value` pairs, after a successful run |
| `goto` | page name or `back` | After a successful run |
| `refresh` | list of sources | Run them now, after the run |
| `then` | action call | After success; can read `result` |
| `else` | action call | After failure; can read `result` |
| `busy` | string | Shown while it runs (default: a spinner on the caller) |
| `done` | string, expr | Shown briefly on success; `output` shows stdout |
| `failed` | string, expr | Shown on failure (default: stderr) |

Order: params asked, PIN, confirm, `run`, then on success `set`, `goto`, `refresh`, `then`; on failure `else`. While an action runs, further calls of it are ignored.

`result`: `{ok: bool, code: int, stdout: string, stderr: string, out: <parsed>}`.

### Calling an action

Wherever a type is `action` (`on_tap`, `then`, a component param):

| Form | Example |
| --- | --- |
| By name | `on_tap = refresh_all` |
| By name with arguments | `on_tap vm_stop { vm = {{ item.id }} }` |
| Inline | `on_tap { goto = settings }` |
| Nothing | `on_tap = none` |

## 6. Parameters

Used by actions and components.

```
param <name>: <type> [= <default>] [{ <options> }]
```

| Type | Values |
| --- | --- |
| `string`, `int`, `number`, `bool` | |
| `duration`, `color`, `size` | as in config-format |
| `list` | any list |
| `action` | an action call (section 5) |
| `source` | a source name; inside, `<param>.value` etc. |
| `any` | unchecked |

Options:

| Key | Notes |
| --- | --- |
| `min`, `max` | numbers |
| `pattern` | regex for strings |
| `options` | allowed values (list, expr) |
| `ask` | `select`, `slider`, `keypad`, `toggle`: if the caller did not pass it, a dialog asks for it (actions only) |
| `label` | dialog label |

A value outside its type or options is refused before anything runs.

## 7. Components

```
component stat_tile {
    param label: string
    param value: number
    param unit: string = ""
    param warn: number = 1e9
    param tap: action = none

    column {
        class  = card
        on_tap = {{ tap }}
        text { value = {{ label }}; font = {{ theme.font_small }} }
        text {
            value = {{ string(round(value, 1)) + unit }}
            font  = {{ theme.font_big }}
            color = {{ value > warn ? theme.warning : theme.text }}
        }
    }
}
```

- One root node. Parameters are read by name in expressions.
- Using it: `stat_tile { label = CPU; value = {{ sources.cpu_temp.value }} }`. Common node properties (section 8.1) set on the call apply to the root node.
- Children written inside the call go where the component places `slot { }`. Without a `slot`, children are an error.
- Library components ship with the program; a theme can replace them ([Themes](themes.md)).

## 8. Pages and building blocks

```
page home {
    title = Home
    icon  = home
    nav   = 1
    grid { columns = 3; ... }
}
```

| Key | Type | Notes |
| --- | --- | --- |
| `title` | string, expr | |
| `icon` | icon name | |
| `nav` | int | Position in the navigation; absent: not in it (reached only by `goto`) |
| body | nodes | The page's content |

The theme's `shell` component wraps every page: header, navigation, alert banner, and a `slot` for the body.

### 8.1 Common properties (every building block)

| Key | Type | Notes |
| --- | --- | --- |
| `width`, `height` | size, expr | px, `%` of the parent, `grow`, `content` (default) |
| `min_width`, `max_width`, `min_height`, `max_height` | size | |
| `grow` | number | Share of free space in a row or column |
| `align_self` | `start`, `center`, `end`, `stretch` | In the parent's cross axis |
| `cell` | `col row [colspan rowspan]` | In a `grid`; default: next free cell |
| `x`, `y`, `anchor` | size, size, `top_left`...`center`...`bottom_right` | In a `stack` |
| `padding` | 1, 2 or 4 sizes | |
| `class` | list of names | Theme styles (`style .card`) |
| `background` | color, image path or gradient, expr | |
| `opacity` | 0..1, expr | |
| `radius` | size | |
| `border` | `<width> <color>` | |
| `shadow` | `<size> <color> [<x> <y>]` | |
| `color` | color, expr | Foreground: text, icon, line |
| `font` | font, expr | `<family> <size> [bold]`, e.g. `roboto 20`, or a theme font token ([ADR 0015](decisions/0015-fonts.md)) |
| `visible` | bool, expr | |
| `enabled` | bool, expr | Disabled: no input, `:disabled` style |
| `on_tap`, `on_hold` | action | |

### 8.2 Layout

| Block | Keys | Notes |
| --- | --- | --- |
| `row`, `column` | `gap`, `justify` (`start`, `center`, `end`, `between`, `around`, `evenly`), `align` (`start`, `center`, `end`, `stretch`), `wrap` (bool) | Flex |
| `grid` | `columns`, `rows` (count, or list of tracks: px, `1fr`, `content`), `gap`, `row_gap`, `column_gap` | |
| `stack` | | Children on top of each other, placed with `x`, `y`, `anchor` |
| `scroll` | `direction` (`vertical`, `horizontal`), `snap` (`none`, `start`, `center`), `gap` | Children laid out as a column or row, scrollable |
| `slot` | | Inside components only |
| `repeat <name> in {{ list }} { }` | | Body once per item |

### 8.3 Visuals

| Block | Keys |
| --- | --- |
| `text` | `value` (string, expr), `align` (`left`, `center`, `right`), `overflow` (`wrap`, `clip`, `dots`, `scroll`) |
| `icon` | `name` (theme icon or built-in symbol, expr), `size` |
| `image` | `src` (path, expr), `fit` (`contain`, `cover`, `fill`, `none`), `recolor` (color) |
| `rect` | none beyond the common ones: a shape |
| `arc` | `value`, `min` = 0, `max` = 100 (expr), `start` = 135, `end` = 45 (degrees), `thickness`, `track` (color), `rounded` (bool) |
| `bar` | `value`, `min`, `max` (expr), `direction` (`horizontal`, `vertical`), `track` (color) |
| `line` | `points` (list of `[x, y]`, expr), `thickness`, `rounded` |
| `chart` | `type` (`line`, `bar`, `area`), `min`, `max`, `points` (count), `divisions` (`<h> <v>`), `series { values = {{ }}; color = ... }` repeatable |
| `qr` | `data` (string, expr), `size` |
| `spinner` | `size` |

Value changes of `arc`, `bar` and `chart` animate when the theme's motion allows it.

### 8.4 Inputs

| Block | Keys |
| --- | --- |
| `button` | `action` (action), plus children or `label` / `icon` shortcuts. Shows the action's state: `:running`, `:failed` styles, `busy` and `done` texts |
| `toggle` | `value` (bool, expr), `on_change` (action; `value` = the new state) |
| `slider` | `value`, `min`, `max`, `step` (expr), `direction`, `on_change` (on release) |
| `select` | `options` (list, expr), `value` (expr), `style` (`dropdown`, `roller`, `buttons`), `on_change` |
| `keypad` | `length` (max digits), `masked` (bool), `on_submit` (action; `value` = the digits) |

Inputs show the value from their `value` expression; a change runs `on_change` and the display follows the source of `value` (for example a `var` set by the action, or a refreshed source).

## 9. Alerts

```
alert ntfy {
    from     = ntfy
    when     = {{ event.event == 'message' }}
    id       = {{ event.tags[0] }}
    state    = {{ event.tags[1] }}
    severity = {{ event.priority >= 5 ? 'critical' : event.priority == 4 ? 'warning' : 'info' }}
    title    = {{ event.title }}
    text     = {{ event.message }}
    link     = {{ event.click }}
}

alert pi_hot {
    when     = {{ sources.cpu_temp.value > 70 }}
    for      = 2m
    severity = warning
    title    = Pi temperature high
    text     = {{ round(sources.cpu_temp.value, 1) }} C
}
```

| Key | Type | Notes |
| --- | --- | --- |
| `from` | source | Event mode: each new value (each line of a stream) is an `event` |
| `when` | bool expr | Event mode: filter. Condition mode (no `from`): firing while true, resolved when false |
| `for` | duration | Condition mode: must hold this long before firing |
| `id` | string expr | Instance id, so one rule can track many alerts; default the rule's name |
| `state` | expr: `firing`, `resolved` | Event mode; default `firing` |
| `severity` | `critical`, `warning`, `info`, expr | |
| `title`, `text`, `link` | string expr | `link` can be shown as a QR code |
| `action` | action call | Optional button on the alert ("Restart") |

`alerts { }` category: `history = 30d`, `snooze = [1d, 1w]`, `source_down = 2m` (after this long a failing source used by an alert raises `source_down:<name>`, [ADR 0013](decisions/0013-command-runner.md)).

In expressions, `alerts`: `active` (list, worst first), `count`, `worst` (severity), `shown` (active and neither acknowledged nor snoozed: what the banner shows), `snooze` (the `alerts:snooze` choices), each alert with `id`, `rule`, `severity`, `title`, `text`, `link`, `since` and `snoozed_until` (Unix seconds), `acked`, `snoozed`. `ctl alert fire|resolve` creates the same alerts.

Acknowledge and snooze are the built-in actions `alert_ack { id }` and `alert_snooze { id; for }` ([ADR 0016](decisions/0016-builtin-alert-actions.md)). The shipped page `alerts` lists every alert with both buttons; the shell's banner and alert count open it.

## 10. Reminders

```
reminder restore_test {
    every = 1 month
    start = 2026-11-01
    title = Test a restore
    link  = https://github.com/.../restore.md
}
```

| Key | Type | Notes |
| --- | --- | --- |
| `every` | duration | |
| `start` | date | First due date; default: one interval after first load |
| `once` | date | Instead of `every`: a one-time reminder |
| `title`, `text`, `link` | string | |
| `severity` | | default `info` |

Due reminders appear as alerts; Done records the date and schedules the next.

## 11. Themes

See [Themes](themes.md) for the folder, styles, components and motion. Tokens every theme must define (directly or through `extends`):

| Group | Tokens |
| --- | --- |
| Colors | `background`, `surface`, `text`, `muted`, `accent`, `ok`, `info`, `warning`, `critical` |
| Fonts | `font_small`, `font`, `font_big`, `font_huge`: font values with size, e.g. `roboto 16` |
| Sizes | `gap`, `padding`, `radius`, `tap_min` (smallest touch target) |
| Other | `shadow`, `background_image` (optional), `icons` (icon set folder), `motion` |

A theme may add any other token; they are read as `theme.<name>`.

## 12. Screen

```
screen {
    theme      = dark
    idle       = 2m
    brightness = 80
    on         = sh -c 'echo 0 > /sys/class/backlight/*/bl_power'
    off        = sh -c 'echo 1 > /sys/class/backlight/*/bl_power'
    dim        = sh -c 'echo {{ level }} > /sys/class/backlight/*/brightness'
    night { from = 22:00; to = 07:00; theme = dim; idle = 30s; brightness = 20 }
}
```

| Key | Type | Notes |
| --- | --- | --- |
| `theme` | theme name | Active theme |
| `idle` | duration | Turn off after no touch |
| `brightness` | 0..100 | Passed to `dim` as `level` |
| `on`, `off`, `dim` | command | Backlight control |
| `wake_on` | severities | Default `[critical]` |
| `first_tap` | `wake`, `act` | Default `wake`: the tap that wakes the screen does nothing else |
| `night { }` | `from`, `to`, `theme`, `idle`, `brightness` | |

## 13. Display

| Key | Type | Notes |
| --- | --- | --- |
| `backend` | `drm`, `fbdev`, `sdl` | Default: `drm` on a device, `sdl` with a desktop |
| `device` | path | e.g. `/dev/dri/card1`, `/dev/fb0` |
| `touch` | path | Input device, e.g. `/dev/input/event0`; default: first touchscreen found |
| `width`, `height` | int | Default: from the display |
| `rotation` | `0`, `90`, `180`, `270` | |

## 14. Security

| Key | Type | Notes |
| --- | --- | --- |
| `pin` | secret | Hash of the PIN (argon2) |
| `unlock_for` | duration | Default 1m |
| `attempts` | int | Default 5, then locked out |
| `lockout` | duration | Default 5m |

`paneld hash-pin` reads a PIN (digits) on stdin and prints the hash to store in that secret. A `pin = true` action with no usable hash refuses to run.

## 15. App

| Key | Type | Default |
| --- | --- | --- |
| `start_page` | page | the first in `nav` |
| `timeout` | duration | 10s, for commands |
| `max_output` | bytes | `1MiB` of stdout per command |
| `max_running` | int | 8 commands at once |
| `state` | path | `/var/lib/paneld/state` |
| `log` | `error`, `warn`, `info`, `debug` | `info` |
| `action_log` | path | `/var/log/paneld/actions.log` |
| `socket` | path | `/run/paneld/ctl.sock` |
| `shell` | path | none: `run` never uses one unless written |

## 16. Expression environment

| Name | Where |
| --- | --- |
| `sources`, `vars`, `alerts`, `theme`, `now` | everywhere |
| `secrets` | only in `run` and `env` |
| params by name | in components and actions |
| `<repeat name>` | inside `repeat` |
| `out` | in a source's `value` |
| `event` | in an alert with `from` |
| `value` | in `on_change` / `on_submit` calls |
| `result` | in `then` / `else` |
| `level` | in `screen:dim` |

Extra functions on top of CEL: `round(x, n)`, `format(x, unit)`, `duration(x)` and `ago(t)` for display, `clamp(x, a, b)`, `color(token or hex)`, `mix(color, color, t)`.
