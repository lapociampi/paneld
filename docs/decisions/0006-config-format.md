---
title: "ADR 0006: Hypr-style config format"
description: "Why an own hypr-style config format instead of YAML or Lua."
date: 2026-10-29
updated: 2026-11-05
tags: [paneld, adr]
series: paneld
order: 13
---

- Status: Accepted
- Date: 2026-10-29

## Context

The config holds deep UI trees full of `{{ }}` expressions, plus commands, sources and actions. The first drafts used YAML.

## Options

- **YAML:** known, editor support for free through a JSON Schema. But `{` starts a flow map, so every `{{ }}` needs quotes; indentation errors are easy; long trees get noisy.
- **Lua** (Hyprland's current config): config as code, but arbitrary code needs runaway-script protection and cannot be checked before it runs.
- **Hypr-style, after hyprlang** (Hyprland's classic language): `key = value` lines, `kind name { }` blocks, `$vars`, includes, `category:key` one-liners, last option wins.

## Decision

An own hypr-style format, parsed in Rust ([Config format](../config-format.md)). From hyprlang: `key = value`, braces, `#` comments, `$vars`, includes with globs (`include`, since `source` is one of our items), named blocks, `kind[name]:key = value`, options where the last one wins, reload on save with an error banner, a fallback screen when nothing valid ever loaded. Not taken: Lua, `noerror` switches.

Amended 2026-11-05: inside a value, `#` starts a comment only between whitespace and whitespace (or the end of the line), so a color or `#3` after a space stays text (`border = 2 #ff0000`).

## Consequences

- No quoting around expressions; UI trees nest like QML.
- Defaults files and personal overrides combine naturally.
- The parser is ours (lexer, blocks, assignments, includes) and must give file, line and column.
- No ready-made editor support: `paneld check` first, a highlighting grammar later.
