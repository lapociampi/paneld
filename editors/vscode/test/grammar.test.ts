// The grammar against real configs: every line must close what it
// opens (no rule may leak into the next line), and the common forms
// get their scopes.
import { expect, test } from "bun:test";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { INITIAL, Registry, parseRawGrammar, type IGrammar } from "vscode-textmate";
import { loadWASM, OnigScanner, OnigString } from "vscode-oniguruma";

const here = dirname(new URL(import.meta.url).pathname);
const repo = join(here, "..", "..", "..");

await loadWASM(readFileSync(require.resolve("vscode-oniguruma/release/onig.wasm")).buffer);
const registry = new Registry({
  onigLib: Promise.resolve({
    createOnigScanner: (p: string[]) => new OnigScanner(p),
    createOnigString: (s: string) => new OnigString(s),
  }),
  loadGrammar: async () =>
    parseRawGrammar(readFileSync(join(here, "..", "syntaxes", "paneld.tmLanguage.json"), "utf8"), "paneld.tmLanguage.json"),
});
const grammar = (await registry.loadGrammar("source.paneld")) as IGrammar;

type Tok = { text: string; scopes: string[] };

/** Each line's tokens, and the lines after which a rule stayed open. */
function tokenize(src: string): { lines: Tok[][]; open: number[] } {
  let stack = INITIAL;
  const lines: Tok[][] = [];
  const open: number[] = [];
  src.split("\n").forEach((line, i) => {
    const r = grammar.tokenizeLine(line, stack);
    lines.push(r.tokens.map((t) => ({ text: line.slice(t.startIndex, t.endIndex), scopes: t.scopes })));
    stack = r.ruleStack;
    // A `\` at the end continues the value on the next line.
    if ((stack as any).depth !== 1 && !line.endsWith("\\")) open.push(i + 1);
  });
  return { lines, open };
}

function scopeOf(src: string, text: string): string {
  for (const line of tokenize(src).lines) {
    const t = line.find((t) => t.text.trim() === text);
    if (t) return t.scopes.join(" ");
  }
  throw new Error(`no token \`${text}\` in ${src}`);
}

function confs(dir: string, out: string[] = []): string[] {
  if (!existsSync(dir)) return out;
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    if (["node_modules", "target", "vendor", ".git"].includes(e.name)) continue;
    const p = join(dir, e.name);
    if (e.isDirectory()) confs(p, out);
    else if (e.name.endsWith(".conf")) out.push(p);
  }
  return out;
}

/** The unlabelled fenced blocks of the docs: config examples. */
function docExamples(): [string, string][] {
  const out: [string, string][] = [];
  const docs = join(repo, "docs");
  for (const f of readdirSync(docs).filter((f) => f.endsWith(".md"))) {
    const md = readFileSync(join(docs, f), "utf8");
    for (const m of md.matchAll(/^```\n([\s\S]*?)^```$/gm)) out.push([`${f}: ${m[1].split("\n")[0]}`, m[1]]);
  }
  return out;
}

test("no rule leaks past its line in any config or doc example", () => {
  const files: [string, string][] = [
    ...confs(repo).map((p) => [p, readFileSync(p, "utf8")] as [string, string]),
    // More configs from a folder of your own, when PANELD_CONFIGS names one.
    ...(process.env.PANELD_CONFIGS ? confs(process.env.PANELD_CONFIGS) : []).map((p) => [p, readFileSync(p, "utf8")] as [string, string]),
    ...docExamples(),
  ];
  expect(files.length).toBeGreaterThan(5);
  const leaks = files.flatMap(([name, src]) => tokenize(src).open.map((n) => `${name}:${n}`));
  expect(leaks).toEqual([]);
  // A comment starts at a `#` and runs to the end of its line.
  const stray = files.flatMap(([name, src]) =>
    tokenize(src).lines.flatMap((line, i) => {
      const c = line.findIndex((t) => t.scopes.some((s) => s.startsWith("comment.")));
      return c >= 0 && (!line[c].text.startsWith("#") || c !== line.length - 1) ? [`${name}:${i + 1}`] : [];
    }),
  );
  expect(stray).toEqual([]);
});

test("blocks, keys and values get their scopes", () => {
  const src = [
    "# rack",
    "include = pages/*.conf",
    "$pve = https://10.0.0.2:8006",
    "remote pve: proxmox { host = $pve; token = tok }",
    "theme[dark]:accent = #44aaff",
    "screen:idle = 2m",
    "action stop {",
    "    param vm: string = 100",
    "    param tint: color = #fff",
    "    param n: int # how many",
    "    run = curl -X POST $pve/x/{{ vm }}/stop  # stop it",
    "    run = awk '{print $1}'",
    "    label = C##1",
    "}",
    "page p { repeat item in {{ sources.vms.value }} { tile { on_tap stop { vm = {{ string(item.id) }} } } } }",
  ].join("\n");
  expect(scopeOf(src, "# rack")).toContain("comment.line");
  expect(scopeOf(src, "include")).toContain("keyword.control.import");
  expect(scopeOf(src, "$pve")).toContain("variable.other.constant");
  expect(scopeOf(src, "remote")).toContain("entity.name.tag");
  expect(scopeOf(src, "proxmox")).toContain("entity.name.type");
  expect(scopeOf(src, "dark")).toContain("entity.name.function");
  expect(scopeOf(src, "accent")).toContain("variable.other.property");
  expect(scopeOf(src, "#44aaff")).toContain("constant.other.color");
  expect(scopeOf(src, "2m")).toContain("constant.numeric");
  expect(scopeOf("url = https://h:8006/api", "https://h:8006/api")).not.toContain("constant.numeric");
  expect(scopeOf("size = [5, 30]", "30")).toContain("constant.numeric");
  expect(scopeOf(src, "param")).toContain("storage.type.param");
  expect(scopeOf(src, "string")).toContain("support.type");
  expect(scopeOf(src, "# stop it")).toContain("comment.line");
  expect(scopeOf(src, "#fff")).toContain("constant.other.color");
  expect(scopeOf(src, "# how many")).toContain("comment.line");
  expect(scopeOf(src, "{{")).toContain("punctuation.section.embedded.begin");
  expect(scopeOf(src, "##")).toContain("constant.character.escape");
  expect(scopeOf(src, "repeat")).toContain("keyword.control.repeat");
  expect(scopeOf(src, "sources")).toContain("variable.language.cel");
  expect(scopeOf(src, "vms")).toContain("variable.other.property.cel");
  expect(scopeOf(src, "on_tap")).toContain("entity.name.tag");
  expect(tokenize(src).open).toEqual([]);
});

test("a # in a value is a comment only between spaces", () => {
  expect(scopeOf("border = 2 #ff0000", "#ff0000")).toContain("constant.other.color");
  expect(scopeOf("border = 2 #ff0000", "#ff0000")).not.toContain("comment");
  const item = tokenize("text = Item #3").lines[0];
  expect(item.map((t) => t.text).join("")).toBe("text = Item #3");
  expect(item.some((t) => t.scopes.some((s) => s.startsWith("comment.")))).toBe(false);
  expect(scopeOf("shadow = 8 #00000080 0 4", "#00000080")).toContain("constant.other.color");
  expect(scopeOf("poll = 1m  # note", "# note")).toContain("comment.line");
  expect(scopeOf("poll = 1m #", "#")).toContain("comment.line");
  const one = "text { border = 2 #f00 }";
  expect(scopeOf(one, "#f00")).toContain("constant.other.color");
  expect(tokenize(one).open).toEqual([]);
});

test("cel maps, strings and braces inside values end where they should", () => {
  const src = [
    "value = {{ {'a': {'b': 1}, 'c': '}}'} }}",
    "after = 1",
    'label = "a; b } c"',
    "next = 2",
  ].join("\n");
  expect(scopeOf(src, "after")).toContain("variable.other.property.paneld");
  expect(scopeOf(src, "next")).toContain("variable.other.property.paneld");
  expect(scopeOf(src, "}}")).toContain("string.quoted.single.cel");
});

test("single quotes, continuations and other header forms", () => {
  const src = [
    "run = sh -c 'echo {; x # y }'",
    "a = 1",
    "run = sh -c 'a=1; echo $$a' sh {{ x }}",
    "b = 2",
    "run = curl -s \\",
    "    http://x/y",
    "c = 3",
    "style .card:pressed { border = 2 }",
    "action VmStop { }",
    'param d: string = {"k": 1}',
    "param e: string = x { ask = text }",
  ].join("\n");
  const { lines, open } = tokenize(src);
  expect(open).toEqual([]);
  for (const key of ["a", "b", "c", "border", "ask"]) expect(scopeOf(src, key)).toContain("variable.other.property.paneld");
  expect(scopeOf(src, "echo {; x # y }")).toContain("string.quoted.single.paneld");
  expect(lines[5][0].scopes.join(" ")).toContain("meta.assignment");
  expect(scopeOf(src, ".card:pressed")).toContain("entity.other.attribute-name.selector");
  expect(scopeOf(src, "VmStop")).toContain("entity.name.function");
  expect(lines[9].map((t) => t.scopes.join(" ")).some((s) => s.includes("punctuation.section.block"))).toBe(false);
  expect(lines[10].find((t) => t.text === "{")!.scopes.join(" ")).toContain("punctuation.section.block.begin");
});

test("an unclosed quote ends with its line", () => {
  for (const q of ["'", '"']) {
    const src = `a = x ${q}open\nb = 1`;
    expect(tokenize(src).open).toEqual([]);
    expect(scopeOf(src, "b")).toContain("variable.other.property.paneld");
  }
});

test("an expression can span lines", () => {
  const src = "v = {{ [1,\n  2] }}\nb = 1";
  expect(tokenize(src).open).toEqual([1]);
  expect(scopeOf(src, "2")).toContain("constant.numeric.cel");
  expect(scopeOf(src, "b")).toContain("variable.other.property.paneld");
});
