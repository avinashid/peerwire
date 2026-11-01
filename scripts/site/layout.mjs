// Page shells for the site (scripts/site/build.mjs): the head every page shares, the top bar and footer, a doc page
// with its sidebar, "On this page" list and prev/next, and a redirect. None of them runs a script: the mobile menus
// are <details>, and light or dark follows prefers-color-scheme in site.css.
//
// Every URL is relative: href(to) is the path from the page being written to `to`, a path from the site's root.

import path from 'node:path';
import { escapeHtml as esc } from './markdown.mjs';

export const REPO_URL = 'https://github.com/avinashid/peerwire';
export const NPM_URL = 'https://www.npmjs.com/package/@peerwire/js';

// The demos' policy is docker/examples-nginx.conf's, less frame-ancestors (a <meta> policy can't set it). The other
// pages run no script at all.
export const DEMO_CSP = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; " +
  "connect-src 'self' wss:; img-src 'self' data:; object-src 'none'; base-uri 'none'; form-action 'none'";
export const PAGE_CSP = "default-src 'none'; style-src 'self'; img-src 'self' data:; base-uri 'none'; " +
  "form-action 'none'";
export const metas = (csp) => `<meta http-equiv="Content-Security-Policy" content="${csp}">\n` +
  '<meta name="referrer" content="no-referrer">';

export const linker = (from) => (to) => {
  const [file, frag] = to.split('#');
  if (frag !== undefined && file === from) return `#${frag}`;
  const rel = path.posix.relative(path.posix.dirname(from), file);
  return frag === undefined ? rel : `${rel}#${frag}`;
};

export function head({ href, id, title, description, extra = '' }) {
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
${metas(PAGE_CSP)}
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" media="(prefers-color-scheme: light)" content="#ffffff">
<meta name="theme-color" media="(prefers-color-scheme: dark)" content="#0b0d13">
${description ? `<meta name="description" content="${esc(description)}">\n` : ''}${extra}<title>${esc(title)}</title>
<link rel="icon" type="image/svg+xml" href="${href(`assets/${id}/img/favicon.svg`)}">
<link rel="stylesheet" href="${href(`assets/${id}/site.css`)}">
</head>`;
}

// on: 'docs' or 'home', the section to mark.
export function topBar({ href, id, on }) {
  const item = (to, text, key) => `<a href="${href(to)}"${key === on ? ' class="on"' : ''}>${text}</a>`;
  return `<header class="top">
<div class="top-in">
<a class="brand" href="${href('index.html')}"><img src="${href(`assets/${id}/img/favicon.svg`)}" alt="" width="26" ` +
    `height="26"><span>Peerwire</span></a>
<nav class="top-nav" aria-label="Site">${item('docs/index.html', 'Docs', 'docs')}${item('index.html#demos', 'Demos')}` +
    `<a href="${REPO_URL}">GitHub</a><a href="${NPM_URL}">npm</a></nav>
</div>
</header>`;
}

export function footer({ commit, extra = '' }) {
  const built = commit ? `<span>Built from <a href="${REPO_URL}/commit/${commit}">${commit.slice(0, 7)}</a></span>` : '';
  return `<footer class="foot">
${extra}<div class="foot-line"><span>Peerwire · MIT OR Apache-2.0</span>${built}</div>
</footer>`;
}

// The docs' navigation: the overview, then each group of docs/README.md, with the page `here` marked.
function sideNav(nav, here, href) {
  const link = (p) => `<a href="${href(p.page)}"${p.page === here ? ' aria-current="page"' : ''}>${esc(p.title)}</a>`;
  return `<ul class="side-home"><li>${link(nav.home)}</li></ul>\n` + nav.groups.map((g) =>
    `<p class="side-group">${esc(g.name)}</p>\n<ul>\n${g.items.map((p) => `<li>${link(p)}</li>`).join('\n')}\n</ul>`)
    .join('\n');
}

const tocList = (headings) => `<ul>\n${headings.map((h) =>
  `<li class="l${h.level}"><a href="#${esc(h.id)}">${esc(h.text)}</a></li>`).join('\n')}\n</ul>`;

// page: { page, title (the sidebar's), heading (its h1), group, source, description }; body: its rendered HTML;
// headings: its h2s and h3s; prev, next: pages in sidebar order, or undefined.
export function docPage({ id, commit, nav, page, body, headings, prev, next, index = false }) {
  const here = page.page;
  const href = linker(here);
  const side = sideNav(nav, here, href);
  const pager = (p, cls, label) => (p ? `<a class="${cls}" href="${href(p.page)}"><span>${label}</span>` +
    `${esc(p.title)}</a>` : '<span></span>');
  return `${head({ href, id, title: `${page.heading ?? page.title} · Peerwire`, description: page.description })}
<body class="docs">
${topBar({ href, id, on: 'docs' })}
<div class="shell">
<nav class="side" aria-label="Docs">
${side}
</nav>
<main class="doc-main">
<details class="mnav">
<summary><span class="mnav-label">Menu</span><span class="mnav-here">${esc(page.group)} › ${esc(page.title)}</span></summary>
<nav aria-label="Docs">
${side}
</nav>
</details>
${headings.length ? `<details class="mtoc">\n<summary>On this page</summary>\n${tocList(headings)}\n</details>\n` : ''}` +
    `<article class="doc${index ? ' index' : ''}">
<p class="eyebrow">${esc(page.group)}</p>
${body}
</article>
<nav class="pager" aria-label="Previous and next">${pager(prev, 'prev', 'Previous')}${pager(next, 'next', 'Next')}</nav>
<p class="edit"><a href="${REPO_URL}/edit/main/${page.source}">Edit this page on GitHub</a></p>
</main>
<aside class="toc" aria-label="On this page">
${headings.length ? `<p class="toc-title">On this page</p>\n${tocList(headings)}` : ''}
</aside>
</div>
${footer({ commit })}
</body>
</html>
`;
}

// A page that has moved: a refresh to its new place, and a link for when the refresh doesn't run.
export function redirectPage({ id, from, to, title }) {
  const href = linker(from);
  return `${head({ href, id, title: `${title} · Peerwire`,
    extra: `<meta http-equiv="refresh" content="0; url=${esc(href(to))}">\n<meta name="robots" content="noindex">\n` })}
<body class="moved">
<main class="moved-in">
<p>This page has moved to <a href="${esc(href(to))}">${esc(title)}</a>.</p>
</main>
</body>
</html>
`;
}
