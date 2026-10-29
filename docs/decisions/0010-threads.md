---
title: "ADR 0010: Plain threads and channels, no async runtime"
description: "Why paneld uses a few plain threads and channels instead of an async runtime."
date: 2026-10-29
tags: [paneld, adr, architecture]
series: paneld
order: 17
---

- Status: Accepted
- Date: 2026-10-29

## Context

Besides the UI, the program waits on a handful of things at once: poll timers, running commands (with timeouts), stream sources reading lines, the `ctl` socket, config file changes. A panel has tens of sources and a few commands running at a time, not thousands. LVGL is single-threaded ([Architecture](../architecture.md)).

## Options

- **Async runtime (tokio, smol):** one model for timers, processes, sockets and timeouts; but a large dependency (tokio) or a smaller one with fewer ready parts (smol), async code through the engine, and no gain at this scale.
- **Plain threads and channels:** a few long-lived threads plus one per running command or stream; standard library only, simple to read and debug; memory per thread is small with a reduced stack size.

## Decision

Plain `std` threads and channels, no async runtime.

| Thread | Does |
| --- | --- |
| UI (main) | Owns every LVGL object; runs the LVGL timer loop; applies changes from the channel between frames |
| Engine | Holds config, values and vars; evaluates bindings; sends property changes to the UI; receives taps and value changes |
| Scheduler | Poll timers, `for` durations of alerts, reminders, idle timer; starts commands when due |
| Command | One per running command: spawns it, reads stdout and stderr, enforces the timeout and output limit, sends the result |
| Stream | One per stream source: reads lines, sends events, restarts with backoff |
| ctl | Accepts socket connections; one short-lived thread per connection |
| Watcher | Config file changes (inotify) |

- Channels: `std::sync::mpsc` (or `crossbeam-channel` if `select` across channels proves needed).
- The UI thread sleeps until the next LVGL timer is due or a message arrives, so an idle panel uses no CPU.
- Command timeouts: the command thread waits with a deadline and kills the process group when it passes.
- Threads that run commands get a small stack (64 KiB).

## Consequences

- No async in the codebase; every module is ordinary blocking Rust.
- Thread count follows running commands and streams, bounded by the config; fine at panel scale.
- If the program ever needed thousands of concurrent tasks this would have to change; nothing in the requirements points there.
