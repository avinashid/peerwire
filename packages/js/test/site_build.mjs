// The site's build (NB-016, scripts/site/build.mjs) and its markdown renderer, with no browser:
//  - the renderer escapes raw HTML, refuses link schemes but http(s) and mailto, makes GitHub's heading ids, and
//    renders the lists, tables, alerts and emphasis the docs use;
//  - every page has its Content-Security-Policy first after <meta charset>, only the demos run scripts, and those
//    scripts, every module they import and the wasm live under assets/<id>/;
//  - <id> is a hash of the assets: the same for the same inputs, new when the wasm changes;
//  - doc links point at the rendered page or at GitHub, a broken one fails the build, and so does every broken
//    href or src of a page;
//  - .github/workflows/pages.yml, which builds what Pages serves, runs no action but GitHub's own actions/* (NB16-1).
// node packages/js/test/site_build.mjs (needs packages/js/pkg/peerwire_ffi.wasm: packages/js/build.sh)

import { buildSite, checkLinks, DEMO_CSP, PAGE_CSP, REPO_URL } from '../../../scripts/site/build.mjs';
import { render } from '../../../scripts/site/markdown.mjs';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..');
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const html = (md) => render(md).html;
const throws = (fn) => {
  try {
    fn();
    return false;
  } catch {
    return true;
  }
};
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'peerwire-site-build-'));

// 1. The renderer.
check(html('<script>alert(1)</script> <img src=x onerror=alert(1)>') ===
  '<p>&lt;script&gt;alert(1)&lt;/script&gt; &lt;img src=x onerror=alert(1)&gt;</p>', 'raw HTML is escaped');
check(html('a<br>\nb') === '<p>a<br>\nb</p>', '<br> is the one tag kept');
check(['[x](javascript:alert(1))', '[x](JavaScript:alert(1))', '![x](javascript:alert(1))', '[x](data:text/html,x)',
  '[x](vbscript:x)', '[x](<java\tscript:x>)'].every((md) => throws(() => html(md))), 'javascript:, data: and other schemes throw');
check(html('<javascript:alert(1)>') === '<p>&lt;javascript:alert(1)&gt;</p>', 'only http(s) is an autolink');
check(html('[x](https://a.example/"onmouseover=alert(1))') === '<p><a href="https://a.example/&quot;onmouseover=alert(1)">x</a></p>',
  'a quote in a link stays inside the attribute');
check(html('`<b>` and\n\n```html\n<i>\n```') === '<p><code>&lt;b&gt;</code> and</p>\n<pre><code class="language-html">&lt;i&gt;</code></pre>',
  'code spans and fenced code are escaped');
const heads = render('# Shared docs (NB-006b, NB-052) — NB6B-1…NB6B-4\n## A b\n## A b\n### `x_y` & **z**');
check(heads.ids.join(' ') === 'shared-docs-nb-006b-nb-052--nb6b-1nb6b-4 a-b a-b-1 x_y--z' &&
  heads.title === 'Shared docs (NB-006b, NB-052) — NB6B-1…NB6B-4', `GitHub's heading ids (${heads.ids.join(' ')})`);
check(html('- a\n  - b\n    more\n- c') === '<ul>\n<li>a\n<ul>\n<li>b\nmore</li>\n</ul></li>\n<li>c</li>\n</ul>',
  'a tight nested list');
check(html('1. a\n\n2. b\n   lazy\ncontinued') === '<ol>\n<li><p>a</p></li>\n<li><p>b\nlazy\ncontinued</p></li>\n</ol>',
  'a loose ordered list, with a lazy continuation line');
check(html('3) x') === '<ol start="3">\n<li>x</li>\n</ol>', 'an ordered list keeps its start');
check(html('| a | b |\n|:--|--:|\n| `x \\| y` | *z* |') === '<div class="table"><table>\n<thead><tr><th class="l">a</th>' +
  '<th class="r">b</th></tr></thead>\n<tbody>\n<tr><td class="l"><code>x | y</code></td><td class="r"><em>z</em></td></tr>\n' +
  '</tbody>\n</table></div>', 'a table: alignment by class, \\| inside code');
check(html('> [!IMPORTANT]\n> **Read** this.') === '<div class="alert important">\n<p class="alert-title">Important</p>\n' +
  '<p><strong>Read</strong> this.</p>\n</div>', "GitHub's alert");
check(html('**the `x` call** and snake_case_name, 2 * 3 * 4, _em_ and *em*') ===
  '<p><strong>the <code>x</code> call</strong> and snake_case_name, 2 * 3 * 4, <em>em</em> and <em>em</em></p>', 'emphasis');
check(html('see https://example.com/a_(b). and \\*not em\\*') ===
  '<p>see <a href="https://example.com/a_(b)">https://example.com/a_(b)</a>. and *not em*</p>', 'bare URLs and escapes');
check(render('[![CI](https://img.example/b.svg)](https://ci.example)', { image: (src, alt) => `[${alt}]` }).html ===
  '<p><a href="https://ci.example">[CI]</a></p>', 'an image inside a link goes through image()');
check(html('---\n\n    indented\n    code') === '<hr>\n<pre><code>indented\ncode</code></pre>', 'a rule and indented code');
check(render('## a &#9999999; &amp; b').ids[0] === 'a-9999999--b', 'an entity past U+10FFFF stays text');

// 2. The site.
const out = path.join(tmp, 'site');
const { id, pages, assets } = buildSite({ root: ROOT, out });
const read = (p) => fs.readFileSync(path.join(out, p), 'utf8');
const demos = pages.filter((p) => p.startsWith('demo/'));
check(demos.sort().join(' ') === 'demo/arena.html demo/chat.html demo/index.html demo/netlab.html demo/poll.html' &&
  !assets.some((a) => /crew/.test(a)), `the demos are index, chat, poll, arena and netlab, not crew (${demos.join(' ')})`);
check(['index.html', 'README.html', 'packages/js/README.html', 'packages/react/README.html', 'docs/THREAT-MODEL.html']
  .every((p) => pages.includes(p)) && pages.filter((p) => p.startsWith('docs/')).length ===
  fs.readdirSync(path.join(ROOT, 'docs')).filter((f) => f.endsWith('.md')).length, 'the landing page, the READMEs and every docs/*.md');
const policy = (p) => new RegExp(`<meta charset="utf-8">\\s*<meta http-equiv="Content-Security-Policy" content="` +
  `${(p.startsWith('demo/') ? DEMO_CSP : PAGE_CSP).replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}">\\s*<meta name="referrer" content="no-referrer">`);
check(pages.every((p) => policy(p).test(read(p))), 'every page has its policy and no-referrer right after <meta charset>');
check(pages.filter((p) => !p.startsWith('demo/')).every((p) => !/<script|\sstyle="|\son\w+="/i.test(read(p))),
  'pages but the demos have no script, inline style or handler (their policy allows none)');
const scripts = demos.flatMap((p) => [...read(p).matchAll(/<script\b[^>]*>/gi)].map((m) => [p, m[0]]));
check(scripts.length === demos.length && scripts.every(([, tag]) =>
  new RegExp(`^<script type="module" src="\\.\\./assets/${id}/demo/[\\w.-]+\\.js"></script>`).test(tag + '</script>')),
  `each demo's one script is a module under assets/${id}/demo/`);
// Every relative module specifier and new URL(…, import.meta.url) in the assets resolves inside assets/<id>/, and no
// static import is bare or absolute, so nothing is loaded from outside the versioned directory.
const js = assets.filter((a) => a.endsWith('.js'));
const unresolved = [];
for (const a of js) {
  const code = read(a);
  for (const m of code.matchAll(/(?:\bfrom\s*|\bimport\s*\(?\s*|new URL\(\s*)(['"])([^'"\n]+)\1/g)) {
    const target = path.posix.normalize(path.posix.join(path.posix.dirname(a), m[2]));
    if (!/^\.{1,2}\//.test(m[2]) || !target.startsWith(`assets/${id}/`) || !fs.existsSync(path.join(out, target))) {
      unresolved.push(`${a}: ${m[2]}`);
    }
  }
}
check(js.length >= 15 && unresolved.length === 0, `${js.length} modules: every import resolves inside assets/${id}/` +
  `${unresolved.map((u) => `\n     ${u}`).join('')}`);
check(/new URL\('\.\.\/pkg\/peerwire_ffi\.wasm', import\.meta\.url\)/.test(read(`assets/${id}/src/room.js`)) &&
  fs.readFileSync(path.join(out, `assets/${id}/pkg/peerwire_ffi.wasm`)).equals(fs.readFileSync(path.join(ROOT, 'packages/js/pkg/peerwire_ffi.wasm'))),
  'the wasm room.js loads by default is the built one, inside the same directory');
check(fs.readdirSync(path.join(out, 'assets')).join() === id, 'one assets directory');

// The id: the same for the same inputs, new for a changed wasm.
const again = buildSite({ root: ROOT, out: path.join(tmp, 'again') });
const wasm2 = path.join(tmp, 'other.wasm');
fs.writeFileSync(wasm2, Buffer.concat([fs.readFileSync(path.join(ROOT, 'packages/js/pkg/peerwire_ffi.wasm')), Buffer.from([0])]));
const other = buildSite({ root: ROOT, out: path.join(tmp, 'other'), wasm: wasm2 });
check(again.id === id && other.id !== id && /^[\da-f]{12}$/.test(other.id) &&
  read('demo/chat.html') !== fs.readFileSync(path.join(tmp, 'other/demo/chat.html'), 'utf8'),
  `the assets id is a hash of their contents (${id}, again ${again.id}, new wasm ${other.id}), and the pages name it`);

// Doc links.
const readme = read('README.html');
check(/href="docs\/PERFORMANCE\.html"/.test(readme) && /href="docs\/GODOT\.html"/.test(readme) &&
  read('docs/THREAT-MODEL.html').includes(`href="${REPO_URL}/blob/main/SECURITY.md"`) &&
  readme.includes(`href="${REPO_URL}/tree/main/crates/peerwire-core"`) && !/<img/.test(readme) &&
  !/shields\.io[^"]*"[^>]*src|src="[^"]*shields/.test(readme), 'doc links: .md to .html, other repo paths to GitHub, no badge images');
// A repo of symlinks to this one, with one more doc, docs/A.md, that breaks a link.
const fake = path.join(tmp, 'fake');
fs.mkdirSync(path.join(fake, 'docs'), { recursive: true });
for (const p of fs.readdirSync(ROOT)) {
  if (!['docs', 'target', 'tasks', '.git'].includes(p)) fs.symlinkSync(path.join(ROOT, p), path.join(fake, p));
}
for (const p of fs.readdirSync(path.join(ROOT, 'docs'))) fs.symlinkSync(path.join(ROOT, 'docs', p), path.join(fake, 'docs', p));
fs.writeFileSync(path.join(fake, 'docs/A.md'), '# A\n\n[gone](../crates/nothing-here)\n');
const broken = (() => {
  try {
    buildSite({ root: fake, out: path.join(tmp, 'fake-out') });
  } catch (e) {
    return e.message;
  }
})();
fs.writeFileSync(path.join(fake, 'docs/A.md'), '# A\n\n[here](#nope) and [x](javascript:alert(1))\n');
const refused = (() => {
  try {
    buildSite({ root: fake, out: path.join(tmp, 'fake-out2') });
  } catch (e) {
    return e.message;
  }
})();
check(/docs\/A\.md links to \.\.\/crates\/nothing-here/.test(broken ?? '') && /refused link/.test(refused ?? ''),
  `a doc link to no file, or with a javascript: scheme, fails the build (${broken}; ${refused})`);
fs.writeFileSync(path.join(fake, 'docs/A.md'), '# A\n\n[here](#nope)\n');
const anchor = (() => {
  try {
    buildSite({ root: fake, out: path.join(tmp, 'fake-out3') });
  } catch (e) {
    return e.message;
  }
})();
check(/docs\/A\.html: href="#nope": no id "nope"/.test(anchor ?? ''), 'a #fragment naming no heading fails the build');

// The checker on its own.
const site = path.join(tmp, 'check');
fs.mkdirSync(path.join(site, 'd'), { recursive: true });
fs.writeFileSync(path.join(site, 'd/a.html'), '<h2 id="x">x</h2><a href="/root.html"></a><a href="//cdn.example/x.js"></a>' +
  '<a href="b.html#y"></a><a href="#x"></a><a href="#z"></a><a href="javascript:x"></a><img src="../up.png">' +
  '<a href="b.html?q=1#x"></a><a href="https://example.com/"></a><a href="d"></a><a href=""></a><a href="/b.html"></a>');
fs.writeFileSync(path.join(site, 'd/b.html'), '<p id="x"></p>');
const problems = checkLinks(site, ['d/a.html', 'd/b.html']);
const want = ['"/root.html" isn\'t a relative link', '"//cdn.example/x.js" isn\'t a relative link',
  '"javascript:x" isn\'t a relative link', 'href="" isn\'t a relative link', '"/b.html" isn\'t a relative link',
  'no id "y"', 'no id "z"', '"../up.png" names no file', 'href="d" names no file'];
check(problems.length === want.length && want.every((s) => problems.some((p) => p.includes(s))),
  `checkLinks: root-relative (even one naming a file), protocol-relative, other schemes, missing files and ids, ` +
  `empty (${problems.length})`);
fs.writeFileSync(path.join(tmp, 'keep.txt'), 'x');
check(throws(() => buildSite({ root: ROOT, out: tmp })) && fs.existsSync(path.join(tmp, 'keep.txt')),
  "the build won't clear a directory that holds something but an earlier site");

// 3. The workflow: a third-party action, or one on a branch, could change what Pages serves without a commit here.
const uses = [...fs.readFileSync(path.join(ROOT, '.github/workflows/pages.yml'), 'utf8')
  .matchAll(/^[ \t]*(?:-[ \t]+)?uses:[ \t]*['"]?([^\s'"#]+)/gm)].map((m) => m[1]);
check(uses.length >= 4 && uses.every((u) => /^actions\/[\w.-]+@(?:v\d+|[\da-f]{40})$/.test(u)),
  `pages.yml uses GitHub's own actions/* only, each on a major version or a commit (${uses.join(' ')})`);

fs.rmSync(tmp, { recursive: true, force: true });
console.log(failed ? `${failed} check(s) failed` : 'all passed');
process.exit(failed ? 1 : 0);
