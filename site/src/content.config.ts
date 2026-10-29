import { defineCollection } from 'astro:content';
import { glob } from 'astro/loaders';
import { z } from 'astro/zod';
import { docId } from './lib/id';

const docs = defineCollection({
  loader: glob({
    base: '../docs',
    // README.md files only exist for GitHub's folder view.
    pattern: ['**/*.md', '!**/README.md'],
    generateId: ({ entry }) => docId(entry),
  }),
  schema: z.object({
    title: z.string(),
    description: z.string(),
    date: z.coerce.date(),
    updated: z.coerce.date().optional(),
    tags: z.array(z.string()).default([]),
    series: z.string().optional(),
    order: z.number(),
  }),
});

export const collections = { docs };
