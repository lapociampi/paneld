---
title: "ADR 0009: Hand-written config parser"
description: "Why the config parser is a hand-written lexer and recursive-descent parser instead of a parser crate."
date: 2026-10-29
tags: [paneld, adr, config]
series: paneld
order: 16
---

- Status: Accepted
- Date: 2026-10-29

## Context

The config format ([Config format](../config-format.md), [ADR 0006](0006-config-format.md)) is small: `key = value` lines, `kind name { }` blocks, `$vars`, `include`, `;` and `\` continuation, `{{ }}` expressions. But it has a few context rules a grammar tool handles badly:

- a value is raw text to the end of the line, `;` or `}`, except inside quotes and `{{ }}`;
- `$vars` are replaced before parsing, yet errors must still point at the original file, line and column;
- `{{ }}` contents are CEL, parsed by the CEL crate ([ADR 0004](0004-cel.md)), whose error positions must be shifted to the file position;
- every error carries file, line and column, and one run of `paneld check` should report as many errors as it can, not only the first.

## Options

- **Parser crates (winnow, chumsky, pest):** less code to start, but the line-oriented value rules and the `$var` span mapping fight their model; good multi-error recovery is still work; chumsky adds compile time and deep generic types.
- **Hand-written lexer and recursive-descent parser:** a few hundred lines for a grammar this size, full control over spans, messages and recovery, no dependency.

## Decision

A hand-written lexer and recursive-descent parser in the `config` module, with no parser dependency.

- Every token and node carries a span: file id and byte range. Line and column are computed only when an error is printed.
- `$var` substitution keeps a map from substituted text back to the original spans, so errors inside a variable's value point at its use and its definition.
- Recovery: on an error the parser skips to the end of the line or the closing `}` of the current block and continues, so `paneld check` lists every error it finds.
- CEL expressions are handed to the CEL crate with their span; its error offsets are added to the span start.
- Parsing (syntax) and the schema check (which keys exist, their types) are separate steps; the parser knows no item kinds.

## Consequences

- No dependency and fast builds; error messages are ours to word.
- The lexer can be reused later for a syntax-highlighting grammar or a language server.
- The parser needs its own tests: a table of small inputs with the expected tree or the expected error and position.
