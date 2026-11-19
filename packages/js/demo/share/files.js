// Peerwire Share's piece protocol (share.js): how a file goes from one member to another, as room messages on the
// reliable lane. No DOM here, so Node tests it (test/share.mjs).
//
// Every message is bytes (room.send of a Uint8Array); its first byte says what it is:
//
//   PIECE    kind(1) | transfer id(16) | index(4, big-endian) | SHA-256 of the data(32) | data
//   CONTROL  kind(1) | JSON, at most MAX_CONTROL bytes: { t: 'offer' | 'accept' | 'decline' | 'cancel' | 'ack' |
//            'end' | 'done', id, ... }
//
// A file is cut into pieces of `piece` bytes (the last may be shorter), each well under the reliable lane's largest
// message (MAX_FRAME, the room's tag byte included). The sender keeps at most WINDOW pieces it hasn't seen acked, so
// neither side holds more than a few MiB of a file of any size. The receiver takes pieces only in order and only for a
// transfer it accepted, checks each piece's SHA-256 (WebCrypto) and the whole file's (Sha256 below, as it streams),
// writes each piece before it acks it, and on a new link says how many pieces it has (`accept` with `have`), so the
// sender goes on from there (resumeAt).
//
// Everything a peer sends is untrusted: offers are checked and their names cleaned (parseOffer, cleanName), control
// messages over MAX_CONTROL bytes are never parsed, and a piece that isn't the next one, is the wrong length or fails
// its hash is refused (checkPiece).

export const PIECE_FRAME = 1;
export const CONTROL_FRAME = 2;
export const MAX_FRAME = 1 << 20; // the room's largest reliable message, its own tag byte included (room.js MAX_FRAME)
export const ID_BYTES = 16;
export const HEADER = 1 + ID_BYTES + 4 + 32;
export const PIECE = 512 * 1024; // what this page sends
export const MIN_PIECE = 256 * 1024; // what it takes from others
export const MAX_PIECE = MAX_FRAME - 1 - HEADER;
export const WINDOW = 8; // pieces in flight per transfer: 4 MiB at PIECE
export const MAX_QUEUED = 2 * WINDOW; // pieces a receiver holds unwritten; more means the sender ignores acks
export const MAX_SIZE = 2 ** 40; // 1 TiB
export const MAX_CONTROL = 4096; // bytes of a control message's JSON
export const MAX_NAME = 120; // characters (code points) of a file name, once cleaned
export const MAX_RAW_NAME = 1024; // characters of a name as sent: longer is no offer
export const MAX_TYPE = 100;

const enc = new TextEncoder();
const dec = new TextDecoder();

export const toHex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
export function fromHex(s) {
  const b = new Uint8Array(s.length / 2);
  for (let i = 0; i < b.length; i++) b[i] = parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return b;
}
const ID = /^[0-9a-f]{32}$/;
export const isId = (s) => typeof s === 'string' && ID.test(s);
export const newId = () => toHex(crypto.getRandomValues(new Uint8Array(ID_BYTES)));

// A file name from anyone, safe to show and to save under: no control or format characters (bidi overrides,
// zero-width ones), no lone surrogates, no path separators or other characters Windows forbids (each becomes _), no
// leading dots (no hidden file, no '..') or trailing dots and spaces, no Windows device name (CON, NUL, COM1...),
// whitespace collapsed, at most MAX_NAME characters with a short extension kept. 'file' if nothing is left.
export function cleanName(raw) {
  if (typeof raw !== 'string') return 'file';
  let s = raw.slice(0, MAX_RAW_NAME).replace(/\p{Cs}/gu, '').normalize('NFC')
    .replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, '')
    .replace(/[\\/:*?"<>|]/g, '_')
    .replace(/\s+/g, ' ')
    .replace(/^[\s.]+/, '')
    .replace(/[\s.]+$/, '');
  if (/^(con|prn|aux|nul|com[0-9¹²³]|lpt[0-9¹²³])(\.|$)/i.test(s)) s = `_${s}`;
  const cps = [...s];
  if (cps.length > MAX_NAME) {
    const dot = s.lastIndexOf('.');
    const ext = dot > 0 && s.length - dot <= 16 ? [...s.slice(dot)] : [];
    s = cps.slice(0, MAX_NAME - ext.length).join('').replace(/[\s.]+$/, '') + ext.join('');
  }
  return s || 'file';
}

// A MIME type to show: a plain type/subtype, else ''. Never used to open or save anything.
export function cleanType(raw) {
  return typeof raw === 'string' && raw.length <= MAX_TYPE && /^[a-z0-9][\w.+-]*\/[\w.+-]+$/i.test(raw) ? raw.toLowerCase() : '';
}

// How many pieces a file of `size` bytes takes.
export const pieceCount = (size, piece) => (size === 0 ? 0 : Math.ceil(size / piece));
// The bytes [start, end) piece `i` covers.
export function pieceRange({ size, piece }, i) {
  const start = i * piece;
  return [start, Math.min(size, start + piece)];
}

// An offer we send: what parseOffer takes back.
export function makeOffer({ id, name, size, type }, piece = PIECE) {
  return { t: 'offer', v: 1, id, name: cleanName(name), size, type: cleanType(type), piece, count: pieceCount(size, piece) };
}

// A peer's offer as { id, name, size, type, piece, count }, or null if it isn't a valid one. The name comes back
// cleaned, the type checked.
export function parseOffer(m) {
  if (!m || m.t !== 'offer' || !isId(m.id)) return null;
  const { size, piece } = m;
  if (!Number.isSafeInteger(size) || size < 0 || size > MAX_SIZE) return null;
  if (!Number.isSafeInteger(piece) || piece < MIN_PIECE || piece > MAX_PIECE) return null;
  const count = pieceCount(size, piece);
  if (m.count !== undefined && m.count !== count) return null;
  if (typeof m.name !== 'string' || m.name.length > MAX_RAW_NAME) return null;
  if (m.type !== undefined && typeof m.type !== 'string') return null;
  return { id: m.id, name: cleanName(m.name), size, type: cleanType(m.type), piece, count };
}

// A whole number from `lo` to `hi`, else null.
export const intIn = (v, lo, hi) => (Number.isSafeInteger(v) && v >= lo && v <= hi ? v : null);
export const isSha = (s) => typeof s === 'string' && /^[0-9a-f]{64}$/.test(s);

// A piece as one message. `hash` is the SHA-256 of `data` (32 bytes).
export function encodePiece(id, index, hash, data) {
  const f = new Uint8Array(HEADER + data.length);
  f[0] = PIECE_FRAME;
  f.set(fromHex(id), 1);
  new DataView(f.buffer).setUint32(1 + ID_BYTES, index);
  f.set(hash, 1 + ID_BYTES + 4);
  f.set(data, HEADER);
  return f;
}

// A piece message as { id, index, hash, data } (views into `f`), or null if it isn't one.
export function decodePiece(f) {
  if (!(f instanceof Uint8Array) || f.length < HEADER || f[0] !== PIECE_FRAME) return null;
  return {
    id: toHex(f.subarray(1, 1 + ID_BYTES)),
    index: new DataView(f.buffer, f.byteOffset, f.length).getUint32(1 + ID_BYTES),
    hash: f.subarray(1 + ID_BYTES + 4, HEADER),
    data: f.subarray(HEADER),
  };
}

// A control message as one message; RangeError over MAX_CONTROL bytes.
export function encodeControl(m) {
  const body = enc.encode(JSON.stringify(m));
  if (body.length > MAX_CONTROL) throw new RangeError(`control message of ${body.length} bytes`);
  const f = new Uint8Array(1 + body.length);
  f[0] = CONTROL_FRAME;
  f.set(body, 1);
  return f;
}

// A control message's object, or null: not one, over MAX_CONTROL bytes (never parsed), not JSON, or not an object
// with a string `t`.
export function decodeControl(f) {
  if (!(f instanceof Uint8Array) || f[0] !== CONTROL_FRAME || f.length > 1 + MAX_CONTROL) return null;
  try {
    const m = JSON.parse(dec.decode(f.subarray(1)));
    return m && typeof m === 'object' && !Array.isArray(m) && typeof m.t === 'string' ? m : null;
  } catch {
    return null;
  }
}

// Whether piece `p` ({ index, data }) is the one transfer `t` ({ size, piece, count }) takes next, `next` being the
// index it expects (pieces it has, and those it holds still unwritten): 'ok', or why not. Pieces come in order on the
// reliable lane, so anything else is refused: 'duplicate' (one it has), 'gap' (one past the next), 'out-of-range'
// (past the file's last), 'bad-length' (not the piece's size).
export function checkPiece(t, next, p) {
  if (p.index >= t.count) return 'out-of-range';
  if (p.index < next) return 'duplicate';
  if (p.index > next) return 'gap';
  const [start, end] = pieceRange(t, p.index);
  return p.data.length === end - start ? 'ok' : 'bad-length';
}

// Whether two hashes (Uint8Array or ArrayBuffer) are the same.
export function sameHash(a, b) {
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  if (x.length !== y.length) return false;
  let d = 0;
  for (let i = 0; i < x.length; i++) d |= x[i] ^ y[i];
  return d === 0;
}

// Where a sender goes on from when the receiver says it has `have` pieces of transfer `t` ({ count, piece, size }),
// of which the sender has read (and hashed) `read`: { next, offset }, or null if `have` can't be (not a whole number,
// more than the file has, or more than was ever sent).
export function resumeAt(t, read, have) {
  if (intIn(have, 0, Math.min(t.count, read)) === null) return null;
  return { next: have, offset: Math.min(t.size, have * t.piece) };
}

// Whether a route (room.route(peer)) may carry file data: the peer's own WebRTC DataChannel, not through TURN, and not
// a channel adopted from a birthday punch, whose link goes back to its fallback (the signalling relays) when the
// channel closes, taking the data not yet acknowledged with it. A direct link that closes ends its session instead.
export const directForFiles = (r) => r?.kind === 'direct' && !r.turn && !r.adopted;

// SHA-256 over a stream of bytes, as WebCrypto has none: update() with each piece in order, then digest(). About
// 150 MB/s on one core of the test host, so share.js runs it in a worker (hash-worker.js).
const K = new Int32Array([
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01,
  0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
  0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
  0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
  0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08,
  0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
  0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
]);
const W = new Int32Array(64);

export class Sha256 {
  constructor() {
    this.h = new Int32Array([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
    this.buf = new Uint8Array(64); // a block begun
    this.used = 0;
    this.bytes = 0;
  }

  update(data) {
    let off = 0;
    this.bytes += data.length;
    if (this.used) {
      off = Math.min(64 - this.used, data.length);
      this.buf.set(data.subarray(0, off), this.used);
      this.used += off;
      if (this.used < 64) return this;
      this.blocks(this.buf, 0, 64);
      this.used = 0;
    }
    const whole = off + ((data.length - off) & ~63);
    this.blocks(data, off, whole);
    this.buf.set(data.subarray(whole), 0);
    this.used = data.length - whole;
    return this;
  }

  // The hash (32 bytes); the hasher is spent.
  digest() {
    const bits = this.bytes * 8;
    const tail = new Uint8Array(this.used < 56 ? 64 : 128);
    tail.set(this.buf.subarray(0, this.used));
    tail[this.used] = 0x80;
    const v = new DataView(tail.buffer);
    v.setUint32(tail.length - 8, Math.floor(bits / 2 ** 32));
    v.setUint32(tail.length - 4, bits >>> 0);
    this.blocks(tail, 0, tail.length);
    const out = new Uint8Array(32);
    const o = new DataView(out.buffer);
    for (let i = 0; i < 8; i++) o.setInt32(i * 4, this.h[i]);
    return out;
  }

  blocks(b, off, end) {
    const h = this.h;
    const w = W;
    let [h0, h1, h2, h3, h4, h5, h6, h7] = h;
    for (; off < end; off += 64) {
      for (let i = 0; i < 16; i++) {
        const j = off + i * 4;
        w[i] = (b[j] << 24) | (b[j + 1] << 16) | (b[j + 2] << 8) | b[j + 3];
      }
      for (let i = 16; i < 64; i++) {
        const x = w[i - 15];
        const y = w[i - 2];
        const s0 = ((x >>> 7) | (x << 25)) ^ ((x >>> 18) | (x << 14)) ^ (x >>> 3);
        const s1 = ((y >>> 17) | (y << 15)) ^ ((y >>> 19) | (y << 13)) ^ (y >>> 10);
        w[i] = (s0 + w[i - 16] + s1 + w[i - 7]) | 0;
      }
      let a = h0, bb = h1, c = h2, d = h3, e = h4, f = h5, g = h6, hh = h7;
      for (let i = 0; i < 64; i++) {
        const S1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
        const t1 = (hh + S1 + ((e & f) ^ (~e & g)) + K[i] + w[i]) | 0;
        const S0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
        const t2 = (S0 + ((a & bb) ^ (a & c) ^ (bb & c))) | 0;
        hh = g; g = f; f = e; e = (d + t1) | 0; d = c; c = bb; bb = a; a = (t1 + t2) | 0;
      }
      h0 = (h0 + a) | 0; h1 = (h1 + bb) | 0; h2 = (h2 + c) | 0; h3 = (h3 + d) | 0;
      h4 = (h4 + e) | 0; h5 = (h5 + f) | 0; h6 = (h6 + g) | 0; h7 = (h7 + hh) | 0;
    }
    h[0] = h0; h[1] = h1; h[2] = h2; h[3] = h3; h[4] = h4; h[5] = h5; h[6] = h6; h[7] = h7;
  }
}

// A name for a download's Content-Disposition (sw.js): the cleaned name as UTF-8 (RFC 6266 filename*), and an ASCII
// fallback. cleanName has taken out quotes, backslashes and control characters, so neither can break the header.
export function disposition(name) {
  const n = cleanName(name);
  const ascii = n.replace(/[^\x20-\x7e]/g, '_').replace(/["\\%]/g, '_');
  const utf8 = encodeURIComponent(n).replace(/['()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
  return `attachment; filename="${ascii}"; filename*=UTF-8''${utf8}`;
}
