---
title: "ADR 0013: Command runner limits, environment and retries"
description: "How paneld runs commands: working directory, environment, output limits, concurrency, timeouts and retries."
date: 2026-10-29
tags: [paneld, adr, commands]
series: paneld
order: 20
---

- Status: Accepted
- Date: 2026-10-29

## Context

Every integration is a command ([ADR 0005](0005-commands-only.md)). The requirements left open the working directory, the environment, an output size limit, concurrency between actions, retries for failing polls, and what alerts do when their stream source is down.

## Decision

### Running a command

| Topic | Rule |
| --- | --- |
| Working directory | The directory of the main config file, so `./scripts/x.sh` works; `cwd` on a source or action overrides it |
| stdin | `/dev/null` |
| Environment | A clean one: `PATH=/usr/local/bin:/usr/bin:/bin`, `HOME`, `LANG=C.UTF-8`, `PANELD=1`, plus `PANELD_SOURCE` or `PANELD_ACTION` with the item's name, plus the item's `env { }`. Nothing else is inherited from the service, so secrets in its environment reach a command only through `secret` items |
| Process | Its own process group (`setsid`), so a timeout kills the command and everything it started |
| Timeout | `timeout`, default `app:timeout` (10s): SIGTERM to the group, SIGKILL 2s later; counts as a failure (`error` = `timeout`) |
| Output | stdout up to `app:max_output` (default 1 MiB), stderr up to 64 KiB; more fails the run (`error` = `output too large`). Stream sources: each line up to 64 KiB, longer lines are dropped with a warning |
| Encoding | Output read as UTF-8, invalid bytes replaced |

### Concurrency

- An action runs once at a time; calls while it runs are ignored (unchanged).
- Different actions and sources run in parallel, at most `app:max_running` commands at once (default 8); more wait in a queue, actions before sources.
- A poll is skipped if its previous run is still going.

### Retries

- **Poll source failing:** the next run waits the poll interval, doubled after each further failure, up to `max(interval, 5m)`; a success resets it. Opening a page that shows the source runs it at once, at most every 5s.
- **Stream source exiting:** restart after 1s, doubling up to 60s; reset once it has run for 60s.
- **Stream behind an alert down:** if a source used by an `alert ... from` has failed or been down for longer than `alerts:source_down` (default 2m), a built-in warning alert `source_down:<name>` fires ("Alerts from ntfy may be missing"); it resolves when the source works again. Alerts already raised from that source keep their state.

## Consequences

- Commands behave the same under systemd and in a terminal, since the environment is built, not inherited.
- New schema keys: `cwd` on sources and actions; `app:max_output`, `app:max_running`; `alerts:source_down`.
- A broken source cannot flood the CPU with retries, and a silent alert stream is itself an alert.
