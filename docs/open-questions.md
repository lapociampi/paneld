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
- [ ] `ssh host cmd {{ x }}` written by hand: ssh hands its command to a remote shell, so a value there is shell code on the far side. A `remote_kind` with `via = ssh` quotes it ([ADR 0018](decisions/0018-remotes.md)); `paneld check` does not flag the hand-written form yet.
- [ ] Remotes, later: alerts declared by a kind, a `via` override per instance, ssh connection reuse (ControlMaster), the HTTP status in `result` ([ADR 0018](decisions/0018-remotes.md)).
- [x] `active = visible` on a source: applied ([Schema](schema.md), section 4).
- [x] A `secret` with a relative `file` is read from the main config file's folder.
- [x] Reload: a value from a run that started before the one shown is dropped, and vars are the same map across a reload, so a run started before it sets them where the screen reads. (A `refresh` from such a run is lost; the next poll brings it.)
- [x] ctl limits: a request line must arrive whole within 60s of the last reply, so a client sending a byte now and then cannot hold a slot.
- [ ] The UI thread wakes on LVGL's own timers even when nothing changes: every 5 ms with the SDL driver, about every 33 ms (input read, display refresh) on the device. Truly idle would need LVGL's event-driven input mode and pausing the refresh timer while asleep.
- [ ] A CEL macro (`l.map(x, ...)`) copies the expression context once per call, and a nested one once per outer item: fine for page-sized lists, costly for long ones with big source histories.
