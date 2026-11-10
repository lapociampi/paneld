---
title: "ADR 0025: The screen sets its frame rate"
description: "Why the refresh and animation period became a screen setting, by day and at night, what follows it and what does not, and what it costs on a Pi."
date: 2026-11-10
tags: [paneld, adr, rendering, screen]
series: paneld
order: 32
---

- Status: Accepted. Adds `fps` to `screen { }` and `night { }` ([Schema](../schema.md) 12); the page transition lead of [ADR 0023](0023-motion.md) follows it.
- Date: 2026-11-10

## Context

- The frame period was fixed at build time: `LV_DEF_REFR_PERIOD` 33 ms (30 fps) in `lv_conf.h`, for every display and for LVGL's animation timer.
- The Pi 3B+ draws in software: a full-screen fade frame takes about 16 to 30 ms there ([ADR 0023](0023-motion.md), scaled from x86), half or more of a 33 ms frame. A faster device could draw at 60; a weak one, or a panel nobody looks at by night, needs fewer frames.
- Idle draws nothing: the refresh timer pauses while nothing changes ([ADR 0023](0023-motion.md)), so the rate only matters while something moves or changes.

## Decision

| Point | Rule |
| --- | --- |
| Key | `screen { fps = N }`, 1..60, default 30; a frame every `1000 / fps` ms, rounded. `paneld check` refuses anything else |
| Night | `screen { night { fps = N } }` replaces it during the night hours; without it the night keeps the day's |
| When | At start, on reload (only when it changed) and when the night hours start or end; asleep too, so the wake draws at the right rate |
| Follows the rate | Every display's refresh timer, LVGL's animation timer (so animations step at the rate), and the page transition lead (one refresh period, at most a quarter of the transition) |
| Does not | Touch: on the device (evdev) it is read when the device has data (LVGL's event mode), so input is noticed at any rate; the SDL mouse on a desktop keeps LVGL's own read timer, a fixed 33 ms at any `fps`. Paused timers stay paused: `set_fps` only sets the periods, so a screen with nothing changing wakes no more often at 60 than at 1 |
| Measure | `ctl status`: `fps`, `refresh_ms`, `anim_ms` (the timers' periods as LVGL has them) |

## Consequences

- CPU while something animates grows with `fps`. Idle stays as it was by construction (the refresh and animation timers pause while nothing changes, and `set_fps` only sets their periods); the daemon's idle was not measured at each rate. A page that shows a spinner or scrolling text is never idle: it draws at the rate set.
- What a touch or a change shows can wait up to one frame period: at `fps = 1` the first frame came about 1 s after a tap that followed a redraw.
- LVGL's own animations (a spinner, a scrolling `text`, the cursor blink) step at the rate: the same speed, in fewer and coarser steps at a low rate.
- `motion` removes animations instead of spacing their frames: `reduced` (the default) drops only slides, `none` also drops fades, value animations, entrances and tap feedback. Spinners and scrolling text are content and still step at `fps` at every level. Where those are rare, `motion = none` is the bigger lever on weak devices ([Themes](../themes.md#motion)).

Measured on the Pi 3B+ (one A53 core, static aarch64 build, `paneld shot` on the memory display beside the running service): 45 s of animation on LVGL's clock. CPU is user + sys of the whole run (start included); the % is that over the 45 s of LVGL's clock, i.e. per second of animation (the wall time was longer, 47-72 s, since `shot` sleeps 20 ms per step besides drawing). `shot` steps LVGL's clock 20 ms at a time, so a timer fires on the next step: frames come every 100, 60, 40 and 20 ms (10, 17, 25, 50 a second); the daemon draws at the rate set. "Frames drawn" is the count of `PANELD_PERF` frame lines, the same on both pages. "Frame time" is the mean of those lines' `total` (drawing and flush only), so it is below CPU s / frames (26.0, 20.9, 16.8, 12.4 ms), which also holds `shot`'s loop and start.

| `fps` | Frames drawn (a second) | Spinner + scrolling text: CPU s, % of a core | 4 spinners + 48 px scrolling text on a radial gradient: CPU s, % | Frame time, heavy page (mean `total`, ms) |
| --- | --- | --- | --- | --- |
| 10 | 454 (10) | 1.53, 3.4% | 11.8, 26% | 23.8 |
| 20 | 756 (17) | 1.93, 4.3% | 15.8, 35% | 19.2 |
| 30 | 1134 (25) | 2.63, 5.8% | 19.1, 42% | 15.6 |
| 60 | 2265 (50) | 3.75, 8.3% | 28.1, 62% | 11.5 |

- Static page, 45 s: 0.37 s at `fps = 10`, 0.35 s at 60, 3 frames each (the first draw): equal. That is `shot`'s own 20 ms stepping and start; the daemon idles at about 0.2%.
- Twice the frames (50 against 25 a second) cost about 1.5 times the CPU: each frame redraws less, since less has moved since the last. The daemon at a true 60 against 30 was not measured.
- `fps = 10` against `fps = 30` (25 frames a second under `shot`) used about 40% less CPU on the heavy page.

## Alternatives considered

| Option | Why not |
| --- | --- |
| Compile time only (`LV_DEF_REFR_PERIOD`) | One rate for every device and every hour; changing it means a rebuild |
| A theme token | The rate is about the device and the hour, not the look; `screen` already has the night hours, and a theme switch would change it as a side effect |
| Tie touch reads to the rate | A low rate would then also delay or lose input (short taps between reads); reading on data keeps input prompt and costs nothing while idle |
