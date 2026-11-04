---
title: "ADR 0019: regex-lite for param patterns"
description: "How a param's pattern is matched, and why with regex-lite."
date: 2026-11-04
tags: [paneld, adr, config]
series: paneld
order: 26
---

- Status: Accepted
- Date: 2026-11-04

## Context

Schema 6 has given params a `pattern` (a regex for strings) from the start, but nothing enforced it. A free-text param that reaches a command needs it most: `systemctl restart -- {{ unit }}` is safe as argv, but the command still decides what a value like `--force` means.

## Decision

- `pattern` is matched with [regex-lite](https://crates.io/crates/regex-lite), anchored at both ends: the whole text must match.
- A value that does not match is refused before anything runs, like one outside `min` / `max` or `options`.
- `paneld check` refuses a pattern that does not compile, and a literal default that does not match it.
- Patterns are limited to 64 KiB compiled.

## Consequences

- One dependency, no transitive ones: about 60 KB in the Pi binary (2.44 to 2.50 MB). The full `regex` crate would be several times that for speed a panel does not need: patterns run on one short string per tap.
- regex-lite has no Unicode classes (`\p{L}`); `[a-z]` style classes and the usual operators work.
