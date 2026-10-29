---
title: "ADR 0015: Built-in fonts"
description: "Which font families ship inside the binary, in what form, and how a config names a font."
date: 2026-10-29
tags: [paneld, adr, themes]
series: paneld
order: 22
---

- Status: Accepted. Changes the font part of [ADR 0008](0008-themes.md).
- Date: 2026-10-29

## Context

[ADR 0008](0008-themes.md) planned a small set of prebuilt bitmap fonts in the binary, plus TTF files from themes. A choice of families is wanted, but bitmaps are one file per family, size and weight: five families in four sizes and two weights would be 40 fonts and several MB, and a theme could not pick a size that was not prebuilt. The font notation also differed between [Schema](../schema.md) and [Themes](../themes.md).

## Decision

### Families

The five most used families on Google Fonts, all under the SIL Open Font License:

| Name in config | Family |
| --- | --- |
| `roboto` (also `sans`, the default) | Roboto |
| `open_sans` | Open Sans |
| `noto_sans` | Noto Sans |
| `montserrat` | Montserrat |
| `lato` | Lato |

### Form

- Each family ships as TTF in two weights, regular and bold, cut down to Latin-1 plus common punctuation and symbols (degree, arrows, bullets), and is rendered at runtime with LVGL's TinyTTF and its glyph cache. Any size works. Expected total: well under 1 MB.
- One bitmap font stays: LVGL's own Montserrat 14 with its symbol glyphs, used for the built-in error page and as the fallback if a TTF cannot be loaded.
- Themes can still bring any `.ttf` from their folder.
- License texts ship with the binary's docs.

### Notation

A font value is `<family> <size> [bold]`:

- `font = roboto 20`, `font_big = montserrat 32 bold`, `font = fonts/Inter.ttf 18`;
- the family is a built-in name or a path to a `.ttf` inside the theme folder;
- the theme tokens `font_small`, `font`, `font_big`, `font_huge` are font values, so they carry size and weight.

## Consequences

- One binary, five families, any size, for a fraction of the size the bitmaps would take.
- TinyTTF renders each glyph once per size, then serves it from the cache; on a Pi 4 that is not noticeable.
- Inter and Poppins, next in popularity, are not built in; a theme can ship them as TTF.
