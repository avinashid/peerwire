// Syntax highlighting for the site's code blocks, at build time and with no dependencies: rust, js/ts/tsx, sh,
// toml, json, html, gdscript and c. The pages' policy allows no inline style, so tokens get classes only:
//
//   c comment   s string   n number   k keyword   o constant (true, null, ALL_CAPS)   t type   f function
//   m macro, attribute, decorator or preprocessor   v variable   p key or flag   g tag   a attribute name
//
// highlight(code, lang) returns the code escaped, with tokens wrapped in <span class="X">, or null for a language
// it doesn't know. The text of what it returns is always the code itself: tokens only ever wrap it.

import { escapeHtml } from './markdown.mjs';

const ALIASES = {
  rust: 'rust', rs: 'rust',
  js: 'js', javascript: 'js', mjs: 'js', cjs: 'js', jsx: 'jsx',
  ts: 'ts', typescript: 'ts', tsx: 'jsx',
  sh: 'sh', bash: 'sh', shell: 'sh', zsh: 'sh', console: 'sh',
  toml: 'toml', json: 'json', jsonc: 'json', html: 'html', xml: 'html', svg: 'html',
  gdscript: 'gdscript', gd: 'gdscript', c: 'c', h: 'c',
};
export const LANGUAGES = [...new Set(Object.values(ALIASES))];

const words = (s) => new Set(s.split(/\s+/));
const RUST_K = words('as async await break const continue crate dyn else enum extern fn for if impl in let loop ' +
  'match mod move mut pub ref return self Self static struct super trait type unsafe use where while');
const RUST_T = words('u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64 bool char str');
const JS_K = words('as async await break case catch class const continue debugger default delete do else export ' +
  'extends finally for from function get if import in instanceof let new of return set static super switch this ' +
  'throw try typeof var void while yield');
const TS_K = words('abstract declare enum implements interface keyof namespace private protected public readonly ' +
  'satisfies type');
const TS_T = words('string number boolean bigint symbol object unknown any never void');
const JS_O = words('true false null undefined NaN Infinity');
const GD_K = words('and as await break class class_name const continue elif else enum extends for func if in is ' +
  'match not or pass return self signal static super var while');
const GD_T = words('void bool int float');
const GD_O = words('true false null PI TAU INF NAN');
const C_K = words('auto break case const continue default do else enum extern for goto if inline register ' +
  'restrict return sizeof static struct switch typedef union volatile while');
const C_T = words('void char short int long float double signed unsigned bool _Bool');
const C_O = words('true false NULL');

const CONST = /^[A-Z][A-Z\d_]+$/;

// Tokens of the code, each [class or null, text], from rules tried in order at each position: [sticky regex,
// class, or a function of the match returning a class or a list of tokens]. Nothing matching: one plain character.
function lex(code, rules) {
  const out = [];
  let i = 0;
  const push = (cls, text) => {
    if (!text) return;
    const last = out.at(-1);
    if (last && last[0] === cls) last[1] += text;
    else out.push([cls, text]);
  };
  next: while (i < code.length) {
    for (const [re, cls] of rules) {
      re.lastIndex = i;
      const m = re.exec(code);
      if (!m || !m[0].length) continue;
      const got = typeof cls === 'function' ? cls(m, code, i + m[0].length) : cls;
      if (Array.isArray(got)) for (const [c, t] of got) push(c, t);
      else push(got, m[0]);
      i += m[0].length;
      continue next;
    }
    push(null, code[i++]);
  }
  return out;
}

const call = (code, end) => /^\s*\(/.test(code.slice(end, end + 64));
// A word: keyword, constant, type, function (a name followed by "("), or plain.
const word = ({ k, t = new Set(), o = new Set(), types = true }) => (m, code, end) => {
  const w = m[0];
  if (k.has(w)) return 'k';
  if (o.has(w) || CONST.test(w)) return 'o';
  if (t.has(w) || (types && /^[A-Z]/.test(w))) return 't';
  return call(code, end) ? 'f' : null;
};

const LINE = /\/\/.*/y;
const BLOCK = /\/\*[^]*?(?:\*\/|$)/y;
const HASH = /#.*/y;
const DQ = /"(?:[^"\\\n]|\\.)*"?/y;
const SQ = /'(?:[^'\\\n]|\\.)*'?/y;
const NUM = /(?:0[xX][\da-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?)[a-zA-Z\d_]*/y;

const RULES = {};
RULES.rust = [
  [LINE, 'c'], [BLOCK, 'c'],
  [/b?r(#*)"[^]*?"\1/y, 's'],
  [/b?"(?:[^"\\]|\\[^])*"?/y, 's'],
  [/b?'(?:\\(?:x[\da-fA-F]{2}|u\{[\da-fA-F]{1,6}\}|.)|[^\\'\n])'/y, 's'],
  [/'[A-Za-z_]\w*/y, 'v'],
  [/#!?\[[^\]\n]*\]?/y, 'm'],
  [/[A-Za-z_]\w*!(?!=)/y, 'm'],
  [NUM, 'n'],
  [/[A-Za-z_]\w*/y, word({ k: RUST_K, t: RUST_T, o: words('true false') })],
];
const jsRules = (k, jsx, t) => [
  [LINE, 'c'], [BLOCK, 'c'],
  [DQ, 's'], [SQ, 's'], [/`(?:[^`\\]|\\[^])*`?/y, 's'],
  [/@[A-Za-z_$][\w$]*/y, 'm'],
  ...(jsx ? [
    // A tag where an expression starts (after "(", ",", "=", "return", ...), so a < b and f<T>() aren't tags.
    [/(?<=(?:^|[(,=?:&|{}>]|\breturn)\s*)<\/?[A-Za-z][\w.]*/my, 'g'],
    [/[A-Za-z_$][\w$]*(?==[^=>])/y, 'a'],
  ] : []),
  [NUM, 'n'],
  [/[A-Za-z_$][\w$]*/y, word({ k, t, o: JS_O })],
];
RULES.js = jsRules(JS_K, false);
RULES.ts = jsRules(new Set([...JS_K, ...TS_K]), false, TS_T);
RULES.jsx = jsRules(new Set([...JS_K, ...TS_K]), true, TS_T);
RULES.toml = [
  [HASH, 'c'],
  [/(?<=^[ \t]*)\[\[?[^\]\n]*\]?\]?/my, 't'],
  [/"""[^]*?(?:"""|$)/y, 's'], [/'''[^]*?(?:'''|$)/y, 's'], [DQ, 's'], [/'[^'\n]*'?/y, 's'],
  [/[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*(?=[ \t]*=)/y, 'p'],
  [/\d{4}-\d\d-\d\d(?:[T ]\d\d:\d\d:\d\d(?:\.\d+)?(?:Z|[+-]\d\d:\d\d)?)?/y, 'n'],
  [/[+-]?(?:inf|nan)\b/y, 'n'], [/0[xob][\da-fA-F_]+|[+-]?\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?\b/y, 'n'],
  [/\b(?:true|false)\b/y, 'o'],
  [/[A-Za-z_]\w*/y, null],
];
RULES.json = [
  [LINE, 'c'], [BLOCK, 'c'],
  [/"(?:[^"\\\n]|\\.)*"(?=\s*:)/y, 'p'],
  [DQ, 's'],
  [/-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/y, 'n'],
  [/\b(?:true|false|null)\b/y, 'o'],
  [/[A-Za-z_]\w*/y, null],
];
RULES.gdscript = [
  [HASH, 'c'],
  [/"""[^]*?(?:"""|$)/y, 's'], [/[&^]?"(?:[^"\\\n]|\\.)*"?/y, 's'], [/[&^]?'(?:[^'\\\n]|\\.)*'?/y, 's'],
  [/@[A-Za-z_]\w*/y, 'm'],
  [/[$%](?:[A-Za-z_][\w/]*|"[^"\n]*")/y, 'v'],
  [NUM, 'n'],
  [/[A-Za-z_]\w*/y, word({ k: GD_K, t: GD_T, o: GD_O })],
];
const cWord = word({ k: C_K, t: C_T, o: C_O, types: false });
RULES.c = [
  [LINE, 'c'], [BLOCK, 'c'],
  [/(?<=^[ \t]*)#[ \t]*[a-z]+/my, 'm'],
  [/(?<=#[ \t]*include[ \t]*)<[^>\n]*>/y, 's'],
  [DQ, 's'], [/'(?:[^'\\\n]|\\.)*'/y, 's'],
  [NUM, 'n'],
  [/[A-Za-z_]\w*/y, (m, code, end) => (/^[a-z]\w*_t$/.test(m[0]) ? 't' : cWord(m, code, end))],
];

// sh: comments, quotes, $variables, --flags, and the command word of each command (first on the line, or after
// |, ||, &&, ; or "("), with NAME=value assignments before it.
const SH_K = words('if then else elif fi for while until do done case esac in function return export local ' +
  'set unset source exit select');
function sh(code) {
  let command = true; // the next word is a command
  let value = false; // the next word is an assignment's value
  let name = false; // after for, case or select: a name, then "in"
  const starts = () => {
    command = true;
    value = false;
    return null;
  };
  const arg = (cls) => () => {
    command = value = false;
    return cls;
  };
  return lex(code, [
    [/(?<![^\s;|&(])#.*/y, 'c'],
    [/\n|\|\||&&|[|;(]|\$\(|`/y, starts],
    [/"(?:[^"\\]|\\[^])*"?/y, arg('s')],
    [/'[^']*'?/y, arg('s')],
    [/\$\{[^}\n]*\}?|\$[A-Za-z_]\w*|\$[\d@#?$!*-]/y, arg('v')],
    [/[A-Za-z_]\w*=/y, () => {
      if (!command) return null;
      value = true;
      return 'v';
    }],
    [/(?<=\s)--?[A-Za-z\d][\w-]*/y, arg('p')],
    [/[^\s|&;()<>"'`$#\\=]+/y, (m) => {
      if (value) {
        value = false;
        return null;
      }
      if (name && m[0] === 'in') {
        name = false;
        return 'k';
      }
      if (!command) return null;
      if (SH_K.has(m[0])) {
        name = ['for', 'case', 'select'].includes(m[0]);
        command = !name && !['in', 'esac', 'fi', 'done'].includes(m[0]);
        return 'k';
      }
      command = false;
      return 'f';
    }],
    [/[ \t]+/y, () => {
      value = false;
      return null;
    }],
    [/\\[^]/y, null],
  ]);
}

// html: comments, the doctype, tags with their attribute names and values, entities. Script and style contents
// stay plain.
function html(code) {
  let inTag = false;
  return lex(code, [
    [/<!--[^]*?(?:-->|$)/y, 'c'],
    [/<![A-Za-z][^>]*>?/y, 'm'],
    [/<\/?[A-Za-z][\w:-]*/y, () => { inTag = true; return 'g'; }],
    [/\/?>/y, () => { const was = inTag; inTag = false; return was ? 'g' : null; }],
    [/&(?:#\d+|#x[\da-fA-F]+|[A-Za-z]\w*);/y, 'n'],
    [/[^\s"'=<>/&]+/y, () => (inTag ? 'a' : null)],
    [/"[^"]*"?|'[^']*'?/y, () => (inTag ? 's' : null)],
  ]);
}

export function highlight(code, lang) {
  const name = ALIASES[String(lang).toLowerCase().split(/[,{\s]/)[0]];
  if (!name) return null;
  const tokens = name === 'sh' ? sh(code) : name === 'html' ? html(code) : lex(code, RULES[name]);
  return tokens.map(([cls, text]) => (cls ? `<span class="${cls}">${escapeHtml(text)}</span>` : escapeHtml(text)))
    .join('');
}
