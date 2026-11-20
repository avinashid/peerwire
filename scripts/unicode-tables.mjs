// Writes crates/peerwire/src/files/unicode_tables.rs, the tables of files/unicode.rs: NFC and the format characters
// (\p{Cf}) exactly as JavaScript has them, so a file name from a peer cleans the same in Rust as in files.js
// cleanName. Made from this Node's own Unicode data (its ICU), through String.prototype.normalize and regexes, as no
// crate or UCD file is wanted. The file's header names the Unicode version (process.versions.unicode): run this
// again, and commit the file, when Node's moves on. No dependencies.
//
//   node scripts/unicode-tables.mjs
//
// Node lists no table of its own, so each is read off what normalize does:
// - format characters: every code point tested against /\p{Cf}/u, as ranges;
// - non-starters (canonical combining class above 0): a character NFD swaps with U+0334 (class 1) after it or with
//   U+0301 (class 230) before it. Node gives no class, so theirs are written as ranks, 1 for the lowest: the order
//   NFD sorts them in, which is all NFC asks of a class;
// - decompositions: NFD of each character, written as one step: a single character, or a pair (a, b) where b is
//   the last of the NFD and a has the rest as its own;
// - primary composites: characters NFC keeps and NFD changes. NFC composes each from its pair: b the last of its
//   NFD, a the NFC of the rest, which has to be one character. The characters NFC changes (singletons, non-starters
//   like U+0344, exclusions like Devanagari QA) are decomposed and never composed.
// Hangul syllables decompose and compose by rule (unicode.rs), so they are in no table.
//
// The script fails rather than write a table that disagrees with normalize: every non-starter's rank is checked
// against NFD on every pair of them, every decomposition expanded again and compared with NFD, and every composite
// with NFC of its pair.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const OUT = path.join(ROOT, 'crates/peerwire/src/files/unicode_tables.rs');

const str = String.fromCodePoint;
const cps = (s) => Array.from(s, (c) => c.codePointAt(0));
const nfd = (s) => s.normalize('NFD');
const nfc = (s) => s.normalize('NFC');
const hex = (c) => (c ? `0x${c.toString(16).toUpperCase()}` : '0');
const name = (c) => `U+${c.toString(16).toUpperCase().padStart(4, '0')}`;
function check(ok, what) {
  if (!ok) throw new Error(`unicode-tables: ${what}`);
}

// Every Unicode scalar value: surrogates are no characters, and a Rust char can't hold one.
const SCALARS = [];
for (let c = 0; c <= 0x10ffff; c++) if (c < 0xd800 || c > 0xdfff) SCALARS.push(c);
// Hangul by rule: syllables, leading consonants (L), vowels (V) and trailing consonants (T) that compose.
const isSyllable = (c) => c >= 0xac00 && c <= 0xd7a3;
const isL = (c) => c >= 0x1100 && c <= 0x1112;
const isV = (c) => c >= 0x1161 && c <= 0x1175;
const isLV = (c) => isSyllable(c) && (c - 0xac00) % 28 === 0;
const isT = (c) => c >= 0x11a8 && c <= 0x11c2;

// Format characters, as ranges [first, last].
const FORMAT = /^\p{Cf}$/u;
const format = [];
for (const c of SCALARS) {
  if (!FORMAT.test(str(c))) continue;
  const last = format.at(-1);
  if (last && last[1] === c - 1) last[1] = c;
  else format.push([c, c]);
}

// Decompositions: NFD of each character it changes.
const decomposed = new Map();
for (const c of SCALARS) {
  if (isSyllable(c)) continue;
  const d = cps(nfd(str(c)));
  if (d.length !== 1 || d[0] !== c) decomposed.set(c, d);
}

// Non-starters, among the characters NFD keeps: only those are left once a string is decomposed.
const moves = (s) => nfd(s) !== s;
const nonStarters = SCALARS.filter((c) => !decomposed.has(c) && !isSyllable(c)
  && (moves(`a${str(c)}\u0334`) || moves(`a\u0301${str(c)}`)));
// Whether NFD puts y before x: x's class is above y's.
const above = (x, y) => moves(`a${str(x)}${str(y)}`);
const order = [...nonStarters].sort((x, y) => (above(x, y) ? 1 : above(y, x) ? -1 : 0));
const rank = new Map();
order.forEach((c, i) => rank.set(c, i === 0 ? 1 : rank.get(order[i - 1]) + (above(c, order[i - 1]) ? 1 : 0)));
check(rank.get(order.at(-1)) <= 255, 'more ranks than a u8 holds');
for (const x of nonStarters) {
  for (const y of nonStarters) check(above(x, y) === rank.get(x) > rank.get(y), `${name(x)}, ${name(y)} out of rank`);
}

// One step of each decomposition, and the primary composites.
const key = (d) => str(...d);
const byNfd = new Map();
for (const [c, d] of decomposed) if (!byNfd.has(key(d))) byNfd.set(key(d), c);
const singles = new Map();
const pairs = new Map();
const composites = [];
for (const [c, d] of decomposed) {
  if (d.length === 1) {
    singles.set(c, d[0]);
    continue;
  }
  const b = d.at(-1);
  const rest = d.slice(0, -1);
  let a;
  if (nfc(str(c)) === str(c)) {
    const composed = cps(nfc(key(rest)));
    check(composed.length === 1, `${name(c)}: its NFD less the last is no one character in NFC`);
    a = composed[0];
    check(nfc(str(a, b)) === str(c), `${name(c)}: NFC of its pair is not it`);
    check(!(isL(a) && isV(b)) && !(isLV(a) && isT(b)), `${name(c)}: composes from Hangul jamo`);
    composites.push(c);
  } else {
    a = rest.length === 1 ? rest[0] : byNfd.get(key(rest));
    check(a !== undefined, `${name(c)}: no character decomposes to its NFD less the last`);
  }
  pairs.set(c, [a, b]);
}
// The decomposition unicode.rs makes of c: a pair's a followed down, then its b's, in turn.
function expand(c, depth = 0) {
  check(depth < 8, `${name(c)}: a decomposition that never ends`);
  if (pairs.has(c)) return [...expand(pairs.get(c)[0], depth + 1), pairs.get(c)[1]];
  return [singles.get(c) ?? c];
}
for (const [c, d] of decomposed) check(key(expand(c)) === key(d), `${name(c)}: its steps don't make its NFD`);
for (const [, b] of pairs.values()) check(!decomposed.has(b), `${name(b)}: the second of a pair decomposes`);
for (const t of singles.values()) check(!decomposed.has(t), `${name(t)}: a singleton's target decomposes`);
const pairOf = (c) => pairs.get(c)[0] * 0x110000 + pairs.get(c)[1];
composites.sort((x, y) => pairOf(x) - pairOf(y));
composites.forEach((c, i) => check(i === 0 || pairOf(c) > pairOf(composites[i - 1]), `${name(c)}: a pair taken twice`));

// Below the first character that is a non-starter, that NFC changes, or that composes with one before it, a string
// is already NFC.
const below = Math.min(...nonStarters, ...[...decomposed.keys()].filter((c) => nfc(str(c)) !== str(c)),
  ...composites.map((c) => pairs.get(c)[1]), 0x1161, 0x11a8);

// Runs of consecutive keys of a map: [first key, index of its value] each, and the values in key order. A run passes
// over up to `gap` keys in a row that the map hasn't, with `filler` for each, as that is shorter than a new run.
function runs(map, filler, gap) {
  const heads = [];
  const values = [];
  const index = new Map();
  let next = -Infinity;
  for (const k of [...map.keys()].sort((x, y) => x - y)) {
    if (k - next > gap) heads.push([k, values.length]);
    else for (; next < k; next++) values.push(filler);
    index.set(k, values.length);
    values.push(map.get(k));
    next = k + 1;
  }
  check(values.length <= 0xffff, 'a run table past u16 indexes');
  return { heads, values, index };
}
const klass = runs(rank, 0, 3);
const single = runs(singles, 0, 3);
const pair = runs(pairs, [0, 0], 1);

// Rust source: items packed into lines of at most 120 characters.
function rows(items) {
  const lines = [];
  let line = '';
  for (const item of items) {
    if (line && line.length + item.length + 2 > 120) {
      lines.push(line);
      line = '';
    }
    line += line ? ` ${item},` : `    ${item},`;
  }
  return [...lines, line].join('\n');
}
const tuple = (xs) => `(${xs.join(', ')})`;
const table = (doc, item, type, items) => `${doc.trim().split('\n').map((l) => `/// ${l}`).join('\n')}
#[rustfmt::skip]
static ${item}: [${type}; ${items.length}] = [
${rows(items)}
];
`;

const UNICODE = process.versions.unicode;
const source = `// Unicode ${UNICODE} tables of files/unicode.rs: NFC and the format characters, as JavaScript has them.
// Generated by scripts/unicode-tables.mjs from Node's own Unicode data: do not edit, run it again instead
// (node scripts/unicode-tables.mjs).

/// Below this, every character is a starter that NFC keeps and that composes with nothing before it: a string of
/// them is already NFC.
const NFC_BELOW: char = '\\u{${below.toString(16).toUpperCase()}}';

${table(`
The format characters (general category Cf), as ranges (first, last).
`, 'FORMAT', '(u32, u32)', format.map((r) => tuple(r.map(hex))))}
${table(`
The non-starters, in runs: (first character, index of its class in CLASSES).
`, 'CLASS_RUNS', '(u32, u16)', klass.heads.map(([c, i]) => tuple([hex(c), i])))}
${table(`
Their canonical combining classes, as ranks: 1 for the lowest class, 2 for the next, and so on. NFC asks only
whether one class is below another, which ranks tell as well as the classes, which Node doesn't give. 0 for a
starter a run passes over.
`, 'CLASSES', 'u8', klass.values.map(String))}
${table(`
Characters that decompose to one other, in runs: (first character, index of its target in SINGLES).
`, 'SINGLE_RUNS', '(u32, u16)', single.heads.map(([c, i]) => tuple([hex(c), i])))}
${table(`
The targets, which never decompose; 0 for a character a run passes over that doesn't decompose.
`, 'SINGLES', 'u32', single.values.map(hex))}
${table(`
Characters that decompose to more than one, in runs: (first character, index of its pair in PAIRS).
`, 'PAIR_RUNS', '(u32, u16)', pair.heads.map(([c, i]) => tuple([hex(c), i])))}
${table(`
The pairs (a, b): the decomposition is a's, decomposed in turn, then b, which never decomposes. (0, 0) for a
character a run passes over that doesn't decompose.
`, 'PAIRS', '(u32, u32)', pair.values.map((p) => tuple(p.map(hex))))}
${table(`
The primary composites, as the indexes of their pairs in PAIRS, ordered by pair: what NFC composes a pair to. A
character that decomposes and isn't here (a singleton, a non-starter, an exclusion) is never composed.
`, 'COMPOSITES', 'u16', composites.map((c) => String(pair.index.get(c))))}`;

fs.writeFileSync(OUT, source);
console.log(`${path.relative(process.cwd(), OUT)}: Unicode ${UNICODE}, ${source.length} bytes;`,
  `${format.length} format ranges, ${nonStarters.length} non-starters in ${klass.heads.length} runs of`,
  `${rank.get(order.at(-1))} classes, ${singles.size} singletons, ${pairs.size} pairs, ${composites.length}`,
  `composites; NFC below ${name(below)}`);
