---
title: "ADR 0021: Theme folders are found by name beside the config"
description: "Where paneld looks for a theme folder, how its asset paths resolve, and what its components must match."
date: 2026-11-05
tags: [paneld, adr, themes]
series: paneld
order: 28
---

- Status: Accepted. Implements the folder of [ADR 0008](0008-themes.md).
- Date: 2026-11-05

## Context

- [ADR 0008](0008-themes.md) makes a theme a folder (`theme.conf`, fonts, images, icons), but until now a theme was only a block in the config and every asset path was relative to the config folder.
- A theme must be easy to copy between panels: one folder, no edits to the config but its name.
- Assets are read by the UI thread (fonts, icons, images); a theme from somewhere else must not read files outside its folder (a `.ttf` is parsed by stb_truetype, which trusts it).
- Pages are checked once; a theme that swaps a component must not break a call that checked fine.

## Decision

| Point | Rule |
| --- | --- |
| Where | `<config folder>/themes/<name>/theme.conf`, nothing else: no search path setting, no system folder |
| When | Every folder under `themes/` with a `theme.conf`, at load and on every reload, also for `paneld check`: each is checked, so `paneld ctl theme` can switch to any of them. A name the config uses that has no folder and no definition is a check error |
| Inline wins | A `theme <name> { }` in the config hides a folder of the same name; a folder hides a shipped theme (`light`, `dark`). Folder themes never become the default: with no `screen:theme`, the config's first theme, else `light` |
| The file | One `theme <name> { }`, named like its folder; anything else in it is an error at that line. `include` and `$vars` work as in the config |
| `extends` | By name, folder or inline alike: the parent is loaded the same way (`themes/<parent>/theme.conf`). An unknown parent is a check error |
| Asset paths | Relative to the theme folder: font files (`fonts/Inter.ttf 18`), `icons`, `background_image` (and `background` when it is a path), every literal `src` in its components (a value from an expression, a page's param say, stays relative to the config folder). The loader rewrites them to `themes/<name>/...`, so the renderer still reads one asset folder |
| Confinement | Asset paths (fonts, icons, theme images) are relative with no `..`. Folders on the way may be symlinks the owner placed (a theme folder, an icons or fonts folder kept elsewhere); the file itself must, with symlinks resolved, stay inside the folder it is named in. Config images keep their old rule |
| Broken, unused | A theme folder nothing names that has a problem is a warning: left out (ctl cannot switch to it), the config still loads. A named one is an error |
| Components | A theme's `component <name>` replaces the config's or the library's one while that theme (or a theme extending it) is active; nearest theme first |
| Params | A theme's component takes the same params as the one it replaces (name, type, optional or not; defaults may differ). A component only themes define: all of them agree, and every theme that can be active (configured, night) has it when a page or config component calls it |

## Consequences

- A theme folder is copied and named in one line; nothing in the config changes.
- A call is type-checked once and fits every theme; a theme cannot add a required param to `shell`.
- A theme that wants new params needs a new component name.
- The config watcher sleeps on inotify: the folders of every file the last load read (for those names), each theme folder and its subfolders whole (assets), and `themes` itself (a new theme folder). A save reloads once the events settle (200 ms); nothing polls.
- A theme kept elsewhere (a shared folder) is linked in: `themes/<name>` as a symlink. A `themes` search path setting can come later if one is asked for.
