// Markdown to HTML for the site (scripts/site/build.mjs): the subset this repo's docs use, with no dependencies.
//
// Blocks: ATX headings (with GitHub's anchor ids), paragraphs, fenced and indented code, pipe tables, bullet and
// ordered lists (nested, tight or loose), block quotes with GitHub's alerts ("> [!NOTE]"), thematic breaks, and the
// README's <div align="center">. Inline: code spans, links, images, autolinks, **strong**, *em* and _em_,
// backslash escapes, entities and <br>.
//
// Raw HTML is never passed through: any tag but <br> shows as text. A link or image whose scheme isn't http(s) or
// mailto throws. Otherwise the caller's link(href) and image(src, alt) decide what each becomes.
//
//   render(markdown, { link, image }) -> { html, title, ids }   title: the first heading's text; ids: every id made

const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' };
export const escapeHtml = (s) => s.replace(/[&<>"]/g, (c) => ESC[c]);

// The text of rendered HTML: tags dropped, entities decoded.
const NAMED = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: '\u00a0' };
export const textOf = (html) => html.replace(/<[^>]*>/g, '').replace(/&(#x[\da-f]+|#\d+|[a-z]+);/gi, (e, n) => {
  if (n[0] !== '#') return NAMED[n] ?? e;
  const code = n[1] === 'x' || n[1] === 'X' ? parseInt(n.slice(2), 16) : +n.slice(1);
  return code <= 0x10ffff ? String.fromCodePoint(code) : e;
});

// GitHub's heading ids (github-slugger): lower case, then everything but letters, marks, numbers, connector
// punctuation, spaces and hyphens dropped, and spaces made hyphens; a repeat gets -1, -2, ...
export function slugger() {
  const seen = new Map();
  return (text) => {
    const base = text.toLowerCase().replace(/[^\p{L}\p{M}\p{N}\p{Pc} -]/gu, '').replace(/ /g, '-');
    let slug = base;
    while (seen.has(slug)) {
      seen.set(base, seen.get(base) + 1);
      slug = `${base}-${seen.get(base)}`;
    }
    seen.set(slug, 0);
    return slug;
  };
}

// A link or image destination: relative, a fragment, http(s) or mailto. Anything else (javascript:, data:, ...)
// throws, so a doc can't put one on the site.
export function safeUrl(dest) {
  const scheme = /^([a-z][a-z\d+.-]*):/i.exec(dest)?.[1].toLowerCase();
  if (/[\x00-\x20\x7f]/.test(dest) || (scheme && !['http', 'https', 'mailto'].includes(scheme))) {
    throw new Error(`refused link: ${JSON.stringify(dest)}`);
  }
  return dest;
}

const FENCE = /^( {0,3})(`{3,}|~{3,})[ \t]*([^\s`]*)[^`]*$/;
const FENCE_END = /^ {0,3}(`{3,}|~{3,})[ \t]*$/;
const ATX = /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$/;
const HR = /^ {0,3}(?:(?:\*[ \t]*){3,}|(?:-[ \t]*){3,}|(?:_[ \t]*){3,})$/;
const QUOTE = /^ {0,3}> ?/;
const ITEM = /^( {0,3})([-*+]|\d{1,9}[.)])(?=[ \t]|$)/;
const DELIM = /^ *\|? *:?-+:? *(\| *:?-+:? *)*\|? *$/;
const DIV = /^<(\/?)div(?: align="center")?>[ \t]*$/;
const ALERT = /^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\][ \t]*(?:\n|$)/;

const blank = (l) => /^[ \t]*$/.test(l);
const indent = (l) => /^ */.exec(l)[0].length;

// A table row's cells: outer pipes dropped, split on pipes, "\|" is a pipe inside a cell (code spans too, as GFM).
function cells(line) {
  let s = line.trim();
  if (s.startsWith('|')) s = s.slice(1);
  if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1);
  const out = [];
  let cur = '';
  for (let k = 0; k < s.length; k++) {
    if (s[k] === '\\' && s[k + 1] === '|') {
      cur += '|';
      k++;
    } else if (s[k] === '|') {
      out.push(cur.trim());
      cur = '';
    } else {
      cur += s[k];
    }
  }
  out.push(cur.trim());
  return out;
}

const isTable = (lines, i) => i + 1 < lines.length && lines[i].includes('|') && lines[i + 1].includes('|') &&
  DELIM.test(lines[i + 1]) && cells(lines[i]).length === cells(lines[i + 1]).length;

// Does line i start a block that ends a paragraph? An ordered list does only from 1, and an empty item never.
function interrupts(lines, i) {
  const l = lines[i];
  if (FENCE.test(l) || ATX.test(l) || HR.test(l) || QUOTE.test(l) || DIV.test(l)) return true;
  const m = ITEM.exec(l);
  if (m && !blank(l.slice(m[0].length)) && (!/\d/.test(m[2]) || parseInt(m[2], 10) === 1)) return true;
  return isTable(lines, i);
}

const sameList = (m, ordered, kind) => m && (ordered ? /\d/.test(m[2]) && m[2].slice(-1) === kind : m[2] === kind);

// Lines -> blocks. `gaps` says whether a blank line stood between two of them (a list item with gaps is loose).
function blocks(lines) {
  const out = [];
  out.gaps = false;
  let gap = false;
  const push = (b) => {
    if (gap && out.length) out.gaps = true;
    gap = false;
    out.push(b);
  };
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    let m;
    if (blank(line)) {
      gap = true;
      i++;
    } else if ((m = FENCE.exec(line))) {
      const [, ind, fence, lang] = m;
      const body = [];
      for (i++; i < lines.length; i++) {
        const end = FENCE_END.exec(lines[i]);
        if (end && end[1][0] === fence[0] && end[1].length >= fence.length) {
          i++;
          break;
        }
        body.push(lines[i].slice(Math.min(ind.length, indent(lines[i]))));
      }
      push({ t: 'code', lang, text: body.join('\n') });
    } else if ((m = ATX.exec(line))) {
      push({ t: 'h', level: m[1].length, text: m[2] ?? '' });
      i++;
    } else if (HR.test(line)) {
      push({ t: 'hr' });
      i++;
    } else if ((m = DIV.exec(line))) {
      push({ t: 'div', close: m[1] === '/' });
      i++;
    } else if (QUOTE.test(line)) {
      const body = [];
      while (i < lines.length && QUOTE.test(lines[i])) body.push(lines[i++].replace(QUOTE, ''));
      push({ t: 'quote', children: blocks(body) });
    } else if (ITEM.test(line)) {
      const [list, next] = listAt(lines, i);
      push(list);
      i = next;
    } else if (isTable(lines, i)) {
      const head = cells(line);
      const align = cells(lines[i + 1]).map((c) =>
        c.startsWith(':') && c.endsWith(':') ? 'c' : c.endsWith(':') ? 'r' : c.startsWith(':') ? 'l' : '');
      const rows = [];
      for (i += 2; i < lines.length && !blank(lines[i]) && lines[i].includes('|'); i++) {
        const row = cells(lines[i]).slice(0, head.length);
        while (row.length < head.length) row.push('');
        rows.push(row);
      }
      push({ t: 'table', head, align, rows });
    } else if (indent(line) >= 4) {
      const body = [];
      while (i < lines.length && (blank(lines[i]) || indent(lines[i]) >= 4)) body.push(lines[i++].slice(4));
      while (blank(body.at(-1))) body.pop();
      push({ t: 'code', lang: '', text: body.join('\n') });
    } else {
      const para = [line.trimStart()];
      for (i++; i < lines.length && !blank(lines[i]) && !interrupts(lines, i); i++) para.push(lines[i].trimStart());
      push({ t: 'p', text: para.join('\n') });
    }
  }
  return out;
}

// The list starting at line i: its items' lines, de-indented to their content column, parsed as blocks.
function listAt(lines, i) {
  const first = ITEM.exec(lines[i]);
  const ordered = /\d/.test(first[2]);
  const kind = ordered ? first[2].slice(-1) : first[2];
  const list = { t: 'list', ordered, start: ordered ? parseInt(first[2], 10) : 1, loose: false, items: [] };
  for (;;) {
    const m = ITEM.exec(lines[i]);
    const rest = lines[i].slice(m[0].length);
    const spaces = indent(rest);
    const width = m[0].length + (blank(rest) || spaces > 4 ? 1 : spaces);
    const body = [blank(rest) ? '' : lines[i].slice(width)];
    for (i++; i < lines.length; ) {
      const l = lines[i];
      if (blank(l)) {
        let j = i;
        while (j < lines.length && blank(lines[j])) j++;
        if (j === lines.length || indent(lines[j]) < width) break;
        for (; i < j; i++) body.push('');
      } else if (indent(l) >= width) {
        body.push(l.slice(width));
        i++;
      } else if (!ITEM.test(l) && !interrupts(lines, i) && !blank(body.at(-1))) {
        body.push(l.trimStart()); // a lazy continuation line of the item's paragraph
        i++;
      } else {
        break;
      }
    }
    const children = blocks(body);
    if (children.gaps) list.loose = true;
    list.items.push(children);
    let j = i;
    while (j < lines.length && blank(lines[j])) j++;
    if (j === lines.length || !sameList(ITEM.exec(lines[j]), ordered, kind)) return [list, i];
    if (j > i) list.loose = true;
    i = j;
  }
}

function findRun(text, from, n) {
  for (let j = text.indexOf('`', from); j >= 0; ) {
    let k = j;
    while (text[k] === '`') k++;
    if (k - j === n) return j;
    j = text.indexOf('`', k);
  }
  return -1;
}

// [label](dest "title") starting at text[i] === '[': { label, dest, end } or null.
function linkAt(text, i) {
  let depth = 0;
  let k = i;
  for (; k < text.length; k++) {
    const c = text[k];
    if (c === '\\') {
      k++;
    } else if (c === '`') {
      let r = k;
      while (text[r] === '`') r++;
      const close = findRun(text, r, r - k);
      k = close >= 0 ? close + (r - k) - 1 : r - 1;
    } else if (c === '[') {
      depth++;
    } else if (c === ']' && --depth === 0) {
      break;
    }
  }
  if (k >= text.length || text[k + 1] !== '(') return null;
  let p = k + 2;
  const space = () => { while (text[p] === ' ' || text[p] === '\n') p++; };
  space();
  let dest;
  if (text[p] === '<') {
    const e = text.indexOf('>', p);
    if (e < 0) return null;
    dest = text.slice(p + 1, e);
    p = e + 1;
  } else {
    const s = p;
    for (let parens = 0; p < text.length; p++) {
      const c = text[p];
      if (c === '\\') p++;
      else if (/\s/.test(c)) break;
      else if (c === '(') parens++;
      else if (c === ')' && parens-- === 0) break;
    }
    dest = text.slice(s, p);
  }
  space();
  if (text[p] === '"' || text[p] === "'") {
    const e = text.indexOf(text[p], p + 1);
    if (e < 0) return null;
    p = e + 1;
    space();
  }
  if (text[p] !== ')') return null;
  return { label: text.slice(i + 1, k), dest: dest.replace(/\\([!-/:-@[-`{-~])/g, '$1'), end: p + 1 };
}

const PUNCT = /[!-/:-@[-`{-~]/;
const ENTITY = /^&(?:#\d{1,7}|#[xX][\da-fA-F]{1,6}|[a-zA-Z][a-zA-Z\d]{1,31});/;

// **strong**, then *em* and _em_, on escaped text whose code spans, links and escapes are already slots.
function emphasis(s) {
  return s
    .replace(/\*\*(?=[^\s*])([^]*?[^\s*])\*\*(?!\*)/g, '<strong>$1</strong>')
    .replace(/(^|[^\w*])\*(?=[^\s*])([^]*?[^\s*])\*(?![\w*])/g, '$1<em>$2</em>')
    .replace(/(^|[^\p{L}\p{N}_])_(?=[^\s_])([^]*?[^\s_])_(?![\p{L}\p{N}_])/gu, '$1<em>$2</em>');
}

function inline(text, ctx) {
  const slots = [];
  const put = (html) => `\uE000${slots.push(html) - 1}\uE001`;
  let out = '';
  for (let i = 0; i < text.length; ) {
    const c = text[i];
    let m;
    if (c === '\\' && text[i + 1] === '\n') {
      out += put('<br>\n');
      i += 2;
    } else if (c === '\\' && PUNCT.test(text[i + 1] ?? '')) {
      out += put(escapeHtml(text[i + 1]));
      i += 2;
    } else if (c === '`') {
      let k = i;
      while (text[k] === '`') k++;
      const close = findRun(text, k, k - i);
      if (close < 0) {
        out += put(text.slice(i, k));
        i = k;
        continue;
      }
      let code = text.slice(k, close).replace(/\n/g, ' ');
      if (code.length > 2 && code[0] === ' ' && code.at(-1) === ' ' && code.trim()) code = code.slice(1, -1);
      out += put(`<code>${escapeHtml(code)}</code>`);
      i = close + (k - i);
    } else if ((c === '[' || (c === '!' && text[i + 1] === '[')) && (m = linkAt(text, c === '!' ? i + 1 : i))) {
      const dest = safeUrl(m.dest);
      out += put(c === '!'
        ? ctx.image(dest, textOf(inline(m.label, ctx)))
        : `<a href="${escapeHtml(ctx.link(dest))}">${inline(m.label, ctx)}</a>`);
      i = m.end;
    } else if (c === '<' && (m = /^<(https?:\/\/[^\s<>]+)>/.exec(text.slice(i, i + 2048)))) {
      out += put(`<a href="${escapeHtml(ctx.link(safeUrl(m[1])))}">${escapeHtml(m[1])}</a>`);
      i += m[0].length;
    } else if (c === '<' && (m = /^<br\s*\/?>/i.exec(text.slice(i, i + 8)))) {
      out += put('<br>');
      i += m[0].length;
    } else if (c === 'h' && (i === 0 || /[\s(*_~]/.test(text[i - 1])) &&
      (m = /^https?:\/\/[^\s<>]+/.exec(text.slice(i, i + 2048)))) {
      let url = m[0].replace(/[?!.,:;*_~'"]+$/, '');
      while (url.endsWith(')') && url.split('(').length < url.split(')').length) url = url.slice(0, -1);
      out += put(`<a href="${escapeHtml(ctx.link(safeUrl(url)))}">${escapeHtml(url)}</a>`);
      i += url.length;
    } else if (c === '&' && (m = ENTITY.exec(text.slice(i, i + 40)))) {
      out += put(m[0]);
      i += m[0].length;
    } else if (c === ' ' && (m = /^ {2,}\n/.exec(text.slice(i, i + 64)))) {
      out += put('<br>\n');
      i += m[0].length;
    } else {
      out += escapeHtml(c);
      i++;
    }
  }
  return emphasis(out.trimEnd()).replace(/\uE000(\d+)\uE001/g, (_, k) => slots[k]);
}

const ALERTS = { NOTE: 'Note', TIP: 'Tip', IMPORTANT: 'Important', WARNING: 'Warning', CAUTION: 'Caution' };
const ALIGN = { l: ' class="l"', c: ' class="c"', r: ' class="r"', '': '' };

function html(list, ctx, tight = false) {
  return list.map((b) => {
    switch (b.t) {
      case 'p':
        return tight ? inline(b.text, ctx) : `<p>${inline(b.text, ctx)}</p>`;
      case 'h': {
        const inner = inline(b.text, ctx);
        const id = ctx.slug(textOf(inner));
        ctx.ids.push(id);
        ctx.title ??= textOf(inner);
        return `<h${b.level} id="${escapeHtml(id)}">${inner}</h${b.level}>`;
      }
      case 'code': {
        const lang = b.lang ? ` class="language-${escapeHtml(b.lang)}"` : '';
        return `<pre><code${lang}>${escapeHtml(b.text)}</code></pre>`;
      }
      case 'hr':
        return '<hr>';
      case 'div':
        return b.close ? '</div>' : '<div class="center">';
      case 'quote': {
        const [first, ...rest] = b.children;
        const m = first?.t === 'p' && ALERT.exec(first.text);
        if (!m) return `<blockquote>\n${html(b.children, ctx)}\n</blockquote>`;
        const text = first.text.slice(m[0].length);
        const body = text.trim() ? [{ ...first, text }, ...rest] : rest;
        return `<div class="alert ${m[1].toLowerCase()}">\n<p class="alert-title">${ALERTS[m[1]]}</p>\n` +
          `${html(body, ctx)}\n</div>`;
      }
      case 'list': {
        const tag = b.ordered ? 'ol' : 'ul';
        const start = b.ordered && b.start !== 1 ? ` start="${b.start}"` : '';
        const items = b.items.map((item) => `<li>${html(item, ctx, !b.loose)}</li>`).join('\n');
        return `<${tag}${start}>\n${items}\n</${tag}>`;
      }
      case 'table': {
        const cell = (tag, k, c) => `<${tag}${ALIGN[b.align[k]]}>${inline(c, ctx)}</${tag}>`;
        const row = (cs, tag) => `<tr>${cs.map((c, k) => cell(tag, k, c)).join('')}</tr>`;
        return `<div class="table"><table>\n<thead>${row(b.head, 'th')}</thead>\n<tbody>\n` +
          `${b.rows.map((r) => row(r, 'td')).join('\n')}\n</tbody>\n</table></div>`;
      }
    }
    throw new Error(`unknown block ${b.t}`);
  }).join('\n');
}

export function render(markdown, { link = (href) => href, image = (src, alt) => escapeHtml(alt) } = {}) {
  const lines = markdown.replace(/\r\n?/g, '\n').replace(/\t/g, '    ').split('\n');
  const ctx = { link, image, slug: slugger(), ids: [], title: undefined };
  const out = html(blocks(lines), ctx);
  return { html: out, title: ctx.title, ids: ctx.ids };
}
