// Builds the static site for GitHub Pages (.github/workflows/pages.yml): a landing page, the docs with a sidebar,
// and the browser demos that need no server of ours. Every URL in it is relative, so it works at
// https://<user>.github.io/peerwire/ as at any other path. No dependencies; scripts/site.sh builds the wasm first.
//
//   node scripts/site/build.mjs [--out target/site] [--wasm packages/js/pkg/peerwire_ffi.wasm]
//
// The docs: docs/README.md is their index. Its "## Group" headings and "- [Title](path.md): blurb" lines are the
// sidebar, in order, and the build fails if a docs/**/*.md is missing from it or listed twice. Each page is at its
// repo path with .html (docs/guides/rooms.md is docs/guides/rooms.html), docs/README.md at docs/index.html; entries
// outside docs/ (../CHANGELOG.md) are rendered too. The URLs of the first site (README.html, docs/ARCHITECTURE.html,
// ...) are small pages that refresh to the new ones (REDIRECTS). Code blocks are highlighted here, at build time
// (highlight.mjs), and docs/assets/ holds the images docs may show, the logo and the favicon.
//
// Caching: Pages serves everything with a 10-minute max-age. The demos' modules, the wasm, the stylesheet and the
// images live under assets/<id>/, where <id> is a hash of their contents, so a page from a new build never runs a
// module, or loads a wasm, cached from an old one: the demos import each other relatively and load the wasm relative
// to import.meta.url, so all of it moves to the new directory together. The pages keep stable URLs (invite links
// point at them); only their <script src> and <link href> name the directory.
//
// The build fails on a broken link: a relative href, src or refresh URL that names no file of the site, a #fragment
// that names no id on its page, a doc link to a repo path that doesn't exist, or a link with another scheme than
// http(s) or mailto. Doc links to repo files that aren't rendered point at GitHub. It fails too on an SVG under
// docs/assets/ that holds a script, a handler, a reference to anything outside it, an element or namespace that
// isn't SVG's, or a character outside printable ASCII (checkSvg).

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';
import { render, escapeHtml, textOf, slugger } from './markdown.mjs';
import { highlight } from './highlight.mjs';
import { REPO_URL, DEMO_CSP, PAGE_CSP, metas, linker, docPage, redirectPage } from './layout.mjs';
import { landingPage } from './landing.mjs';

export { REPO_URL, DEMO_CSP, PAGE_CSP };
const HERE = path.dirname(fileURLToPath(import.meta.url));

// The demos on the site: none of them needs a server of ours. crew.html needs crew-server.mjs, so it isn't here.
export const DEMOS = [
  {
    page: 'chat.html', name: 'Chat',
    blurb: 'Rooms by invite link or QR code, nicknames, members and their routes, a typing indicator.',
    how: 'Any two browsers, over public Nostr relays and STUN.',
  },
  {
    page: 'netlab.html', name: 'Net Lab',
    blurb: 'What your network allows: the NAT behind you, the route each link takes (direct, punched, or ' +
      'over the signalling relays), pings and a chat.',
    how: 'Any two browsers, over public Nostr relays and STUN.',
  },
  {
    page: 'index.html', name: 'Round trips',
    blurb: 'peerwire-core on its own: WebRTC DataChannels, the Noise handshake, a round-trip time per peer.',
    how: 'Tabs of one browser.',
  },
  {
    page: 'poll.html', name: 'Poll',
    blurb: 'joinRoom with the pull-style API, poll() and sendTo() every frame as Godot drives it: a dot per tab, ' +
      'moved with the arrow keys.',
    how: 'Tabs of one browser.',
  },
  {
    page: 'arena.html', name: 'Arena', extra: [['arena.html?role=super', 'super-peer tab']],
    blurb: 'Super-peer lockstep islands: open the super-peer tab first, then player tabs (bots until you press ' +
      'the arrow keys).',
    how: 'Tabs of one browser.',
  },
];

// The first site's pages, which links elsewhere may still name, and where each is now.
export const REDIRECTS = {
  'README.html': 'index.html',
  'packages/js/README.html': 'docs/reference/javascript.html',
  'packages/react/README.html': 'docs/reference/react.html',
  'docs/ARCHITECTURE.html': 'docs/concepts/architecture.html',
  'docs/NAT-TRAVERSAL.html': 'docs/concepts/nat-traversal.html',
  'docs/GODOT.html': 'docs/guides/godot.html',
  'docs/DOCKER.html': 'docs/guides/always-on-peer.html',
  'docs/PERFORMANCE.html': 'docs/project/performance.html',
  'docs/COMPARISON.html': 'docs/project/comparison.html',
  'docs/TEST-PLAN.html': 'docs/project/testing.html',
  'docs/THREAT-MODEL.html': 'docs/project/threat-model.html',
  'docs/STORE-DESIGN.html': 'docs/project/offline-store-design.html',
};

export const INDEX = 'docs/README.md';
const ASSETS = 'docs/assets/';
const IMAGE = /^[\w-]+(?:\/[\w-]+)*\.(?:svg|png|jpe?g|gif|webp)$/;

const posix = path.posix;
const sitePathOf = (source) => (source === INDEX ? 'docs/index.html' : source.replace(/\.md$/, '.html'));
const inlineText = (md) => textOf(render(md).html).replace(/\s+/g, ' ').trim();
const sentence = (s) => s && s[0].toUpperCase() + s.slice(1);

// Every file under dir (a path from root), symlinks followed, as posix paths from root.
function walk(root, dir) {
  const out = [];
  for (const name of fs.readdirSync(path.join(root, dir)).sort()) {
    const file = posix.join(dir, name);
    if (fs.statSync(path.join(root, file)).isDirectory()) out.push(...walk(root, file));
    else out.push(file);
  }
  return out;
}

// docs/README.md: its text before the first "## " (the overview's), then each "## Group" and its
// "- [Title](path.md): blurb" lines (a blurb may go on over indented lines): the sidebar, in order. Anything else
// under a group, a link that isn't a .md file of the repo, and a docs/**/*.md listed twice or not at all throw.
export function readIndex(root) {
  const intro = [];
  const groups = [];
  let item = null;
  for (const [k, line] of fs.readFileSync(path.join(root, INDEX), 'utf8').replace(/\r\n?/g, '\n').split('\n').entries()) {
    const where = `${INDEX}:${k + 1}`;
    let m;
    if ((m = /^## +(.*\S)/.exec(line))) {
      groups.push({ name: inlineText(m[1]), items: [] });
      item = null;
    } else if (!groups.length) {
      intro.push(line);
    } else if ((m = /^- \[([^\]]+)\]\(([^()\s]+)\)(?::[ \t]+(.*))?$/.exec(line))) {
      const source = posix.normalize(posix.join(posix.dirname(INDEX), decodeURI(m[2])));
      if (source.startsWith('../') || !source.endsWith('.md') || !fs.existsSync(path.join(root, source))) {
        throw new Error(`site: ${where}: ${m[2]} is no .md file of the repo`);
      }
      item = { title: inlineText(m[1]), source, blurb: m[3] ?? '', group: groups.at(-1).name };
      groups.at(-1).items.push(item);
    } else if (item && /^[ \t]+\S/.test(line)) {
      item.blurb += `\n${line.trim()}`;
    } else if (line.trim()) {
      throw new Error(`site: ${where}: a group holds only "- [Title](path.md): blurb" lines, not ${JSON.stringify(line)}`);
    } else {
      item = null;
    }
  }
  const listed = groups.flatMap((g) => g.items.map((i) => i.source));
  const twice = [...new Set(listed.filter((s, k) => listed.indexOf(s) !== k || s === INDEX))];
  const missing = walk(root, 'docs').filter((f) => f.endsWith('.md') && f !== INDEX && !listed.includes(f));
  if (twice.length || missing.length) {
    throw new Error(`site: ${INDEX} is the docs' sidebar: ${[...twice.map((s) => `${s} is listed twice`),
      ...missing.map((s) => `${s} isn't listed`)].join('; ')}`);
  }
  return { intro: intro.join('\n'), groups };
}

// An SVG under docs/assets/ is served from the site's origin with no policy of ours: opened on its own, script in
// it would run there. What it may not hold: a script, an on* handler, a foreignObject, a DTD, or a reference to
// anything outside itself (href and url() name only #ids; no URL scheme but its xmlns). Opened on its own it's XML,
// where an element's namespace, not its spelling, makes it a script (<s:script>), and character references and
// processing instructions are spellings a regex can't see through: so no prefixed element, no &#, and no <? but the
// XML declaration (NB67-1). An XHTML element needs no prefix in the default namespace (<g xmlns=xhtml><iframe
// srcdoc>), so the elements are an allow-list of shapes and paint servers, and the only namespaces SVG's and xlink's.
// With no DTD, &# or processing instruction, every element of the parsed document is a literal <name in the source.
// Only printable ASCII, tab, LF and CR, so that \s and \w agree with XML's names: U+1680 and U+FEFF are XML name
// characters that \s matches, so a prefix of g and U+1680 would hide a script behind an allowed <g. No backslash: a CSS
// escape spells url( as u\72l(. And no // outside the xmlns, since //host is a URL with no scheme (NB67-I4).
const SVG_ELEMENTS = new Set(['svg', 'g', 'defs', 'title', 'desc', 'symbol', 'use', 'path', 'rect', 'circle', 'ellipse',
  'line', 'polyline', 'polygon', 'linearGradient', 'radialGradient', 'stop', 'clipPath', 'mask', 'pattern']);
export function checkSvg(svg) {
  const problems = [];
  if (/[^\t\n\r\x20-\x7e]/.test(svg)) problems.push('a character outside printable ASCII');
  for (const [, name] of svg.matchAll(/<\/?([^\s/>!?]+)/g)) {
    if (!SVG_ELEMENTS.has(name)) problems.push(`a <${name}>`);
  }
  for (const [, prefix, , uri] of svg.matchAll(/\sxmlns(:[\w.-]+)?\s*=\s*(["'])(.*?)\2/gs)) {
    if (uri !== (prefix === ':xlink' ? 'http://www.w3.org/1999/xlink' : 'http://www.w3.org/2000/svg')) {
      problems.push(`the namespace ${JSON.stringify(uri)}`);
    }
  }
  if (/<script/i.test(svg)) problems.push('a <script>');
  if (/<\/?[A-Za-z_][\w.-]*:/.test(svg)) problems.push('an element with a namespace prefix');
  if (/&#/.test(svg)) problems.push('a character reference');
  if (/\\/.test(svg)) problems.push('a backslash');
  if (/<\?(?!xml\s)/.test(svg)) problems.push('a processing instruction');
  if (/\son[a-z]+\s*=/i.test(svg)) problems.push('an on* handler');
  if (/<foreignObject/i.test(svg)) problems.push('a <foreignObject>');
  if (/<!(?:DOCTYPE|ENTITY)/i.test(svg)) problems.push('a DTD');
  for (const m of svg.matchAll(/(?:href|src)\s*=\s*(["'])(.*?)\1/gis)) {
    if (!m[2].startsWith('#')) problems.push(`a reference to ${JSON.stringify(m[2])}`);
  }
  if (/url\(\s*(?!['"]?#)/i.test(svg) || /@import/i.test(svg)) problems.push('a url() or @import outside it');
  if (/\/\/|javascript:|data:/i.test(svg.replace(/\sxmlns(?::\w+)?\s*=\s*(["'])[^"']*\1/g, ''))) {
    problems.push('a URL');
  }
  return [...new Set(problems)];
}

// The demos' module graph, followed from their entries: static and dynamic imports and new URL(…, import.meta.url),
// relative ones only (packages/js has no others). Paths are relative to packages/js.
const SPECIFIERS = [
  /\bfrom\s*(['"])(\.{1,2}\/[^'"\n]+)\1/g,
  /\bimport\s*\(?\s*(['"])(\.{1,2}\/[^'"\n]+)\1/g,
  /\bnew URL\(\s*(['"])(\.{1,2}\/[^'"\n]+)\1\s*,\s*import\.meta\.url\s*\)/g,
];
export function moduleGraph(jsRoot, entries) {
  const seen = new Set();
  const todo = [...entries];
  while (todo.length) {
    const file = todo.pop();
    if (seen.has(file)) continue;
    if (!/^(demo|src|pkg)\//.test(file)) throw new Error(`site: ${file} is outside packages/js/{demo,src,pkg}`);
    seen.add(file);
    if (!file.endsWith('.js')) continue;
    const code = fs.readFileSync(path.join(jsRoot, file), 'utf8');
    for (const re of SPECIFIERS) {
      for (const m of code.matchAll(re)) todo.push(posix.normalize(posix.join(posix.dirname(file), m[2])));
    }
  }
  return [...seen].sort();
}

function git(root, ...args) {
  try {
    return execFileSync('git', ['-C', root, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
  } catch {
    return '';
  }
}

// Puts the policy after the demo's <meta charset> and points its module script into assets/<id>/demo/.
function demoPage(html, id, name) {
  let metaCount = 0;
  html = html.replace(/<meta charset="utf-8">/i, (m) => {
    metaCount++;
    return `${m}\n${metas(DEMO_CSP)}`;
  });
  if (metaCount !== 1) throw new Error(`site: demo/${name} has no <meta charset="utf-8"> to put its policy after`);
  return html.replace(/<script\b([^>]*)>/gi, (tag, attrs) => {
    const src = /\ssrc="([\w.-]+\.js)"/.exec(attrs);
    if (!src || !/\stype="module"/.test(attrs)) throw new Error(`site: demo/${name}: ${tag} isn't a module script`);
    return tag.replace(src[0], ` src="../assets/${id}/demo/${src[1]}"`);
  });
}

// Every href, src and refresh URL of every page: relative, naming a file of the site, and a #fragment an id on its
// page.
export function checkLinks(out, pages) {
  const ids = new Map();
  for (const p of pages) {
    const html = fs.readFileSync(path.join(out, p), 'utf8');
    ids.set(p, new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((m) => textOf(m[1]))));
  }
  const problems = [];
  for (const p of pages) {
    const html = fs.readFileSync(path.join(out, p), 'utf8');
    const urls = [...html.matchAll(/\s(href|src)="([^"]*)"/g)].map((m) => [m[1], m[2]]);
    for (const m of html.matchAll(/<meta http-equiv="refresh" content="\d+; *url=([^"]*)"/gi)) urls.push(['refresh', m[1]]);
    for (const [attr, raw] of urls) {
      const url = textOf(raw);
      if (/^(https?:|mailto:)/i.test(url)) continue;
      if (/^[a-z][a-z\d+.-]*:/i.test(url) || url.startsWith('/') || url === '') {
        problems.push(`${p}: ${attr}="${url}" isn't a relative link`);
        continue;
      }
      const [target, frag] = url.split('#');
      const file = target ? posix.normalize(posix.join(posix.dirname(p), target.split('?')[0])) : p;
      const full = path.join(out, file);
      if (file.startsWith('../') || !fs.existsSync(full) || fs.statSync(full).isDirectory()) {
        problems.push(`${p}: ${attr}="${url}" names no file of the site`);
      } else if (frag && ids.has(file) && !ids.get(file).has(decodeURIComponent(frag))) {
        problems.push(`${p}: ${attr}="${url}": no id "${frag}" on ${file}`);
      }
    }
  }
  return problems;
}

export function buildSite({ root, out, wasm }) {
  const jsRoot = path.join(root, 'packages/js');
  wasm ??= path.join(jsRoot, 'pkg/peerwire_ffi.wasm');
  if (!fs.existsSync(wasm)) throw new Error(`site: no ${wasm}: run packages/js/build.sh first`);
  if (fs.existsSync(out) && fs.readdirSync(out).length && !fs.existsSync(path.join(out, 'assets'))) {
    throw new Error(`site: ${out} isn't empty and holds no earlier site (no assets/): not clearing it`);
  }
  const nav = readIndex(root);
  fs.rmSync(out, { recursive: true, force: true });
  const commit = process.env.GITHUB_SHA || git(root, 'rev-parse', 'HEAD');

  // 1. assets/<id>/: the demos' module graph, the wasm, the stylesheet and docs/assets/ (as img/), under a hash of
  //    all of them.
  const demoHtml = new Map(DEMOS.map((d) => [d.page, fs.readFileSync(path.join(jsRoot, 'demo', d.page), 'utf8')]));
  const entries = [...demoHtml.values()].flatMap((html) => [...html.matchAll(/<script\b[^>]*\ssrc="([\w.-]+\.js)"/gi)])
    .map((m) => `demo/${m[1]}`);
  const modules = moduleGraph(jsRoot, entries);
  const assets = modules.map((m) => [m, m === 'pkg/peerwire_ffi.wasm' ? wasm : path.join(jsRoot, m)]);
  if (!modules.includes('pkg/peerwire_ffi.wasm')) throw new Error('site: no demo loads pkg/peerwire_ffi.wasm');
  assets.push(['site.css', path.join(HERE, 'site.css')]);
  for (const file of walk(root, ASSETS.slice(0, -1))) {
    const name = file.slice(ASSETS.length);
    if (!IMAGE.test(name)) throw new Error(`site: ${file}: docs/assets/ holds images only, named [\\w-] (svg, png, jpg, gif, webp)`);
    if (name.endsWith('.svg')) {
      const problems = checkSvg(fs.readFileSync(path.join(root, file), 'utf8'));
      if (problems.length) throw new Error(`site: ${file} holds ${problems.join(', ')}`);
    }
    assets.push([`img/${name}`, path.join(root, file)]);
  }
  for (const need of ['img/favicon.svg', 'img/logo.svg']) {
    if (!assets.some(([name]) => name === need)) throw new Error(`site: no docs/assets/${need.slice(4)}`);
  }
  const hash = createHash('sha256');
  for (const [name, file] of assets.sort(([a], [b]) => (a < b ? -1 : 1))) {
    hash.update(`${name}\0`).update(fs.readFileSync(file)).update('\0');
  }
  const id = hash.digest('hex').slice(0, 12);
  for (const [name, file] of assets) {
    fs.mkdirSync(path.dirname(path.join(out, 'assets', id, name)), { recursive: true });
    fs.copyFileSync(file, path.join(out, 'assets', id, name));
  }
  const pages = [];
  const write = (name, html) => {
    if (pages.includes(name)) throw new Error(`site: two pages at ${name}`);
    fs.mkdirSync(path.dirname(path.join(out, name)), { recursive: true });
    fs.writeFileSync(path.join(out, name), html);
    pages.push(name);
  };

  // 2. The demos, their script pointed into assets/<id>/.
  for (const [name, html] of demoHtml) write(`demo/${name}`, demoPage(html, id, name));

  // 3. The docs, in sidebar order: the overview (docs/README.md), then each group's pages.
  const home = { source: INDEX, page: sitePathOf(INDEX), title: 'Overview', group: 'Docs', blurb: '' };
  const order = [home, ...nav.groups.flatMap((g) => g.items)];
  for (const p of order) p.page = sitePathOf(p.source);
  const sitePath = new Map(order.map((p) => [p.source, p.page]));
  for (const d of DEMOS) sitePath.set(`packages/js/demo/${d.page}`, `demo/${d.page}`);
  const sidebar = { home, groups: nav.groups };
  for (const [k, p] of order.entries()) {
    const href = linker(p.page);
    const link = (url) => {
      if (/^(https?:|mailto:)/i.test(url) || url.startsWith('#')) return url;
      const [file, frag] = url.split('#');
      const target = posix.normalize(posix.join(posix.dirname(p.source), decodeURI(file))).replace(/\/$/, '');
      if (target.startsWith('../')) throw new Error(`site: ${p.source} links outside the repo: ${url}`);
      const hashPart = frag === undefined ? '' : `#${frag}`;
      if (sitePath.has(target)) return href(sitePath.get(target) + hashPart);
      if (!fs.existsSync(path.join(root, target))) throw new Error(`site: ${p.source} links to ${url}: no such file in the repo`);
      const kind = fs.statSync(path.join(root, target)).isDirectory() ? 'tree' : 'blob';
      return `${REPO_URL}/${kind}/main/${encodeURI(target)}${hashPart}`;
    };
    // An image from the repo is a file under docs/assets/; one from elsewhere (a README badge) isn't loaded, and its
    // alt text stands in.
    const image = (src, alt) => {
      if (/^https?:/i.test(src)) return `<span class="badge">${escapeHtml(alt)}</span>`;
      const target = posix.normalize(posix.join(posix.dirname(p.source), decodeURI(src)));
      if (!target.startsWith(ASSETS) || !fs.existsSync(path.join(root, target))) {
        throw new Error(`site: ${p.source} shows ${src}: an image from the repo must be a file under docs/assets/`);
      }
      return `<img src="${href(`assets/${id}/img/${target.slice(ASSETS.length)}`)}" alt="${escapeHtml(alt)}" loading="lazy">`;
    };
    const opts = { link, image, highlight };
    let body;
    let headings;
    let heading;
    if (p === home) {
      // The overview: docs/README.md's own text, then a card for each page of each group.
      const r = render(nav.intro, opts);
      const slug = slugger();
      r.ids.forEach(slug);
      headings = nav.groups.map((g) => ({ level: 2, id: slug(g.name), text: g.name }));
      body = [r.html, ...nav.groups.map((g, n) => `<h2 id="${escapeHtml(headings[n].id)}">${escapeHtml(g.name)}</h2>
<ul class="cards">
${g.items.map((i) => `<li><a href="${href(i.page)}">${escapeHtml(i.title)}</a>\n${render(i.blurb, opts).html}</li>`).join('\n')}
</ul>`)].join('\n');
      heading = r.title;
      home.blurb = (/<p>([^]*?)<\/p>/.exec(r.html) ?? [])[1] ?? '';
      p.description = textOf(home.blurb).replace(/\s+/g, ' ').trim();
    } else {
      const r = render(fs.readFileSync(path.join(root, p.source), 'utf8'), opts);
      body = r.html;
      headings = r.headings;
      heading = r.title;
      p.description = sentence(inlineText(p.blurb));
    }
    write(p.page, docPage({ id, commit, nav: sidebar, page: { ...p, heading: heading ?? p.title }, body, headings,
      prev: order[k - 1], next: order[k + 1], index: p === home }));
  }

  // 4. The landing page.
  const wasmBytes = fs.readFileSync(wasm);
  const sizes = { wasm: Math.round(wasmBytes.length / 1024), gzip: Math.round(gzipSync(wasmBytes, { level: 9 }).length / 1024) };
  write('index.html', landingPage({ id, commit, demos: DEMOS, sizes, highlight }));

  // 5. The first site's URLs, each a page that refreshes to where its page is now.
  const titles = new Map([['index.html', 'the Peerwire home page'], ...order.map((p) => [p.page, p.title])]);
  for (const [from, to] of Object.entries(REDIRECTS)) {
    if (!titles.has(to)) throw new Error(`site: the redirect from ${from} names ${to}, which isn't a page`);
    write(from, redirectPage({ id, from, to, title: titles.get(to) }));
  }

  // 6. Every link of every page resolves.
  const problems = checkLinks(out, pages);
  if (problems.length) throw new Error(`site: ${problems.length} broken link(s):\n  ${problems.join('\n  ')}`);
  return { id, pages, assets: assets.map(([name]) => `assets/${id}/${name}`), redirects: Object.keys(REDIRECTS),
    docs: order.map((p) => p.page), sizes };
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const arg = (name, dflt) => {
    const k = process.argv.indexOf(name);
    return k > 0 ? process.argv[k + 1] : dflt;
  };
  const root = path.resolve(HERE, '../..');
  const out = path.resolve(arg('--out', path.join(root, 'target/site')));
  const wasm = arg('--wasm') && path.resolve(arg('--wasm'));
  try {
    const { id, pages, assets } = buildSite({ root, out, wasm });
    console.log(`site: ${pages.length} pages and ${assets.length} assets (assets/${id}/) in ${out}`);
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
