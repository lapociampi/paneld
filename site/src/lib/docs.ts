import { getCollection, type CollectionEntry } from 'astro:content';
import { docId } from './id';

export type Doc = CollectionEntry<'docs'>;

export function docUrl(id: string): string {
  const base = import.meta.env.BASE_URL.replace(/\/$/, '');
  return id === 'index' ? `${base}/` : `${base}/${id}/`;
}

export async function allDocs(): Promise<Doc[]> {
  const docs = await getCollection('docs');
  return docs.sort((a, b) => a.data.order - b.data.order);
}

// The docs link to each other with relative .md paths, which work on
// GitHub. Rewrite them to the page URLs of this site.
export function rewriteLinks(html: string, doc: Doc): string {
  const source = (doc.filePath ?? '').replace(/^.*?docs\//, '');
  const dir = source.includes('/') ? source.slice(0, source.lastIndexOf('/')) : '';
  return html.replace(/href="([^"#:]+\.md)(#[^"]*)?"/g, (_, target: string, hash = '') => {
    const parts = (dir ? `${dir}/${target}` : target).split('/');
    const path: string[] = [];
    for (const part of parts) {
      if (part === '..') path.pop();
      else if (part !== '.') path.push(part);
    }
    return `href="${docUrl(docId(path.join('/')))}${hash}"`;
  });
}

export function formatDate(date: Date): string {
  return date.toISOString().slice(0, 10);
}
