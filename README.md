# paneld

A generic touch panel: "Grafana for actions". The program draws widgets, runs commands and keeps a little state. Everything specific lives in config files (a hypr-style format): the pages and how they look, where every value comes from, what every button does, what counts as an alert. It knows no service: every integration is a command, mostly `curl`.

## Status

Design done. No code yet. Next step: the first code.

## Design in short

One native Rust + LVGL process, a hypr-style config with CEL expressions, and commands as the only integration. The overview is in [`docs/index.md`](docs/index.md).

## Docs

The design docs live in [`docs/`](docs/index.md), starting from [`docs/index.md`](docs/index.md). They are written to be published as is on a blog or docs site:

- every page starts with YAML front matter: `title`, `description`, `date`, `tags`, `series: paneld` and `order` (reading order);
- the page title is in the front matter only, so the body has no `#` heading;
- pages link to each other with relative `.md` links, which work on GitHub and in most site generators;
- `docs/decisions/` holds the design decisions (ADRs), with `index.md` as their list; its `README.md` only points there for GitHub's folder view and is not published.

`site/` is an Astro site that builds `docs/` into a static blog (pages, sidebar, previous/next by `order`, light and dark):

```
cd site
bun install
bun run dev      # http://localhost:4321
bun run build    # static output in site/dist/
```

For deployment set `SITE` (full URL) and, if it is served under a path, `BASE` (e.g. `/paneld`).

`@astrojs/markdown-satteri` is pinned to the version `astro` itself depends on (it is only there to turn off smart punctuation); upgrade the two together.

## First target

The touchscreen of a home server rack: Raspberry Pi 4 (arm64, 2GB RAM shared with other services), Waveshare 5" DSI touchscreen, 800x480, Raspberry Pi OS Lite with no desktop.
