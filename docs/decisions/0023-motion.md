---
title: "ADR 0023: Motion runs once, from what was shown, and never while asleep"
description: "The theme's motion levels and tokens, how values animate from what was shown when a page is drawn again, and how animations end so the panel sleeps again."
date: 2026-11-05
updated: 2026-11-06
tags: [paneld, adr, themes, rendering]
series: paneld
order: 30
---

- Status: Accepted. Fills in the Motion of [ADR 0008](0008-themes.md) and [Themes](../themes.md#motion), and section 8.3 of [Schema](../schema.md) ("value changes animate").
- Date: 2026-11-05

## Context

The docs promised page transitions, tap feedback, animated values and an alert banner entrance under a theme's `motion = full | reduced | none`; nothing animated. Three things make it harder than an `lv_anim` per widget:

- A page is rebuilt whole on every value change (and on navigation), so a new `bar` has no previous value to move from. (So it was when this was decided. Since [ADR 0024](0024-in-place-updates.md) a value change updates the widgets that show it in place, and a page left is kept; a page is still built afresh the first time it shows, after a reload or a theme switch, and a subtree when its shape changes. The remembered values below serve those, and an update reads them the same way.)
- Idle must stay asleep: no timer may keep running once an animation is done.
- The Pi draws in software; a full-screen effect redraws 800x480 on every frame.

## Decision

| Point | Rule |
| --- | --- |
| Levels | `none`: nothing moves. `reduced` (default, the shipped `light` and `dark`): fades, value changes, tap feedback. `full`: also slides. Each effect has a lowest level (`fade` and values `reduced`, `slide` `full`); below it the change is instant, never swapped for another effect |
| Tokens | `page_transition` (`<fade\|slide\|none> [<duration>]`, default `fade 120ms`) and `banner_transition` (same, default `fade 200ms`), `value_transition` (`300ms`), `tap_transition` (`50ms`); durations at most `2s`, `none` is 0. Built-in tokens, so `theme.banner_transition` always resolves; checked like other tokens |
| Node key | `enter = <transition>` on any node: it comes in when it shows on a page already on screen and was not shown on the last render. The shipped shell's banner has `enter = {{ theme.banner_transition }}`, so a theme picks the banner's entrance without replacing the shell |
| Values across rebuilds | The renderer remembers, for the page on screen only, what each `arc`, `bar` and `chart` showed or is moving to (from, to, start tick, duration), keyed by a hash of the node's place in the config, the component calls and the `repeat` items it is under (an item's `id` or `name`, else its position). A rebuild starts the new widget at the value shown at that moment and moves it to the new one; same target mid-way: the rest of the time. At most 4096 widgets and 65536 chart points; a navigation forgets the old page |
| Entrances | Kept under their own key beside the node's value (a `bar` with `enter` still moves its value). A rebuild during an entrance carries it on from the progress shown, for the time left |
| Reload, theme switch | The next render starts afresh: values at their new value, no entrance, since a remembered place (file and offset) may now be another node; the render after compares against it as usual |
| Charts | Point by point; a series whose points moved left by k (a `history` gaining points) is shifted first, the new points start at the last one, so the line does not wobble |
| Easing | Ease-out cubic, computed in Rust both in the animation and when reading the value shown, so a rebuild mid-way does not jump |
| Page transitions | Only when the page changes (`same_page` false); a value change redraws in place. The new page is drawn in a holder over the old one; `fade` animates its opacity, `slide` both holders' `translate_x`. It starts one lead in (33 ms, at most a quarter of the duration) and runs from there over the whole duration, so the first frame (drawn once the new page is laid out, some 15-25 ms later) already shows the new page partly in, and the transition still takes its duration on the clock. A transparent blocker on top takes taps until the end, then the old page and the blocker are deleted. The frame after the end is drawn at once, not a refresh period later A redraw asked for meanwhile waits for the end, which wakes the loop. If LVGL cannot start the animation, the new page is in at once |
| Tap feedback | Local `transition` styles on buttons and nodes with `on_tap` / `on_hold` for the pressed state and back (scale, opacity, colors, border). The LVGL default theme's own transitions are off (`LV_THEME_DEFAULT_TRANSITION_TIME 0`), so `none` means none. `scale` pivots about the node's middle |
| Sleep | Going to sleep ends every animation at its end value (`finish_motion`) before the cover goes up; the render on waking is instant (no values, entrances or transition move). While asleep nothing is rendered at all |
| One-shot | Every animation is a one-shot `lv_anim` with its data freed in its `deleted_cb`; LVGL pauses its animation timer when the list is empty, so the pump's hint goes back to the idle step |
| Measure | `ctl status` has `animations` (running) and `idle_ms`: what the timers let the last loop step sleep, up to 1000 ms, leaving out the SDL driver's own timers (it reads events every 5 ms), so the daemon test sees the panel's rate on a desktop |

## Consequences

- Measured on x86 ([Themes](../themes.md#motion)): a page fade costs 4.1 ms a frame at 800x480 against 3.5 ms for a full rebuild, a slide 2.5-3.1 ms, 12 moving bars 1.4 ms; about 16 to 30 ms on the Pi's A53, inside a 33 ms frame. CPU returns to the idle level at the end of each animation.
- During a page transition both pages exist: their grid and line buffers stay until the next full clear.
- A tap during a page transition is dropped (up to `page_transition`, 120 ms by default).
- A value change and the page transition cannot overlap: the redraw comes after the page is in.
- Spinners and scrolling text are content and move at every level; the sleep cover stops them as before.
