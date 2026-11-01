// Every relative link in the Markdown resolves, as on GitHub (NB-070):
//  - links, images, reference definitions and HTML href/src, outside code, to a file or directory git has (a file on
//    disk that git ignores is a 404 on GitHub), inside the repo; a [text][label] has its definition;
//  - an #anchor is a heading of the page it points at (GitHub's slugs: Markdown dropped, lower case, a repeat gets
//    -1, -2 …; headings in code blocks don't count) or an id or name in its HTML; #L12 or #L12-L20 is a line the
//    file has. A directory's anchors are its README's.
// Links with a scheme (https:, mailto:) aren't fetched. The slugger is checked first, on headings with known slugs.
// node packages/js/test/docs_links.mjs [checkout]

import { anchors, files, lineAt, pages, prose, read, root, slug } from './markdown.mjs';
import fs from 'node:fs';
import path from 'node:path';

const P = path.posix;
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

// 1. The slugger, on github-slugger's rules. The first is a heading in docs/project/threat-model.md and the slug its
// links use.
const same = (md, want) => [...anchors(md)].join(' ') === want;
check(slug('`@peerwire/react` hooks (NB-049) — RH-1…RH-6, NB53-1') === 'peerwirereact-hooks-nb-049--rh-1rh-6-nb53-1',
  'slug: code, punctuation and dashes dropped, each space a -');
check(slug('**Bold**, _em_ and [a link](x.md) & `snake_case` __init__') === 'bold-em-and-a-link--snake_case-init',
  'slug: emphasis, links and entities dropped, _ kept in code and words');
check(slug('Ünïcode café 日本 🚀') === 'ünïcode-café-日本-', 'slug: letters in any script kept, emoji dropped');
check(same('# A\n## A\nA\n---\n### A-1 ###\n', 'a a-1 a-2 a-1-1'), 'anchors: repeats get -1, -2, even past an "A-1"');
check(same('```sh\n# not a heading\n```\n~~~\n# nor this\n~~~\n# Real\n', 'real'), 'anchors: none from code blocks');
check(same('Two line\nheading\n===\n\n<a id="x"></a> <span name=\'y\'>\n', 'two-line-heading x y'),
  'anchors: setext headings, id and name attributes');
check(same('- item\n---\n\n| a |\n|---|\n', ''), 'anchors: --- under a list item or a table is no heading');

// 2. The links.
const fileSet = new Set(files);
const dirs = new Set(['']);
for (const f of files) for (let d = P.dirname(f); d !== '.' && !dirs.has(d); d = P.dirname(d)) dirs.add(d);
const anchorsOf = new Map();
const pageAnchors = (f) => anchorsOf.get(f) ?? anchorsOf.set(f, anchors(read(f))).get(f);
const problems = [];
let checked = 0;
const problem = (page, text, at, what) => problems.push({ page, line: lineAt(text, at), what });

for (const page of pages) {
  const text = prose(read(page));
  const links = [];
  // Inline links and images: [text](dest "title"), dest maybe in <>, with balanced parentheses.
  const inline = /\]\([ \t]*(<[^>\n]*>|(?:[^\s()]|\((?:[^\s()])*\))*)(?:[ \t]+(?:"[^"]*"|'[^']*'|\([^)]*\)))?[ \t]*\)/g;
  for (const m of text.matchAll(inline)) {
    links.push([m.index, m[1].replace(/^<|>$/g, '')]);
  }
  // Reference definitions: [label]: dest.
  const labels = new Set();
  for (const m of text.matchAll(/^ {0,3}\[([^\]\n]+)\]:[ \t]*(<[^>\n]*>|\S+)/gm)) {
    labels.add(m[1].trim().toLowerCase().replace(/\s+/g, ' '));
    links.push([m.index, m[2].replace(/^<|>$/g, '')]);
  }
  // HTML.
  for (const m of text.matchAll(/<(?:a|img|source|link|script)\b[^>]*?\s(?:href|src)\s*=\s*(?:"([^"]*)"|'([^']*)')/gi)) {
    links.push([m.index, m[1] ?? m[2]]);
  }
  // [text][label] and [label][]: the label is defined (footnotes, [^1], are GitHub's own).
  for (const m of text.matchAll(/\[([^\]\n]*)\]\[([^\]\n]*)\]/g)) {
    const label = (m[2] || m[1]).trim().toLowerCase().replace(/\s+/g, ' ');
    if (label && !label.startsWith('^') && !labels.has(label)) {
      problem(page, text, m.index, `no definition for [${m[2] || m[1]}]`);
    }
  }

  checked += links.length;
  for (const [at, link] of links) {
    const bad = (what) => problem(page, text, at, `${link}: ${what}`);
    if (/^[a-z][a-z\d+.-]*:|^\/\//i.test(link)) continue;
    const hash = link.indexOf('#');
    const raw = (hash < 0 ? link : link.slice(0, hash)).replace(/\?.*$/, '');
    let rel, frag;
    try {
      rel = decodeURIComponent(raw);
      frag = hash < 0 ? '' : decodeURIComponent(link.slice(hash + 1));
    } catch {
      bad('bad percent-encoding');
      continue;
    }
    // GitHub resolves a link from the page's directory, or from the repo's root if it starts with /.
    const joined = rel.startsWith('/') ? P.normalize(rel.slice(1)) : P.normalize(P.join(P.dirname(page), rel));
    const target = rel === '' ? page : joined.replace(/\/+$/, '').replace(/^\.$/, '');
    if (target === '..' || target.startsWith('../')) {
      bad('leaves the repo');
      continue;
    }
    const isFile = fileSet.has(target);
    if (!isFile && !dirs.has(target)) {
      const there = fs.existsSync(path.join(root, target));
      bad(there ? 'not in git (GitHub has no such file)' : 'no such file');
      continue;
    }
    if (!frag) continue;
    const readme = P.join(target, 'README.md');
    const md = isFile ? (target.endsWith('.md') ? target : null) : fileSet.has(readme) ? readme : null;
    const lines = /^L(\d+)(?:-L(\d+))?$/.exec(frag);
    if (md) {
      const have = pageAnchors(md);
      if (!have.has(frag)) {
        const lower = [...have].find((a) => a.toLowerCase() === frag.toLowerCase());
        bad(lower ? `the anchor is #${lower} (GitHub's are lower case)` : `no #${frag} in ${md}`);
      }
    } else if (isFile && lines) {
      const count = read(target).replace(/\n$/, '').split('\n').length;
      if (Math.max(+lines[1], +(lines[2] ?? 0)) > count) bad(`${target} has ${count} lines`);
    }
  }
}

problems.sort((a, b) => a.page.localeCompare(b.page) || a.line - b.line);
for (const p of problems) console.log(`  ${p.page}:${p.line}: ${p.what}`);
check(problems.length === 0,
  `${pages.length} pages, ${checked} links: every relative link and anchor resolves (${problems.length} problems)`);
console.log(failed ? `${failed} check(s) failed` : 'all passed');
process.exit(failed ? 1 : 0);
