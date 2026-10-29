---
title: "ADR 0005: Commands are the only integration"
description: "Why running a command is the only way paneld talks to the outside world."
date: 2026-10-29
tags: [paneld, adr]
series: paneld
order: 12
---

- Status: Accepted
- Date: 2026-10-29

## Context

The program must be agnostic: no knowledge of any service behind it.

## Decision

The only way the program reaches the outside world is running a command and reading its exit code and output. No built-in HTTP client, no plugins, no service integrations. HTTP is `curl` in the config; anything privileged goes through sudo rules or ssh forced commands set up outside the program.

## Consequences

- Any service with a CLI or an HTTP API works, with no program change.
- Inputs from the UI are typed and passed as argv items or env vars, never pasted into a shell string.
- A process per poll costs a little CPU; poll intervals are set in the config, and streams cover push sources.
