---
title: "Schema"
description: "Every item, category, building block and property the config can hold."
date: 2026-10-29
updated: 2026-11-04
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
| `remote_kind` | `remote_kind <name> { }` | [17](#17-remotes) |
| `remote` | `remote <name>: <kind> { }` | [17](#17-remotes) |
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
| `file` | path | File content, trailing newline stripped; a relative path is from the main config file's folder |

Exactly one of the two. Read at load and on reload. In expressions: `secrets.<name>`, only allowed inside `run` and `env` of sources and actions; masked in logs and `ctl` output.

## 3. Vars

UI state that actions can change (`set`), typed so expressions check at load.

```
var vm: string = ""
var tab: int = 0
```

In expressions: `vars.<name>`. Kept in memory; reset on restart. A reload keeps a value the screen set while the var keeps its name and type; one still at its default takes the new default.

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
| `history` | duration | none | Keep past values in memory for charts: numbers, bools and text up to 64 bytes (pick one with `value`); at most 2000 |
| `active` | `always`, `visible` | `always` | `visible`: poll only while a page that reads it (directly, through a component or the shell) is on screen and the screen is awake; back on screen it runs at once if its last run is 5s old. A page that reads `sources` whole or by a computed name (`sources[x]`) reads them all. A source an alert or an action reads always polls |
| `env { }` | block | | `KEY = value` pairs passed to the command |
| `cwd` | path | config folder | Working directory ([ADR 0013](decisions/0013-command-runner.md)) |

In expressions, `sources.<name>`:

| Field | Type |
| --- | --- |
| `value` | the value's type |
| `ok` | bool, last run succeeded; `false` (with `value` null) before the first result. A run with no result (timeout, unparseable output, a stream that stopped or cannot start) changes only `ok` and `error`; the rest of the last result stays |
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
| `title` | string, expr | How the action is named in its dialogs and messages (`PIN for <title>`, `<title> failed`); without it they name no action |
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

Order: params asked, PIN, confirm, `run`, then on success `set`, `goto`, `refresh`, `then`; on failure `else`. While an action runs, further calls of it with the same args are ignored, also across a reload; calls with other args (another row of a `repeat`, a toggle switched back) wait and run in the order they were made. Different actions run side by side, at most `app:max_running` commands at once.

`result`: `{ok: bool, code: int, stdout: string, stderr: string, out: <parsed>}`.

### Calling an action

Wherever a type is `action` (`on_tap`, `then`, a component param):

| Form | Example |
| --- | --- |
| By name | `on_tap = refresh_all` |
| By an expression giving a name | `on_tap = {{ tap }}` (a component's `action` param), `on_tap = {{ vars.on ? 'stop' : 'start' }}`; `none` binds nothing |
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
| `pattern` | regex the whole text must match ([ADR 0019](decisions/0019-param-patterns.md)); `paneld check` checks it compiles and that a default matches |
| `options` | allowed values (list, expr) |
| `ask` | `select`, `slider`, `keypad`, `toggle`: if the caller did not pass it, a dialog asks for it (actions only) |
| `label` | dialog label |

A value outside its type, `min` / `max` or `options` is refused before anything runs, with a notice on screen. A param that is not passed takes its default; with `ask`, a dialog asks for it instead (Cancel drops the call).

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

Layout blocks are transparent and borderless: only a `background`, `border` or theme style draws them.

| Block | Keys | Notes |
| --- | --- | --- |
| `row`, `column` | `gap`, `justify` (`start`, `center`, `end`, `between`, `around`, `evenly`), `align` (`start`, `center`, `end`, `stretch`), `wrap` (bool) | Flex |
| `grid` | `columns`, `rows` (count, or list of tracks: px, `1fr`, `content`), `gap`, `row_gap`, `column_gap` | |
| `stack` | | Children on top of each other, placed with `x`, `y`, `anchor` |
| `scroll` | `direction` (`vertical`, `horizontal`), `snap` (`none`, `start`, `center`), `gap`, `align` | Children laid out as a column or row, scrollable along it. Fills the parent's width and, in a column, the space left, unless `height` or `grow` is set |
| `slot` | | Inside components only |
| `repeat <name> in {{ list }} { }` | | Body once per item |

### 8.3 Visuals

| Block | Keys |
| --- | --- |
| `text` | `value` (string, expr), `align` (`left`, `center`, `right`), `overflow` (`wrap`, `clip`, `dots`, `scroll`) |
| `icon` | `name` (theme icon or built-in symbol, expr), `size` |
| `image` | `src` (path to a PNG, expr; relative to the config folder), `fit` (`contain`, `cover`, `fill`, `none`), `recolor` (color) |
| `rect` | none beyond the common ones: a shape |
| `arc` | `value`, `min` = 0, `max` = 100 (expr), `start` = 135, `end` = 45 (degrees), `thickness`, `track` (color), `rounded` (bool) |
| `bar` | `value`, `min`, `max` (expr), `direction` (`horizontal`, `vertical`), `track` (color) |
| `line` | `points` (list of `[x, y]` in px, expr), `thickness` (default 2), `rounded`, `color` (default the theme's `text`) |
| `chart` | `type` (`line`, `bar`; `area` draws as `line` for now), `min`, `max` (default: the data's range), `points` (count, default: the longest series), `divisions` (`<h> <v>`, default `3 0`), `series { values = {{ }}; color = ... }` repeatable. `values` is a list of numbers or of `{value}` maps, such as a source's `history`; the newest `points` are shown, against the right edge |
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

`on_change` and `on_submit` take the same forms as `on_tap`: `= action`, `action { args }`, or an inline block. The new value arrives as `value` and `event.value`. An inline block with action keys (`run`, `pin`, `confirm`, `then`, ...) is an action of its own, gates included; one with only `set { }` just sets.

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

In expressions, `alerts`: `active` (list, worst first), `count`, `worst` (severity), `shown` (active and neither acknowledged nor snoozed: what the banner shows), `snooze` (the `alerts:snooze` choices), `history` (resolved alerts, newest first, kept for `alerts:history`, default 30d, at most 200; each with `id`, `severity`, `title`, `text`, `since`, `ended`), `recent` (the newest 10 of `history`), each alert with `id`, `rule`, `severity`, `title`, `text`, `link`, `since` and `snoozed_until` (Unix seconds), `acked`, `snoozed`. `ctl alert fire|resolve` creates the same alerts.

Acknowledge and snooze are the built-in actions `alert_ack { id }` and `alert_snooze { id; for }` ([ADR 0016](decisions/0016-builtin-alert-actions.md)). The shipped page `alerts` lists every alert with both buttons, then the `recent` resolved ones; the shell's banner and alert count open it.

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
| `every` | duration | Months and years step the calendar (Jan 31 + 1 month is Feb 28); the rest rounds up to whole days |
| `start` | date (`YYYY-MM-DD`) | First due date; default: one `every` after the reminder is first seen |
| `once` | date | Instead of `every`: a one-time reminder |
| `title`, `text`, `link` | string | `title` default: the name |
| `severity` | | default `info` |

Exactly one of `every` and `once`. A reminder is due from local midnight of its date and shows as the alert `reminder:<name>` (rule `reminder`), so the banner, ack and snooze work as for any alert. The built-in action `reminder_done { id }` (the name or the alert id) records today and resolves it; the next one is due one `every` after that day, or never for `once`. The shipped `alerts` page shows Done instead of Acknowledge for reminders; `paneld ctl reminder done <name>` does the same. First-seen and done dates live in the state file.

The alert names `ctl`, `reminder` and `source_down` are reserved for the panel's own alerts.

## 11. Themes

See [Themes](themes.md) for the folder, styles, components and motion. Tokens every theme must define (directly or through `extends`):

| Group | Tokens |
| --- | --- |
| Colors | `background`, `surface`, `text`, `muted`, `accent`, `ok`, `info`, `warning`, `critical` |
| Fonts | `font_small`, `font`, `font_big`, `font_huge`: font values with size, e.g. `roboto 16` |
| Sizes | `gap`, `padding`, `radius`, `tap_min` (smallest touch target) |
| Other | `shadow`, `background_image` (optional), `icons` (icon set folder), `motion`, `dialog_backdrop` (behind a dialog, over the page: `transparent` (default), `#rrggbb[aa]`, or `blur <px>` up to 20: the Pi blurs in software) |

A theme may add any other token; they are read as `theme.<name>`.

## 12. Screen

```
screen {
    theme      = dark
    idle       = 2m
    brightness = 80
    on         = sh -c 'echo 0 > /sys/class/backlight/*/bl_power'
    off        = sh -c 'echo 1 > /sys/class/backlight/*/bl_power'
    dim        = sh -c 'echo "$$1" > /sys/class/backlight/*/brightness' sh {{ level }}
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
| remote instances by name (section 17) | everywhere |
| `secrets` | only in `run` and `env` |
| params by name | in components and actions |
| `<repeat name>` | inside `repeat` |
| `out` | in a source's `value` |
| `event` | in an alert with `from` |
| `value`, `event.value` | in `on_change` / `on_submit` calls |
| `result` | in `then` / `else` |
| `level` | in `screen:dim` |

Secret values (4 characters or more) are replaced with `****` in everything commands print before it is logged, shown, or put in `result` or a source's value: in parsed output each string, and each number whose digits hold a secret, then once more in the source's `value` (a masked number becomes the text `****`). Shorter secrets, and encoded forms (base64, URL escapes), are not caught. Prefer `env { TOKEN = {{ secrets.token }} }` over putting a secret in `run`: argv items are visible to every local user in `/proc/<pid>/cmdline`, the environment is not.

`ago(t)` is worked out when the page is drawn; an idle screen is not redrawn, so the text can lag until something changes.

Functions (`paneld check` refuses any other name, or a wrong number of arguments):

| Function | Result |
| --- | --- |
| `string(x)`, `int(x)`, `number(x)` / `double(x)` | conversions; `int` drops the fraction, reads text and bools |
| `len(x)` / `size(x)` | characters of text, items of a list or map |
| `round(x)`, `round(x, n)` | to a whole number, or `n` decimals |
| `has(a.b)` | `true` when the field exists |
| `ago(t)` | Unix seconds as `just now`, `5m ago`, `3h ago`, `2d ago` |
| `duration(x)` | seconds as the two largest units: `45s`, `5m 3s`, `1h 5m`, `3d 4h` |
| `format(x, unit)` | `bytes` in steps of 1024 (`1.5 KiB`), `si` in steps of 1000 (`2.3 M`), `%` (`42%`), any other unit after the number (`21.5 C`); at most one decimal |
| `timestamp(t)` | Unix seconds as `2026-11-04T18:30:00Z` (UTC) |
| `clamp(x, a, b)` | `x` kept within `a`..`b` |
| `color(c)` | a theme color token or `#rgb` / `#rrggbb` / `#rrggbbaa`, as `#rrggbb[aa]` |
| `mix(a, b, t)` | colors `a` and `b` blended, `t` from 0 to 1 |

Methods: `l.all(x, p)`, `l.exists(x, p)`, `l.exists_one(x, p)`, `l.filter(x, p)`, `l.map(x, e)` and `l.map(x, p, e)` over a list (or a map's keys); `size()`, `contains(s)`, `startsWith(s)`, `endsWith(s)`, `lowerAscii()`, `upperAscii()`, `trim()`, `split(sep)` on text; `join()` / `join(sep)` on a list of text.

## 17. Remotes

A machine or service described once: how to reach it and what it exposes ([ADR 0018](decisions/0018-remotes.md), which has the exact commands).

```
# remotes/linux.conf
remote_kind linux {
    param host: string
    param user: string = admin

    via  = ssh
    host = {{ host }}
    user = {{ user }}
    key  = /etc/paneld/id_ed25519

    value temp {
        run   = cat /sys/class/thermal/thermal_zone0/temp
        parse = number
        value = {{ out / 1000.0 }}
        poll  = 30s
    }
    action reboot {
        pin     = true
        confirm = Reboot {{ host }}?
        run     = sudo systemctl reboot
    }
    action restart {
        param unit: string { ask = select; options = [nginx, ssh] }
        run = sudo systemctl restart -- {{ unit }}
    }
}

# paneld.conf
include = remotes/*.conf
remote device1: linux { host = 10.0.0.5 }
remote device2: linux { host = 10.0.0.6; user = pi }

page home {
    text { value = {{ device1.status.temp.ok ? string(round(device1.temp, 1)) + ' C' : device1.status.temp.error }} }
    button { label = Reboot; on_tap = device1.reboot }
}
```

### 17.1 Kind

| Key | Notes |
| --- | --- |
| `param` | repeatable; per-instance settings, not named like an expression root (section 17.3): `string`, `int`, `number`, `bool`, `duration`, `color`, `size`, or `secret` (the name of a `secret` item; usable only where `secrets` is). No `ask` |
| `via` | `local`, `ssh`, `http`, `socket`; required |
| connection keys | per `via`, below; may use `{{ }}` with the params only (and `secrets` where noted) |
| `timeout` | duration; default for its members |
| `value <name> { }` | a source (section 4) |
| `action <name> { }` | an action (section 5) |

| `via` | Keys |
| --- | --- |
| `local` | none |
| `ssh` | `host` (required), `key` (private key file, required), `user`, `port` (22), `known_hosts` (default `known_hosts` in the folder of `app:state`), `host_key` (`check` default, or `accept_new`) |
| `http` | `url` (base URL, required; `secrets` allowed), `headers { Name = value }` (`secrets` allowed; `_` in a name is sent as `-`: `X_Api_Key`), `ca` (CA file), `insecure` (bool, skip TLS checks) |
| `socket` | `path` (Unix socket), or `host` and `port` (TCP) |

### 17.2 Members

A `value` takes the source keys and an `action` the action keys, with the request in place of `run` where the `via` needs it:

| `via` | Request |
| --- | --- |
| `local` | `run`, as anywhere |
| `ssh` | `run`: argv, each item quoted for the remote (POSIX) shell. No `env { }`, no `cwd`, no `secrets` |
| `http` | `get`, `post`, `put`, `patch` or `delete` = path after `url` (each `{{ }}` in it percent-encoded; no `secrets`), `body` (string, expr; `secrets` allowed), `headers { }` (added to the kind's) |
| `socket` | `send` (string, expr): written to the socket; the reply is the output |

A `value` needs its request (`run`, a method, or `send`). Member names `ok`, `error` and `status` are reserved. Kind params and an action's own params may not share a name. Inside a kind, `refresh`, `then` and `else` can name a sibling member (`refresh = temp`).

### 17.3 Instance

`remote <name>: <kind> { <param> = <value> }`, a literal or `$variable` per param. `remote[device1]:host = ...` overrides one param. Names are `[a-z][a-z0-9_]*`; an instance name may not be an expression root (section 16), a CEL keyword, a `repeat` binding or a param name.

| Expression | Value |
| --- | --- |
| `device1.temp` | the value of member `temp` (null before its first result) |
| `device1.ok` | bool: at least one member value has a result and none has failed since |
| `device1.error` | the first failing member's `error` in declaration order, or `""` |
| `device1.status.temp` | the source map of `temp` (section 4: `ok`, `error`, `at`, `history`, ...) |

Members are named `device1.temp`, `device1.reboot` wherever a source or action name fits: `on_tap = device1.reboot`, `on_tap device1.restart { unit = nginx }`, `refresh = device1.temp`, `alert ... { from = device1.log }`, `ctl` commands. They are not in `sources`.
