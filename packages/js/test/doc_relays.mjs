// room.doc(name, { relays: true }) (src/relays.js): a doc's sealed copy on the signalling relays, so a member that
// opens it while no other member is online still gets it (docs/STORE-DESIGN.md, route 1). All in this process: relays
// that keep events as strfry does (an addressable event replaced only by a newer created_at, none taken over 64 KiB
// or more than 15 min ahead, NIP-40 expiry, NIP-09 deletion), members on a fake network and clock as in
// doc_crdt.mjs, and the real wasm signer. Checks:
// - an author alone saves; a joiner alone loads it, 'sync' saying from the relays and as of when; nothing in clear;
// - sizes: a whiteboard in a few pieces each within 60 KiB of content and 1 MiB inflated; a doc of more than 8
//   pieces, or with one entry past a piece, keeps no relay copy and warns once; a piece holds 16 frames at most;
// - both banks: each save writes the bank not holding the newest whole save, which stands meanwhile;
// - torn saves: a reader joins only whole saves, and saving after a torn read loses nothing;
// - a relay that took only part of a save keeps its own newest whole save while the next is written (NB-006g-9a);
// - bomb pieces: one inflates to 1 MiB at most, and a read stops at MAX_STORED;
// - a relay that ignores the filter: one event per slot kept, the newest;
// - a relay serving created_at a year ahead fails our save on that relay only (NB-006g-9b), and can't set sync.at;
// - another VERSION in a bank: that bank is left alone on that relay, and a relay with one in both is skipped;
// - a member sending t: 5 without saving holds a save back once, 5 min at most, then is ignored until it rejoins
//   (NB-006g-8), and its t: 5 during our read then holds nothing back; one naming a save already made is ignored
//   at once; one saver per room; t: 5 dropped by a doc without relays;
// - a save whose read no relay completed stays due: saved at the next slot, or when the page hides;
// - renewal: opening a doc whose copy expires within 7 days saves it again;
// - a deletion joined from the relays is reported from 'relays';
// - the last member online saves when the page hides, once a minute at most;
// - forget() wipes the slots: empty pieces, then a NIP-09 deletion; no saves after;
// - Room: relays: true needs signaling 'nostr' and a roomId of randomRoomId()'s shape; then end to end over the real
//   NostrSignaling and fake relay sockets: a member saves alone and leaves, another joins alone and gets the doc,
//   the pieces one a second per relay.
//
//   node packages/js/test/doc_relays.mjs

import { loadPeerwire } from '../src/peerwire.js';
import { RoomProof, randomRoomId } from '../src/proof.js';
import { Doc } from '../src/doc.js';
import { joinRoom } from '../src/index.js';
import { RelayCopy, KIND, SLOTS, BANKS, VERSION, QUIET_MS, JITTER_MS, SAVE_EVERY_MS, DEFER_MS, EXPIRY_S, RENEW_S, AHEAD_S, READ_MS, MAX_PIECE_CONTENT, PIECE_INFLATED, PIECE_DEFLATED, HIDE_EVERY_MS } from '../src/relays.js';
import { MAX_STORED } from '../src/store.js';
import { b64, unb64, STORE_EVERY_MS } from '../src/nostr.js';
import { deflate } from '../src/zip.js';

let failed = 0;
const fail = (m) => {
  console.error(`FAIL ${m}`);
  failed++;
};
const ok = (m) => console.log(`ok   ${m}`);
const check = (cond, m) => (cond ? ok(m) : fail(m));
const id = (c) => c.repeat(64);
const enc = new TextEncoder();
const dec = new TextDecoder();
const DAY = 86400000;
const WALL0 = Date.UTC(2026, 9, 30, 12);

// mulberry32, for the jitter (Math.random) and the doc's incarnations: every run the same.
function rng(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
Math.random = rng(1);
const warnings = [];
console.warn = (...a) => warnings.push(a.join(' '));

const pw = await loadPeerwire(new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname);
const ROOM = randomRoomId();
const proof = await RoomProof.create(`peerwire-test/${ROOM}`);

// A relay as strfry keeps events. `wall` gives its clock (ms). `refuse(ev)`: a reason to refuse it, or null.
// `ignoresFilter`: serves everything it holds and `junk`, whatever the filter. `put(ev)` stores an event as a
// relay that ignores the rules would serve it.
class Relay {
  constructor(url, wall) {
    this.url = url;
    this.wall = wall;
    this.held = new Map(); // kind:pubkey:d -> event
    this.log = []; // events taken, in order: [event, wall ms]
    this.refuse = null;
    this.down = false;
    this.ignoresFilter = false;
    this.junk = [];
  }

  now() {
    return Math.floor(this.wall() / 1000);
  }

  // NIP-01's answer: [taken, why].
  publish(ev) {
    this.attempts = (this.attempts ?? 0) + 1;
    const why = this.refuse?.(ev);
    if (why) return [false, why];
    if (JSON.stringify(ev).length > 65536) return [false, 'invalid: event too large'];
    if (ev.created_at > this.now() + 900) return [false, 'invalid: created_at too late'];
    this.log.push([ev, this.wall()]);
    if (ev.kind === 5) {
      for (const [t, v] of ev.tags) {
        for (const [key, old] of this.held) {
          if (old.pubkey !== ev.pubkey || old.created_at > ev.created_at) continue;
          if ((t === 'a' && key === v) || (t === 'e' && old.id === v)) this.held.delete(key);
        }
      }
      return [true, ''];
    }
    const d = ev.tags.find((t) => t[0] === 'd')?.[1] ?? '';
    const key = `${ev.kind}:${ev.pubkey}:${d}`;
    const old = this.held.get(key);
    // Older than what it holds: taken, and dropped.
    if (old && (old.created_at > ev.created_at || (old.created_at === ev.created_at && old.id <= ev.id))) return [true, 'duplicate: have a newer event'];
    this.held.set(key, ev);
    return [true, ''];
  }

  put(ev) {
    const d = ev.tags.find((t) => t[0] === 'd')?.[1] ?? '';
    this.held.set(`${ev.kind}:${ev.pubkey}:${d}`, ev);
  }

  // What it serves for `filter` (kinds, authors, '#d'): NIP-40 expired events are not.
  serve(f) {
    const now = this.now();
    const live = [...this.held.values()].filter((ev) => {
      const exp = Number(ev.tags.find((t) => t[0] === 'expiration')?.[1] ?? Infinity);
      return !(exp <= now);
    });
    if (this.ignoresFilter) return [...this.junk, ...live];
    return live.filter((ev) => (!f.kinds || f.kinds.includes(ev.kind)) && (!f.authors || f.authors.includes(ev.pubkey)) && (!f['#d'] || f['#d'].includes(ev.tags.find((t) => t[0] === 'd')?.[1])));
  }

  // Our signer's events, by slot index.
  slots(k) {
    const out = new Map();
    for (const ev of this.held.values()) if (ev.pubkey === k.pubkey && ev.kind === KIND) out.set(k.index.get(ev.tags.find((t) => t[0] === 'd')[1]), ev);
    return out;
  }
}

// The room's signalling as relays.js uses it (query, store), over `relays`, at once.
class Sig {
  constructor(relays) {
    this.relays = relays;
    this.queries = 0;
  }

  async query(filter, { keep }) {
    this.queries++;
    const out = new Map();
    for (const r of this.relays) {
      const res = { complete: false, events: new Map() };
      out.set(r.url, res);
      await null;
      if (r.down) continue;
      for (const ev of r.serve(filter)) {
        const slot = keep(structuredClone(ev));
        if (slot === null || slot === undefined) continue;
        const old = res.events.get(slot);
        if (!old || ev.created_at > old.created_at || (ev.created_at === old.created_at && ev.id < old.id)) res.events.set(slot, ev);
      }
      res.complete = !r.noEose;
    }
    return out;
  }

  store(ev, urls) {
    return new Map(urls.map((url) => [url, (async () => {
      await null;
      const r = this.relays.find((x) => x.url === url);
      return !!r && !r.down && r.publish(structuredClone(ev))[0];
    })()]));
  }
}

// Members of one room, as in doc_crdt.mjs: frames delivered in order per link, timers on a fake clock. Each member's
// doc keeps a relay copy over `sig`, unless opened with { relays: false }.
class Net {
  constructor(relayCount = 3) {
    this.now = 0;
    this.timers = [];
    this.flight = [];
    this.nodes = new Map();
    this.r = rng(7);
    this.relays = Array.from({ length: relayCount }, (_, i) => new Relay(`wss://r${i + 1}.test`, () => this.wall()));
    this.sig = new Sig(this.relays);
  }

  wall() {
    return WALL0 + this.now;
  }

  node(c, { relays = true } = {}) {
    const self = id(c);
    const n = { self, peers: new Set(), doc: null, copy: relays ? new RelayCopy(this.sig, pw, proof, 'board') : null, events: [] };
    n.host = {
      self,
      peers: () => [...n.peers],
      send: (to, m) => this.flight.push([self, to, JSON.stringify(m)]),
      after: (ms, fn) => {
        const t = { at: this.now + ms, fn };
        this.timers.push(t);
        return () => (t.fn = null);
      },
      now: () => this.now,
      wall: () => this.wall(),
      incarnation: () => Math.floor(this.r() * 2 ** 32).toString(16).padStart(8, '0'),
      relays: n.copy,
    };
    n.doc = new Doc('board', n.host);
    n.doc.on('change', (ev) => n.events.push(ev));
    n.doc.open();
    this.nodes.set(self, n);
    return n;
  }

  link(a, b) {
    a.peers.add(b.self);
    b.peers.add(a.self);
    a.doc.joined(b.self);
    b.doc.joined(a.self);
  }

  unlink(a, b) {
    a.peers.delete(b.self);
    b.peers.delete(a.self);
    this.flight = this.flight.filter(([f, t]) => !((f === a.self && t === b.self) || (f === b.self && t === a.self)));
    a.doc.left(b.self);
    b.doc.left(a.self);
  }

  step() {
    const f = this.flight.shift();
    if (!f) return false;
    const n = this.nodes.get(f[1]);
    if (n?.peers.has(f[0]) && !n.doc.closed) n.doc.receive(f[0], JSON.parse(f[2]));
    return true;
  }

  // Lets frames and the relay work under way (reads, saves: real promises) run until all is quiet. Fires no timer.
  async settle() {
    for (let quiet = 0; quiet < 2;) {
      while (this.step());
      const busy = [...this.nodes.values()].map((n) => n.copy?.busy).filter(Boolean);
      await Promise.all(busy);
      await new Promise((r) => setImmediate(r));
      quiet = busy.length || this.flight.length ? 0 : quiet + 1;
    }
  }

  // Lets `ms` pass on the fake clock: each timer due fires in turn, and what it starts runs before the next.
  async pass(ms) {
    const end = this.now + ms;
    await this.settle();
    for (;;) {
      this.timers = this.timers.filter((t) => t.fn);
      const t = this.timers.reduce((a, b) => (!a || b.at < a.at ? b : a), null);
      if (!t || t.at > end) break;
      this.now = Math.max(this.now, t.at);
      const fn = t.fn;
      t.fn = null;
      fn();
      await this.settle();
    }
    this.now = end;
  }

  // Everything the relays took from our signer, by kind: [kind 30078 events, kind 5 events].
  taken(k) {
    const all = this.relays.flatMap((r) => r.log.map(([ev]) => ev)).filter((ev) => ev.pubkey === k.pubkey);
    return [all.filter((ev) => ev.kind === KIND), all.filter((ev) => ev.kind === 5)];
  }
}

// A doc's strokes, meta and log, comparable across members.
const view = (doc) => JSON.stringify([doc.set('strokes').entries().map((e) => [e.id, e.value]), doc.map('meta').entries().map((e) => [e.key, e.value])]);
// Strokes that wander like a hand's, as the whiteboard stores them: points in board units rounded to 0.1.
let seq = 0;
const hand = rng(3);
function stroke(points) {
  let x = hand() * 3000 - 1500;
  let y = hand() * 1800 - 900;
  let a = hand() * 2 * Math.PI;
  const p = [];
  for (let j = 0; j < points; j++) {
    a += (hand() - 0.5) * 0.6;
    x += Math.cos(a) * 3;
    y += Math.sin(a) * 3;
    p.push(Math.round(x * 10) / 10, Math.round(y * 10) / 10);
  }
  return { k: 'pen', n: seq++, p };
}
function draw(doc, n, points = 8) {
  const ids = [];
  for (let i = 0; i < n; i++) ids.push(doc.set('strokes').add(stroke(points)));
  return ids;
}
const SAVE_WAIT = QUIET_MS + JITTER_MS + 1000;
// From one save to the next while the doc changes: SAVE_EVERY_MS, then the jitter.
const NEXT = SAVE_EVERY_MS + JITTER_MS + 1000;
const keysOf = (n) => n.copy.keys();
// A relay's whole saves, as relays.js reads them: s -> banks.
async function whole(net, relay, k, reader) {
  const read = await reader.copy.read();
  const slots = read.relays.get(relay.url).slots;
  const saves = new Map();
  slots.forEach((slot, j) => {
    const p = slot?.piece;
    if (!p?.n) return;
    const sv = saves.get(p.s) ?? { n: p.n, is: new Set(), banks: new Set() };
    sv.is.add(p.i);
    sv.banks.add(Math.floor(j / SLOTS));
    saves.set(p.s, sv);
  });
  for (const [s, sv] of saves) if (sv.is.size !== sv.n) saves.delete(s);
  return saves;
}

// The note's numbers, and the keys derived again here from its byte layout (a vector for other implementations).
{
  const pinned = { KIND: [KIND, 30078], VERSION: [VERSION, 1], BANKS: [BANKS, 2], SLOTS: [SLOTS, 8], PIECE_DEFLATED: [PIECE_DEFLATED, 45056], PIECE_INFLATED: [PIECE_INFLATED, 1048576], MAX_PIECE_CONTENT: [MAX_PIECE_CONTENT, 61440], READ_MS: [READ_MS, 10000], QUIET_MS: [QUIET_MS, 10000], JITTER_MS: [JITTER_MS, 30000], SAVE_EVERY_MS: [SAVE_EVERY_MS, 300000], DEFER_MS: [DEFER_MS, 300000], HIDE_EVERY_MS: [HIDE_EVERY_MS, 60000], EXPIRY_S: [EXPIRY_S, 2592000], RENEW_S: [RENEW_S, 604800], AHEAD_S: [AHEAD_S, 900], MAX_STORED: [MAX_STORED, 64 * 512 * 1024 + 1024] };
  const off = Object.entries(pinned).filter(([, [v, want]]) => v !== want).map(([k, [v]]) => `${k} ${v}`);
  check(!off.length, `the note's numbers: 30078, 2 banks of 8, 44 KiB / 1 MiB / 60 KiB a piece, 10 s, 10 + 30 s, 5 min, 5 min, 1 min, 30 d, 7 d, 15 min, 32 MiB a read${off.length ? ` (off: ${off})` : ''}`);
  const room = await RoomProof.create('vector-app/AAAAAAAAAAAAAAAAAAAAAA');
  const raw = new Uint8Array(await crypto.subtle.digest('SHA-256', enc.encode('peerwire/room-key/vector-app/AAAAAAAAAAAAAAAAAAAAAA')));
  const hmac = async (key, bytes) => new Uint8Array(await crypto.subtle.sign('HMAC', await crypto.subtle.importKey('raw', key, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']), bytes));
  const hex = (b) => Buffer.from(b).toString('hex');
  const name = 'boardé';
  const e = [0, 0, 0, 0];
  const lab = (l, ...rest) => Uint8Array.from([...enc.encode(l), ...enc.encode(name), ...e, ...rest]);
  const copy = new RelayCopy(null, pw, room, name);
  const k = await copy.keys();
  const slots = [];
  for (let b = 0; b < 2; b++) for (let i = 0; i < 8; i++) slots.push(hex(await hmac(raw, lab('pw-snap-id', b, i))));
  const seed = await hmac(raw, lab('pw-snap-sk'));
  const signer = pw.nostrKeypair(seed);
  const sealKey = await hmac(raw, lab('pw-snap-key'));
  const iv = new Uint8Array(12);
  const ct = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: Uint8Array.from([1, ...enc.encode(slots[9])]) }, await crypto.subtle.importKey('raw', sealKey, 'AES-GCM', false, ['encrypt']), enc.encode('x')));
  const sealed = b64(Uint8Array.from([1, ...iv, ...ct]));
  const opened = await copy.unseal(sealed, 9, k, Infinity);
  const wrongSlot = await copy.unseal(sealed, 8, k, Infinity);
  check(slots.join() === k.slots.join() && hex(signer.pubkey) === k.pubkey && opened.used > 0 && !opened.piece && wrongSlot.used === 0,
    `keys from the note's layout (label || UTF-8 name || 4-byte epoch [|| bank || slot]): slot 0 ${slots[0].slice(0, 16)}…, slot 15 ${slots[15].slice(0, 16)}…, signer ${k.pubkey.slice(0, 16)}…; content sealed by hand for slot 9 opens there (to no piece: it isn't deflated) and not in slot 8`);
  check(pw.nostrKeypair(new Uint8Array(32)) === null && pw.nostrKeypair(new Uint8Array(32).fill(255)) === null && pw.nostrKeypair(seed).pubkey.join() === signer.pubkey.join(), 'a seed that is no secret key (zero, past the order) gives null; a seed, the same key every time');
}

// An author alone saves; a joiner alone loads it.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  const strokes = draw(a.doc, 50);
  a.doc.map('meta').set('title', 'Plans for the summer');
  for (const x of strokes.slice(0, 5)) a.doc.set('strokes').delete(x);
  await net.pass(QUIET_MS);
  check(net.taken(k)[0].length === 0, 'nothing saved while the doc changes or within the quiet');
  await net.pass(JITTER_MS + 1000);
  const [evs] = net.taken(k);
  check(evs.length === 3 && net.relays.every((r) => r.slots(k).size === 1 && r.slots(k).has(0)), `one piece on each relay, in bank 0 slot 0 (${evs.length} events)`);
  const ev = evs[0];
  const exp = Number(ev.tags.find((t) => t[0] === 'expiration')[1]);
  check(ev.kind === 30078 && ev.tags.length === 2 && ev.tags[0][1] === k.slots[0] && Math.abs(exp - ev.created_at - EXPIRY_S) <= 60, 'kind 30078, tags d and expiration 30 days on, nothing else');
  const bytes = unb64(ev.content);
  const clear = dec.decode(bytes) + ev.content + JSON.stringify(ev.tags);
  check(bytes[0] === 1 && !['board', 'strokes', 'Plans', 'meta', ROOM, 'peerwire'].some((s) => clear.includes(s)), 'the event shows neither the doc, the room nor what it holds');
  check(a.copy.last?.n === 1 && [...a.copy.last.wrote.values()].every(Boolean), 'the save took on every relay');
  const reads = net.sig.queries;
  const b = net.node('b');
  await net.settle();
  check(view(b.doc) === view(a.doc) && b.doc.set('strokes').entries().length === 45, 'a joiner alone holds the doc from the relays');
  check(b.doc.sync.state === 'alone' && b.doc.sync.source === 'relays' && b.doc.sync.at === ev.created_at * 1000, `its sync: from the relays, as of the save (${JSON.stringify(b.doc.sync)})`);
  check(b.events.length && b.events.every((e) => e.from === a.self), "joined entries keep their writer as 'from'");
  const before = net.taken(k)[0].length;
  await net.pass(SAVE_EVERY_MS + SAVE_WAIT);
  check(net.taken(k)[0].length === before && net.sig.queries === reads + 1, 'a joiner that holds nothing more reads once and saves nothing');

  const off = net.node('c', { relays: false });
  await net.settle();
  check(off.doc.set('strokes').entries().length === 0 && off.doc.sync.source === null, 'a doc without relays reads none');
}

// When: 10 s without a change, then the jitter (0 to 30 s); 5 minutes from one save to the next.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  const random = Math.random;
  Math.random = () => 0.999;
  draw(a.doc, 3);
  await net.pass(39960);
  const before = net.taken(k)[0].length;
  await net.pass(20);
  check(before === 0 && net.taken(k)[0].length === 3, 'saved 10 s after the change plus the jitter (29.97 s here), no sooner');
  Math.random = () => 0;
  draw(a.doc, 1);
  await net.pass(298000);
  const early = net.taken(k)[0].length;
  await net.pass(3000);
  check(early === 3 && net.taken(k)[0].length === 6, 'a change just after a save: saved 5 minutes after that save, no sooner');
  await net.pass(300000);
  draw(a.doc, 1);
  await net.pass(9990);
  const quiet = net.taken(k)[0].length;
  await net.pass(20);
  check(quiet === 6 && net.taken(k)[0].length === 9, 'with no jitter and the last save long past: 10 s after the change, no sooner');
  Math.random = random;
}

// Sizes: a whiteboard goes in a few pieces; a doc past 8 pieces, or an entry past a piece, keeps no relay copy.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 1400, 20);
  await net.pass(SAVE_WAIT);
  const evs = net.relays[0].slots(k);
  const reader = net.node('7');
  await net.settle();
  const read = await reader.copy.read();
  const pieces = read.relays.get(net.relays[0].url).slots.filter(Boolean);
  const sizes = [...evs.values()].map((ev) => ev.content.length);
  check(evs.size >= 2 && evs.size <= SLOTS && sizes.every((n) => n <= MAX_PIECE_CONTENT) && [...evs.values()].every((ev) => JSON.stringify(ev).length < 65536), `a whiteboard of 1400 strokes: ${evs.size} pieces of ${sizes.join(', ')} characters`);
  check(pieces.every((s) => s.piece && JSON.stringify(s.piece).length <= PIECE_INFLATED), 'each inflates within 1 MiB');
  check(view(reader.doc) === view(a.doc), 'a reader holds all of it');
  const big = new Net();
  const x = big.node('6');
  await big.settle();
  const kx = await keysOf(x);
  for (let i = 0; i < 12; i++) x.doc.map('blobs').set(String(i), b64(crypto.getRandomValues(new Uint8Array(30000))));
  warnings.length = 0;
  await big.pass(SAVE_WAIT);
  x.doc.map('blobs').set('more', 'x');
  await big.pass(SAVE_EVERY_MS + SAVE_WAIT);
  check(big.taken(kx)[0].length === 0 && warnings.length === 1 && /too large to keep on the relays \(more than 8 pieces\)/.test(warnings[0]), `a doc of 12 random 40 K-character values: no relay copy, one warning (${warnings.join(' | ')})`);
  const one = new Net();
  const y = one.node('5');
  await one.settle();
  const ky = await keysOf(y);
  y.doc.map('blobs').set('x', b64(crypto.getRandomValues(new Uint8Array(48000))));
  warnings.length = 0;
  await one.pass(SAVE_WAIT);
  check(one.taken(ky)[0].length === 0 && /an entry too large for a piece/.test(warnings[0] ?? ''), 'one entry that deflates past a piece: no relay copy, a warning');
  // A pack() that cuts a frame per entry: a piece still holds 16 frames at most, as readers take no more.
  const many = new Net();
  const z = many.node('4');
  await many.settle();
  const items = z.doc.relayItems.bind(z.doc);
  z.doc.relayItems = () => {
    const it = items();
    return { ...it, pack: (entries, ranges) => [...entries.flatMap((e) => it.pack([e], [])), ...(ranges.length ? it.pack([], ranges) : [])] };
  };
  draw(z.doc, 17);
  await many.pass(SAVE_WAIT);
  const zr = many.node('3');
  await many.settle();
  const zs = (await zr.copy.read()).relays.get(many.relays[0].url).slots.filter((slot) => slot?.piece);
  check(z.copy.last?.n === 2 && zs.length === 2 && zs.every((slot) => slot.piece.f.length <= 16) && view(zr.doc) === view(z.doc), `a pack() of one frame per entry: 17 strokes in ${z.copy.last?.n} pieces of ${zs.map((slot) => slot.piece.f.length).join(', ')} frames, and a reader holds them all`);
}

// Both banks: each save writes the bank that doesn't hold the newest whole save.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 900, 20);
  await net.pass(SAVE_WAIT);
  const r = net.relays[0];
  const s1 = await whole(net, r, k, a);
  draw(a.doc, 5);
  await net.pass(NEXT);
  const s2 = await whole(net, r, k, a);
  draw(a.doc, 5);
  await net.pass(NEXT);
  const s3 = await whole(net, r, k, a);
  const [one] = s1;
  const two = [...s2].find(([s]) => s !== one[0]);
  const three = [...s3].find(([s]) => s !== two?.[0]);
  check(s1.size === 1 && one[1].banks.has(0) && one[1].n >= 2, `the first save in bank 0 (${one[1].n} pieces)`);
  check(s2.size === 2 && s2.has(one[0]) && two?.[1].banks.has(1), 'the second in bank 1, the first standing in bank 0');
  check(s3.size === 2 && s3.has(two[0]) && three?.[1].banks.has(0) && !s3.has(one[0]), 'the third in bank 0 again, the second standing in bank 1');
}

// Torn saves: a reader joins only whole saves, and saving after a torn read loses nothing.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  const first = draw(a.doc, 900, 20);
  await net.pass(SAVE_WAIT);
  const atFirst = view(a.doc);
  const n1 = a.copy.last.n;
  for (const x of first.slice(0, 20)) a.doc.set('strokes').delete(x);
  draw(a.doc, 300, 20);
  // Every relay takes the first piece of the second save, then refuses.
  for (const r of net.relays) {
    let taken = 0;
    r.refuse = (ev) => (ev.pubkey === k.pubkey && ++taken > 1 ? 'rate-limited: slow down' : null);
  }
  await net.pass(NEXT);
  for (const r of net.relays) r.refuse = null;
  check(n1 >= 2 && a.copy.last.n >= 2 && [...a.copy.last.wrote.values()].every((x) => !x), `the second save torn after its first piece on every relay (${a.copy.last.n} pieces)`);
  a.doc.close();
  const b = net.node('b');
  await net.settle();
  check(view(b.doc) === atFirst, 'a reader holds the first save as it was: no entry of the torn one, no deletion undone or added');
  const z = net.node('e');
  await net.settle();
  const own = draw(z.doc, 10);
  await net.pass(SAVE_WAIT);
  const d = net.node('d');
  await net.settle();
  const held = new Set(d.doc.set('strokes').entries().map((e) => e.id));
  const want = new Set(b.doc.set('strokes').entries().map((e) => e.id));
  check([...want].every((x) => held.has(x)) && own.every((x) => held.has(x)) && held.size === want.size + 10, `saving after the torn read loses nothing: ${held.size} of ${want.size} + 10`);
}

// A relay that took only part of a save keeps its own newest whole save while the next is written (NB-006g-9a).
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 900, 20);
  await net.pass(SAVE_WAIT);
  const [r1, r2] = net.relays;
  const [s1] = [...(await whole(net, r2, k, a)).keys()];
  draw(a.doc, 5);
  let n = 0;
  r2.refuse = (ev) => (++n > 1 ? 'rate-limited: slow down' : null);
  const tried = r2.attempts;
  await net.pass(NEXT);
  r2.refuse = null;
  check(r2.attempts - tried === 2, `r2 took the first piece and refused the second: nothing more was sent it (${r2.attempts - tried} events)`);
  const w1 = await whole(net, r1, k, a);
  const w2 = await whole(net, r2, k, a);
  const s2 = a.copy.last.s;
  check(w1.has(s2) && w1.get(s2).banks.has(1) && !w2.has(s2) && w2.has(s1), 'r1 holds the second save in bank 1; r2 took part of it, its whole save the first, in bank 0');
  draw(a.doc, 5);
  n = 0;
  r2.refuse = (ev) => (++n > 1 ? 'rate-limited: slow down' : null);
  await net.pass(NEXT);
  r2.refuse = null;
  const after1 = await whole(net, r1, k, a);
  const after2 = await whole(net, r2, k, a);
  const s3 = a.copy.last.s;
  check(after1.has(s3) && after1.get(s3).banks.has(0) && after1.has(s2), 'r1: the third save in bank 0, beside the second');
  check(after2.has(s1) && after2.get(s1).banks.has(0) && !after2.has(s3), 'r2: the third went to bank 1 and tore; the first still stands in bank 0');
  const only = new Net(1);
  only.relays[0] = r2;
  only.sig.relays = [r2];
  const c = only.node('c');
  await only.settle();
  check(c.doc.set('strokes').entries().length === 900, 'a joiner reaching only r2 holds the first save, not nothing');
}

// Pieces that don't belong count as absent: one moved to another slot (its d is in the seal), one naming another
// slot, one over 60 Ki characters of content, and those of a save whose pieces disagree on n. A save whose newest
// whole copy on a relay spans both banks leaves that relay alone.
{
  const net = new Net(4);
  const [r1, r2, r3, r4] = net.relays;
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  const w = 'c'.repeat(64) + '00000001';
  let dot = 0; // each planted entry on a dot of its own, so none is taken for another
  const piece = (s, i, n, values) => ({ v: 1, s, i, n, c: 1, f: [{ d: 'board', t: 3, ws: [w], e: values.map((v) => [0, ++dot, 1, 's', 'strokes', null, v]) }] });
  const now = Math.floor(net.wall() / 1000);
  const sealedFor = async (j, p) => a.copy.seal(await deflate(enc.encode(JSON.stringify(p))), j, k);
  const plant = async (r, j, content, at = now) => r.put(await a.copy.event(KIND, at, [['d', k.slots[j]]], content, k));
  await plant(r1, 0, await sealedFor(0, piece('1111111111111111', 0, 1, ['ok'])));
  await plant(r1, SLOTS, await sealedFor(0, piece('2222222222222222', 0, 1, ['moved'])));
  await plant(r1, 4, await sealedFor(4, piece('3333333333333333', 0, 1, ['wrong slot'])));
  const frames17 = piece('7777777777777777', 0, 1, ['frames']);
  for (let i = 0; i < 16; i++) frames17.f.push({ d: 'board', t: 3, ws: [] });
  await plant(r2, 2, await sealedFor(2, { ...frames17, i: 2, n: 3 }));
  await plant(r2, 1, await sealedFor(1, piece('7777777777777777', 1, 3, ['frames 1'])));
  await plant(r2, 0, await sealedFor(0, piece('7777777777777777', 0, 3, ['frames 0'])));
  const mixed = piece('8888888888888888', 0, 1, ['bad frame mate']);
  mixed.f.push({ d: 'another doc', t: 3, ws: [] });
  await plant(r3, SLOTS + 2, await sealedFor(SLOTS + 2, { ...mixed, i: 2, n: 3 }));
  await plant(r3, SLOTS, await sealedFor(SLOTS, piece('8888888888888888', 0, 3, ['mixed 0'])));
  await plant(r3, SLOTS + 1, await sealedFor(SLOTS + 1, piece('8888888888888888', 1, 3, ['mixed 1'])));
  const big = await sealedFor(0, piece('4444444444444444', 0, 1, [b64(crypto.getRandomValues(new Uint8Array(30000))), b64(crypto.getRandomValues(new Uint8Array(30000)))]));
  await plant(r4, 0, big);
  await plant(r3, 0, await sealedFor(0, piece('5555555555555555', 0, 2, ['n of 2'])));
  await plant(r3, 1, await sealedFor(1, piece('5555555555555555', 1, 3, ['n of 3'])));
  const b = net.node('b');
  await net.settle();
  const got = b.doc.set('strokes').entries().map((e) => e.value);
  check(big.length > MAX_PIECE_CONTENT && got.join() === 'ok', `a reader joins only the save that belongs: ${JSON.stringify(got)} (absent: a piece moved to another slot, one naming another slot, ${big.length} characters of content, 17 frames; not joined: pieces disagreeing on n, a save with a frame of another doc)`);
  // r1's newest whole save spans both banks: a's save goes to r2 and r3 only.
  await plant(r1, 1, await sealedFor(1, piece('6666666666666666', 1, 2, ['span 1'])), now + 60);
  await plant(r1, SLOTS, await sealedFor(SLOTS, piece('6666666666666666', 0, 2, ['span 0'])), now + 60);
  draw(a.doc, 3);
  await net.pass(SAVE_WAIT);
  check(a.copy.last && !a.copy.last.wrote.has(r1.url) && a.copy.last.wrote.get(r2.url) === true, "r1's newest whole save is in both banks: a writes neither there");
}

// A slot's newest event, up to 15 minutes ahead, sets the floor: our event goes 1 s past it, or the relay would keep
// the old one.
{
  const net = new Net(1);
  const [r1] = net.relays;
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  const now = Math.floor(net.wall() / 1000);
  r1.put(await a.copy.event(KIND, now + 600, [['d', k.slots[0]]], b64(Uint8Array.of(1, ...crypto.getRandomValues(new Uint8Array(64)))), k));
  draw(a.doc, 3);
  await net.pass(SAVE_WAIT);
  const ev = r1.slots(k).get(0);
  const b = net.node('b');
  await net.settle();
  check(ev.created_at === now + 601 && b.doc.set('strokes').entries().length === 3, `slot 0 held an event 10 minutes ahead: ours went at +601 s and the relay kept it`);
}

// Empty pieces only where another save's piece sits: a 1-piece save after a 3-piece one in the same bank writes 3.
{
  const net = new Net();
  const [r1] = net.relays;
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  const ids = draw(a.doc, 900, 20);
  await net.pass(SAVE_WAIT);
  const n1 = a.copy.last.n;
  draw(a.doc, 1);
  await net.pass(NEXT);
  for (const x of ids.slice(0, 890)) a.doc.set('strokes').delete(x);
  const before = r1.log.length;
  await net.pass(NEXT);
  const read = await a.copy.read();
  const bank0 = read.relays.get(r1.url).slots.slice(0, SLOTS).map((x) => x?.piece?.n ?? null);
  check(n1 === 3 && a.copy.last.n === 1 && r1.log.length - before === 3 && bank0.join() === '1,0,0,,,,,', `a 1-piece save over a 3-piece one: the piece and 2 empty ones, not 8 (bank 0 now ${JSON.stringify(bank0)})`);
  const b = net.node('b');
  await net.settle();
  check(b.doc.set('strokes').entries().length === 11, 'a reader holds what the doc holds');
}

// Bomb pieces: one inflates to 1 MiB at most; a read stops at MAX_STORED.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 30);
  await net.pass(SAVE_WAIT);
  const pad = 'a'.repeat(45 * 1024 * 1024);
  const z = await deflate(enc.encode(`{"v":1,"s":"0123456789abcdef","i":0,"n":1,"c":0,"f":[{"d":"board","t":3,"ws":[],"x":"${pad}"}]}`));
  const bomb = async (j, at) => a.copy.event(KIND, at, [['d', k.slots[j]]], await a.copy.seal(z, j, k), k);
  const now = Math.floor(net.wall() / 1000);
  // Into bank 1 slot 0, the bank the next save writes.
  for (const r of net.relays) r.put(await bomb(SLOTS, now));
  let used = 0;
  const unseal = a.copy.unseal.bind(a.copy);
  const spy = (copy) => (copy.unseal = async (...x) => {
    const o = await unseal(...x);
    used += o.used ?? 0;
    return o;
  });
  const b = net.node('b');
  spy(b.copy);
  const t0 = performance.now();
  await net.settle();
  check(z.length < 60 * 1024 && view(b.doc) === view(a.doc) && used <= 3 * PIECE_INFLATED + 1024 * 1024, `a ${(z.length / 1024).toFixed(0)} KiB piece of 45 MiB: absent, ${(used / 2 ** 20).toFixed(1)} MiB inflated in all, the save beside it joined (${(performance.now() - t0).toFixed(0)} ms)`);
  draw(b.doc, 1);
  await net.pass(SAVE_WAIT);
  check(net.relays.every((r) => r.slots(k).get(SLOTS)?.id !== undefined && unb64(r.slots(k).get(SLOTS).content).length < 10000), 'the next save writes over it');
  for (const r of net.relays) for (let j = 0; j < BANKS * SLOTS; j++) r.put(await bomb(j, now + 10));
  const c = net.node('c');
  spy(c.copy);
  await net.settle();
  used = 0;
  const read = await c.copy.read();
  const cut = [...read.relays.values()].filter((x) => !x.complete).length;
  check(used <= MAX_STORED && used > MAX_STORED - 2 * PIECE_INFLATED && cut >= 1, `48 bombs: ${(used / 2 ** 20).toFixed(0)} MiB inflated (MAX_STORED ${(MAX_STORED / 2 ** 20).toFixed(0)} MiB), ${cut} relay reads cut short`);
}

// A relay that ignores the filter: one event per slot kept, the newest.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 30);
  await net.pass(SAVE_WAIT);
  const r = net.relays[0];
  r.ignoresFilter = true;
  const other = pw.nostrKeypair();
  const hex = (b) => Buffer.from(b).toString('hex');
  const mine = r.slots(k).get(0);
  // Newer than ours, all of them: of another kind, by another key, or in no slot of ours.
  for (let i = 0; i < 3000; i++) r.junk.push({ id: hex(crypto.getRandomValues(new Uint8Array(32))), pubkey: i % 3 ? hex(other.pubkey) : k.pubkey, created_at: mine.created_at + 1 + i, kind: i % 3 === 1 ? KIND : i % 3 === 2 ? 1 : KIND, tags: [['d', i % 3 ? k.slots[0] : 'x'.repeat(64)]], content: 'junk', sig: '' });
  for (let i = 0; i < 20; i++) r.junk.push({ id: hex(crypto.getRandomValues(new Uint8Array(32))), pubkey: k.pubkey, created_at: mine.created_at + 1 + i, kind: 1, tags: [['d', k.slots[0]]], content: 'junk', sig: '' });
  for (let i = 0; i < 40; i++) r.junk.push({ ...mine, id: hex(crypto.getRandomValues(new Uint8Array(32))), created_at: mine.created_at - 1 - i, content: 'old' });
  const b = net.node('b');
  const read = await b.copy.read();
  await net.settle();
  const kept = read.relays.get(r.url).slots.filter(Boolean);
  check(read.relays.get(r.url).complete && kept.length === 1 && kept[0].id === mine.id && view(b.doc) === view(a.doc), `3060 events past the filter, newer than ours (other kinds, keys, slots): ours kept, the newest of its slot, and the doc joined`);
}

// A relay serving created_at a year ahead fails our save on that relay only (NB-006g-9b).
{
  const net = new Net();
  const [r1, r2, r3] = net.relays;
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  const now = Math.floor(net.wall() / 1000);
  const junk = () => b64(Uint8Array.of(1, ...crypto.getRandomValues(new Uint8Array(64))));
  r3.put(await a.copy.event(KIND, now + 365 * 86400, [['d', k.slots[0]]], junk(), k));
  r2.put(await a.copy.event(KIND, now + 365 * 86400, [['d', k.slots[SLOTS + 3]]], junk(), k));
  draw(a.doc, 30);
  await net.pass(SAVE_WAIT);
  const at = Math.floor(net.wall() / 1000);
  const w = a.copy.last.wrote;
  check(w.get(r1.url) === true && w.get(r2.url) === true && !w.has(r3.url), `r1 and r2 took the save; r3, whose slot 0 holds an event a year ahead, wasn't written (${JSON.stringify([...w])})`);
  check([r1, r2].every((r) => r.slots(k).get(0)?.created_at <= at + 1), 'their pieces are created_at now, not past the year-ahead one');
  check(r2.slots(k).get(SLOTS + 3)?.created_at > at + 86400, "r2's year-ahead event in the other bank is left as it is");
}
// A relay serving our pieces with created_at rewritten a year ahead can't set a joiner's sync.at: that is the other
// relays' time, or the read's at most.
{
  const net = new Net();
  const [r1, r2, r3] = net.relays;
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 5);
  await net.pass(SAVE_WAIT);
  const made = r2.slots(k).get(0)?.created_at;
  for (const ev of r1.slots(k).values()) r1.put({ ...ev, created_at: ev.created_at + 365 * 86400 });
  a.doc.close();
  await net.pass(60000);
  const b = net.node('b');
  await net.settle();
  check(b.doc.set('strokes').entries().length === 5 && b.doc.sync.at === made * 1000, `r1 serves the save a year ahead: a joiner's sync.at is the save's time all the same (${new Date(b.doc.sync.at).toISOString()})`);
  b.doc.close();
  r2.down = r3.down = true;
  const c = net.node('c');
  await net.settle();
  check(c.doc.set('strokes').entries().length === 5 && c.doc.sync.source === 'relays' && c.doc.sync.at > made * 1000 && c.doc.sync.at <= net.wall(), `only r1 reached: the read's time, not a year ahead (${new Date(c.doc.sync.at).toISOString()})`);
}

// Another VERSION in a bank: left alone on that relay; a relay with one in both banks is skipped.
{
  const net = new Net();
  const [r1, r2, r3] = net.relays;
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  const now = Math.floor(net.wall() / 1000);
  const foreign = async (j) => a.copy.event(KIND, now - 100, [['d', k.slots[j]]], b64(Uint8Array.of(2, ...crypto.getRandomValues(new Uint8Array(80)))), k);
  const v2 = await foreign(3);
  r1.put(v2);
  r2.put(await foreign(1));
  r2.put(await foreign(SLOTS + 2));
  draw(a.doc, 30);
  await net.pass(SAVE_WAIT);
  draw(a.doc, 3);
  await net.pass(NEXT);
  const w1 = await whole(net, r1, k, a);
  const w3 = await whole(net, r3, k, a);
  check(r1.slots(k).get(3)?.id === v2.id && [...w1.values()].every((sv) => sv.banks.has(1) && !sv.banks.has(0)) && w1.has(a.copy.last.s), 'r1: bank 0 holds another version, so both saves went to bank 1, and the event is untouched');
  check(r2.log.every(([ev]) => ev.pubkey !== k.pubkey) && !a.copy.last.wrote.has(r2.url), 'r2, another version in both banks: nothing written');
  check(w3.size === 2, 'r3: both saves, one per bank');
}

// A member sending t: 5 without saving (NB-006g-8), one saver per room, t: 5 without relays.
{
  const net = new Net();
  const a = net.node('a');
  const b = net.node('b');
  const m = net.node('9', { relays: false });
  const m2 = net.node('7', { relays: false });
  const plain = net.node('8', { relays: false });
  for (const [x, y] of [[a, b], [a, m], [b, m], [a, m2], [b, m2], [a, plain]]) net.link(x, y);
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 30);
  await net.pass(SAVE_WAIT);
  await net.settle();
  const saved = net.taken(k)[0].length;
  check(saved === 3 && (a.copy.last || b.copy.last) && !(a.copy.last && b.copy.last), `one saver per room: one member saved, the other deferred and found nothing to add (${saved} events)`);
  await net.pass(SAVE_EVERY_MS + SAVE_WAIT);
  check(net.taken(k)[0].length === saved, 'and no more while nothing changes');
  // M, who knows the roomId, wipes the slots, then announces saves it never makes, every 4 minutes.
  const wiper = new RelayCopy(net.sig, pw, proof, 'board');
  wiper.doc = a.doc;
  await wiper.forget();
  check(net.relays.every((r) => r.slots(k).size === 0), 'm wiped the relay copy');
  const t5 = (from = m) => {
    for (const p of [a, b]) from.host.send(p.self, { d: 'board', t: 5, s: Buffer.from(crypto.getRandomValues(new Uint8Array(8))).toString('hex') });
  };
  // m and m2 take turns, each every 8 minutes, one every 4.
  const t0 = net.now;
  t5();
  await net.settle();
  draw(b.doc, 1);
  for (let i = 1; i <= 3; i++) {
    await net.pass(4 * 60000);
    t5(i % 2 ? m2 : m);
    await net.settle();
  }
  const repaired = Math.min(...net.relays.map((r) => r.log.findLast(([ev]) => ev.pubkey === k.pubkey && ev.kind === KIND)?.[1] ?? Infinity)) - WALL0;
  check(repaired >= t0 + 300000 && repaired <= t0 + 300000 + 31000, `the wipe repaired ${((repaired - t0) / 1000).toFixed(0)} s after m's first t: 5, m2's and later ones not pushing the save further`);
  check(a.copy.ignoring.has(m.self) && b.copy.ignoring.has(m.self) && a.copy.ignoring.has(m2.self) && (await whole(net, net.relays[0], k, a)).size === 1, "both ignore m's t: 5 now (and m2's), and the relays hold a whole save again");
  draw(a.doc, 1);
  await net.settle();
  t5();
  await net.settle();
  check(a.copy.deferUntil === null && b.copy.deferUntil === null, 'an ignored t: 5 defers nothing');
  net.unlink(a, m);
  check(!a.copy.ignoring.has(m.self) && b.copy.ignoring.has(m.self), 'm left a: a no longer keeps it among those it ignores; b, still linked, does');
  net.link(a, m);
  m.host.send(a.self, { d: 'board', t: 5, s: 'nothex' });
  m.host.send(a.self, { d: 'board', t: 5 });
  await net.settle();
  const malformed = a.copy.deferUntil;
  t5();
  await net.settle();
  check(malformed === null && a.copy.deferUntil !== null && b.copy.deferUntil === null, 'once m joins a again, a honours its t: 5 again (not a malformed one); b, not rejoined, still ignores it');
  // One t: 5 a member per 5 minutes, even once the save it deferred has run.
  a.copy.deferUntil = null;
  t5();
  await net.settle();
  const again = a.copy.deferUntil;
  await net.pass(DEFER_MS + JITTER_MS + 1000);
  t5();
  await net.settle();
  check(again === null && a.copy.deferUntil !== null, "a second t: 5 from m within 5 minutes isn't honoured; one 5 minutes on is");
  const before = view(plain.doc);
  m.host.send(plain.self, { d: 'board', t: 5, s: '0123456789abcdef' });
  a.host.send(plain.self, { d: 'board', t: 5, s: 'nothex' });
  m.host.send(a.self, { d: 'board', t: 5, s: 'nothex' });
  await net.settle();
  check(view(plain.doc) === before && !plain.doc.closed, 't: 5 dropped by a doc without relays, and a malformed one by any');
}

// A member's t: 5 while we read: we write nothing, and save once the deferral ends. An announcer whose save is whole
// on one relay that answered, the others having refused it, isn't ignored. A save no relay took is tried again
// SAVE_EVERY_MS on, or at once when the page hides.
{
  const net = new Net();
  const a = net.node('a');
  const b = net.node('b', { relays: false });
  net.link(a, b);
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 10);
  await net.settle();
  const query = net.sig.query.bind(net.sig);
  let during = true;
  net.sig.query = async (...x) => {
    if (during) {
      during = false;
      b.host.send(a.self, { d: 'board', t: 5, s: 'feedfacefeedface' });
      while (net.step());
    }
    return query(...x);
  };
  const t0 = net.now;
  await net.pass(SAVE_WAIT);
  check(net.sig.queries >= 2 && net.taken(k)[0].length === 0 && a.copy.last === null && a.copy.deferUntil !== null, "b's t: 5 landed while a read: a wrote nothing");
  await net.pass(DEFER_MS + JITTER_MS);
  const at = net.relays[0].log.find(([ev]) => ev.pubkey === k.pubkey)?.[1] - WALL0;
  check(net.taken(k)[0].length === 3 && at - t0 >= DEFER_MS && a.copy.ignoring.has(b.self), `a saved ${((at - t0) / 1000).toFixed(0)} s on, once the deferral ended, and ignores b, whose save never came`);
}
// A member that announced a save it never made announces another during our read as its deferral ends: once it is
// ignored, that one holds nothing back, so we save then, not a deferral later.
{
  const net = new Net();
  const a = net.node('a');
  const m = net.node('9', { relays: false });
  net.link(a, m);
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 5);
  await net.settle();
  const t0 = net.now;
  m.host.send(a.self, { d: 'board', t: 5, s: 'feedfacefeedface' });
  await net.settle();
  const query = net.sig.query.bind(net.sig);
  net.sig.query = async (...x) => {
    m.host.send(a.self, { d: 'board', t: 5, s: 'facadefacadefaca' });
    while (net.step());
    return query(...x);
  };
  await net.pass(DEFER_MS + JITTER_MS + 1000);
  net.sig.query = query;
  const at = net.relays[0].log.find(([ev]) => ev.pubkey === k.pubkey)?.[1] - WALL0;
  check(a.copy.honoured.get(m.self) > t0 && a.copy.ignoring.has(m.self) && a.copy.deferUntil === null && net.taken(k)[0].length === 3 && at - t0 <= DEFER_MS + JITTER_MS + 1000, `m announced again during the read as its deferral ended: a saved ${((at - t0) / 1000).toFixed(0)} s after m's first t: 5, not a deferral later`);
}
// A t: 5 naming a save already made (ours, or whole at any read since the latest that some relay completed) announces
// nothing new: its member is ignored at once and defers nothing, also when it lands during our read.
{
  const net = new Net();
  const a = net.node('a');
  const m = net.node('9', { relays: false });
  net.link(a, m);
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 5);
  await net.pass(SAVE_WAIT);
  const s1 = a.copy.last?.s;
  m.host.send(a.self, { d: 'board', t: 5, s: s1 });
  await net.settle();
  check(s1 && a.copy.ignoring.has(m.self) && a.copy.deferUntil === null, "m replays the t: 5 of a's own save: a ignores m at once, deferring nothing");
  const b = net.node('b');
  await net.settle();
  net.link(b, m);
  await net.settle();
  m.host.send(b.self, { d: 'board', t: 5, s: s1 });
  await net.settle();
  check(b.copy.saved.has(s1) && b.copy.ignoring.has(m.self) && b.copy.deferUntil === null, 'b, which read that save whole as it opened, ignores the same t: 5 at once');
  net.unlink(b, m);
  b.doc.close();
  net.unlink(a, m);
  net.link(a, m);
  draw(a.doc, 1);
  const query = net.sig.query.bind(net.sig);
  let during = true;
  net.sig.query = async (...x) => {
    if (during) {
      during = false;
      m.host.send(a.self, { d: 'board', t: 5, s: s1 });
      while (net.step());
    }
    return query(...x);
  };
  await net.pass(NEXT);
  net.sig.query = query;
  check(!during && a.copy.ignoring.has(m.self) && net.taken(k)[0].length === 6 && a.copy.last.s !== s1, 'the same t: 5 during our next read: m ignored, and a saved all the same');
}
{
  const net = new Net();
  const [r1, r2, r3] = net.relays;
  const a = net.node('a');
  const b = net.node('b');
  net.link(a, b);
  await net.settle();
  const k = await keysOf(a);
  draw(b.doc, 10);
  await net.settle();
  // b's timer, made to fire first; r2 and r3 refuse its pieces.
  for (const r of [r2, r3]) r.refuse = () => 'rate-limited: slow down';
  b.copy.timer?.();
  b.copy.run();
  await net.settle();
  for (const r of [r2, r3]) r.refuse = null;
  const sb = b.copy.last?.s;
  check(b.copy.last?.wrote.get(r1.url) === true && !b.copy.last.wrote.get(r2.url) && a.copy.deferUntil !== null, "b saved: r1 took it, r2 and r3 refused; a deferred");
  await net.pass(DEFER_MS + JITTER_MS + 1000);
  const w2 = await whole(net, r2, k, a);
  check(!a.copy.ignoring.has(b.self) && a.copy.last && w2.size === 1 && !w2.has(sb), "a doesn't ignore b (its save is whole on r1), and a's own save filled r2 and r3");
}
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 10);
  for (const r of net.relays) r.refuse = () => 'blocked: no';
  await net.pass(SAVE_WAIT);
  for (const r of net.relays) r.refuse = null;
  check(a.copy.last && [...a.copy.last.wrote.values()].every((x) => !x) && a.copy.pending && net.taken(k)[0].length === 0, 'every relay refused the save: it stays due');
  await net.pass(NEXT);
  check(net.taken(k)[0].length === 3, 'tried again SAVE_EVERY_MS on, and taken');
  for (const r of net.relays) r.refuse = () => 'blocked: no';
  draw(a.doc, 1);
  await net.pass(NEXT);
  for (const r of net.relays) r.refuse = null;
  a.copy.hidden();
  await net.settle();
  check(net.taken(k)[0].length === 6, 'refused again, then the page hidden: saved at once');
}
// No relay's read completes at the save (offline, the sockets reconnecting): the save stays due, at the next slot or
// at once when the page hides, and so does the check of a member that announced one (NB6L-1).
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 5);
  for (const r of net.relays) r.noEose = true;
  await net.pass(SAVE_WAIT);
  for (const r of net.relays) r.noEose = false;
  check(net.sig.queries === 2 && net.taken(k)[0].length === 0 && a.copy.pending, 'no relay sent EOSE at the save: nothing written, and the save stays due');
  await net.pass(NEXT);
  check(net.taken(k)[0].length === 3, 'saved at the next slot, once they answer');
  draw(a.doc, 1);
  for (const r of net.relays) r.down = true;
  await net.pass(NEXT);
  for (const r of net.relays) r.down = false;
  const down = net.taken(k)[0].length;
  a.copy.hidden();
  await net.settle();
  check(down === 3 && net.taken(k)[0].length === 6, 'every relay down at the next save, then the page hidden: saved at once');
}
{
  const net = new Net();
  const a = net.node('a');
  const m = net.node('9', { relays: false });
  net.link(a, m);
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 5);
  await net.settle();
  m.host.send(a.self, { d: 'board', t: 5, s: 'feedfacefeedface' });
  await net.settle();
  for (const r of net.relays) r.noEose = true;
  // m announces again during that read: the next read checks the newer, whose deferral it waits out.
  const query = net.sig.query.bind(net.sig);
  net.sig.query = async (...x) => {
    m.host.send(a.self, { d: 'board', t: 5, s: 'facadefacadefaca' });
    while (net.step());
    return query(...x);
  };
  await net.pass(DEFER_MS + JITTER_MS + 1000);
  net.sig.query = query;
  for (const r of net.relays) r.noEose = false;
  check(net.sig.queries === 2 && !a.copy.ignoring.has(m.self) && a.copy.announced.get(m.self) === 'facadefacadefaca' && net.taken(k)[0].length === 0, "no relay answered as m's deferral ended: m's announcements kept for the next read, the newer one checked");
  await net.pass(NEXT);
  check(a.copy.ignoring.has(m.self) && net.taken(k)[0].length === 3, 'that read finds its save never came: m ignored, and a saved');
}
// A read no relay completed says nothing of the saves already made: a replay of ours after it is still ignored at
// once (NB6L-6).
{
  const net = new Net();
  const a = net.node('a');
  const m = net.node('9', { relays: false });
  net.link(a, m);
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 5);
  await net.pass(SAVE_WAIT);
  const s1 = a.copy.last?.s;
  draw(a.doc, 1);
  for (const r of net.relays) r.down = true;
  await net.pass(NEXT);
  for (const r of net.relays) r.down = false;
  m.host.send(a.self, { d: 'board', t: 5, s: s1 });
  await net.settle();
  check(s1 && net.taken(k)[0].length === 3 && a.copy.saved.has(s1) && a.copy.ignoring.has(m.self) && a.copy.deferUntil === null, "every relay down at a's next read, then m replays a's save: ignored at once, deferring nothing");
  await net.pass(NEXT);
  check(net.taken(k)[0].length === 6, 'and the save that read left due goes at the next slot');
  // Two saves on, with r3 down, s1 is whole on no relay read: a read complete on some relays keeps only what it
  // found whole (and ours since).
  const s2 = a.copy.last?.s;
  net.relays[2].down = true;
  for (let i = 0; i < 2; i++) {
    draw(a.doc, 1);
    await net.pass(NEXT);
  }
  net.relays[2].down = false;
  const s4 = a.copy.last?.s;
  check(new Set([s1, s2, s4]).size === 3 && !a.copy.saved.has(s1) && a.copy.saved.has(s4), 'two saves on with r3 down, a read complete on r1 and r2 drops s1, overwritten there, from the saves a counts as made');
  // b opens while no relay sends EOSE: the save it read whole there counts as made all the same.
  for (const r of net.relays) r.noEose = true;
  const b = net.node('b');
  await net.settle();
  for (const r of net.relays) r.noEose = false;
  net.link(b, m);
  await net.settle();
  m.host.send(b.self, { d: 'board', t: 5, s: s4 });
  await net.settle();
  check(b.copy.saved.has(s4) && b.copy.ignoring.has(m.self) && b.copy.deferUntil === null, "b's read complete on no relay, then m replays a's save b read there: ignored at once");
}

// Renewal: opening a doc whose copy expires within 7 days saves it again.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 30);
  await net.pass(SAVE_WAIT);
  a.doc.close();
  net.now += 20 * DAY;
  const b = net.node('b');
  await net.settle();
  await net.pass(SAVE_WAIT);
  check(net.taken(k)[0].length === 3, 'opened 20 days on: nothing saved (it expires in 10)');
  b.doc.close();
  net.now += 4 * DAY;
  const c = net.node('c');
  await net.settle();
  await net.pass(SAVE_WAIT);
  const [evs] = net.taken(k);
  const exp = Number(evs.at(-1).tags.find((t) => t[0] === 'expiration')[1]);
  check(evs.length === 6 && exp - Math.floor(net.wall() / 1000) > EXPIRY_S - RENEW_S && c.doc.set('strokes').entries().length === 30, 'opened 24 days on: saved again, expiring 30 days from then');
}

// A deletion joined from the relays is reported from 'relays'; sync says where the doc came from.
{
  const net = new Net();
  const a = net.node('a');
  const b = net.node('b');
  net.link(a, b);
  await net.settle();
  const [x] = draw(a.doc, 3);
  await net.pass(SAVE_EVERY_MS + 2 * SAVE_WAIT);
  net.unlink(a, b);
  check(b.doc.sync.source === 'members' && b.doc.sync.at === null, `caught up from a member: source 'members' (${JSON.stringify(b.doc.sync)})`);
  a.doc.set('strokes').delete(x);
  await net.pass(SAVE_EVERY_MS + SAVE_WAIT);
  b.events.length = 0;
  draw(b.doc, 1);
  await net.pass(SAVE_EVERY_MS + SAVE_WAIT);
  const gone = b.events.find((e) => e.id === x);
  check(gone?.deleted === true && gone.from === 'relays' && !b.doc.set('strokes').has(x), `the deletion b joined from the relays: from 'relays' (${JSON.stringify(gone)})`);
  check(b.copy.at !== null && b.doc.sync.source === 'members', "b, caught up from a member before, says 'members' after joining from the relays too");
}

// A relay whose read didn't end (no EOSE) isn't written; nor is the doc saved while it loads from a member, nor read
// again when the page hides with nothing changed.
{
  const net = new Net();
  const [r1, , r3] = net.relays;
  r3.noEose = true;
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 3);
  await net.pass(SAVE_WAIT);
  check(a.copy.last?.wrote.get(r1.url) === true && !a.copy.last.wrote.has(r3.url) && r3.slots(k).size === 0, 'r3 sent no EOSE: the save went to r1 and r2 only');
  r3.noEose = false;
  const reads = net.sig.queries;
  a.copy.hidden();
  await net.settle();
  check(net.sig.queries === reads, 'the page hidden with nothing changed since: no read, no save');
  // m never answers a's ask: a stays 'loading' from it, and its save waits for that to end.
  const m = net.node('9', { relays: false });
  m.doc.close();
  net.link(a, m);
  await net.settle();
  const saves = net.taken(k)[0].length;
  draw(a.doc, 1);
  await net.pass(SAVE_EVERY_MS + SAVE_WAIT);
  const loading = a.doc.sync.state;
  const held = net.taken(k)[0].length;
  net.unlink(a, m);
  await net.settle();
  check(loading === 'loading' && held === saves && net.taken(k)[0].length === saves + 3, `no save while 'loading' from a member that never answers; once it left ('alone'), saved`);
}

// The last member online saves when the page hides, once a minute at most.
{
  const net = new Net();
  const a = net.node('a');
  const b = net.node('b');
  net.link(a, b);
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 5);
  await net.settle();
  a.copy.hidden();
  await net.settle();
  check(net.taken(k)[0].length === 0, 'with a member online, hiding the page saves nothing at once');
  await net.pass(SAVE_WAIT);
  const first = net.taken(k)[0].length;
  net.unlink(a, b);
  b.doc.close();
  draw(a.doc, 1);
  a.copy.hidden();
  await net.settle();
  check(net.taken(k)[0].length === first + 3, 'alone, a change then the page hidden: saved at once, within 5 minutes of the last save');
  draw(a.doc, 1);
  a.copy.hidden();
  await net.settle();
  check(net.taken(k)[0].length === first + 3, `hidden again within ${HIDE_EVERY_MS / 1000} s: not again`);
  await net.pass(HIDE_EVERY_MS);
  a.copy.hidden();
  await net.settle();
  check(net.taken(k)[0].length === first + 6, 'a minute on: again');
}

// forget() wipes the slots, best effort: empty pieces, then a NIP-09 deletion; no saves after.
{
  const net = new Net();
  const a = net.node('a');
  await net.settle();
  const k = await keysOf(a);
  draw(a.doc, 900, 20);
  await net.pass(SAVE_WAIT);
  draw(a.doc, 5);
  await net.pass(NEXT);
  const held = net.relays[0].slots(k).size;
  await a.doc.forget();
  const [, dels] = net.taken(k);
  const a16 = dels[0]?.tags.filter((t) => t[0] === 'a').map((t) => t[1]);
  check(dels.length === 3 && a16.length === 16 && k.slots.every((d) => a16.includes(`${KIND}:${k.pubkey}:${d}`)) && dels[0].tags.some((t) => t[0] === 'e'), `a NIP-09 deletion to each relay, naming all 16 slots and the events (${held} held)`);
  const empties = net.relays[0].log.filter(([ev]) => ev.kind === KIND).slice(-held);
  check(empties.every(([ev]) => ev.content.length < 200) && net.relays.every((r) => r.slots(k).size === 0), 'an empty piece in each held slot first; the relays hold nothing now');
  const c = net.node('c');
  await net.settle();
  check(c.doc.set('strokes').entries().length === 0 && c.doc.sync.source === null, 'a joiner finds nothing');
  const n = net.taken(k)[0].length;
  draw(a.doc, 1);
  await net.pass(SAVE_EVERY_MS + SAVE_WAIT);
  check(net.taken(k)[0].length === n, 'the doc that forgot saves no more');
}

// Room: the opt-in checks, then end to end over NostrSignaling and fake relay sockets.
{
  const relays = ['wss://e2e-1.test', 'wss://e2e-2.test'].map((u) => new Relay(u, () => Date.now()));
  const sockets = new Set();
  globalThis.WebSocket = class FakeSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    constructor(url) {
      this.url = url;
      this.relay = relays.find((r) => r.url === url);
      this.readyState = 0;
      this.subs = new Map();
      setTimeout(() => {
        this.readyState = 1;
        sockets.add(this);
        this.onopen?.();
      }, 5);
    }
    deliver(m) {
      setTimeout(() => this.readyState === 1 && this.onmessage?.({ data: JSON.stringify(m) }), 5);
    }
    send(raw) {
      if (this.readyState !== 1) return;
      const m = JSON.parse(raw);
      if (m[0] === 'REQ') {
        this.subs.set(m[1], m[2]);
        if (m[2].kinds?.includes(KIND)) {
          for (const ev of this.relay.serve(m[2])) this.deliver(['EVENT', m[1], ev]);
          this.deliver(['EOSE', m[1]]);
        }
      } else if (m[0] === 'CLOSE') {
        this.subs.delete(m[1]);
      } else if (m[0] === 'EVENT') {
        const [taken, why] = this.relay.publish(m[1]);
        this.deliver(['OK', m[1].id, taken, why]);
      }
    }
    close() {
      this.readyState = 3;
      sockets.delete(this);
      setTimeout(() => this.onclose?.());
    }
  };
  class NoPc {
    constructor() {
      throw new Error('no peer to dial here');
    }
  }
  const opts = { appId: 'peerwire-test', signaling: 'nostr', relays: relays.map((r) => r.url), RTCPeerConnection: NoPc };
  const local = await joinRoom({ appId: 'peerwire-test', roomId: randomRoomId() });
  let threw = null;
  try {
    local.doc('board', { relays: true });
  } catch (e) {
    threw = e;
  }
  check(threw instanceof TypeError && /signaling: 'nostr'/.test(threw.message), `signaling 'local': ${threw?.message}`);
  await local.leave();
  const lobby = await joinRoom({ ...opts, roomId: 'lobby' });
  const errs = [];
  for (const [name, o] of [['board', { relays: true }], ['board', { relays: 'yes' }]]) {
    try {
      lobby.doc(name, o);
      errs.push(null);
    } catch (e) {
      errs.push(e);
    }
  }
  check(errs[0] instanceof RangeError && /randomRoomId/.test(errs[0].message) && errs[1] instanceof TypeError, `roomId 'lobby': ${errs[0]?.message}; relays 'yes': ${errs[1]?.message}`);
  await lobby.leave();
  const roomId = randomRoomId();
  const surrogate = await joinRoom({ ...opts, roomId });
  let bad = null;
  try {
    surrogate.doc('\ud800x', { relays: true });
  } catch (e) {
    bad = e;
  }
  check(bad instanceof RangeError, `a doc name with a lone surrogate: ${bad?.message}`);
  await surrogate.leave();

  const a = await joinRoom({ ...opts, roomId });
  const ad = a.doc('board', { relays: true });
  await ad.relays.busy;
  for (let i = 0; i < 600; i++) ad.set('strokes').add(stroke(30));
  ad.map('meta').set('title', 'End to end');
  const t0 = Date.now();
  ad.relays.hidden();
  await ad.relays.busy;
  const last = ad.relays.last;
  const k = await ad.relays.keys();
  const times = relays[0].log.filter(([ev]) => ev.pubkey === k.pubkey).map(([, t]) => t);
  check(last?.n >= 2 && [...last.wrote.values()].every(Boolean), `alone, hidden: a ${last?.n}-piece save on both relays in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
  check(times.length === last.n && times.every((t, i) => !i || t - times[i - 1] >= STORE_EVERY_MS - 50), `its pieces ${STORE_EVERY_MS} ms apart on a relay (${times.map((t) => t - times[0]).join(', ')} ms)`);
  const want = JSON.stringify([ad.set('strokes').entries().map((e) => e.value), ad.map('meta').get('title')]);
  await a.leave();
  const c = await joinRoom({ ...opts, roomId });
  const cd = c.doc('board', { relays: true });
  for (let i = 0; i < 200 && cd.set('strokes').entries().length < 600; i++) await new Promise((r) => setTimeout(r, 50));
  check(JSON.stringify([cd.set('strokes').entries().map((e) => e.value), cd.map('meta').get('title')]) === want && cd.sync.source === 'relays', `a member joining alone holds the doc from the relays (${cd.set('strokes').entries().length} strokes, ${JSON.stringify(cd.sync)})`);
  await c.leave();
  check(sockets.size === 0, 'every socket closed');
}

if (failed) {
  console.error(`doc relays: ${failed} failed`);
  process.exit(1);
}
console.log('doc relays: all passed');
process.exit(0);
