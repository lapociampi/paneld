---
title: "ADR 0022: Backgrounds are a color, a gradient or a fitted image; shadows are capped"
description: "The background and shadow syntax, how image backgrounds are decoded and kept, and what they cost a Pi drawing in software."
date: 2026-11-05
tags: [paneld, adr, themes, rendering]
series: paneld
order: 29
---

- Status: Accepted. Fills in `background` and `shadow` of [Schema](../schema.md) 8.1 and the theme's `background_image`.
- Date: 2026-11-05

## Context

The schema promised `background` as a color, an image path or a gradient on any node, a `shadow`, and a theme `background_image`; the renderer drew hex colors only. LVGL 9 has multi-stop gradients (`lv_grad_dsc_t`), background images and box shadows, all in software on the Pi.

## Decision

| Point | Rule |
| --- | --- |
| Gradient syntax | `linear <angle> <c> <c> [<c> <c>]` (CSS degrees: 180 top to bottom) and `radial <c> <c> [...]` (center to corners). 2 to 4 stops (`LV_GRADIENT_MAX_STOPS`), spread evenly; stop positions are left out until needed. Colors go through the theme like any color: names and alpha work |
| Cheap angles | 0, 90, 180, 270 use LVGL's row / column gradients; other angles and `radial` need `LV_USE_DRAW_SW_COMPLEX_GRADIENTS` (computed per pixel) |
| Image | `<path>.png [cover \| contain \| fill \| tile]`, `cover` by default (a background has no natural size to keep). PNG only, the asset rules of [ADR 0021](0021-theme-folders.md) |
| Image decoding | Decoded once, fitted to the node's drawn size (box filtered) and set as the LVGL background image, so it is clipped to `radius` and drawn under the border. Fitted buffers are cached by file, size, fit and mtime with a count of the objects showing each; unshown ones stay for the next redraw up to 8 MiB, least recently used freed first |
| Size known after layout | The renderer lays the page out once after drawing it (LVGL would before the next frame anyway) and fits the images then; a size change refits |
| Page background | Two layers under the page: a backdrop of the screen's size draws the theme's `background` (any form), the page root draws `background_image` over it; each has its own LVGL background slot. The shipped shell draws no background of its own, so both show through |
| Shadow | `none` or `<size> [<color>] [<x> <y>]`, color default `#00000060`, or a theme token holding one (`shadow = shadow`). Size capped at 32 px, offsets at 64. `LV_DRAW_SW_SHADOW_CACHE_SIZE` 64 keeps the last blurred corner (4 KiB), so identical shadows blur once |
| Image size | At most twice the screen's pixels (800x480 at least) and the 4 MiB decoded-image cache; `paneld check` reads the PNG header and applies the same limit with `display` `width` x `height` |
| Tiled images | Cached by file only, whatever the node's size |
| Check | Literal values are parsed; image files must exist inside their folder and start like a PNG |

## Consequences

- Measured on x86 ([Themes](../themes.md)): 12 shadowed cards add about 1 ms to a 1.46 ms full redraw, a radial gradient 1 ms, an image background 0.25 ms. A page is rebuilt and drawn whole on every value change, so this is paid on each change; the Pi is about 10 times slower.
- A full-screen image costs 1.5 MiB while shown (4 bytes a pixel), plus what stays in the cache.
- A theme `background` that is a gradient or image cannot be used where a plain color is needed (a toast's `background = background`).
