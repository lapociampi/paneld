// Entry id from its path inside docs/: "schema.md" -> "schema",
// "decisions/index.md" -> "decisions", "index.md" -> "index".
export function docId(path: string): string {
  const id = path.replace(/\.md$/, '').replace(/(^|\/)index$/, '');
  return id || 'index';
}
