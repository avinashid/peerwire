// Builds the static site for GitHub Pages (.github/workflows/pages.yml): a landing page, README.md and the docs
// rendered to HTML, and the browser demos that need no server of ours. Every URL in it is relative, so it works
// at https://<user>.github.io/peerwire/ as at any other path. No dependencies; scripts/site.sh builds the wasm first.
//
//   node scripts/site/build.mjs [--out target/site] [--wasm packages/js/pkg/peerwire_ffi.wasm]
//
// Caching: Pages serves everything with a 10-minute max-age. The demos' modules, the wasm and the stylesheet live
// under assets/<id>/, where <id> is a hash of their contents, so a page from a new build never runs a module, or
// loads a wasm, cached from an old one: the demos import each other relatively and load the wasm relative to
// import.meta.url, so all of it moves to the new directory together. The pages keep stable URLs (invite links
// point at them); only their <script src> names the directory.
//
// The build fails on a broken link: a relative href or src that names no file of the site, a #fragment that names
// no id on its page, a doc link to a repo path that doesn't exist, or a link with another scheme than http(s) or
// mailto. Doc links to repo files that aren't rendered point at GitHub.

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { render, escapeHtml, textOf } from './markdown.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const REPO_URL = 'https://github.com/avinashid/peerwire';

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

// Rendered markdown, in the order the landing page lists it; every docs/*.md is added after these. The landing
// page's line for each: [title, blurb]; a doc not named here gets its first heading and first paragraph.
const DOCS_FIRST = ['README.md', 'packages/js/README.md', 'packages/react/README.md'];
const DOC_LINES = {
  'README.md': ['README', 'What Peerwire is, a quick start, how it works, and the status of each part.'],
  'packages/js/README.md': ['@peerwire/js', 'The JS package: joinRoom, members and routes, shared docs, offline ' +
    'messages, and the wasm module underneath.'],
  'packages/react/README.md': ['@peerwire/react', 'React hooks for rooms, members, chat and shared state.'],
  'docs/ARCHITECTURE.md': ['Architecture', 'How the parts fit: the sans-IO core, its transports and signalling, ' +
    'and the crates around it.'],
  'docs/COMPARISON.md': ['How Peerwire compares', 'Next to other peer-to-peer libraries, feature by feature.'],
  'docs/DOCKER.md': ['Docker', 'The optional always-on peer, and these demos served from a container.'],
  'docs/GODOT.md': ['Godot 4', "Godot's own multiplayer API (@rpc, synchronizers, spawners) over Peerwire."],
  'docs/NAT-TRAVERSAL.md': ['NAT traversal', 'How peers behind NATs find and reach each other, and what they fall ' +
    'back to.'],
  'docs/PERFORMANCE.md': ['Performance model', 'Measured and theoretical numbers, and how to reproduce each.'],
  'docs/STORE-DESIGN.md': ['Store design', "Keeping a room's data while nobody is online."],
  'docs/TEST-PLAN.md': ['Test plan', 'How Peerwire is tested, and the gate every change passes.'],
  'docs/THREAT-MODEL.md': ['Threat model', "What Peerwire defends against, what it doesn't, and every review " +
    'finding with its status.'],
};

// The demos' policy is docker/examples-nginx.conf's, less frame-ancestors (a <meta> policy can't set it). The other
// pages run no script at all.
export const DEMO_CSP = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; " +
  "connect-src 'self' wss:; img-src 'self' data:; object-src 'none'; base-uri 'none'; form-action 'none'";
export const PAGE_CSP = "default-src 'none'; style-src 'self'; img-src 'self' data:; base-uri 'none'; " +
  "form-action 'none'";
const metas = (csp) => `<meta http-equiv="Content-Security-Policy" content="${csp}">\n` +
  '<meta name="referrer" content="no-referrer">';

const posix = path.posix;
const rel = (from, to) => posix.relative(posix.dirname(from), to);

// The modules a demo loads, followed from its entry: static and dynamic imports and new URL(…, import.meta.url),
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

function page({ title, csp, depth, id, body, source, commit }) {
  const up = '../'.repeat(depth);
  const from = source ? `<a href="${REPO_URL}/blob/main/${source}">${escapeHtml(source)} on GitHub</a> · ` : '';
  const built = commit ? ` · built from <a href="${REPO_URL}/commit/${commit}">${commit.slice(0, 7)}</a>` : '';
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
${metas(csp)}
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="theme-color" content="#0b0d13">
<title>${escapeHtml(title)}</title>
<link rel="stylesheet" href="${up}assets/${id}/site.css">
</head>
<body>
<header class="top">
<a class="brand" href="${up}index.html"><span class="logo"></span>Peerwire</a>
<nav><a href="${up}index.html#demos">Demos</a><a href="${up}index.html#docs">Docs</a>` +
    `<a href="${REPO_URL}">GitHub</a></nav>
</header>
${body}
<footer>${from}MIT OR Apache-2.0${built}</footer>
</body>
</html>
`;
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

// Every href and src of every page: relative, naming a file of the site, and a #fragment an id on its page.
export function checkLinks(out, pages) {
  const ids = new Map();
  for (const p of pages) {
    const html = fs.readFileSync(path.join(out, p), 'utf8');
    ids.set(p, new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((m) => textOf(m[1]))));
  }
  const problems = [];
  for (const p of pages) {
    const html = fs.readFileSync(path.join(out, p), 'utf8');
    for (const [, attr, raw] of html.matchAll(/\s(href|src)="([^"]*)"/g)) {
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
  fs.rmSync(out, { recursive: true, force: true });
  const commit = process.env.GITHUB_SHA || git(root, 'rev-parse', 'HEAD');

  // 1. assets/<id>/: the demos' module graph, the wasm and the stylesheet, under a hash of all of them.
  const demoHtml = new Map(DEMOS.map((d) => [d.page, fs.readFileSync(path.join(jsRoot, 'demo', d.page), 'utf8')]));
  const entries = [...demoHtml.values()].flatMap((html) => [...html.matchAll(/<script\b[^>]*\ssrc="([\w.-]+\.js)"/gi)])
    .map((m) => `demo/${m[1]}`);
  const modules = moduleGraph(jsRoot, entries);
  const assets = modules.map((m) => [m, m === 'pkg/peerwire_ffi.wasm' ? wasm : path.join(jsRoot, m)]);
  if (!modules.includes('pkg/peerwire_ffi.wasm')) throw new Error('site: no demo loads pkg/peerwire_ffi.wasm');
  assets.push(['site.css', path.join(HERE, 'site.css')]);
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
    fs.mkdirSync(path.dirname(path.join(out, name)), { recursive: true });
    fs.writeFileSync(path.join(out, name), html);
    pages.push(name);
  };

  // 2. The demos, their script pointed into assets/<id>/.
  for (const [name, html] of demoHtml) write(`demo/${name}`, demoPage(html, id, name));

  // 3. The docs: README.md, the package READMEs and docs/*.md, each at its repo path with .html for .md.
  const docs = [...DOCS_FIRST, ...fs.readdirSync(path.join(root, 'docs')).filter((f) => f.endsWith('.md')).sort()
    .map((f) => `docs/${f}`)];
  const sitePath = new Map(docs.map((d) => [d, d.replace(/\.md$/, '.html')]));
  for (const d of DEMOS) sitePath.set(`packages/js/demo/${d.page}`, `demo/${d.page}`);
  const listed = [];
  for (const doc of docs) {
    const here = sitePath.get(doc);
    const link = (href) => {
      if (/^(https?:|mailto:)/i.test(href) || href.startsWith('#')) return href;
      const [p, frag] = href.split('#');
      const target = posix.normalize(posix.join(posix.dirname(doc), decodeURI(p))).replace(/\/$/, '');
      if (target.startsWith('../')) throw new Error(`site: ${doc} links outside the repo: ${href}`);
      const hashPart = frag === undefined ? '' : `#${frag}`;
      if (sitePath.has(target)) return rel(here, sitePath.get(target)) + hashPart;
      const file = path.join(root, target);
      if (!fs.existsSync(file)) throw new Error(`site: ${doc} links to ${href}: no such file in the repo`);
      return `${REPO_URL}/${fs.statSync(file).isDirectory() ? 'tree' : 'blob'}/main/${encodeURI(target)}${hashPart}`;
    };
    // Images from elsewhere (README's shields.io badges) aren't loaded: their alt text stands in.
    const image = (src, alt) => {
      if (/^https?:/i.test(src)) return `<span class="badge">${escapeHtml(alt)}</span>`;
      throw new Error(`site: ${doc} shows ${src}: images from the repo aren't supported yet`);
    };
    const r = render(fs.readFileSync(path.join(root, doc), 'utf8'), { link, image });
    const title = r.title ?? doc;
    const depth = here.split('/').length - 1;
    write(here, page({ title: `${title} · Peerwire`, csp: PAGE_CSP, depth, id, source: doc, commit,
      body: `<main class="doc">\n${r.html}\n</main>` }));
    const first = /<p>([^]*?)<\/p>/.exec(r.html);
    const [name, blurb] = DOC_LINES[doc] ?? [title, first ? textOf(first[1]).replace(/\s+/g, ' ').trim() : ''];
    listed.push({ page: here, title: name, blurb });
  }

  // 4. The landing page.
  const sentence = (s) => (s.length <= 200 ? s : `${s.slice(0, s.lastIndexOf(' ', 200))} …`);
  const extra = (d) => (d.extra ?? []).map(([href, text]) => ` <a href="demo/${href}">Open the ${text}</a>.`).join('');
  const demos = DEMOS.map((d) => `<li class="card"><h3><a href="demo/${d.page}">${d.name}</a></h3>
<p>${escapeHtml(d.blurb)}</p>
<p class="how">${escapeHtml(d.how)}${extra(d)}</p></li>`);
  const docItems = listed.map((d) => `<li><a href="${d.page}">${escapeHtml(d.title)}</a>` +
    `${d.blurb ? `<span class="dim"> ${escapeHtml(sentence(d.blurb))}</span>` : ''}</li>`);
  write('index.html', page({ title: 'Peerwire: a direct wire between peers', csp: PAGE_CSP, depth: 0, id, commit,
    body: `<main class="home">
<section class="hero">
<h1>Peerwire</h1>
<p class="tag">A direct wire between peers.</p>
<p>Peer-to-peer networking for games, whiteboards, meetings and chat: encrypted, NAT-traversing and real-time, with
no server to run and no relay to pay for. Rust crates, a zero-import WebAssembly module with a small JS package,
a C ABI and a Godot binding.</p>
<p class="links"><a class="button primary" href="README.html">Read the README</a>
<a class="button" href="#demos">Try a demo</a>
<a class="button" href="${REPO_URL}">Source on GitHub</a></p>
</section>
<section id="demos">
<h2>Live demos</h2>
<p class="dim">Each runs in your browser and talks to other browsers directly; nothing goes through a server of
ours. Open one in two tabs, or on two devices where it says so.</p>
<ul class="cards">
${demos.join('\n')}
</ul>
</section>
<section id="docs">
<h2>Docs</h2>
<ul class="docs">
${docItems.join('\n')}
</ul>
</section>
</main>` }));

  // 5. Every link of every page resolves.
  const problems = checkLinks(out, pages);
  if (problems.length) throw new Error(`site: ${problems.length} broken link(s):\n  ${problems.join('\n  ')}`);
  return { id, pages, assets: assets.map(([name]) => `assets/${id}/${name}`) };
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
