---
title: "Themes"
description: "What a theme is and what it can change, layout included."
date: 2026-10-29
updated: 2026-11-04
tags: [paneld, themes, ui]
series: paneld
order: 5
---

v1 ([ADR 0008](decisions/0008-themes.md)). The required tokens are in [Schema](schema.md).

## A theme is a folder

```
themes/glass/
  theme.conf        # theme glass { ... }
  fonts/Inter.ttf
  images/bg.jpg
  icons/...
```

Asset paths in `theme.conf` are relative to the theme's folder.

## What a theme sets

| Part | What | Example |
| --- | --- | --- |
| Tokens | Colors, fonts, sizes, spacing, radius, shadows, background image; read as `theme.x` | `accent = #44aaff` |
| Styles | Properties for every node (`*`), per building block and per named class (a node opts in with `class = ...`), applied in that order under what the node sets itself; then per state of either: `:pressed`, `:checked`, `:disabled` (a node with `enabled = false`), `:focused`, `:hover` | `style button:pressed { scale = 0.96 }`, `style .card { radius = 16 }`, `style .card:pressed { background = #333 }` |
| Components | Replacements for library components, including the page shell (header, navigation, where banners appear): this is how a theme changes the layout | `component stat_tile { ... }` |
| Motion | Page transitions, tap feedback, animated value changes, alert banner entrance | `page_transition = fade 200ms` |
| Inheritance | Start from another theme and change a few things | `extends = dark` |

Pages only use components, so the same pages look and are laid out differently under each theme.

```
theme glass {
    extends    = dark
    background = images/bg.jpg
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
| 2 | Library components (shipped) |
| 3 | The active theme, then the themes it extends (nearest first) |
| 4 | A property written directly on a node in a page |

A raw value on a node ignores the theme; prefer tokens.

## Fonts

Details: [ADR 0015](decisions/0015-fonts.md).

- **Built in:** Roboto (the default, also `sans`), Open Sans, Noto Sans, Montserrat and Lato, regular and bold, Latin-1, rendered at any size by LVGL's TinyTTF.
- **From the theme:** any `.ttf` in the theme folder.
- A font value is `<family> <size> [bold]`: `font = roboto 20`, `font_big = montserrat 32 bold`, `font = fonts/Inter.ttf 18`.

## Motion

- `motion = full | reduced | none`, default `reduced`: fades and value animations, no full-screen slides. The Pi draws in software, so full-screen effects cost CPU.
- Each animation in a theme says the lowest level it runs at; below it, the change happens instantly.

## Switching

- One theme is active (`screen:theme`). A night theme takes over in the screen's night hours (`screen { night { theme = dim } }`), or `paneld ctl` switches it.
- A switch is checked before it happens: every token and component the pages use must exist in the new theme or its parents. A failing theme is not applied; the error is shown.
- The program ships a light and a dark theme.
