// Peerwire Share's piece protocol (demo/share/files.js), in Node: piece and control framing, the per-piece and
// whole-file SHA-256 against node:crypto, pieces refused out of range, out of order or twice, resume offsets, offers
// from a hostile peer, file names cleaned, the download header, and which routes may carry file data.
//
//   node packages/js/test/share.mjs

import { createHash, randomBytes } from 'node:crypto';
import * as F from '../demo/share/files.js';

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const sha = (b) => createHash('sha256').update(b).digest();
const enc = new TextEncoder();

// 1. Sizes: a piece with its header and the room's tag byte fits the reliable lane.
check(F.HEADER === 53 && F.MAX_PIECE === (1 << 20) - 1 - 53, `the piece header is 53 bytes, so a piece is at most ${F.MAX_PIECE} bytes`);
check(F.PIECE >= F.MIN_PIECE && F.PIECE <= F.MAX_PIECE && F.PIECE === 512 * 1024, 'this page sends 512 KiB pieces, within 256 KiB and MAX_PIECE');
check(F.WINDOW * F.PIECE <= 4 << 20 && F.MAX_QUEUED === 2 * F.WINDOW, 'at most 4 MiB in flight per peer; a receiver holds at most 2 windows');

// 2. Piece framing.
const id = F.newId();
check(F.isId(id) && !F.isId(id.toUpperCase()) && !F.isId(`${id}0`) && !F.isId(42), 'ids are 32 lower-case hex characters');
const data = randomBytes(F.PIECE);
const frame = F.encodePiece(id, 70000, sha(data), data);
check(frame.length === F.HEADER + F.PIECE && frame[0] === F.PIECE_FRAME && frame.length + 1 <= F.MAX_FRAME, 'a full piece frame fits the lane with the room\'s tag byte');
const p = F.decodePiece(frame);
check(p.id === id && p.index === 70000 && Buffer.compare(Buffer.from(p.data), data) === 0 && F.sameHash(p.hash, sha(data)), 'a piece decodes to its id, index, hash and data');
const big = F.decodePiece(F.encodePiece(id, 0xfffffffe, sha(data), data.subarray(0, 1)));
check(big.index === 0xfffffffe, 'indexes are unsigned 32-bit');
const other = frame.slice();
other[0] = F.CONTROL_FRAME;
check(F.decodePiece(frame.subarray(0, F.HEADER - 1)) === null && F.decodePiece(other) === null &&
  F.decodePiece('x') === null && F.decodePiece(new Uint8Array(0)) === null, 'a short frame, another kind or not bytes is no piece');
const slice = new Uint8Array(frame.length + 7);
slice.set(frame, 7);
check(F.decodePiece(slice.subarray(7)).index === 70000, 'a piece decodes from a view at an offset (as the room hands it over)');

// 3. Control framing: never parsed over MAX_CONTROL bytes.
const ctl = F.encodeControl({ t: 'ack', id, have: 3 });
check(ctl[0] === F.CONTROL_FRAME && F.decodeControl(ctl)?.have === 3, 'a control message round-trips');
let threw = null;
try { F.encodeControl({ t: 'offer', name: 'x'.repeat(F.MAX_CONTROL) }); } catch (e) { threw = e; }
check(threw instanceof RangeError, 'encodeControl throws RangeError over MAX_CONTROL bytes');
const json = (n) => enc.encode(`{"t":"x","p":"${' '.repeat(n - 16)}"}`); // n bytes
const at = (n) => {
  const f = new Uint8Array(1 + n);
  f[0] = F.CONTROL_FRAME;
  f.set(json(n), 1);
  return f;
};
check(F.decodeControl(at(F.MAX_CONTROL))?.t === 'x' && F.decodeControl(at(F.MAX_CONTROL + 1)) === null,
  'a control message of MAX_CONTROL bytes is read; one byte more is dropped unparsed');
const raw = (s) => Uint8Array.of(F.CONTROL_FRAME, ...enc.encode(s));
check(['not json', '[1]', 'null', '42', '"t"', '{"t":5}', '{}'].every((s) => F.decodeControl(raw(s)) === null) &&
  F.decodeControl(Uint8Array.of(F.PIECE_FRAME, 123, 125)) === null, 'not JSON, not an object or no string t: dropped');

// 4. The whole-file SHA-256, streamed, against node:crypto: any split, any length.
const blob = randomBytes(3 * 1024 * 1024 + 77);
let allOk = true;
for (const cut of [1, 3, 63, 64, 65, 1000, 4096, 524288, 3 * 1024 * 1024 + 77]) {
  const h = new F.Sha256();
  for (let o = 0; o < blob.length; o += cut) h.update(blob.subarray(o, o + cut));
  if (Buffer.compare(Buffer.from(h.digest()), sha(blob)) !== 0) allOk = false;
}
check(allOk, 'Sha256 matches node:crypto over 3 MiB in chunks of 1 B to 3 MiB');
const lens = [0, 1, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128, 129, 1000];
check(lens.every((n) => F.toHex(new F.Sha256().update(blob.subarray(0, n)).digest()) === sha(blob.subarray(0, n)).toString('hex')),
  'Sha256 matches at every padding edge (0, 55, 56, 63, 64, 119, 120, 128 bytes, ...)');
check(F.toHex(new F.Sha256().update(enc.encode('abc')).digest()) === 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad', 'Sha256("abc") is the FIPS 180-2 answer');
const piece = blob.subarray(0, F.PIECE);
const wc = new Uint8Array(await crypto.subtle.digest('SHA-256', piece));
check(F.sameHash(wc, sha(piece)) && !F.sameHash(wc, sha(blob.subarray(1, F.PIECE + 1))) && !F.sameHash(wc, wc.subarray(1)),
  'sameHash: the same piece hash from WebCrypto, not another piece\'s or a shorter one');

// 5. Offers: what a peer may send, and what is refused.
const offer = F.makeOffer({ id, name: 'holiday.mp4', size: 5 * 1024 ** 3 + 1, type: 'video/mp4' });
check(offer.count === 10241 && offer.piece === F.PIECE && JSON.stringify(F.encodeControl(offer)).length > 0, 'a 5 GiB + 1 B file is 10241 pieces of 512 KiB');
const o = F.parseOffer(JSON.parse(JSON.stringify(offer)));
check(o && o.id === id && o.name === 'holiday.mp4' && o.size === offer.size && o.count === 10241 && o.type === 'video/mp4', 'an offer parses back');
const bad = {
  'no id': { id: undefined },
  'an id in capitals': { id: id.toUpperCase() },
  'a negative size': { size: -1 },
  'a fractional size': { size: 1.5 },
  'a size as a string': { size: '100' },
  'a size over 1 TiB': { size: 2 ** 40 + 1, count: undefined },
  'an unsafe size': { size: 2 ** 60, count: undefined },
  'a piece under 256 KiB': { piece: 1024, count: undefined },
  'a piece over MAX_PIECE': { piece: F.MAX_PIECE + 1, count: undefined },
  'a count that doesn\'t match': { count: 3 },
  'a name that isn\'t a string': { name: ['a'] },
  'a name over 1024 characters': { name: 'a'.repeat(1025) },
  'a type that isn\'t a string': { type: 7 },
  'another t': { t: 'offerx' },
};
const misses = Object.entries(bad).filter(([, d]) => F.parseOffer({ ...offer, ...d }) !== null).map(([k]) => k);
check(misses.length === 0, `offers refused: ${Object.keys(bad).join(', ')}${misses.length ? ` (taken: ${misses.join(', ')})` : ''}`);
check(F.parseOffer(null) === null && F.parseOffer('offer') === null && F.parseOffer([]) === null, 'null, a string or an array is no offer');
const empty = F.parseOffer({ ...offer, size: 0, count: 0 });
check(empty && empty.count === 0, 'an empty file is an offer of no pieces');
check(F.parseOffer({ ...offer, type: 'text/html;charset=x"<' }).type === '' && F.parseOffer({ ...offer, type: 'Image/PNG' }).type === 'image/png',
  'a type with parameters or odd characters shows as none; a plain one in lower case');
check(F.parseOffer({ ...offer, count: undefined }).count === 10241, 'an offer without a count gets its own');

// 6. File names.
const names = [
  ['../../etc/passwd', '_.._etc_passwd'],
  ['..\\..\\Windows\\system.ini', '_.._Windows_system.ini'],
  ['/abs/path.txt', '_abs_path.txt'],
  ['C:\\x.txt', 'C__x.txt'],
  ['..', 'file'],
  ['.bashrc', 'bashrc'],
  ['   ', 'file'],
  ['', 'file'],
  ['a\u0000b\u0007c\nd\re\tf.txt', 'abcdef.txt'],
  ['evil\u202Etxt.exe', 'eviltxt.exe'],
  ['zero\u200Bwidth\uFEFF.png', 'zerowidth.png'],
  ['line\u2028sep\u2029.txt', 'linesep.txt'],
  ['lone\uD800surrogate.txt', 'lonesurrogate.txt'],
  ['what?*<>|".txt', 'what______.txt'],
  ['trailing dots...  ', 'trailing dots'],
  ['CON', '_CON'],
  ['nul.txt', '_nul.txt'],
  ['com1.log', '_com1.log'],
  ['console.log', 'console.log'],
  ['  many    spaces  here .md', 'many spaces here .md'],
  ['café.txt', 'café.txt'],
  ['cafe\u0301.txt', 'café.txt'],
  ['日本語のファイル.pdf', '日本語のファイル.pdf'],
  [42, 'file'],
  [null, 'file'],
];
const nameMisses = names.filter(([i, want]) => F.cleanName(i) !== want).map(([i, want]) => `${JSON.stringify(i)} -> ${JSON.stringify(F.cleanName(i))}, want ${JSON.stringify(want)}`);
check(nameMisses.length === 0, `file names lose path separators, control and format characters, leading dots, trailing dots and Windows device names${nameMisses.map((m) => `\n     ${m}`).join('')}`);
const long = F.cleanName(`${'x'.repeat(300)}.tar.gz`);
check([...long].length === F.MAX_NAME && long.endsWith('.gz') && long.startsWith('xxx'), 'a long name is cut to 120 characters, keeping its extension');
const longEmoji = F.cleanName('😀'.repeat(200));
check([...longEmoji].length === F.MAX_NAME && !/\p{Cs}/u.test(longEmoji.replace(/\p{Extended_Pictographic}/gu, '')), 'a long name of emoji is cut on code points, with no half surrogate');
check(F.cleanName('a'.repeat(5000)).length === F.MAX_NAME, 'a name of 5000 characters is cut too');
const allClean = names.map(([i]) => F.cleanName(i)).concat(long, longEmoji)
  .every((n) => n.length > 0 && !/[\\/:*?"<>|\p{Cc}\p{Cf}]/u.test(n) && !n.startsWith('.') && !/[ .]$/.test(n));
check(allClean, 'no cleaned name is empty, holds a separator or control character, starts with a dot or ends with a dot or space');

// 7. The download header (sw.js) is printable ASCII, with the name in UTF-8 too.
const disp = F.disposition('naïve "quote"\r\nSet-Cookie: x=1;.txt');
check(/^[\x20-\x7e]+$/.test(disp) && !/\r|\n/.test(disp) && disp.startsWith('attachment; filename="') && disp.includes("filename*=UTF-8''"),
  `the Content-Disposition is printable ASCII: ${disp}`);
check(decodeURIComponent(F.disposition('日本(1).pdf').split("UTF-8''")[1]) === '日本(1).pdf' && !/[()']/.test(F.disposition('日本(1).pdf').split("UTF-8''")[1]),
  'its filename* decodes to the cleaned name, with ( ) \' escaped');

// 8. Pieces the receiver takes: only the next one, of the right length.
const t = F.parseOffer(F.makeOffer({ id, name: 'f', size: 3 * F.PIECE + 10 }));
const pc = (index, len) => ({ index, data: { length: len } });
check(F.checkPiece(t, 0, pc(0, F.PIECE)) === 'ok' && F.checkPiece(t, 3, pc(3, 10)) === 'ok', 'the next piece, full or the short last one, is taken');
check(F.checkPiece(t, 2, pc(1, F.PIECE)) === 'duplicate' && F.checkPiece(t, 2, pc(0, F.PIECE)) === 'duplicate', 'a piece it has is a duplicate');
check(F.checkPiece(t, 1, pc(2, F.PIECE)) === 'gap', 'a piece past the next is refused (pieces come in order)');
check(F.checkPiece(t, 4, pc(4, 10)) === 'out-of-range' && F.checkPiece(t, 0, pc(0xffffffff, 1)) === 'out-of-range', 'a piece past the last is out of range');
check(F.checkPiece(t, 0, pc(0, F.PIECE - 1)) === 'bad-length' && F.checkPiece(t, 3, pc(3, 11)) === 'bad-length' &&
  F.checkPiece(t, 3, pc(3, F.PIECE)) === 'bad-length', 'a piece of the wrong length is refused');
check(F.checkPiece(F.parseOffer({ ...offer, size: 0, count: 0 }), 0, pc(0, 0)) === 'out-of-range', 'an empty file takes no piece');
check(JSON.stringify(F.pieceRange(t, 0)) === `[0,${F.PIECE}]` && JSON.stringify(F.pieceRange(t, 3)) === `[${3 * F.PIECE},${3 * F.PIECE + 10}]`, 'pieceRange covers the file, the last piece short');

// 9. Resume: the sender goes on from what the receiver has written, never past what it read.
const r = F.parseOffer(F.makeOffer({ id, name: 'f', size: 10 * F.PIECE + 5 }));
check(JSON.stringify(F.resumeAt(r, 7, 4)) === JSON.stringify({ next: 4, offset: 4 * F.PIECE }), 'a resume at 4 of 7 read goes on from byte 4 × 512 KiB');
check(JSON.stringify(F.resumeAt(r, 11, 11)) === JSON.stringify({ next: 11, offset: r.size }) && F.resumeAt(r, 7, 0).offset === 0,
  'a resume with every piece goes on at the end (only the end is left to send); one with none at 0');
check([-1, 8, 12, 2.5, '3', null, undefined, NaN, Infinity].every((h) => F.resumeAt(r, 7, h) === null),
  'a resume past what was read, past the file, negative, fractional or not a number is refused');
check(F.intIn(5, 0, 5) === 5 && F.intIn(6, 0, 5) === null && F.intIn('5', 0, 5) === null, 'intIn takes whole numbers in range');

// 10. Which routes carry files.
const routes = [
  [{ kind: 'direct', rttMs: 3 }, true],
  [{ kind: 'direct', turn: true, rttMs: 3 }, false],
  [{ kind: 'direct', adopted: true, rttMs: 3 }, false],
  [{ kind: 'peer', via: 'ab', rttMs: 3 }, false],
  [{ kind: 'signaling', rttMs: 300 }, false],
  [null, false],
  [undefined, false],
];
check(routes.every(([route, ok]) => F.directForFiles(route) === ok), 'file data goes only over a direct route: not TURN, a punched channel, a member or the signalling relays');

console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
