// NB-159: golden vectors for peerwire::files (crates/peerwire/src/files), the Rust port of Peerwire Share's piece
// protocol (demo/share/files.js), so the two stay one protocol. It makes the vectors from files.js and checks they are
// the ones committed in crates/peerwire/tests/vectors/files/ (`--write` writes them instead; then run
// `cargo test -p peerwire --features files files::`, whose `vectors` tests read them):
// - names.json: file names, hand-picked hostile ones and seeded random ones (control, format, bidi and zero-width
//   characters, lone surrogates, path and Windows characters, dots and whitespace, device names, combining marks and
//   characters NFC composes, long names with and without extensions), each with cleanName's answer; types.json the
//   same for cleanType. A name is its UTF-16 code units in hex (4 digits each), so lone surrogates survive JSON.
// - controls.json: control frames, valid and mutated (cut, bytes changed, padded to the size limit and past it,
//   BOMs, invalid UTF-8, escapes, numbers of every shape, duplicate keys, deep nesting), each with what
//   decodeControl made of it (null, or the fields the protocol reads, each as ["u"] undefined, ["n"] null,
//   ["b", v], ["d", f64 bits in hex], ["s", UTF-16 hex] or ["o"] an object or array) and what parseOffer made of
//   that (null, or the offer).
// - pieces.json: checkPiece's verdicts, resumeAt's answers (with `have` as above), and decodePiece of frames cut or
//   changed at random.
// - out.json: frames files.js and share.js make, which Rust must make byte for byte: offers (makeOffer then
//   encodeControl), the control messages share.js sends, and pieces (encodePiece).
// - sha.json: files.js's streaming Sha256 over seeded bytes in seeded splits.
//
//   node packages/js/test/rust_files.mjs [--write]
import { readFileSync, writeFileSync, existsSync, mkdirSync } from 'node:fs';
import * as F from '../demo/share/files.js';

const DIR = new URL('../../../crates/peerwire/tests/vectors/files/', import.meta.url);
const WRITE = process.argv.includes('--write');
const enc = new TextEncoder();
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

// mulberry32
function rng(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const R = rng(0x6e62_0159);
const int = (n) => Math.floor(R() * n);
const pick = (xs) => xs[int(xs.length)];
const hex = (b) => Buffer.from(b).toString('hex');
const units = (s) => Array.from({ length: s.length }, (_, i) => s.charCodeAt(i).toString(16).padStart(4, '0')).join('');
const bytesOf = (n) => Uint8Array.from({ length: n }, () => int(256));

// ---------- names and types ----------

const handNames = [
  '../../etc/passwd', '..\\..\\Windows\\system.ini', '/abs/path.txt', 'C:\\x.txt', '..', '.bashrc', '   ', '',
  'a\u0000b\u0007c\nd\re\tf.txt', 'evil\u202Etxt.exe', 'zero\u200Bwidth\uFEFF.png', 'line\u2028sep\u2029.txt',
  'lone\uD800surrogate.txt', '\uDC00', 'pair\uD83D\uDE00ok', 'what?*<>|".txt', 'trailing dots...  ', 'CON', 'nul.txt',
  'com1.log', 'Com\u00b9.txt', 'LPT\u00b3', 'lpt0', 'lpt10.txt', 'aux.', 'console.log', 'prn.txt.bak', 'nulx',
  '  many    spaces  here .md', 'café.txt', 'cafe\u0301.txt', '\u212b\u2126\u212a.txt', 'a\u0301\u0327\u0323.txt',
  '\u1100\u1161\u11a8 \uac00\u11a8', 'q\u0307\u0323', '\u0344\u0f73\u0f75', '日本語のファイル.pdf', 'tab\u000bvt\u000cff',
  'nbsp\u00a0\u1680\u2000\u200a\u202f\u205f\u3000x', 'nel\u0085x', 'soft\u00adhyphen', 'alm\u061cx', 'mvs\u180ex',
  'tag\u{e0001}\u{e0020}x', 'music\u{1d173}x', 'ſ.txt', '\u037e;', ' .hidden. ', '...a...', '.', '..\u2028..',
  `${'x'.repeat(300)}.tar.gz`, '😀'.repeat(200), 'a'.repeat(5000), `${'a'.repeat(118)}.txt`, `${'a'.repeat(117)}.txt`,
  `${'a'.repeat(200)}.${'b'.repeat(15)}`, `${'a'.repeat(200)}.${'b'.repeat(16)}`, `${'a'.repeat(200)}. .  txt`,
  `${'é'.repeat(100)}${'e\u0301'.repeat(30)}.txt`, `${'a'.repeat(119)} .x`, `${'a'.repeat(117)}..  .x`,
  `.${'a'.repeat(200)}`, `${'a'.repeat(1023)}\uD83D\uDE00`, `${'😀'.repeat(119)}.😀`, `${'a'.repeat(130)}.😀😀😀`,
  `CON${'.x'.repeat(100)}`,
];
// Characters a name is made of at random: the interesting ones often, any code point now and then.
const POOL = [
  ...'abcXYZ019 ._-', '.', '.', ' ', ' ', '/', '\\', ':', '*', '?', '"', '<', '>', '|', '\t', '\n', '\r', '\u0000',
  '\u001f', '\u007f', '\u0085', '\u00a0', '\u00ad', '\u0301', '\u0327', '\u0323', '\u0308', '\u0345', '\u05b0',
  '\u0344', '\u0f73', '\u0f81', 'é', 'Å', 'e', 'E', 'a', 'A', '\u212b', '\u2126', '\u1e69', '\u1100', '\u1161',
  '\u11a8', '\uac00', '\ud7a3', '\u1680', '\u2000', '\u2005', '\u200a', '\u200b', '\u200c', '\u200d', '\u200e',
  '\u200f', '\u2028', '\u2029', '\u202a', '\u202e', '\u202f', '\u205f', '\u2060', '\u2066', '\u2069', '\u3000',
  '\ufeff', '\ufff9', '\ufffd', '\ud800', '\udbff', '\udc00', '\udfff', '😀', '\u{1d173}', '\u{e0001}', '\u{e007f}',
  'ſ', '\u212a', '\u00b9', '\u00b2', '\u00b3', '\u037e', '日', '\u{20000}', '\u0915\u093c', '\u09c7\u09be',
  '\u{1d15e}', '\u{1d165}', '\u{110a5}\u{110ba}', '\u0d4a', '\u0b47\u0b3e',
];
const anyChar = () => {
  if (R() < 0.1) return String.fromCharCode(int(0x10000)); // any unit, lone surrogates too
  let c;
  do c = int(0x110000); while (c >= 0xd800 && c <= 0xdfff);
  return String.fromCodePoint(c);
};
const randomName = () => {
  const n = R() < 0.9 ? int(24) : 100 + int(140);
  let s = R() < 0.1 ? pick(['con', 'NUL', 'Aux', 'prn', 'com', 'LPT', 'com¹', 'lpt9']) : '';
  for (let i = 0; i < n; i++) s += R() < 0.15 ? anyChar() : pick(POOL);
  if (R() < 0.3) s += pick(['.txt', '.tar.gz', '.', '. ', '.😀', `.${'x'.repeat(int(20))}`, '..', ' ']);
  return s;
};
const names = [...handNames, ...Array.from({ length: 1000 }, randomName)].map((s) => [units(s), F.cleanName(s)]);

const handTypes = [
  'text/plain', 'Image/PNG', 'application/vnd.ms-excel', 'text/html;charset=x"<', 'a/b', '/b', 'a/', 'a', '',
  '_a/b', 'a_/b_', 'a.b+c-d/e.f+g-h_i', 'a/b/c', 'a b/c', 'ſ/x', 'K/\u212a', 'é/x', `a/${'b'.repeat(98)}`,
  `a/${'b'.repeat(99)}`, 'a/b\n', '\na/b', 'A/B', '0/0', '-/a', '.a/b',
];
const TYPE_POOL = [...'abcXYZ09_.+-/', '/', ' ', ';', '"', 'é', 'ſ', '\u212a', '\n', '\ud800'];
const types = [
  ...handTypes,
  ...Array.from({ length: 600 }, () => Array.from({ length: 1 + int(R() < 0.9 ? 12 : 110) }, () => pick(TYPE_POOL)).join('')),
].map((s) => [units(s), F.cleanType(s)]);

// ---------- control frames ----------

const ID = '0123456789abcdef0123456789abcdef';
const SHA = 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad';
const FIELDS = ['t', 'id', 'have', 'ok', 'sha256', 'name', 'size', 'type', 'piece', 'count'];
function val(v) {
  if (v === undefined) return ['u'];
  if (v === null) return ['n'];
  if (typeof v === 'boolean') return ['b', v];
  if (typeof v === 'number') {
    const d = new DataView(new ArrayBuffer(8));
    d.setFloat64(0, v);
    return ['d', d.getBigUint64(0).toString(16).padStart(16, '0')];
  }
  if (typeof v === 'string') return ['s', units(v)];
  return ['o'];
}
// What files.js makes of a frame: decodeControl's fields, and parseOffer's offer.
function control(f) {
  const m = F.decodeControl(f);
  if (!m) return { f: hex(f), m: null, offer: null };
  const o = F.parseOffer(m);
  return {
    f: hex(f),
    m: FIELDS.map((k) => val(Object.hasOwn(m, k) ? m[k] : undefined)),
    offer: o && { id: o.id, name: o.name, size: o.size, type: o.type, piece: o.piece, count: o.count },
  };
}
const frameOf = (text) => {
  const b = typeof text === 'string' ? enc.encode(text) : text;
  const f = new Uint8Array(1 + b.length);
  f[0] = F.CONTROL_FRAME;
  f.set(b, 1);
  return f;
};
const offerText = (o = {}) => JSON.stringify({ t: 'offer', v: 1, id: ID, name: 'x.bin', size: 600000, type: 'text/plain', piece: 524288, count: 2, ...o });
const NUMBERS = [
  '0', '-0', '1', '2', '1.0', '2.0', '1e0', '0.5e1', '600000', '6e5', '600000.0000000001', '599999.99999999999',
  '1099511627776', '1099511627777', '1.099511627776e12', '9007199254740991', '9007199254740992', '9007199254740993',
  '1e400', '-1e400', '1e-400', '-1', '262144', '262143', '1048522', '1048523', '524288', '524288.5', '01', '1.',
  '.5', '+1', '1e', '1e+', '-', '0x10', 'NaN', 'Infinity', '1E2', '1e+2', '1e-2', '2.5E-3', '123456789012345678901234567890',
];
const handControls = [
  offerText(), offerText({ count: undefined }), offerText({ type: undefined }), offerText({ type: null }),
  offerText({ name: 5 }), offerText({ name: 'a'.repeat(1024) }), offerText({ name: 'a'.repeat(1025) }),
  offerText({ size: 0, count: 0 }), offerText({ count: 3 }), offerText({ count: null }), offerText({ id: ID.toUpperCase() }),
  offerText({ t: 'Offer' }), offerText({ v: 2 }), offerText({ name: 'lone\ud800.txt' }), offerText({ name: '../x\u202e' }),
  '[]', '[{"t":"offer"}]', 'null', '"offer"', '1', 'true', '{}', '{"t":1}', '{"t":null}', '{"t":"x"}', ' {"t":"x"} ',
  '\ufeff{"t":"x"}', '\ufeff\ufeff{"t":"x"}', '{"t":"x"}\u0000', '{"t":"x",}', '{"t":"x"', '{"t":"x"}}', "{'t':'x'}",
  '{"t":"x","t":"cancel"}', '{"t":"cancel","t":7}', '{"\\u0074":"cancel","id":"' + ID + '"}', '{"t":"a\\ud800b"}',
  '{"t":"\\uD83D\\uDE00"}', '{"t":"\\u00e9"}', '{"t":"\\x"}', '{"t":"\\u12"}', '{"t":"a\u0001b"}', '{"t":"a\tb"}',
  '{"t":"x","a":[1,]}', '{"t":"x","a":{"b"}}', '{"t":"x","a":{"b":1,}}', '{"t":"x","a":tru}', '{"t":"x","a":nul}',
  '{"t":"x","a":[[[[]]]],"b":{"c":{"d":[{}]}}}', '{\n\t"t" :\r"x"\n}', '{"t":"x"}\n', '{"t" "x"}', '{t:"x"}',
  `{"x":${'['.repeat(1500)}${']'.repeat(1500)},"t":"cancel","id":"${ID}"}`,
  `{"x":${'['.repeat(1500)}${']'.repeat(1499)},"t":"cancel","id":"${ID}"}`,
  `{"x":${'{"a":'.repeat(500)}1${'}'.repeat(500)},"t":"x"}`,
  `{"t":"accept","id":"${ID}","have":0}`, `{"t":"accept","id":"${ID}","have":-0}`, `{"t":"accept","id":"${ID}"}`,
  `{"t":"ack","id":"${ID}","have":3}`, `{"t":"end","id":"${ID}","sha256":"${SHA}"}`,
  `{"t":"end","id":"${ID}","sha256":"${SHA.toUpperCase()}"}`, `{"t":"done","id":"${ID}","ok":true,"sha256":"${SHA}"}`,
  `{"t":"done","id":"${ID}","ok":"true","sha256":"${SHA}"}`, `{"t":"resume","id":"${ID}"}`, `{"t":"decline","id":"${ID}"}`,
  `{"t":"cancel","id":"${ID}","x":1e400}`, `{"t":"cancel","id":${JSON.stringify(ID)},"__proto__":{"t":"x"}}`,
  ...NUMBERS.flatMap((n) => [`{"t":"offer","id":"${ID}","name":"n","size":${n},"piece":262144}`,
    `{"t":"offer","id":"${ID}","name":"n","size":1,"piece":${n}}`, `{"t":"offer","id":"${ID}","name":"n","size":600000,"piece":524288,"count":${n}}`,
    `{"t":"ack","id":"${ID}","have":${n}}`]),
].map(frameOf);
// Raw bytes: invalid UTF-8 in and out of strings, a BOM alone, overlong and surrogate encodings.
const bad8 = [[0xff], [0xc3], [0xc3, 0x28], [0xe2, 0x82], [0xed, 0xa0, 0x80], [0xc0, 0xaf], [0xf4, 0x90, 0x80, 0x80], [0xef, 0xbb, 0xbf]];
const rawControls = bad8.flatMap((b) => {
  const [pre, post] = [enc.encode(`{"t":"offer","id":"${ID}","name":"a`), enc.encode('b","size":1,"piece":524288}')];
  return [frameOf(Uint8Array.from([...pre, ...b, ...post])), frameOf(Uint8Array.from([...b, ...enc.encode('{"t":"x"}')]))];
});
// At the size limit and one past it.
const pad = (n) => `{"t":"cancel","id":"${ID}","p":"${'p'.repeat(n)}"}`;
const limit = [pad(F.MAX_CONTROL - pad(0).length), pad(F.MAX_CONTROL + 1 - pad(0).length), `{"t":"x","p":"${'é'.repeat(2046)}"}`].map(frameOf);
// Mutations of valid ones: cut, a byte changed or inserted, a field's value swapped for another shape.
const seeds = handControls.slice(0, 15).concat(handControls.slice(-40)).filter((f) => f.length < 400);
const SHAPES = ['null', 'true', '"s"', '[]', '{}', '0', '-0', '1.5', '1e400', '"\\ud800"', `"${ID}"`, `"${SHA}"`];
const mutated = Array.from({ length: 900 }, () => {
  const f = pick(seeds).slice();
  const r = R();
  if (r < 0.25) return f.subarray(0, 1 + int(f.length));
  if (r < 0.5) {
    f[1 + int(f.length - 1)] = pick([int(256), 0x22, 0x5c, 0x7b, 0x7d, 0x5b, 0x5d, 0x3a, 0x2c, 0x30, 0x2d, 0x2e, 0x65]);
    return f;
  }
  if (r < 0.65) {
    const at = 1 + int(f.length - 1);
    return Uint8Array.from([...f.subarray(0, at), ...enc.encode(pick(['\\', '"', ',', ' ', '\ufeff', '\u0000', '{', '['])), ...f.subarray(at)]);
  }
  const text = new TextDecoder().decode(f.subarray(1));
  const keys = [...text.matchAll(/"(\w+)":/g)];
  if (!keys.length) return f;
  const k = pick(keys)[1];
  return frameOf(text.replace(new RegExp(`"${k}":("[^"]*"|[^,}]*)`), `"${k}":${pick(SHAPES)}`));
});
const controls = [...handControls, ...rawControls, ...limit, ...mutated, Uint8Array.of(F.CONTROL_FRAME), Uint8Array.of(F.PIECE_FRAME, 0x7b, 0x7d), new Uint8Array(0)].map(control);

// ---------- pieces ----------

const checks = [];
for (let i = 0; i < 600; i++) {
  const piece = pick([F.MIN_PIECE, F.PIECE, F.MAX_PIECE, F.MIN_PIECE + int(F.MAX_PIECE - F.MIN_PIECE)]);
  const size = pick([0, 1, piece - 1, piece, piece + 1, 3 * piece, 3 * piece + 7, int(10 * piece), F.MAX_SIZE, F.MAX_SIZE - 1]);
  const t = { size, piece, count: F.pieceCount(size, piece) };
  const next = pick([0, 1, int(t.count + 2), t.count, Math.max(0, t.count - 1)]);
  const index = pick([next, next + 1, next - 1, t.count, t.count - 1, 0, 0xffffffff, int(t.count + 3)]);
  if (index < 0 || index > 0xffffffff) continue;
  const [s, e] = F.pieceRange(t, Math.min(index, Math.max(0, t.count - 1)));
  const len = pick([e - s, e - s - 1, e - s + 1, 0, piece, piece - 1]);
  if (len < 0) continue;
  checks.push([size, piece, next, index, len, F.checkPiece(t, next, { index, data: { length: len } })]);
}
const resumes = [];
for (let i = 0; i < 300; i++) {
  const count = pick([0, 1, 5, 2097152]);
  const read = pick([0, 1, int(count + 1), count]);
  const have = pick([0, -0, 1, 2, read, read + 1, count, count + 1, -1, 1.5, '1', null, true, undefined, 1e400, 9007199254740993, { a: 1 }]);
  const r = F.resumeAt({ count, piece: F.PIECE, size: count * F.PIECE }, read, have);
  resumes.push([count, read, val(have), r && r.next]);
}
const pieceFrames = Array.from({ length: 150 }, () => {
  const data = bytesOf(int(70));
  let f = F.encodePiece(hex(bytesOf(16)), int(2 ** 32), bytesOf(32), data);
  if (R() < 0.3) f = f.subarray(0, int(f.length + 1));
  if (R() < 0.2) f[0] = int(4);
  const p = F.decodePiece(f);
  return [hex(f), p && [p.id, p.index, hex(p.hash), hex(p.data)]];
});

// ---------- what files.js and share.js send ----------

const out = [];
for (const [name, type, size] of [
  ['holiday.mp4', 'video/mp4', 5 * 1024 ** 3 + 1], ['a "b"\\c.txt', 'Text/Plain', 600000], ['', '', 0], ['ünïcødé 😀.txt', 'x', 1],
  ['line\nbreak', 'a/b;c', F.MAX_SIZE], ['con', 'image/svg+xml', 524288], ['e\u0301.txt', undefined, 524289],
  ...names.slice(0, 60).map(([u], i) => [String.fromCharCode(...(u.match(/.{4}/g) ?? []).map((h) => parseInt(h, 16))), pick(handTypes), i * 99991]),
]) {
  const id = hex(bytesOf(16));
  // [name in UTF-16 hex, type in UTF-16 hex, size, id, the frame]
  out.push(['offer', units(name), units(type ?? ''), size, id, hex(F.encodeControl(F.makeOffer({ id, name, size, type })))]);
}
for (const m of [
  { t: 'accept', id: ID, have: 0 }, { t: 'accept', id: ID, have: 4194304 }, { t: 'decline', id: ID }, { t: 'cancel', id: ID },
  { t: 'ack', id: ID, have: 17 }, { t: 'end', id: ID, sha256: SHA }, { t: 'done', id: ID, ok: true, sha256: SHA },
  { t: 'done', id: ID, ok: false, sha256: SHA }, { t: 'resume', id: ID },
]) out.push(['control', JSON.stringify(m), hex(F.encodeControl(m))]);
for (const n of [0, 1, 63, 64, 1000]) {
  const data = bytesOf(n);
  const h = bytesOf(32);
  const index = int(2 ** 32);
  out.push(['piece', ID, index, hex(h), hex(data), hex(F.encodePiece(ID, index, h, data))]);
}

// ---------- the streaming SHA-256 ----------

const sha = [];
for (const n of [0, 1, 55, 56, 63, 64, 65, 119, 120, 128, 1000, 5000]) {
  const data = bytesOf(n);
  const h = new F.Sha256();
  for (let o = 0; o < n;) {
    const cut = 1 + int(200);
    h.update(data.subarray(o, o + cut));
    o += cut;
  }
  sha.push([hex(data), F.toHex(h.digest())]);
}

// Written, or checked against what is committed.
const files = {
  'names.json': { names, types },
  'controls.json': controls,
  'pieces.json': { checks, resumes, frames: pieceFrames },
  'out.json': out,
  'sha.json': sha,
};
if (WRITE) mkdirSync(DIR, { recursive: true });
for (const [name, v] of Object.entries(files)) {
  const text = `${JSON.stringify(v)}\n`;
  const url = new URL(name, DIR);
  if (WRITE) {
    writeFileSync(url, text);
    console.log(`wrote ${name}: ${text.length} bytes`);
  } else {
    check(existsSync(url) && readFileSync(url, 'utf8') === text, `${name} is what files.js makes now (else: node packages/js/test/rust_files.mjs --write)`);
  }
}
const offers = controls.filter((c) => c.offer).length;
check(offers > 50 && controls.filter((c) => !c.m).length > 200 && checks.length > 400 && pieceFrames.length, `${controls.length} control frames (${offers} offers), ${names.length} names, ${checks.length} piece checks`);
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
