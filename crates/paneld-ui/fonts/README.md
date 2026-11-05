# Built-in fonts

The five families of [ADR 0015](../../../docs/decisions/0015-fonts.md), embedded in the binary with `include_bytes!` (`src/ui.rs`) and drawn by LVGL's TinyTTF.

| File | Family | License |
| --- | --- | --- |
| `roboto-{regular,bold}.ttf` | Roboto | `OFL-roboto.txt` |
| `opensans-{regular,bold}.ttf` | Open Sans | `OFL-opensans.txt` |
| `notosans-{regular,bold}.ttf` | Noto Sans | `OFL-notosans.txt` |
| `montserrat-{regular,bold}.ttf` | Montserrat | `OFL-montserrat.txt` |
| `inter-{regular,bold}.ttf` | Inter | `OFL-inter.txt` |

All under the SIL Open Font License 1.1, none with a Reserved Font Name (a subset is a Modified Version, which may not carry a reserved name; that is why Lato is not here). Source: github.com/google/fonts (`ofl/<family>/`).

How they were cut (191 KB for all ten), from the variable font of each family:

1. `fonttools varLib.instancer` at `wght=400` and `wght=700`; `wdth=100` where the font has that axis, `opsz=14` for Inter; `--update-name-table`, so the names say Regular and Bold.
2. `pyftsubset` to Latin-1 plus common punctuation and symbols (U+0020-007E, U+00A0-00FF, general punctuation such as dashes, quotes, bullet and ellipsis, degree, arrows), with `--no-hinting` and the layout features (GSUB, GPOS) dropped.

TinyTTF reads no GPOS, and these files have no old `kern` table, so text is drawn without kerning. Arrows are only in Inter and Montserrat (the other sources have none): Roboto, Open Sans and Noto Sans fall back to Montserrat of the same size and weight for them, then to the bitmap Montserrat, which also holds LVGL's symbol icons.
