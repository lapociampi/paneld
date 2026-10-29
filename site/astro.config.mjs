// @ts-check
import { defineConfig } from 'astro/config';
import { satteri } from '@astrojs/markdown-satteri';

// The site builds the Markdown pages in ../docs into a static blog.
// Set SITE (full URL) and BASE (path prefix, e.g. /paneld) when deploying.
export default defineConfig({
  site: process.env.SITE,
  base: process.env.BASE ?? '/',
  trailingSlash: 'always',
  markdown: {
    // Keep the text as written: no curly quotes or typographic dashes.
    processor: satteri({ features: { smartPunctuation: false } }),
    shikiConfig: {
      themes: { light: 'github-light', dark: 'github-dark' },
    },
  },
});
