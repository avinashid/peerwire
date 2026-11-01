// Markdown for the docs checks (docs_samples.mjs, docs_links.mjs, NB-070): the repo's pages, their code blocks, their
// text with the code blanked out, and their anchors as GitHub makes them. CommonMark's rules, as far as the docs use
// them.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// The checkout to check: the first argument (another worktree), else this one.
export const root = path.resolve(process.argv[2] ?? fileURLToPath(new URL('../../..', import.meta.url)));

// Its files as git will commit them: tracked, or new and not ignored (a page written but not yet added), and present.
// Paths are relative to the root, with '/'.
export const files = [...new Set(execFileSync('git', ['-C', root, 'ls-files', '-z', '--cached', '--others',
  '--exclude-standard'], { encoding: 'utf8', maxBuffer: 64 << 20 }).split('\0'))]
  .filter((f) => f && fs.existsSync(path.join(root, f)));
export const pages = files.filter((f) => f.endsWith('.md'));
export const read = (f) => fs.readFileSync(path.join(root, f), 'utf8');

// Fenced code blocks: { info, lang, attrs, line, end, body }. `lang` is the info string's first word, lower-cased
// ('' for a bare fence); `attrs` the rest, split on commas and spaces as rustdoc splits them (```rust,no_run). `line`
// is the opening fence's line and `end` the closing one's (1-based); `body` has the fence's indent taken off.
export function blocks(text) {
  const lines = text.split('\n');
  const out = [];
  for (let i = 0; i < lines.length; i++) {
    const m = /^([ \t]*)(`{3,}|~{3,})(.*)$/.exec(lines[i]);
    if (!m || (m[2][0] === '`' && m[3].includes('`'))) continue;
    const [, indent, fence, rest] = m;
    const close = new RegExp(`^[ \\t]*${fence[0]}{${fence.length},}[ \\t]*$`);
    let j = i + 1;
    while (j < lines.length && !close.test(lines[j])) j++;
    const body = lines.slice(i + 1, j).map((l) => l.replace(new RegExp(`^[ \\t]{0,${indent.length}}`), ''));
    const info = rest.trim();
    const [lang = '', ...attrs] = info.split(/[\s,]+/).filter(Boolean);
    out.push({ info, lang: lang.toLowerCase(), attrs, line: i + 1, end: j + 1, body: body.join('\n') });
    i = j;
  }
  return out;
}

// The text with its code blocks and HTML comments blanked, every line kept: what a heading or an id may be in.
export function withoutBlocks(text) {
  const lines = text.split('\n');
  for (const b of blocks(text)) lines.fill('', b.line - 1, Math.min(b.end, lines.length));
  return lines.join('\n').replace(/<!--[\s\S]*?-->/g, blank);
}

// Prose: also without inline code (a span ends at the same number of backticks, within its paragraph), so what is
// left of a link's brackets is a link.
export function prose(text) {
  return withoutBlocks(text).replace(/(?<!`)(`+)(?!`)((?:(?!\n[ \t]*\n)[\s\S])*?)(?<!`)\1(?!`)/g, blank);
}

const blank = (s) => s.replace(/[^\n]/g, ' ');

// 1-based line of an offset.
export const lineAt = (text, i) => text.slice(0, i).split('\n').length;

// A heading's anchor as GitHub makes it (github-slugger): the heading's text without its Markdown, lower-cased,
// everything but letters, marks, numbers, '_', '-' and spaces dropped, each space a '-'.
export function slug(heading) {
  const code = [];
  const text = heading
    .replace(/(`+)([\s\S]*?)\1/g, (_, __, c) => `\0${code.push(c.trim()) - 1}\0`)
    .replace(/!\[[^\]]*\]\([^)]*\)/g, '')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]*)\]\[[^\]]*\]/g, '$1')
    .replace(/<[^>]*>/g, '')
    .replace(/(^|[^\w])(\*\*|__|\*|_)(?=\S)([\s\S]*?\S)\2(?![\w])/g, '$1$3')
    .replace(/&(amp|lt|gt|quot|#39|nbsp);/g, (_, e) => ({ amp: '&', lt: '<', gt: '>', quot: '"', '#39': "'", nbsp: ' ' })[e])
    .replace(/\0(\d+)\0/g, (_, n) => code[n])
    .trim();
  return text.toLowerCase().replace(/[^\p{L}\p{M}\p{N}\p{Pc} -]/gu, '').replace(/ /g, '-');
}

// Every anchor a page has: its headings' slugs (a repeat gets -1, -2 … as on GitHub), and the id and name attributes
// of its HTML. Headings in code blocks don't count.
export function anchors(text) {
  const lines = withoutBlocks(text).split('\n');
  const seen = new Map();
  const out = new Set();
  const add = (heading) => {
    const base = slug(heading);
    let s = base;
    while (seen.has(s)) {
      seen.set(base, seen.get(base) + 1);
      s = `${base}-${seen.get(base)}`;
    }
    seen.set(s, 0);
    out.add(s);
  };
  // A line of a paragraph: not blank, a heading, a list item, a quote, a table row, HTML or a thematic break.
  const other = /^ {0,3}(#{1,6}([ \t]|$)|[-*+][ \t]|\d+[.)][ \t]|>|\||<|(-{3,}|\*{3,}|_{3,}|={3,})[ \t]*$)/;
  const paragraph = (l) => l.trim() !== '' && !other.test(l);
  for (let i = 0; i < lines.length; i++) {
    const atx = /^ {0,3}#{1,6}(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$/.exec(lines[i]);
    if (atx) {
      add(atx[1] ?? '');
      continue;
    }
    // A setext heading: a paragraph, then === or ---.
    if (paragraph(lines[i]) && i + 1 < lines.length && /^ {0,3}(=+|-+)[ \t]*$/.test(lines[i + 1])) {
      let start = i;
      while (start > 0 && paragraph(lines[start - 1])) start--;
      add(lines.slice(start, i + 1).map((l) => l.trim()).join(' '));
      i++;
    }
  }
  for (const m of prose(text).matchAll(/<[a-z][^>]*?\s(?:id|name)\s*=\s*(?:"([^"]*)"|'([^']*)')/gi)) out.add(m[1] ?? m[2]);
  return out;
}
