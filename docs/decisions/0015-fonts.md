---
title: "ADR 0015: Built-in fonts"
description: "Which font families ship inside the binary, in what form, and how a config names a font."
date: 2026-10-29
updated: 2026-11-05
tags: [paneld, adr, themes]
series: paneld
order: 22
---

- Status: Implemented (see [Implementation](#implementation) for where it differs). Changes the font part of [ADR 0008](0008-themes.md).
- Date: 2026-10-29

## Context

[ADR 0008](0008-themes.md) planned a small set of prebuilt bitmap fonts in the binary, plus TTF files from themes. A choice of families is wanted, but bitmaps are one file per family, size and weight: five families in four sizes and two weights would be 40 fonts and several MB, and a theme could not pick a size that was not prebuilt. The font notation also differed between [Schema](../schema.md) and [Themes](../themes.md).

## Decision

### Families

Five of the most used families on Google Fonts, all under the SIL Open Font License with no Reserved Font Name (see [Implementation](#implementation) for why that matters):

| Name in config | Family |
| --- | --- |
| `roboto` (also `sans`, the default) | Roboto |
| `open_sans` | Open Sans |
| `noto_sans` | Noto Sans |
| `montserrat` | Montserrat |
| `inter` | Inter |

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
- TinyTTF renders each glyph once per size, then serves it from the cache; on a Pi 3B+ that is not noticeable.
- Lato and Poppins are not built in; a theme can ship them as TTF.

## Implementation

- The ten files are 191 KB (`crates/paneld-ui/fonts/`, with the OFL texts and how they were cut), embedded with `include_bytes!`. The x86-64 release binary grew by 282 KB.
- A font is created on first use per (family, weight, size; a file by its real path) and kept until the next config load: when a config loads (or reloads), the old screen is deleted and every font freed, so an edited `.ttf` is read again then. At most 32 are live at once; past that, text draws in bitmap Montserrat, logged once.
- Each font caches the bitmaps of about 256 KB of glyphs at its size: 128 glyphs up to 45 px, down to 6 at 200 px (at least 4).
- The default theme's fonts are `roboto` 12, 16, 24 and 32. The theme's `font` is the text font of every node that sets none.

Where it differs from the decision above:

- Inter replaces Lato. Lato's license reserves the name "Lato", and a subset is a Modified Version, which may not use a reserved name. Inter reserves none. The files are cut with `--update-name-table`, so their names match the instance (Regular, Bold).
- Bitmap Montserrat stays in six sizes (12, 14, 16, 20, 24, 32), not only 14: built-in icons draw from it, and it is the last fallback of every TTF font, so LVGL's symbols still draw inside TTF text. The error page and anything drawn before a config loads use Montserrat 16 (LVGL's default font).
- Arrows are only in Inter and Montserrat. Roboto, Open Sans and Noto Sans fall back to Montserrat TTF of the same size and weight, then to bitmap Montserrat.
- No kerning: TinyTTF reads only the old `kern` table, and the subset files have none.
- A `.ttf` path is relative to the config folder until theme folders are loaded. With symlinks resolved it must stay inside that folder (stb_truetype trusts the file it reads), and it must be at most 4 MiB (read with a limit, not trusting the size it reports). `bold` does not apply to a file.
- Sizes are 6 to 200 px. A node's `font` may name a theme token bare (`font = font_big`).
- Checks: an unknown family (with a "did you mean"), a bad size or weight, or a missing file is a check error with file and line. At render, a value from an expression that is not a font, or a file that cannot be read, draws in bitmap Montserrat at its size and is logged once.
