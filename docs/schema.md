---
title: "Schema"
description: "Every item, category, building block and property the config can hold."
date: 2026-10-29
updated: 2026-11-06
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
| `ok` | bool, last run succeeded; `false` (with `value` null) before the first result, and after a failed run whose output does not parse. A run with no result (timeout, unparseable output, a stream that stopped or cannot start) changes only `ok` and `error`; the rest of the last result stays |
| `error` | string, stderr or the reason |
| `at` | timestamp of the last value |
| `age` | int, whole seconds since `at` (none before the first result); a page that reads it by name (`sources.x.age`, not `sources.x` whole or `sources[n]`) is redrawn on the steps of `ago()` (section 16), not every second |
| `running` | bool, a run is under way (a poll or a `refresh`; a stream: while its command lives); redrawn as it changes when read by name, like `age` |
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
| `title` | string, expr | How the action is named in its dialogs and messages (`PIN for <title>`, `<title> failed`); reads the call's params (`title = Stop {{ name }}`); without it they name no action |
| `run` | command | Optional: an action may only change the UI |
| `timeout` | duration | |
| `env { }` | block | |
| `cwd` | path | Working directory; default the config folder |
| `set { }` | block | `var = value` pairs, after a successful run |
| `goto` | page name or `back` | After a successful run |
| `refresh` | list of sources | Run them now, after the run |
| `then` | action call | After success; can read `result` |
| `else` | action call | After failure; can read `result` |
| `busy` | string, expr | Shown on the button that made the call while it runs (default: a spinner). `busy`, `done` and `failed` read the call's params |
| `done` | string, expr | Shown briefly on success; `output` shows stdout |
| `failed` | string, expr | Shown on failure (default: stderr) |

Order: params asked, PIN, confirm, `run`, then on success `set`, `goto`, `refresh`, `then`; on failure `else`. While an action runs, further calls of it with the same args are ignored, also across a reload; calls with other args (another row of a `repeat`, a toggle switched back) wait and run in the order they were made. Different actions run side by side, at most `app:max_running` commands at once.

`result`: `{ok: bool, code: int, stdout: string, stderr: string, out: <parsed>}`; a remote `http` action adds `status`, the HTTP status of a whole answer (0 for none, or one cut off).

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

A default may read the params declared before it, as passed or defaulted: with `param a: int = 1` and `param b: int = {{ a + 1 }}`, a call with `a = 5` gets `b = 6`. A default that reads its own param or a later one is a check error. `options` read no params.

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

- One root node. Parameters are read by name in expressions. A component sees only its own params and the global roots (section 16, with `page` and `nav`), never the names where it is called (a caller's `repeat` binding or params): pass what it needs as args.
- Using it: `stat_tile { label = CPU; value = {{ sources.cpu_temp.value }} }`. Common node properties (section 8.1) set on the call apply to the root node.
- Children written inside the call go where the component places `slot { }`. Without a `slot`, children are an error.
- Library components ship with the program; a config component of the same name replaces one, and the active theme's component replaces both, with the same params ([Themes](themes.md)).

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
| `icon` | icon name (section 8.3) | Shown in the shell's navigation, before the title; read as `page.icon` and as `icon` of each `nav` entry |
| `nav` | int | Position in the navigation; absent: not in it (reached only by `goto`) |
| body | nodes | The page's content |

The theme's `shell` component wraps every page: header, navigation, alert banner, and a `slot` for the body. The shipped shell's navigation buttons share the width, each with the page's icon and title; with more than 5 pages, a page that has an icon shows only the icon.

### 8.1 Common properties (every building block)

| Key | Type | Notes |
| --- | --- | --- |
| `width`, `height` | size, expr | px, `%` of the parent, `grow`, `content` (default) |
| `min_width`, `max_width`, `min_height`, `max_height` | size | |
| `grow` | number | Share of free space in a row or column |
| `align_self` | `start`, `center`, `end`, `stretch` | Across a `row` / `column` (a row's height, a column's width); `stretch` takes all of it. In a `grid`: vertically in its cell |
| `cell` | `col row [colspan rowspan]` | In a `grid`, counted from 0, e.g. `cell = 1 0 2 1`; it must fit in the grid's `columns` and, if set, `rows`. Default: the next free cell, row by row |
| `x`, `y`, `anchor` | px or `%`, px or `%`, `top_left`, `top_center`, `top_right`, `left`, `center`, `right`, `bottom_left`, `bottom_center`, `bottom_right` | In a `stack`: `anchor` (default `top_left`) is the point of the stack the child sits at, `x` / `y` move it from there (right and down are positive, `-8` or `-10%` goes left or up) |
| `padding` | 1, 2 or 4 sizes in px | `8` all sides, `8 16` vertical horizontal, `1 2 3 4` top right bottom left |
| `class` | list of names | Theme styles (`style .card`) |
| `background` | color, gradient or image, expr | See 8.1.1. Default: none (layout blocks are transparent) |
| `opacity` | 0..1, expr | |
| `radius` | size | |
| `border` | `<width> <color>` | |
| `shadow` | `none`, `<size> [<color>] [<x> <y>]`, or a theme token holding one | Blur `size` 1 to 32 px, color default `#00000060`, offset -64 to 64 px (right and down positive), e.g. `shadow = 12 #00000080 0 4`, `shadow = shadow` (the theme's). Drawn outside the node: a parent with no padding cuts it off |
| `color` | color, expr | Foreground: text, icon, line; the value part of an `arc` or `bar` |
| `font` | font, expr | `<family> <size> [bold]`, e.g. `roboto 20`, `inter 16 bold`, or a theme font token named bare (`font = font_big`). Family: `roboto` (also `sans`), `open_sans`, `noto_sans`, `montserrat`, `inter`, or a `.ttf` path relative to the config folder, or in a theme folder's `theme.conf` to the theme folder (no `bold` then: name the bold file). Size 6 to 200 px. Unset: the theme's `font` ([ADR 0015](decisions/0015-fonts.md)) |
| `visible` | bool, expr | |
| `enter` | transition, expr | `<fade\|slide\|none> [<duration>]` (duration default `200ms`, at most `2s`): how the node comes in when it shows on a page already on screen (it was hidden or not drawn on the last render), e.g. the shell's alert banner (`enter = {{ theme.banner_transition }}`). Not on a page's first render, after a navigation, a wake, a reload or a theme switch; a redraw during it carries it on. `fade` needs the theme's `motion` at `reduced`, `slide` (down from one own height above) at `full` ([Themes](themes.md#motion)) |
| `enabled` | bool, expr | Disabled: no input, `:disabled` style |
| `on_tap`, `on_hold` | action | |

#### 8.1.1 Backgrounds

[ADR 0022](decisions/0022-backgrounds-shadows.md).

| Form | Example | Notes |
| --- | --- | --- |
| Color | `#1e1e1e`, `#ffffff20`, `surface` | A hex with optional alpha, or a theme color name |
| None | `none`, `transparent` | |
| Linear gradient | `linear 180 #000000 accent #00000000` | `linear <angle> <color> <color> [<color> <color>]`: 2 to 4 stops, spread evenly, each with its own alpha. CSS angles in degrees: `0` bottom to top, `90` left to right, `180` top to bottom, `270` right to left, others corner to corner. `0`, `90`, `180`, `270` are the cheap path (one color per row or column) |
| Radial gradient | `radial #ffffff #00000000` | `radial <color> <color> [...]`: from the center to the corners |
| Image | `images/bg.png`, `bg.png contain` | `<path>.png [fit]`, relative to the config folder (in a theme folder's `theme.conf`: to the theme folder), no `..`. Fit: `cover` (default: scaled to fill the node, the middle kept), `contain` (scaled to fit inside, centered, the node's parent shows around it), `fill` (stretched), `tile` (its own size, repeated). Clipped to `radius` |
| Theme token | `background = sunset` | A token holding any of the above (`sunset = radial #ff8800 #220044`) |

An image is decoded once per drawn size and kept (8 MiB of fitted images, the least recently used unshown ones freed first), so a page drawn again every second does not decode it again; it is box filtered down, scaled up by the nearest pixel (ship it at the size it is drawn). A file must be a PNG of at most twice the screen's pixels (800x480 at least) and 1 Mi pixels (the 4 MiB decoded-image cache); a tiled image is kept once whatever the node's size. `paneld check` checks literal values: the gradient syntax and colors, and that an image file exists, stays in its folder, is a PNG and is within that size (the screen from `display` `width` x `height`). A value from an expression that does not parse is logged once and leaves the node as it was.

Cost on the Pi (software rendering): a vertical or horizontal gradient costs about what a color does; an angled or radial one is computed per pixel. A page is rebuilt and drawn whole on every value change, so this is paid on each poll that changes a value: measured on x86 ([Themes](themes.md#cost)), the Pi is about 10 times slower.

### 8.2 Layout

Layout blocks are transparent and borderless: only a `background`, `border` or theme style draws them. Only `scroll` scrolls: what overflows any other block is clipped, with no scrollbar. The shipped shell puts the page body in a `scroll`, so a long page scrolls.

| Block | Keys | Notes |
| --- | --- | --- |
| `row`, `column` | `gap`, `justify` (`start`, `center`, `end`, `between`, `around`, `evenly`), `align` (`start`, `center`, `end`, `stretch`: children fill the cross axis unless they set their own `width` (column) or `height` (row) in px or `%`), `wrap` (bool) | Flex. `gap`, `row_gap` and `column_gap` here and below are px (an integer, or `{{ }}` giving one). `wrap` starts a new line when the next child does not fit; it needs a set `width` (row) or `height` (column) |
| `grid` | `columns` (default 2), `rows` (count, or list of tracks: px, `1fr` / `2fr` (a share of the space left), `content`, e.g. `[120, 1fr, content]`), `gap`, `row_gap`, `column_gap` | A count makes equal columns (`1fr` each) and rows sized to their content. Rows are added as the children need them |
| `stack` | | Children on top of each other, placed with `x`, `y`, `anchor` |
| `scroll` | `direction` (`vertical`, `horizontal`), `snap` (`none`, `start`, `center`), `gap`, `align` | Children laid out as a column or row, scrollable along it. Fills the parent's width and, in a column, the space left, unless `height` or `grow` is set |
| `slot` | | Inside components only |
| `repeat <name> in {{ list }} { }` | | Body once per item |

### 8.3 Visuals

| Block | Keys |
| --- | --- |
| `text` | `value` (string, expr), `align` (`left`, `center`, `right`), `overflow` (`wrap` default, `clip`, `dots`: cut with `...`, on one line unless a `height` is set (on the node, a style or a component call); `scroll`: scrolls to and fro). All but `wrap` need a `width`. A `scroll` text that does not fit keeps the screen redrawing while it is shown, which costs CPU; the panel is otherwise asleep when idle |
| `icon` | `name` (built-in icon or theme icon, expr), `size` (px, expr; default the theme's `font` size), `color` (default the text color). A built-in is a glyph of the compiled Montserrat font nearest to `size` (12, 14, 16, 20, 24, 32); above 32 the 32 px glyph is scaled up, to at most 128. With the theme's `icons` set to a folder (relative to the config folder, or to the theme folder for a theme folder's `theme.conf`), `<folder>/<name>.png` replaces the built-in of that name or adds a new one: sized to `size` x `size`, tinted with `color`; decoded once and kept at that size (up to 1 MiB of them, past that read as a plain `image`). A literal name found in neither is a check error when no folder is set; at render an unknown name draws nothing (logged once) |
| `image` | `src` (path to a PNG, expr; relative to the config folder, or in a theme folder's component to the theme folder and inside it), `fit` (`contain`, `cover`, `fill`, `none`), `recolor` (color) |
| `rect` | none beyond the common ones: a shape |
| `arc` | `value`, `min` = 0, `max` = 100 (expr), `start` = 135, `end` = 45 (degrees: 0 at 3 o'clock, clockwise, so the default leaves the gap at the bottom; a span of 360, e.g. `0` to `360`, is a full circle), `thickness` (px, at least 1), `track` (color; default the theme's `track` token when it has one, else its `muted`, faint), `rounded` (bool, default false), `color` (the value; default the theme's `accent`). For display: no knob, and a press never changes the value |
| `bar` | `value`, `min` = 0, `max` = 100 (expr), `direction` (`horizontal` default, `vertical`: fills from the bottom), `track` (color; default as on `arc`), `color` (the value; default the theme's `accent`) |
| `line` | `points` (list of `[x, y]` in px, expr), `thickness` (px, at least 1, default 2), `rounded`, `color` (default the theme's `text`) |
| `chart` | `type` (`line`, `bar`, `area`: a line with the space under it filled, translucent), `min`, `max` (default: the data's range), `points` (count, default: the longest series), `divisions` (`<h> <v>`, default `3 0`), `series { values = {{ }}; color = ... }` repeatable (`color` default the theme's `accent`). `values` is a list of numbers or of `{value}` maps, such as a source's `history`; the newest `points` are shown, against the right edge. A `line` or `area` chart draws no dot on each point. No box of its own: transparent and borderless unless `background` / `border` are set; the division lines in the theme's `muted`, faint |
| `qr` | `data` (string, expr), `size` |
| `spinner` | `size` |

Value changes of `arc`, `bar` and `chart` animate over the theme's `value_transition` (eased, from the value shown, also when the page is rebuilt mid-way) when its `motion` is `reduced` or `full`; with `none`, or on the first render after the screen woke, they show the new value at once. A chart whose points moved left (a `history` with new points) keeps its line and grows the new points out of the last one ([ADR 0023](decisions/0023-motion.md)).

Built-in icons: `home`, `settings`, `power`, `refresh`, `ok`, `close`, `plus`, `minus`, `edit`, `save`, `trash`, `cut`, `copy`, `paste`, `list`, `bars`, `bell`, `warning`, `eye_open`, `eye_close`, `left`, `right`, `up`, `down`, `prev`, `next`, `play`, `pause`, `stop`, `eject`, `shuffle`, `loop`, `audio`, `video`, `volume`, `volume_max`, `mute`, `image`, `file`, `directory`, `drive`, `sd_card`, `usb`, `upload`, `download`, `wifi`, `bluetooth`, `gps`, `call`, `envelope`, `keyboard`, `backspace`, `new_line`, `charge`, `tint`, `battery_full`, `battery_3`, `battery_2`, `battery_1`, `battery_empty`, `bullet` (LVGL's symbol set).

### 8.4 Inputs

| Block | Keys |
| --- | --- |
| `button` | `action` (action: a name, or an expression giving one like `on_tap`, e.g. `{{ tap }}`; run on a tap, unless the button has an `on_tap` too, which wins), plus children or `label` / `icon` shortcuts (both set: the icon, then the label, side by side; centered, the label on one line cut with `...` when the button is narrower, and left out beside the icon when less than two lines' height of room is left). Layout blocks and `rect` inside a button take no taps of their own unless they have `on_tap` / `on_hold`: the tap goes to the button. Shows the call its tap makes (`action`, or `on_tap`): while it runs, the `:running` state and the action's `busy` text or a spinner in place of its content; after it failed, the `:failed` state until the action runs again. A call from a dialog or a row matches when it carries every arg the button passes. A component whose root is a button shows its call's `on_tap` there, and that `on_tap` wins over the root's `action` |
| `toggle` | `value` (bool, expr), `on_change` (action; `value` = the new state), `color` (when on; default the theme's `accent`), `track` (when off; default as on `arc`) |
| `slider` | `value`, `min`, `max`, `step` (expr), `direction`, `on_change` (on release), `color` (the value and the knob; default the theme's `accent`), `track` (default as on `arc`) |
| `select` | `options` (list, expr), `value` (expr; the option, or its position), `style` (`dropdown` default, `roller`, `buttons`: a row with one checked), `on_change` (`value` = the option). A box in the theme's `surface` and `text`; the chosen option in its `accent` |
| `keypad` | `length` (max digits), `masked` (bool), `on_submit` (action; `value` = the digits) |

Inputs, buttons and the dialogs (PIN, `confirm`, `ask`, notices) are drawn in the theme's colors: `accent` for what is on or chosen (and a button, a keypad's OK, a dialog's buttons), the track for what is off (keys, option rows, a dialog's header), `surface` and `text` for boxes ([Themes](themes.md), "Inputs"). Inputs show the value from their `value` expression; a change runs `on_change` and the display follows the source of `value` (for example a `var` set by the action, or a refreshed source).

`on_change` and `on_submit` take the same forms as `on_tap`: `= action`, `action { args }`, or an inline block. The new value arrives as `value` and `event.value`; in `action { args }` an arg that reads them is evaluated when the event fires. An inline block with action keys (`run`, `pin`, `confirm`, `then`, ...) is an action of its own, gates included; one with only `set { }` just sets.

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

See [Themes](themes.md) for styles, components and motion. A theme is a `theme <name> { }` in the config, the file `themes/<name>/theme.conf` beside it (every such folder is read and checked, [ADR 0021](decisions/0021-theme-folders.md)), or a shipped one: `light` (the built-in tokens below) and `dark`; `extends` names a parent the same way. Asset paths written in a theme folder (font files, `icons`, `background_image`, literal `src` in its components) are relative to that folder; a `src` from an expression is relative to the config folder. Tokens every theme must define (directly or through `extends`):

| Group | Tokens |
| --- | --- |
| Colors | `background`, `surface`, `text`, `muted`, `accent`, `ok`, `info`, `warning`, `critical` |
| Fonts | `font_small`, `font`, `font_big`, `font_huge`: font values, e.g. `roboto 16`, `inter 24 bold`; built in `roboto` 12, 16, 24, 32. `font` is the text font of every node that sets none |
| Sizes | `gap`, `padding`, `radius`, `tap_min` (smallest touch target) |
| Other | `track` (optional color: the track of `arc`, `bar`, `toggle` and `slider`, keys and option rows; default the `muted` color, faint), `on_accent` (optional color: text on the accent; default white while it reaches 3:1 contrast there, else black when that contrasts more), `shadow` (a `shadow` value, default `none`: what `shadow = shadow` draws), `background_image` (optional: a PNG drawn behind every page, `cover`, over the `background` token, which may itself be a color, gradient or image; the shipped shell draws no background of its own), `icons` (`builtin`, or a folder of `<name>.png` icons, section 8.3), `motion` (`full`, `reduced` (default), `none`), `page_transition` (`fade 120ms`), `banner_transition` (`fade 200ms`), `value_transition` (`300ms`), `tap_transition` (`50ms`): see [Themes](themes.md#motion), `dialog_backdrop` (behind a dialog, over the page: `transparent` (default), `#rrggbb[aa]`, or `blur <px>` up to 20: the Pi blurs in software) |

A theme may add any other token; they are read as `theme.<name>`.

A `component <name> { }` in a theme replaces the config's or the library's `<name>` while the theme (or one extending it) is active, and takes the same params (name, type, optional or not).

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
| `theme` | theme name | Active theme; default: the config's first `theme`, else the shipped `light`. `paneld ctl theme` overrides it until reset or restart |
| `idle` | duration | Turn off after no touch |
| `brightness` | 0..100 | Passed to `dim` as `level` |
| `on`, `off`, `dim` | command | Backlight control |
| `wake_on` | severities | Default `[critical]` |
| `first_tap` | `wake`, `act` | Default `wake`: the tap that wakes the screen does nothing else |
| `night { }` | `from`, `to`, `theme`, `idle`, `brightness` | |

## 13. Display

| Key | Type | Notes |
| --- | --- | --- |
| `backend` | `drm`, `fbdev`, `sdl` | Default: picked at start (below) |
| `device` | path | e.g. `/dev/dri/card1`, `/dev/fb0`; default: the DRM card with a connected screen (else `/dev/dri/card0`), `/dev/fb0` |
| `touch` | path, `none` | Input device, e.g. `/dev/input/event0`; default: first touchscreen found (below); `none`: no touch input. SDL uses the mouse |
| `width`, `height` | int | The panel's size, unrotated, default 800x480. Only `sdl` uses it; `drm` and `fbdev` take the display's own mode |
| `rotation` | `0`, `90`, `180`, `270` | Default `0`. How far the panel is mounted turned clockwise from its native orientation. Pages lay out in the rotated size (90 on an 800x480 panel: 480x800) and touch turns with the picture. `sdl` opens its window at the rotated size |

The `display` block is read once, at start: a reload (`paneld ctl reload` or a saved file) does not apply changes to it, and logs which keys changed (`paneld: display: rotation changed; restart paneld to apply`). Restart to apply.

Without `backend`, the first that fits, logged at start (`paneld: display: ...`):

1. `sdl`, when the binary has it and a desktop runs. A desktop is only `WAYLAND_DISPLAY` or `DISPLAY` being set, so a desktop build started over `ssh -X` picks SDL.
2. `drm`, when the binary has it and a card in `/sys/class/drm` has a `connected` connector.
3. `fbdev`, when `/dev/fb0` exists.
4. `sdl`, when the binary has it; else the binary's device backend, which then fails with an error.

Each one that does not open is followed by the next in this list. A `backend` or `device` set in the config is tried first; if it does not open, paneld goes on with the list and shows the error as a warning alert, with the file and line of the key (for example `paneld.conf:12:5`, then what did not open and what is used instead), until a restart. Only when nothing opens does paneld exit with an error.

The default touchscreen is the first device in `/proc/bus/input/devices` with absolute X/Y (`ABS_X`/`ABS_Y` or `ABS_MT_POSITION_X`/`_Y`) and `BTN_TOUCH`, not a touchpad (`INPUT_PROP_POINTER`); one marked as on the screen (`INPUT_PROP_DIRECT`) wins. Mice and keyboards never match. It is looked for once, at start: a touchscreen plugged in later needs a restart, and one that goes away is dropped.

Rotation: `fbdev` rotates each drawn area in LVGL's driver; `drm` always draws into one extra upright buffer in ordinary memory (width x height x 4 bytes, 1.5 MB at 800x480; [ADR 0012](decisions/0012-display-input.md)) and on each frame copies the areas that changed in this frame and in the frame before (the panel's two buffers take turns, so each is two frames behind) into the panel's buffer, turned when `rotation` is set, up to 8 areas per frame (more are joined into the nearest), never one box around far-apart changes. Touch coordinates are the panel's, as the controller reports them, and LVGL turns them with the picture, so a touch controller mounted the same way as the panel needs no setting. `paneld shot --rotation 90` renders what the panel gets.

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
| `sources`, `vars`, `theme`, `now`, remote instances by name (section 17) | everywhere but a source's `value` (only `out`, `now` and a kind's params) and `screen` commands |
| `alerts` | everywhere but sources and alerts without `from`, which run before it is built |
| `secrets` | only in `run` and `env` |
| `page`, `nav` | in pages, components and theme styles; not in an inline action (`on_tap { run = ... }`), which runs as an action |
| params by name | in their component or action, a default reading only the params before it (section 6) |
| `<repeat name>` | inside `repeat`, nested ones too |
| `out` | in a source's `value` |
| `event` | in an alert with `from` |
| `value`, `event.value` | in `on_change` / `on_submit` calls (an arg that reads them is evaluated when the event fires: `on_change set_level { level = {{ value }} }`), and in an action one of them names (declare `param value` to type it) |
| `result` | in an action's `done`, `failed` and `then` / `else` blocks, and in an action a `then` / `else` names |
| `output` | in an action's `done` |
| `level` | in `screen:dim` (the other `screen` commands read nothing) |
| a kind's params, `self` | in a remote kind: the params in its members and alerts, `self` in its alerts (section 17) |

`paneld check` refuses a name that is not in scope where it is written, and a field that does not exist where the shape is known: `sources.<name>` (a defined source) and its fields (section 4), `vars.<name>`, `theme.<token>` (one some theme defines, or one of section 11), `now.<field>`, `alerts.<field>` (section 9), `page.<field>` (`name`, `title`, `icon`), `secrets.<name>`, `<remote>.<member>` and `<remote>.status.<member>.<field>`. The error names the closest known name (`vars.tabb`: did you mean `tab`?). A macro's name (`x` in `l.map(x, ...)`) is in scope in that macro only.

Secret values (4 characters or more) are replaced with `****` in everything commands print before it is logged, shown, or put in `result` or a source's value: in parsed output each string, and each number whose digits hold a secret, then once more in the source's `value` (a masked number becomes the text `****`). Shorter secrets, and encoded forms (base64, URL escapes), are not caught. Prefer `env { TOKEN = {{ secrets.token }} }` over putting a secret in `run`: argv items are visible to every local user in `/proc/<pid>/cmdline`, the environment is not.

`now` is the wall clock in local time, taken when the expression is evaluated. The zone is found as libc finds it: `TZ` when set (empty: UTC; a POSIX rule such as `UTC0` or `CET-1CEST,M3.5.0,M10.5.0/3`; else a zone file, `:` in front optional: `Europe/Rome` under `/usr/share/zoneinfo`, or an absolute path), else `/etc/localtime`, else UTC; a zone that does not read is UTC. It is read at start and on reload: after changing the system time zone, reload (`paneld ctl reload`). Night hours and reminders use the same zone. `now` is reserved: a `param` or `repeat` named `now` is a check error.

| Field | Example | Field | Example |
| --- | --- | --- | --- |
| `unix` | `1793883909` (int, seconds) | `weekday` | `Thursday` |
| `year` | `2026` | `weekday_short` | `Thu` |
| `month` | `11` (1-12) | `month_name` | `November` |
| `day` | `5` | `month_short` | `Nov` |
| `hour` | `14` (0-23) | `date` | `5 November` |
| `minute` | `5` | `iso_date` | `2026-11-05` |
| `second` | `9` | `time` | `14:05` (24-hour) |
| | | `time12` | `2:05 PM` |

`paneld check` refuses any other field (`now.clock`, `now['hours']`). English names only; a page that wants another language maps them (`{{ {'Monday': 'Lunedi', ...}[now.weekday] }}`).

Redraws: a page whose expressions (its own, its components' and the shell's) read `now` is drawn again when what they read changes, and no other page is redrawn by the clock:

| The page reads | Redrawn |
| --- | --- |
| no `now`, no `ago()` | never by the clock |
| any field but `second` and `unix` | at the start of each minute |
| `ago(t)` | when a text it shows would change (`5m ago` to `6m ago`): each minute under an hour, then each hour, then each day |
| `sources.x.age` | on the same steps, counted from `x`'s `at` |
| `now.second`, `now.unix`, or `now` whole (`size(now)`, `now[k]`) | every second |

While the screen sleeps nothing is redrawn; it is drawn as it is on waking. No redraw of the page shown (the clock, a new source value) happens while the user is in the middle of something on it: a finger down (a press, a slider or roller dragged) or a dropdown's list open. It waits until that ends, then follows at once; a navigation goes ahead. Each redraw rebuilds the whole page: a seconds clock costs about 0.3% of an x86 core on a simple page (measured over 30 s, 34 redraws), roughly 10 times that on the Pi 3B+ and more on a busy page, and keeps the loop waking each second. Prefer `now.time` unless the seconds matter. `ago(t)` counts from `now.unix` (`ago(now.unix)` is `just now`); a page with `ago()` costs one redraw per change of a text it shows, no more.

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

Methods: `l.all(x, p)`, `l.exists(x, p)`, `l.exists_one(x, p)`, `l.filter(x, p)`, `l.map(x, e)` and `l.map(x, p, e)` over a list (or a map's keys); `size()`, `contains(s)`, `startsWith(s)`, `endsWith(s)`, `lowerAscii()`, `upperAscii()`, `trim()`, `split(sep)` on text; `join()` / `join(sep)` on a list of text. As in CEL, a part that decides the result wins over an error elsewhere: `a || b` is true when either side is, `a && b` false when either is, `exists` true when any item matches and `all` false when any does not, whatever the other sides or items give.

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
| `alert <name> { }` | an alert (section 9), one per instance, named `<instance>.<name>`: its expressions read the kind's params and `self`, the instance as pages see it (`when = {{ self.temp > 80 }}`); `from` and `action` can name a sibling member |

| `via` | Keys |
| --- | --- |
| `local` | none |
| `ssh` | `host` (required), `key` (private key file, required), `user`, `port` (22), `known_hosts` (default `known_hosts` in the folder of `app:state`), `host_key` (`check` default, or `accept_new`), `reuse` (duration, 1m, whole seconds: how long a connection stays open for the next run to the same target; `0s` = a new connection each run) |
| `http` | `url` (base URL, required; `secrets` allowed), `user` (`user:password` for HTTP basic auth; `secrets` allowed), `headers { Name = value }` (`secrets` allowed; `_` in a name is sent as `-`: `X_Api_Key`), `ca` (CA file), `insecure` (bool, skip TLS checks) |
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

`remote <name>: <kind> { <param> = <value> }`, a literal or `$variable` per param. `self` is reserved. `remote[device1]:host = ...` overrides one param. `via = local` on an instance of a `via = ssh` kind runs its members' `run` on the panel itself, as they are (the kind's connection keys are unused; its params are still set); no other `via` can be set per instance, and a kind param may not be named `via`. Names are `[a-z][a-z0-9_]*`; an instance name may not be an expression root (section 16), a CEL keyword, a `repeat` binding or a param name.

| Expression | Value |
| --- | --- |
| `device1.temp` | the value of member `temp` (null before its first result) |
| `device1.ok` | bool: at least one member value has a result and none has failed since |
| `device1.error` | the first failing member's `error` in declaration order, or `""` |
| `device1.status.temp` | the source map of `temp` (section 4: `ok`, `error`, `at`, `history`, ...; `status`, the HTTP status, for an `http` member) |

Members are named `device1.temp`, `device1.reboot` wherever a source or action name fits: `on_tap = device1.reboot`, `on_tap device1.restart { unit = nginx }`, `refresh = device1.temp`, `alert ... { from = device1.log }`, `ctl` commands. They are not in `sources`.
