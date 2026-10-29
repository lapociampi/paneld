---
title: "ADR 0008: Themes: folders with tokens, styles, components and motion"
description: "Themes as folders of tokens, styles, components and motion."
date: 2026-10-29
tags: [paneld, adr]
series: paneld
order: 15
---

- Status: Accepted
- Date: 2026-10-29

## Context

Themes must change much more than colors: background images, fonts, layout, animations, on a device that draws in software.

## Decision

A theme is a folder ([Themes](../themes.md)) with tokens, styles per building block, state and class, replacement components (so a theme changes layout without touching pages), motion settings, and `extends` for variants.

- Fonts: the binary ships a small set of built-in fonts; TTF files are also supported, rendered at runtime with TinyTTF. Changed by [ADR 0015](0015-fonts.md): the built-in fonts are TTF too.
- Motion: `full | reduced | none`, default `reduced`.
- Night: a whole other theme, usually a few lines with `extends`; no separate token-override mechanism.

## Consequences

- Pages stay layout-agnostic: they use components, themes implement them.
- Every theme switch is validated against what the pages use before it is applied.
- Built-in fonts cost binary size: the set stays small ([ADR 0015](0015-fonts.md)).
