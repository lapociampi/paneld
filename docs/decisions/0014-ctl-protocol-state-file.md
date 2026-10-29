---
title: "ADR 0014: JSON lines for ctl, one JSON file for state"
description: "The wire format of the ctl socket and the format of the local state file."
date: 2026-10-29
tags: [paneld, adr, ctl]
series: paneld
order: 21
---

- Status: Accepted
- Date: 2026-10-29

## Context

`paneld ctl` talks to the running program over a Unix socket ([ADR 0007](0007-ctl.md)), and local scripts may talk to it directly. The program also keeps a small state file: acks, snoozes, reminder dates and alert history ([Requirements](../requirements.md), section 10). Both formats were open. The device runs from an SD card, so writes should be rare.

## Decision

### ctl protocol

- One JSON object per line, UTF-8, both ways. A connection may send several requests; each gets one response with the same `id`.
- Request: `{"id": 1, "cmd": "eval", "args": {"expr": "sources.cpu_temp.value"}}`.
- Response: `{"id": 1, "ok": true, "result": 48.2}` or `{"id": 1, "ok": false, "error": {"message": "...", "file": "...", "line": 3, "column": 5}}`.
- The first message from the server is `{"paneld": "<version>", "protocol": 1}`; an incompatible client stops there.
- `paneld ctl` prints results for people by default and the raw JSON with `--json`.
- Socket `app:socket` (default `/run/paneld/ctl.sock`), mode 0660, owned by the service user and group.

### State file

- One JSON file at `app:state` (default `/var/lib/paneld/state`), with a `version` field: `{"version": 1, "alerts": {...}, "history": [...], "reminders": {...}}`.
- Written only when something changed, at most every 10s, and on shutdown: write a temp file, fsync, rename over the old one.
- History is pruned to `alerts:history` on every write.
- A file that cannot be read is renamed to `state.bad`, the program starts with empty state and raises a warning alert.
- Vars are not saved; they reset on restart ([Schema](../schema.md), section 3).

## Consequences

- Scripts can drive the panel with `socat` or any JSON library; the CLI is a thin client of the same protocol.
- The state file is readable and easy to back up or fix by hand.
- At most one SD card write every 10s, and usually far fewer.
