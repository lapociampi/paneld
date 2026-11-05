---
title: "Themes"
description: "What a theme is and what it can change, layout included."
date: 2026-10-29
updated: 2026-11-05
tags: [paneld, themes, ui]
series: paneld
order: 5
---

v1 ([ADR 0008](decisions/0008-themes.md)). The required tokens are in [Schema](schema.md).

## A theme is a folder

```
p.conf              # screen:theme = glass
themes/glass/
  theme.conf        # theme glass { ... }
  fonts/Inter.ttf
  images/bg.png
  icons/...         # <name>.png, used with icons = icons
```

Details: [ADR 0021](decisions/0021-theme-folders.md).

- **Found by name:** every `themes/<name>/theme.conf` beside the config is read at load and on every reload, and checked with the config, so any of them can be named (`screen:theme`, the night `theme`, `extends`, `theme[name]:key`) or switched to with `paneld ctl theme`. A `theme <name> { }` in the config wins over a folder of that name, and a folder over a shipped theme. A name with none of these: a check error. A theme folder nothing names that has a problem is a warning instead: `paneld check` says so and still passes, and the theme is left out (ctl cannot switch to it).
- **Shipped:** `light` (the built-in default look) and `dark`, selectable by name with no folder. With no `screen:theme`, the active theme is the config's first `theme`, else `light`.
- **The file** holds one `theme <name> { }`, named like its folder; anything else in it is an error at its line. `include` and `$vars` work as in the config.
- **`extends`** names a theme the same way, in a folder or in the config: `extends = dark` loads `themes/dark/theme.conf` if the config has no `theme dark`.
- **Asset paths** written in `theme.conf` are relative to the theme's folder: font files, `icons`, `background_image` (and `background` when it is a path), and every literal image `src` in its components. A value from an expression (`src = {{ file }}`, a param a page passes) is the caller's: relative to the config folder. Paths in the config, its pages and its inline themes are relative to the config folder.
- **Symlinks:** a path has no `..`; the folders on its way may be symlinks the owner placed (a theme folder, an icons folder or a fonts folder kept elsewhere), but the file must, with symlinks resolved, stay inside the folder it is named in.
- Saving `theme.conf`, a file it includes, or an asset in a theme folder or its subfolders (`fonts/`, `icons/`, `images/`), or adding a theme folder, reloads like saving the config (inotify: nothing polls). A broken theme is refused like a broken config: the last good one stays, the error has the theme file and line.

## What a theme sets

| Part | What | Example |
| --- | --- | --- |
| Tokens | Colors, fonts, sizes, spacing, radius, shadows, background image; read as `theme.x` | `accent = #44aaff` |
| Styles | Properties for every node (`*`), per building block and per named class (a node opts in with `class = ...`), applied in that order under what the node sets itself (in the default state: a state style wins while the node is in its state); then per state of either: `:pressed`, `:checked`, `:disabled` (a node with `enabled = false`), `:focused`, `:hover`, and on a button `:running` (its action call runs) and `:failed` (its last run failed, until the action runs again; default look: a border in the theme's `critical`). A state body cannot change `width`, `height`, `grow`, `visible` or `enabled` | `style button:pressed { scale = 0.96 }`, `style .card { radius = 16 }`, `style .card:pressed { background = #333 }` |
| Components | Replacements for the config's or the library's components, including the page shell (header, navigation, where banners appear): this is how a theme changes the layout. Same params as the one replaced (see below) | `component shell { ... }` |
| Motion | How much moves (`motion`), page transitions, tap feedback, animated value changes, alert banner entrance (see Motion below) | `page_transition = fade 200ms` |
| Inheritance | Start from another theme and change a few things | `extends = dark` |

Pages only use components, so the same pages look and are laid out differently under each theme.

```
theme glass {
    extends    = dark
    background_image = images/bg.png
    accent     = #44aaff
    font       = fonts/Inter.ttf 18
    motion     = full
    page_transition = slide 250ms

    shadow     = 12 #00000080 0 4

    style .card { radius = 16; shadow = shadow; background = #ffffff20 }
    style button:pressed { scale = 0.96; background = {{ theme.accent }} }

    component stat_tile {
        param label: string
        param value: number
        column { class = card; ... }
    }
}

theme dim {
    extends = glass
    accent  = #886622
    motion  = none
}
```

## Backgrounds and shadows

- **Behind every page:** the `background` token (a color, a gradient or an image, [Schema](schema.md) 8.1.1) on a backdrop the size of the screen, then `background_image` over it, fitted with `cover` (each its own layer, so a gradient or an image `background` shows under a transparent `background_image`). The shipped shell and its body are transparent; its header and navigation bar keep `surface`. A theme's own `shell` that paints a background hides them.
- **Any node:** `background = linear 135 #1e3c72 #6dd5ed`, `background = images/card.png contain`, in a style too (`style .card { background = ... }`); a path in a theme folder's file is relative to that folder.
- **Shadow:** `shadow = <size> [<color>] [<x> <y>]`; the `shadow` token is a shadow value that a style names (`shadow = shadow`); another token works the same (`lifted = 16 #00000080 0 8`, `shadow = lifted`).

### Cost

Measured on x86 (one core, release build), a page with 12 cards on a 4-column grid, 800x480. A page is rebuilt and drawn whole on every value change (a poll, a tap), so the second column is what a page with a changing value costs each time:

| Page | ms per full redraw | ms per rebuild and redraw |
| --- | --- | --- |
| Color background, no shadow | 1.46 | 1.75 |
| 12 shadows of 4 / 16 / 32 px | 2.34 / 2.36 / 2.44 | 2.50 / 2.56 / 2.69 |
| Vertical gradient background | 1.50 | |
| 135 degree gradient | 1.93 | |
| Radial gradient | 2.43 | 2.71 |
| Full-screen image background | 1.71 | 1.98 |

The Pi 3B+ is about 10 times slower. Shadows are capped at 32 px, and LVGL keeps the blurred corner of the last shadow drawn (`LV_DRAW_SW_SHADOW_CACHE_SIZE` 64: shadow plus radius up to 64 px), so cards with the same shadow and radius blur once per frame (12 cards with 32 px shadows: 2.78 ms without it). A gradient is computed again on every redraw; a fitted image is not decoded again. A page with a full-screen background image, 8 shadowed cards and values changing every second held its RSS (15.97 to 15.99 MB over 60 s, +1.7 MB for the fitted image) at 1.5% of one x86 core (1.2% without the image and shadows).

## Precedence

| Order (later wins) | Source |
| --- | --- |
| 1 | Built-in defaults |
| 2 | Library components and pages (shipped) |
| 3 | The config's own components and pages, which replace library ones of the same name |
| 4 | The active theme, then the themes it extends (nearest first): tokens, styles and components |
| 5 | A property written directly on a node in a page |

A raw value on a node ignores the theme; prefer tokens.

## Components

- A theme's `component <name> { }` replaces the config's or the library's `<name>` while the theme, or a theme extending it, is active. The night theme brings its own (or none: then the config's and the library's are back).
- It takes the same params as the one it replaces: names, types, and which are optional (defaults may differ). So a call checked once fits every theme; a theme that needs other params uses a new component name.
- A component only themes define: all their versions take the same params, and every theme that can be active (`screen:theme`, the night theme) defines or inherits it when a page or a config component calls it.
- `paneld check` checks theme components like the config's: body, params, calls inside them.

## Fonts

Details: [ADR 0015](decisions/0015-fonts.md).

- **Built in:** Roboto (the default, also `sans`), Open Sans, Noto Sans, Montserrat and Inter, regular and bold, Latin-1, rendered at any size (6 to 200 px) by LVGL's TinyTTF.
- **From the theme:** any `.ttf` file of at most 4 MiB, relative to the theme's folder (a theme in the config: the config folder), staying in the folder it is named in (see Symlinks above). An edited file is read again when the config reloads. `bold` does not apply to a file: name the bold file.
- A font value is `<family> <size> [bold]`: `font = roboto 20`, `font_big = montserrat 32 bold`, `font = fonts/Inter.ttf 18`.
- The theme's `font` is the text font of everything that sets none; a node's `font` can also name a token bare: `font = font_big`.
- LVGL's symbol glyphs (icons inside text, the keypad's keys) are not in the TTF files; they come from a bitmap Montserrat of the nearest size, each font's last fallback. Roboto, Open Sans and Noto Sans have no arrows and take them from Montserrat.

## Motion

Details: [ADR 0023](decisions/0023-motion.md).

- `motion = full | reduced | none`, default `reduced`: fades and value animations, no full-screen slides. The Pi draws in software, so full-screen effects cost CPU.
- Each animation runs from a lowest level; below it, the change happens at once.

| Token (default) | What moves | Lowest level |
| --- | --- | --- |
| `page_transition` (`fade 200ms`) | Going to another page (a `goto`, the navigation, `ctl goto`): `fade` brings the new page in over the old one; `slide` moves the old one out to the left and the new one in from the right (from the left going `back` or to an earlier `nav` page). Taps wait until it is in. A value change on the same page never moves the page | `fade`: `reduced`, `slide`: `full` |
| `value_transition` (`300ms`) | `arc`, `bar` and `chart` values, eased from the value shown ([Schema](schema.md) 8.3) | `reduced` |
| `tap_transition` (`80ms`) | A press: what a `:pressed` style changes (`scale`, `opacity`, colors, border) moves there and back, on buttons and nodes with `on_tap` / `on_hold` | `reduced` |
| `banner_transition` (`fade 200ms`) | The shipped shell's alert banner coming in (its `enter`, [Schema](schema.md) 8.1); any node can have an `enter` | `fade`: `reduced`, `slide`: `full` |

- A transition is `<fade|slide|none> [<duration>]`, a duration `none` or up to `2s`.
- **Idle means asleep:** each animation runs once and ends; then nothing is redrawn and the loop sleeps its full second again. While the screen is off nothing animates: going to sleep ends every animation where it was going, and the render on waking shows the values as they are, at once.
- A spinner (`spinner`, a running button) and a `text` with `overflow = scroll` are content, not motion: they move at every level while shown.
- **Cost**, measured on x86 (one core, release build), 800x480, a page of 12 cards with a bar each, frames 33 ms apart. A full rebuild and redraw (a value change, no motion) is 3.5 ms:

| Animation (300 ms, 10 frames) | ms per frame, avg / max |
| --- | --- |
| Page `fade` (both pages drawn, the new one blended) | 4.1 / 5.3 |
| Page `slide` (both pages drawn, moved) | 2.5-3.1 / 3.8 |
| 12 bars moving | 1.4 / 3.1 |

The Pi 3B+ is about 4 to 6 times slower per frame on this kind of drawing (10 times on gradients and shadows, see above), so a fade costs about 16 to 30 ms of a 33 ms frame there: it fits, with little to spare. On x86 the daemon used 0.5% of a core idle, 10% during a value animation, 13% during a page fade, and 0.5% again from the end of each.

## Switching

- One theme is active (`screen:theme`). A night theme takes over in the screen's night hours (`screen { night { theme = dim } }`), with its tokens, styles and components.
- `paneld ctl theme <name>` switches to any theme (the config's, a folder's, `light`, `dark`); `paneld ctl theme` says which is active; `paneld ctl theme --reset` gives it back to the config and the night hours. On the socket ([ADR 0014](decisions/0014-ctl-protocol-state-file.md)): `{"cmd": "theme", "args": {"name": "dark"}}` (no `name`: just the answer), `{"cmd": "theme_reset"}`; the result is the active theme's name, also in `status` as `theme`. Like `set` and `goto`, a ctl theme is not saved: it lasts until `--reset` or a restart, and over a reload while the new config still has it. While it is set, night hours still dim and idle but do not change the theme.
- Every theme is checked with the config; a ctl switch also checks that the theme has every component only themes define that the pages call. A theme or config that fails is not applied; the error is shown.
- The program ships a `light` and a `dark` theme (`library.conf`, like the shell).
