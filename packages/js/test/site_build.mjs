// The site's build (scripts/site/build.mjs), its markdown renderer and its highlighter, with no browser:
//  - the renderer escapes raw HTML, refuses link schemes but http(s) and mailto, makes GitHub's heading ids, and
//    renders the lists, tables, alerts and emphasis the docs use; a ```rust block hides the lines rustdoc hides;
//  - the highlighter only wraps the code in <span class="x">: the text of what it returns is the code, for every
//    fenced block in the docs and for random input;
//  - the sidebar is docs/README.md's: a doc missing from it or listed twice, or a stray line in a group, fails the
//    build; each doc page has it with the page marked, its group above the title, "On this page", prev and next in
//    sidebar order and an edit link; a page path is escaped in every link to it, and URL-encoded in the edit link
//    (NB67-I1);
//  - every page has its Content-Security-Policy first after <meta charset>, only the demos run scripts, and those
//    scripts, every module they import, the wasm and the images live under assets/<id>/;
//  - <id> is a hash of the assets: the same for the same inputs, new when the wasm changes;
//  - doc links point at the rendered page or at GitHub, images at assets/<id>/img/, and a broken link, an image
//    from outside docs/assets/ or an SVG there with a script or an outside reference fails the build;
//  - the first site's URLs refresh to the new pages;
//  - .github/workflows/pages.yml, which builds what Pages serves, runs no action but GitHub's own actions/* (NB16-1).
// node packages/js/test/site_build.mjs (needs packages/js/pkg/peerwire_ffi.wasm: packages/js/build.sh)

import { buildSite, checkLinks, checkSvg, readIndex, DEMO_CSP, PAGE_CSP, REDIRECTS, REPO_URL } from '../../../scripts/site/build.mjs';
import { render, textOf } from '../../../scripts/site/markdown.mjs';
import { highlight, LANGUAGES } from '../../../scripts/site/highlight.mjs';
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
const error = (fn) => {
  try {
    fn();
  } catch (e) {
    return e.message;
  }
  return '';
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
const heads = render('# Shared docs (NB-006b, NB-052) — NB6B-1…NB6B-4\n## A b\n## A b\n### `x_y` & **z**\n#### deep');
check(heads.ids.join(' ') === 'shared-docs-nb-006b-nb-052--nb6b-1nb6b-4 a-b a-b-1 x_y--z deep' &&
  heads.title === 'Shared docs (NB-006b, NB-052) — NB6B-1…NB6B-4', `GitHub's heading ids (${heads.ids.join(' ')})`);
check(JSON.stringify(heads.headings) === JSON.stringify([{ level: 2, id: 'a-b', text: 'A b' },
  { level: 2, id: 'a-b-1', text: 'A b' }, { level: 3, id: 'x_y--z', text: 'x_y & z' }]), 'headings: the h2s and h3s, as text');
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
check(render('```rust\nfn x\n```\n\n```\nfn y\n```', { highlight }).html === '<pre><code class="language-rust">' +
  '<span class="k">fn</span> x</code></pre>\n<pre><code>fn y</code></pre>', 'a fence with a language is highlighted, one without isn\'t');
// rustdoc's hidden lines (NB-070's doc tests): "# x" and "#" go, trimmed as Rust trims (U+0085 and U+00A0 are white
// space to it, U+FEFF isn't); "##" shows as "#"; attributes stay. Only in ```rust blocks: rustdoc compiles neither
// ```Rust nor ```rs (highlighted as Rust here) as Rust. rustdoc 1.99 renders these blocks the same.
const codeText = (md) => [...render(md, { highlight }).html.matchAll(/<code[^>]*>([^]*?)<\/code>/g)].map((m) => textOf(m[1]));
const sameList = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const RUST_IN = ['# use std::fmt;', '#', 'fn main() {', '    ## shown', '    #[derive(Debug)]', '    #![allow(x)]',
  '    # let hidden = 1;', '\t#', '    #text', '    let s = "a ## b";', '  ##! ## c', '\u0085# nel', '#\u0085', '#\u00a0 ',
  '\ufeff# bom', '}'];
const RUST_OUT = ['fn main() {', '    # shown', '    #[derive(Debug)]', '    #![allow(x)]', '    #text', '    let s = "a ## b";',
  '  #! ## c', '\ufeff# bom', '}'];
check(sameList(codeText(`\`\`\`rust,no_run\n${RUST_IN.join('\n')}\n\`\`\`\n\n\`\`\`rust ignore\n# a\nb\n\`\`\``), [RUST_OUT.join('\n'), 'b']) &&
  sameList(codeText('```sh\n# a\n#\n## b\n```\n\n```toml\n# a\n```\n\n```rs\n# a\n```\n\n```Rust\n# a\n```\n\n```text\n# a\n```\n\n' +
    '    # a'), ['# a\n#\n## b', '# a', '# a', '# a', '# a', '# a']) &&
  html('```rust\n# a\n<b>\n```') === '<pre><code class="language-rust">&lt;b&gt;</code></pre>',
  'a ```rust block hides "# " and "#" lines and shows "##" as "#"; other blocks keep them');

// 2. The highlighter.
const SPAN = /<span class="[csnkotfmvpga]">|<\/span>/g;
const onlySpans = (out) => !/<(?!\/?span\b)/.test(out) && out.replace(SPAN, '').indexOf('<') < 0 &&
  (out.match(/<span /g) ?? []).length === (out.match(/<\/span>/g) ?? []).length;
const same = (code, lang) => {
  const out = highlight(code, lang);
  return out !== null && onlySpans(out) && textOf(out) === code;
};
const hl = (code, lang) => highlight(code, lang);
check(LANGUAGES.sort().join(' ') === 'c gdscript html js json jsx rust sh toml ts', `languages: ${LANGUAGES.join(' ')}`);
check(hl('x', 'text') === null && hl('x', 'mermaid') === null && hl('x', 'rust,ignore') === 'x', 'an unknown language is null; "rust,ignore" is rust');
const tokens = [
  ['rust', '#[derive(Debug)]\nfn main() { let s = "a\\"b"; println!("{s}"); } // c', ['<span class="m">#[derive(Debug)]</span>',
    '<span class="k">fn</span>', '<span class="f">main</span>', '<span class="s">&quot;a\\&quot;b&quot;</span>', '<span class="m">println!</span>',
    '<span class="c">// c</span>']],
  ['rust', "fn f<'a>(x: &'a str) -> u8 { let c = 'x'; r#\"raw \"q\"\"#; 0x1F_u8 }", ['<span class="v">\'a</span>',
    '<span class="s">\'x\'</span>', '<span class="s">r#&quot;raw &quot;q&quot;&quot;#</span>', '<span class="n">0x1F_u8</span>', '<span class="t">u8</span>']],
  ['js', "import { joinRoom } from '@peerwire/js'; const n = 1.5e3; f(`${x}`, null); /* b */", ['<span class="k">import</span>',
    '<span class="s">\'@peerwire/js\'</span>', '<span class="n">1.5e3</span>', '<span class="f">f</span>', '<span class="s">`${x}`</span>',
    '<span class="o">null</span>', '<span class="c">/* b */</span>']],
  ['tsx', 'const x = a < b && f<T>(y);\nreturn (<RoomProvider appId="a">\n</RoomProvider>);', ['a &lt; b', 'f&lt;<span class="t">T</span>&gt;',
    '<span class="g">&lt;RoomProvider</span>', '<span class="a">appId</span>', '<span class="g">&lt;/RoomProvider</span>']],
  ['ts', 'interface A { readonly x: number }', ['<span class="k">interface</span>', '<span class="k">readonly</span>',
    '<span class="t">number</span>']],
  ['js', 'const string = 1;', ['<span class="k">const</span> string = ']],
  ['sh', 'PORT=1 python3 -m http.server # c\nfor f in *.md; do echo "$f" ${HOME} pw:x#y; done', ['<span class="v">PORT=</span>1',
    '<span class="f">python3</span>', '<span class="p">-m</span>', '<span class="c"># c</span>', '<span class="k">for</span> f <span class="k">in</span>',
    '<span class="f">echo</span>', '<span class="s">&quot;$f&quot;</span>', '<span class="v">${HOME}</span>', 'pw:x#y']],
  ['toml', '[dependencies]\npeerwire = { git = "https://x", features = ["nostr"] } # c\nn = 0x1F', ['<span class="t">[dependencies]</span>',
    '<span class="p">peerwire</span>', '<span class="p">features</span>', '<span class="s">&quot;nostr&quot;</span>', '<span class="c"># c</span>',
    '<span class="n">0x1F</span>']],
  ['json', '{ "name": "@peerwire/js", "n": -1.5, "ok": true }', ['<span class="p">&quot;name&quot;</span>',
    '<span class="s">&quot;@peerwire/js&quot;</span>', '<span class="n">-1.5</span>', '<span class="o">true</span>']],
  ['html', '<!doctype html><!-- c --><a href="x" hidden>T &amp; U</a>', ['<span class="m">&lt;!doctype html&gt;</span>',
    '<span class="c">&lt;!-- c --&gt;</span>', '<span class="g">&lt;a</span>', '<span class="a">href</span>', '<span class="s">&quot;x&quot;</span>',
    '<span class="n">&amp;amp;</span>']],
  ['gdscript', 'extends Node\n@rpc("any_peer")\nfunc _ready() -> void:\n\t$Label.text = &"n" # c', ['<span class="k">extends</span>',
    '<span class="m">@rpc</span>', '<span class="k">func</span>', '<span class="t">void</span>', '<span class="v">$Label</span>',
    '<span class="s">&amp;&quot;n&quot;</span>', '<span class="c"># c</span>']],
  ['c', '#include <stdint.h>\nint32_t pw_send(const uint8_t *b) { return PW_ERR_FULL; } // c', ['<span class="m">#include</span>',
    '<span class="s">&lt;stdint.h&gt;</span>', '<span class="t">int32_t</span>', '<span class="f">pw_send</span>', '<span class="k">const</span>',
    '<span class="o">PW_ERR_FULL</span>']],
];
const missed = tokens.flatMap(([lang, code, want]) => want.filter((w) => !hl(code, lang).includes(w)).map((w) => `${lang}: ${w}`));
check(missed.length === 0 && tokens.every(([lang, code]) => same(code, lang)), `each language's tokens${missed.map((m) => `\n     ${m}`).join('')}`);
check(LANGUAGES.every((l) => {
  const out = hl('</code></pre><script>alert(1)</script><img src=x onerror=alert(1)>"\'&', l);
  return onlySpans(out) && !/<script|<img|<\/code/.test(out);
}), 'markup in code comes out escaped, in every language');
// Every fenced block of every markdown file the site renders, and random input.
const fences = [];
const mdFiles = (dir) => fs.readdirSync(path.join(ROOT, dir), { withFileTypes: true }).flatMap((e) => (e.isDirectory()
  ? mdFiles(path.join(dir, e.name)) : e.name.endsWith('.md') ? [path.join(dir, e.name)] : []));
for (const file of [...mdFiles('docs'), 'CHANGELOG.md', 'CONTRIBUTING.md', 'SECURITY.md', 'README.md']) {
  for (const m of fs.readFileSync(path.join(ROOT, file), 'utf8').matchAll(/^```(\w+)[^\n]*\n([^]*?)^```/gm)) {
    if (highlight('', m[1]) !== null) fences.push([file, m[1], m[2].replace(/\n$/, '')]);
  }
}
const bad = fences.filter(([, lang, code]) => !same(code, lang));
check(fences.length >= 10 && bad.length === 0, `${fences.length} fenced blocks in the docs: the text of each, highlighted, is the code` +
  `${bad.map(([f, l]) => `\n     ${f} (${l})`).join('')}`);
let seed = 7;
const rand = (n) => (seed = (seed * 1103515245 + 12345) % 2147483648) % n;
const ALPHABET = ['a', 'Z', '_', '1', '0x', ' ', '\n', '\t', '"', "'", '`', '\\', '/', '*', '#', '<', '>', '&', '$', '{', '}',
  '(', ')', '[', ']', '!', '@', '=', ';', '|', '-', '.', 'fn', 'let', 'r#"', '"#', '<!--', '-->', '//', '/*', '*/', '"""', '${',
  'é', '\u2028', '\ud83d\ude00'];
let fuzzBad = '';
for (let k = 0; k < 400 && !fuzzBad; k++) {
  const code = Array.from({ length: 1 + rand(40) }, () => ALPHABET[rand(ALPHABET.length)]).join('');
  for (const l of LANGUAGES) if (!same(code, l)) fuzzBad = `${l}: ${JSON.stringify(code)}`;
}
check(!fuzzBad, `400 random inputs × ${LANGUAGES.length} languages: only spans, and the text is the input ${fuzzBad}`);

// 3. checkSvg, and the logo and favicon.
const svgs = {
  script: '<svg><script>alert(1)</script></svg>', handler: '<svg onload="alert(1)"/>', handler2: '<svg><rect ONclick = "x"/></svg>',
  foreign: '<svg><foreignObject/></svg>', href: '<svg><use href="https://x.example/a.svg#b"/></svg>',
  xlink: '<svg><image xlink:href="other.png"/></svg>', url: '<svg><rect fill="url(https://x.example/f)"/></svg>',
  import: '<svg><style>@import "x.css";</style></svg>', dtd: '<!DOCTYPE svg [<!ENTITY x "y">]><svg/>',
  js: '<svg><a><set attributeName="href" to="javascript:alert(1)"/></a></svg>', data: '<svg><animate values="data:x"/></svg>',
  // NB67-1: XML, where the namespace makes the script, and spellings a regex can't see through.
  prefixed: '<svg xmlns:s="http://www.w3.org/2000/svg"><s:script>alert(1)</s:script></svg>',
  xhtml: '<s:svg xmlns:s="http://www.w3.org/2000/svg" xmlns:h="http://www.w3.org/1999/xhtml"><s:foreignObject>' +
    '<h:script>alert(1)</h:script></s:foreignObject></s:svg>',
  charref: '<svg><a><set attributeName="href" to="&#106;avascript:alert(1)"/></a></svg>',
  pi: '<?xml-stylesheet type="text/xsl" href="#x"?><svg/>',
  // XHTML in the default namespace needs no prefix: a srcdoc frame's script runs on the origin. Then each rule alone
  // (an element outside the list, a namespace that isn't SVG's), and a tab the URL parser drops (NB67-I3).
  srcdoc: '<svg xmlns="http://www.w3.org/2000/svg"><g xmlns="http://www.w3.org/1999/xhtml">' +
    '<iframe srcdoc="&lt;script&gt;parent.ran=1&lt;/script&gt;"/></g></svg>',
  htmlroot: '<html xmlns="http://www.w3.org/1999/xhtml">' +
    '<iframe srcdoc="&lt;script&gt;parent.ran=1&lt;/script&gt;"/></html>',
  element: '<svg xmlns="http://www.w3.org/2000/svg"><iframe/></svg>',
  namespace: '<svg xmlns="http://www.w3.org/2000/svg"><g xmlns="http://www.w3.org/1999/xhtml"/></svg>',
  tab: '<svg><a><set attributeName="href" to="java\tscript:alert(1)"/></a></svg>',
  // U+1680 and U+FEFF are XML name characters that \s matches: each prefix reads as <g to the other rules. Then
  // outside fetches with no url( or scheme (NB67-I4): a CSS escape for //, and a scheme-less //host.
  ogham: '<svg xmlns:g\u1680="http://www.w3.org/2000/svg"><g\u1680:script>alert(1)</g\u1680:script></svg>',
  bom: '<svg xmlns:g\ufeff="http://www.w3.org/2000/svg"><g\ufeff:script>alert(1)</g\ufeff:script></svg>',
  escape: '<svg style="background-image:image-set(\'\\2f \\2f x.example/a.png\' 1x)"/>',
  netpath: '<svg style="background-image:image-set(\'//x.example/a.png\' 1x)"/>',
};
const passed = Object.entries(svgs).filter(([, s]) => checkSvg(s).length === 0).map(([k]) => k);
check(passed.length === 0 && checkSvg('<?xml version="1.0" encoding="UTF-8"?>' +
  '<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">' +
  '<defs><linearGradient id="g"/></defs><rect fill="url(#g)"/><use href="#g"/></svg>').length === 0,
  `checkSvg refuses a script, handlers, foreignObject, outside references, url(), @import, a DTD, javascript: and data:, ` +
  `prefixed elements, character references, processing instructions, elements outside its list, other namespaces, ` +
  `characters outside printable ASCII, backslashes and //host, and allows #ids, SVG's and xlink's xmlns and the XML ` +
  `declaration` +
  `${passed.length ? ` (passed: ${passed.join(' ')})` : ''}`);
const logo = fs.readFileSync(path.join(ROOT, 'docs/assets/logo.svg'), 'utf8');
check(['logo.svg', 'favicon.svg'].every((f) => checkSvg(fs.readFileSync(path.join(ROOT, 'docs/assets', f), 'utf8')).length === 0) &&
  /fill="#5b6cf0"/.test(logo) && !/prefers-color-scheme|<text/.test(logo),
  'the logo and favicon pass; the wordmark is outlined, one colour that reads on GitHub light and dark, no media query');

// 4. The sidebar, on small repos of their own.
const mini = (readme, files) => {
  const dir = fs.mkdtempSync(path.join(tmp, 'mini-'));
  for (const [f, text] of Object.entries({ 'docs/README.md': readme, ...files })) {
    fs.mkdirSync(path.dirname(path.join(dir, f)), { recursive: true });
    fs.writeFileSync(path.join(dir, f), text);
  }
  return dir;
};
const okIndex = readIndex(mini('# Docs\n\nIntro.\n\n## One\n\n- [A `b`](a/b.md): first,\n  then more.\n- [C](../C.md): outside\n\n## Two\n\n' +
  '- [D](d.md)\n', { 'docs/a/b.md': '# B', 'C.md': '# C', 'docs/d.md': '# D' }));
check(okIndex.intro.trim() === '# Docs\n\nIntro.' && okIndex.groups.map((g) => `${g.name}: ${g.items.map((i) =>
  `${i.title}=${i.source}|${i.blurb.replace(/\n/g, ' ')}`).join(', ')}`).join('; ') ===
  'One: A b=docs/a/b.md|first, then more., C=C.md|outside; Two: D=docs/d.md|', 'readIndex: groups, titles, paths from the repo root, blurbs');
const indexErrors = [
  [mini('## G\n- [A](a.md)\n', { 'docs/a.md': '', 'docs/sub/z.md': '' }), /docs\/sub\/z\.md isn't listed/],
  [mini('## G\n- [A](a.md)\n## H\n- [A again](a.md)\n', { 'docs/a.md': '' }), /docs\/a\.md is listed twice/],
  [mini('## G\n- [Self](README.md)\n'), /docs\/README\.md is listed twice/],
  [mini('## G\n- [A](gone.md)\n'), /gone\.md is no \.md file/],
  [mini('## G\n- [A](../../x.md)\n'), /is no \.md file/],
  [mini('## G\n- [A](a.txt)\n', { 'docs/a.txt': '' }), /a\.txt is no \.md file/],
  [mini('## G\n- [A](a.md)\nSome prose.\n', { 'docs/a.md': '' }), /a group holds only/],
  [mini('## G\n- [A](a.md)\n\n  orphan\n', { 'docs/a.md': '' }), /a group holds only/],
];
const indexMisses = indexErrors.map(([dir, re]) => [error(() => readIndex(dir)), re]).filter(([m, re]) => !re.test(m));
check(indexMisses.length === 0, `readIndex fails on a doc not listed, one listed twice, a link to no .md, and a stray line` +
  `${indexMisses.map(([m, re]) => `\n     ${re}: got ${JSON.stringify(m)}`).join('')}`);

// 5. The site.
const out = path.join(tmp, 'site');
const built = buildSite({ root: ROOT, out });
const { id, pages, assets, redirects, docs, sizes } = built;
const read = (p) => fs.readFileSync(path.join(out, p), 'utf8');
const demos = pages.filter((p) => p.startsWith('demo/'));
check(demos.sort().join(' ') === 'demo/arena.html demo/chat.html demo/index.html demo/netlab.html demo/poll.html' &&
  !assets.some((a) => /crew/.test(a)), `the demos are index, chat, poll, arena and netlab, not crew (${demos.join(' ')})`);
const index = readIndex(ROOT);
const listed = index.groups.flatMap((g) => g.items);
const want = ['docs/index.html', ...listed.map((i) => i.source.replace(/\.md$/, '.html'))];
check(JSON.stringify(docs) === JSON.stringify(want) && want.every((p) => pages.includes(p)) && pages.includes('index.html') &&
  ['CHANGELOG.html', 'CONTRIBUTING.html', 'SECURITY.html'].every((p) => docs.includes(p)),
  `the docs: docs/index.html, then every page of docs/README.md at its repo path, CHANGELOG, CONTRIBUTING and SECURITY too (${docs.length})`);
check(pages.length === demos.length + docs.length + 1 + Object.keys(REDIRECTS).length &&
  !pages.some((p) => /^packages\/.*\.html$|^README\.html$/.test(p) && !redirects.includes(p)),
  'nothing else: no README rendered (README.html and the packages\' are redirects)');
const policy = (p) => new RegExp(`<meta charset="utf-8">\\s*<meta http-equiv="Content-Security-Policy" content="` +
  `${(p.startsWith('demo/') ? DEMO_CSP : PAGE_CSP).replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}">\\s*<meta name="referrer" content="no-referrer">`);
check(pages.every((p) => policy(p).test(read(p))), 'every page has its policy and no-referrer right after <meta charset>');
check(pages.filter((p) => !p.startsWith('demo/')).every((p) => !/<script|\sstyle="|\son\w+="|<style/i.test(read(p))),
  'pages but the demos have no script, inline style, <style> or handler (their policy allows none)');
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
const images = fs.readdirSync(path.join(ROOT, 'docs/assets'));
check(images.every((f) => fs.readFileSync(path.join(out, `assets/${id}/img/${f}`)).equals(fs.readFileSync(path.join(ROOT, 'docs/assets', f)))) &&
  pages.filter((p) => !p.startsWith('demo/')).every((p) => read(p).includes(`assets/${id}/img/favicon.svg" type="image/svg+xml">`) ||
    new RegExp(`<link rel="icon" type="image/svg\\+xml" href="(?:\\.\\./)*assets/${id}/img/favicon\\.svg">`).test(read(p))),
  `docs/assets/ is under assets/${id}/img/ (${images.join(' ')}), and every page's icon is the favicon there`);

// The id: the same for the same inputs, new for a changed wasm.
const again = buildSite({ root: ROOT, out: path.join(tmp, 'again') });
const wasm2 = path.join(tmp, 'other.wasm');
fs.writeFileSync(wasm2, Buffer.concat([fs.readFileSync(path.join(ROOT, 'packages/js/pkg/peerwire_ffi.wasm')), Buffer.from([0])]));
const other = buildSite({ root: ROOT, out: path.join(tmp, 'other'), wasm: wasm2 });
check(again.id === id && other.id !== id && /^[\da-f]{12}$/.test(other.id) &&
  read('demo/chat.html') !== fs.readFileSync(path.join(tmp, 'other/demo/chat.html'), 'utf8'),
  `the assets id is a hash of their contents (${id}, again ${again.id}, new wasm ${other.id}), and the pages name it`);

// The doc layout, on every doc page.
const layoutBad = [];
for (const [k, page] of docs.entries()) {
  const h = read(page);
  const item = k === 0 ? { title: 'Overview', group: 'Docs', source: 'docs/README.md' } : listed[k - 1];
  const side = /<nav class="side" aria-label="Docs">([^]*?)<\/nav>/.exec(h)?.[1] ?? '';
  const names = [...side.matchAll(/<a href="[^"]*"(?: aria-current="page")?>([^<]*)<\/a>/g)].map((m) => textOf(m[1]));
  const current = [...h.matchAll(/<a href="([^"]*)" aria-current="page">([^<]*)<\/a>/g)];
  const groups = [...side.matchAll(/<p class="side-group">([^<]*)<\/p>/g)].map((m) => textOf(m[1]));
  const toc = /<aside class="toc"[^>]*>([^]*?)<\/aside>/.exec(h)?.[1] ?? '';
  const tocIds = [...toc.matchAll(/href="#([^"]+)"/g)].map((m) => m[1]);
  const article = /<article class="doc[^"]*">([^]*?)<\/article>/.exec(h)?.[1] ?? '';
  const heads2 = [...article.matchAll(/<h([23]) id="([^"]+)"/g)].map((m) => m[2]);
  const prev = /<a class="prev" href="([^"]*)">/.exec(h)?.[1];
  const next = /<a class="next" href="([^"]*)">/.exec(h)?.[1];
  const rel = (to) => path.posix.relative(path.posix.dirname(page), to);
  const problems = [
    JSON.stringify(names) !== JSON.stringify(['Overview', ...listed.map((i) => i.title)]) && 'sidebar entries',
    JSON.stringify(groups) !== JSON.stringify(index.groups.map((g) => g.name)) && 'sidebar groups',
    (current.length !== 2 || current.some((m) => m[1] !== path.posix.basename(page) || textOf(m[2]) !== item.title)) && 'current page',
    !article.startsWith(`\n<p class="eyebrow">${item.group}</p>`) && 'group above the title',
    JSON.stringify(tocIds) !== JSON.stringify(heads2) && 'On this page',
    prev !== (k ? rel(docs[k - 1]) : undefined) && 'prev', next !== (docs[k + 1] ? rel(docs[k + 1]) : undefined) && 'next',
    !h.includes(`<a href="${REPO_URL}/edit/main/${item.source}">Edit this page on GitHub</a>`) && 'edit link',
    !/<details class="mnav">\s*<summary>/.test(h) && 'mobile menu',
  ].filter(Boolean);
  if (problems.length) layoutBad.push(`${page}: ${problems.join(', ')}`);
}
check(layoutBad.length === 0, `${docs.length} doc pages: the sidebar in order with the page marked (sidebar and mobile menu), ` +
  `the group above the title, "On this page" = its h2s and h3s, prev and next, the edit link${layoutBad.map((b) => `\n     ${b}`).join('')}`);
const overview = read('docs/index.html');
check(index.groups.every((g) => overview.includes(`>${g.name}</h2>\n<ul class="cards">`)) &&
  listed.every((i) => overview.includes(`<li><a href="${path.posix.relative('docs', i.source.replace(/\.md$/, '.html'))}">`)),
  'the overview: docs/README.md\'s text, then a card per page under each group');

// The redirects.
const moved = Object.entries(REDIRECTS).filter(([from, to]) => {
  const h = read(from);
  const href = path.posix.relative(path.posix.dirname(from), to);
  return !(h.includes(`<meta http-equiv="refresh" content="0; url=${href}">`) && h.includes(`<a href="${href}">`) &&
    h.includes('<meta name="robots" content="noindex">') && pages.includes(to));
});
check(Object.keys(REDIRECTS).length === 12 && JSON.stringify(redirects) === JSON.stringify(Object.keys(REDIRECTS)) && moved.length === 0,
  `the first site's 12 URLs refresh to their new pages, with a link and noindex${moved.map(([f]) => `\n     ${f}`).join('')}`);

// The landing page.
const landing = read('index.html');
const wasmKb = Math.round(fs.statSync(path.join(ROOT, 'packages/js/pkg/peerwire_ffi.wasm')).size / 1024);
const wasmShown = `~${Math.round(wasmKb / 10) * 10} KB`;
check(['js', 'react', 'rust', 'godot'].every((t) => landing.includes(`<input type="radio" name="code" id="tab-${t}"`) &&
  landing.includes(`<label for="tab-${t}">`) && new RegExp(`<pre class="tab tab-${t}"><code class="language-\\w+"><span class=`).test(landing)) &&
  (landing.match(/ checked>/g) ?? []).length === 1 && landing.includes('<code>npm install @peerwire/js</code>') &&
  landing.includes(`<strong>${wasmShown}</strong>`) && landing.includes(`as a ${wasmShown} WebAssembly module`) &&
  sizes.wasm === wasmKb && sizes.gzip < wasmKb && landing.includes('<code>room.doc()</code>') &&
  landing.includes('id="demos"') && (landing.match(/<li class="feature">/g) ?? []).length === 6 &&
  (landing.match(/<li class="demo">/g) ?? []).length === 5,
  `the landing page: four code tabs, highlighted (one checked), npm install, 6 features, the wasm's measured size ` +
  `(${wasmKb} KB, shown ${wasmShown}), 5 demos`);

// Docs on a repo of symlinks to this one, with a docs/README.md of its own that lists one more page, docs/A.md.
const fake = path.join(tmp, 'fake');
fs.mkdirSync(path.join(fake, 'docs/assets'), { recursive: true });
for (const p of fs.readdirSync(ROOT)) {
  if (!['docs', 'target', 'tasks', '.git'].includes(p)) fs.symlinkSync(path.join(ROOT, p), path.join(fake, p));
}
for (const p of fs.readdirSync(path.join(ROOT, 'docs'))) {
  if (!['README.md', 'assets'].includes(p)) fs.symlinkSync(path.join(ROOT, 'docs', p), path.join(fake, 'docs', p));
}
for (const p of images) fs.symlinkSync(path.join(ROOT, 'docs/assets', p), path.join(fake, 'docs/assets', p));
fs.writeFileSync(path.join(fake, 'docs/README.md'), `${fs.readFileSync(path.join(ROOT, 'docs/README.md'), 'utf8')}\n## Test\n\n` +
  '- [A page](A.md): a page of the test.\n');
const fakeBuild = (md, name) => {
  fs.writeFileSync(path.join(fake, 'docs/A.md'), md);
  return error(() => buildSite({ root: fake, out: path.join(tmp, name) }));
};
const fakeOk = fakeBuild('# A\n\nSee [rooms](guides/rooms.md), [the group](README.md#test), [core](../crates/peerwire-core), [security](../SECURITY.md), ' +
  '[chat](../packages/js/demo/chat.html), [js](../packages/js/README.md) and [top](#a).\n\n![The logo](assets/logo.svg)\n\n' +
  '## Two\n\n```rust,no_run\n# fn main() {\nlet x = 1;\n# }\n```\n\n### Three\n', 'fake-ok');
const a = fs.existsSync(path.join(tmp, 'fake-ok/docs/A.html')) ? fs.readFileSync(path.join(tmp, 'fake-ok/docs/A.html'), 'utf8') : '';
check(!fakeOk && a.includes('href="guides/rooms.html"') && a.includes('href="index.html#test"') && a.includes(`href="${REPO_URL}/tree/main/crates/peerwire-core"`) &&
  a.includes('href="../SECURITY.html"') && a.includes('href="../demo/chat.html"') &&
  a.includes(`href="${REPO_URL}/blob/main/packages/js/README.md"`) && a.includes('href="#a"') &&
  new RegExp(`<img src="\\.\\./assets/[\\da-f]{12}/img/logo\\.svg" alt="The logo" loading="lazy">`).test(a) &&
  a.includes('<code class="language-rust,no_run"><span class="k">let</span> x = <span class="n">1</span>;</code>') &&
  /<li class="l2"><a href="#two">Two<\/a><\/li>\n<li class="l3"><a href="#three">Three<\/a><\/li>/.test(a) &&
  a.includes('<p class="eyebrow">Test</p>') && a.includes('<meta name="description" content="A page of the test.">') &&
  /<a class="prev" href="\.\.\/SECURITY\.html">/.test(a) && a.includes('<span></span></nav>'),
  `a doc's links (to pages, to GitHub, to a demo), its image, code, headings, group, description and place last ${fakeOk}`);
const fails = [
  ['# A\n\n[gone](../crates/nothing-here)\n', /docs\/A\.md links to \.\.\/crates\/nothing-here/],
  ['# A\n\n[x](javascript:alert(1))\n', /refused link/],
  ['# A\n\n[here](#nope)\n', /docs\/A\.html: href="#nope": no id "nope"/],
  ['# A\n\n![x](../README.md)\n', /docs\/A\.md shows \.\.\/README\.md: an image from the repo must be a file under docs\/assets\//],
  ['# A\n\n![x](assets/none.svg)\n', /must be a file under docs\/assets\//],
];
const failMisses = fails.map(([md, re], k) => [fakeBuild(md, `fake-bad${k}`), re]).filter(([m, re]) => !re.test(m));
check(failMisses.length === 0, `a doc link to no file, a javascript: link, a #fragment naming no heading, or an image from outside ` +
  `docs/assets/ fails the build${failMisses.map(([m, re]) => `\n     ${re}: got ${JSON.stringify(m)}`).join('')}`);
fs.writeFileSync(path.join(fake, 'docs/assets/evil.svg'), '<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"/>');
const evil = fakeBuild('# A\n', 'fake-evil');
fs.rmSync(path.join(fake, 'docs/assets/evil.svg'));
fs.writeFileSync(path.join(fake, 'docs/assets/notes.txt'), 'x');
const txt = fakeBuild('# A\n', 'fake-txt');
fs.rmSync(path.join(fake, 'docs/assets/notes.txt'));
fs.writeFileSync(path.join(fake, 'docs/B.md'), '# B\n');
const unlisted = fakeBuild('# A\n', 'fake-unlisted');
fs.rmSync(path.join(fake, 'docs/B.md'));
check(/docs\/assets\/evil\.svg holds an on\* handler/.test(evil) && /notes\.txt: docs\/assets\/ holds images only/.test(txt) &&
  /docs\/B\.md isn't listed/.test(unlisted), `an SVG with a handler, a non-image in docs/assets/, or a doc not in the sidebar fails the build`);

// A doc whose path needs escaping and encoding: its edit link is the path through encodeURI, then escaped, and every
// link to it (the sidebar, the pager, the overview's card) is escaped (NB67-I1).
const odd = 'q" &amp;.md';
const readme = fs.readFileSync(path.join(fake, 'docs/README.md'), 'utf8');
fs.writeFileSync(path.join(fake, 'docs', odd), '# Odd\n');
fs.writeFileSync(path.join(fake, 'docs/README.md'), `${readme}- [Odd](q%22%20&amp;.md): a page with an odd name.\n`);
const oddBuild = fakeBuild('# A\n', 'fake-odd');
fs.writeFileSync(path.join(fake, 'docs/README.md'), readme);
fs.rmSync(path.join(fake, 'docs', odd));
const oddRead = (p) => (fs.existsSync(path.join(tmp, 'fake-odd', p)) ? fs.readFileSync(path.join(tmp, 'fake-odd', p), 'utf8') : '');
const oddLink = 'href="q&quot; &amp;amp;.html"';
check(!oddBuild && oddRead('docs/q" &amp;.html').includes(`<a href="${REPO_URL}/edit/main/docs/q%22%20&amp;amp;.md">Edit this page`) &&
  oddRead('docs/q" &amp;.html').includes(`<a ${oddLink} aria-current="page">Odd</a>`) &&
  oddRead('docs/A.html').includes(`<a class="next" ${oddLink}>`) && oddRead('docs/index.html').includes(`<li><a ${oddLink}>Odd</a>`),
  `a doc named ${odd}: its edit link names docs/q%22%20&amp;.md, and the sidebar, pager and overview link to it escaped ${oddBuild}`);

// The checker on its own.
const site = path.join(tmp, 'check');
fs.mkdirSync(path.join(site, 'd'), { recursive: true });
fs.writeFileSync(path.join(site, 'd/a.html'), '<h2 id="x">x</h2><a href="/root.html"></a><a href="//cdn.example/x.js"></a>' +
  '<a href="b.html#y"></a><a href="#x"></a><a href="#z"></a><a href="javascript:x"></a><img src="../up.png">' +
  '<a href="b.html?q=1#x"></a><a href="https://example.com/"></a><a href="d"></a><a href=""></a><a href="/b.html"></a>');
fs.writeFileSync(path.join(site, 'd/b.html'), '<p id="x"></p>');
fs.writeFileSync(path.join(site, 'd/c.html'), '<meta http-equiv="refresh" content="0; url=gone.html">');
fs.writeFileSync(path.join(site, 'd/e.html'), '<meta http-equiv="refresh" content="0; url=b.html#x">');
const problems = checkLinks(site, ['d/a.html', 'd/b.html', 'd/c.html', 'd/e.html']);
const wantProblems = ['"/root.html" isn\'t a relative link', '"//cdn.example/x.js" isn\'t a relative link',
  '"javascript:x" isn\'t a relative link', 'href="" isn\'t a relative link', '"/b.html" isn\'t a relative link',
  'no id "y"', 'no id "z"', '"../up.png" names no file', 'href="d" names no file', 'd/c.html: refresh="gone.html" names no file'];
check(problems.length === wantProblems.length && wantProblems.every((s) => problems.some((p) => p.includes(s))),
  `checkLinks: root-relative (even one naming a file), protocol-relative, other schemes, missing files and ids, ` +
  `empty, a refresh to no file (${problems.length})`);
fs.writeFileSync(path.join(tmp, 'keep.txt'), 'x');
check(throws(() => buildSite({ root: ROOT, out: tmp })) && fs.existsSync(path.join(tmp, 'keep.txt')),
  "the build won't clear a directory that holds something but an earlier site");

// 6. The workflow: a third-party action, or one on a branch, could change what Pages serves without a commit here.
const uses = [...fs.readFileSync(path.join(ROOT, '.github/workflows/pages.yml'), 'utf8')
  .matchAll(/^[ \t]*(?:-[ \t]+)?uses:[ \t]*['"]?([^\s'"#]+)/gm)].map((m) => m[1]);
check(uses.length >= 4 && uses.every((u) => /^actions\/[\w.-]+@(?:v\d+|[\da-f]{40})$/.test(u)),
  `pages.yml uses GitHub's own actions/* only, each on a major version or a commit (${uses.join(' ')})`);

fs.rmSync(tmp, { recursive: true, force: true });
console.log(failed ? `${failed} check(s) failed` : 'all passed');
process.exit(failed ? 1 : 0);
