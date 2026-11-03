---
title: "Open questions"
description: "Design decisions still to make."
date: 2026-10-29
updated: 2026-11-03
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
- [ ] `ssh host cmd {{ x }}`: ssh hands its command to a remote shell, so a value there is shell code on the far side. `paneld check` does not flag it yet; options are a lint, or a helper that quotes for the remote shell.
- [ ] Reload: a source's background refresh can land after the new poller's first sample and briefly show the older value; `set` and `refresh` of a run started before the reload go to the old config's vars and sources.
- [ ] ctl limits: the idle timeout is per read, so 16 clients sending a byte a minute hold every slot. The service's socket is group-only, so only local admins can do it.
