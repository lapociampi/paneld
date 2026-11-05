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
| Motion | Page transitions, tap feedback, animated value changes, alert banner entrance | `page_transition = fade 200ms` |
| Inheritance | Start from another theme and change a few things | `extends = dark` |

Pages only use components, so the same pages look and are laid out differently under each theme.

```
theme glass {
    extends    = dark
    background_image = images/bg.png
    accent     = #44aaff
    font       = fonts/Inter.ttf 18
    motion     = full

    style .card { radius = 16; shadow = 12; background = #ffffff20 }
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

- `motion = full | reduced | none`, default `reduced`: fades and value animations, no full-screen slides. The Pi draws in software, so full-screen effects cost CPU.
- Each animation in a theme says the lowest level it runs at; below it, the change happens instantly.

## Switching

- One theme is active (`screen:theme`). A night theme takes over in the screen's night hours (`screen { night { theme = dim } }`), with its tokens, styles and components.
- `paneld ctl theme <name>` switches to any theme (the config's, a folder's, `light`, `dark`); `paneld ctl theme` says which is active; `paneld ctl theme --reset` gives it back to the config and the night hours. On the socket ([ADR 0014](decisions/0014-ctl-protocol-state-file.md)): `{"cmd": "theme", "args": {"name": "dark"}}` (no `name`: just the answer), `{"cmd": "theme_reset"}`; the result is the active theme's name, also in `status` as `theme`. Like `set` and `goto`, a ctl theme is not saved: it lasts until `--reset` or a restart, and over a reload while the new config still has it. While it is set, night hours still dim and idle but do not change the theme.
- Every theme is checked with the config; a ctl switch also checks that the theme has every component only themes define that the pages call. A theme or config that fails is not applied; the error is shown.
- The program ships a `light` and a `dark` theme (`library.conf`, like the shell).
