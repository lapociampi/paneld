---
title: "ADR 0001: One process, no browser"
description: "Why paneld is one native process with no browser."
date: 2026-10-29
tags: [paneld, adr]
series: paneld
order: 8
---

- Status: Accepted
- Date: 2026-10-29

## Context

The program must be as light as possible on a 2GB Pi shared with other services. A split was considered: an engine process plus a renderer (a web page in a kiosk browser) talking over a WebSocket.

## Options

- **Engine + web renderer in a kiosk browser:** fastest UI development, a remote view almost for free. But a browser is always a separate process and costs tens to hundreds of MB (Chromium ~300-400MB, cog/WPE less) just to draw.
- **One native process:** engine and drawing in one binary, no IPC, no browser.

## Decision

One native process. Inside it the engine and the drawing code stay separate modules (the engine produces a resolved tree, the UI module draws it), so another renderer remains possible.

## Consequences

- Smallest RAM and CPU footprint, one binary to ship.
- Crash isolation between engine and UI is lost; systemd restarts the program.
- A remote view is no longer free: not in v1.
