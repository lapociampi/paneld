---
title: "Open questions"
description: "Design decisions still to make."
date: 2026-10-29
updated: 2026-11-04
tags: [paneld, planning]
series: paneld
order: 6
---

- [x] Config schema: [Schema](schema.md) v1.
- [x] Built-in fonts: five families as TTF ([ADR 0015](decisions/0015-fonts.md)).
- [x] LVGL bindings: own thin bindings ([ADR 0011](decisions/0011-lvgl-bindings.md)).
- [x] Display and touch: DRM/KMS with fbdev fallback, evdev ([ADR 0012](decisions/0012-display-input.md)).
- [x] Concurrency: plain threads and channels ([ADR 0010](decisions/0010-threads.md)).
- [x] Config parser: hand-written ([ADR 0009](decisions/0009-config-parser.md)).
- [ ] Syntax-highlighting grammar for editors (tree-sitter or TextMate).
- [x] PIN levels: one in v1. Actions without `pin` are for everyone, `pin = true` for the admin; a later `pin = <level>` can add levels without breaking configs.
- [ ] Remote view from a phone or desktop: not in v1; how, if ever.
- [ ] Docker image: deferred. Commands run inside the container, so host actions would need ssh to the host or a privileged container.
- [x] Name of the program: paneld.
- [x] `ssh host cmd {{ x }}` written by hand: `paneld check` rejects a value in the remote command or before the host unless it is typed or held to `options` / `pattern` ([Config format](config-format.md)).
- [ ] Remotes, later: alerts declared by a kind, a `via` override per instance, ssh connection reuse (ControlMaster), the HTTP status in `result` ([ADR 0018](decisions/0018-remotes.md)).
- [x] `active = visible` on a source: applied ([Schema](schema.md), section 4).
- [x] A `secret` with a relative `file` is read from the main config file's folder.
- [x] Reload: a value from a run that started before the one shown is dropped, and vars are the same map across a reload, so a run started before it sets them where the screen reads. (A `refresh` from such a run is lost; the next poll brings it.)
- [x] ctl limits: a request line must arrive whole within 60s of the last reply, so a client sending a byte now and then cannot hold a slot.
- [x] An idle panel sleeps: touch is read in LVGL's event mode when the device has data (the main loop waits in `poll` on it), the display refresh pauses itself when nothing changed, and news for the main loop (a source value, a ctl request, an action's result) wakes it through a pipe; with nothing at all it wakes every 200 ms. The SDL window on a PC still wakes every 5 ms (SDL's own event timer).
- [x] A CEL macro (`l.map(x, ...)`) copies only the names its body reads, not the whole context, so nested macros beside big source histories stay cheap.
