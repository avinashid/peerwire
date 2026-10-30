// A doc's copy on the relays the room signals over, for room.doc(name, { relays: true }): so a member that opens the
// doc while no other member is online still gets it (docs/STORE-DESIGN.md, route 1). doc.js drives it; nostr.js
// reads and keeps the events (query(), store()).
//
// Keys, all under the room key (proof.js), with `e` the epoch as 4 bytes big-endian (0 for now), `b` the bank and
// `i` the slot as one byte each, and the doc's name as UTF-8 (it must be well-formed, as store.js rules):
// - the seal: HMAC-SHA256(room key, 'pw-snap-key' || doc || e), an AES-256-GCM key;
// - the slots: d = hex(HMAC-SHA256(room key, 'pw-snap-id' || doc || e || b || i)), BANKS banks of SLOTS;
// - the signer: the Nostr key whose secret is seed = HMAC-SHA256(room key, 'pw-snap-sk' || doc || e), or, should
//   that not be a secret key (about 2^-128), HMAC-SHA256(seed, 'pw-snap-sk'), and so on: every member signs with
//   the same key, each signature with fresh random bytes (BIP-340), so no nonce is used twice.
// Each slot holds one kind-30078 event (NIP-01 addressable: a relay keeps the newest of each slot) whose content is a
// piece, { v: 1, s, i, n, c, f }: piece i of n of save s (8 random bytes, hex), our Lamport clock c and answer
// frames f (as doc.js snapshots the doc), as JSON of at most PIECE_INFLATED bytes, deflated to at most
// PIECE_DEFLATED, then sealed: base64 of VERSION || iv || AES-256-GCM(seal, iv, the deflated piece, additional
// data VERSION || d), iv 12 random bytes. So a piece opens only in the slot it was written to, and its `i` must be
// that slot's. An empty piece (n 0, no frames) clears a slot. Every event expires (NIP-40) EXPIRY_S after its save.
//
// Reading (read()): from every relay, { kinds: [30078], authors: [the signer], '#d': [the slots] }, until its EOSE or
// READ_MS. Anyone who knows the roomId can write the slots, and a relay can send anything, so: only events of kind
// 30078 by the signer in one of our slots count, the newest of each slot on each relay; content over
// MAX_PIECE_CONTENT characters isn't decoded; a piece is inflated to PIECE_INFLATED bytes at most, and the pieces of
// one read to MAX_STORED (store.js) in all; and one already opened from another relay isn't opened again. A piece
// that doesn't open, inflate, parse or check counts as absent; one of another VERSION (its first byte, in clear)
// means that bank on that relay is left as it is. A read is complete on a relay that sent EOSE in time, unless
// MAX_STORED cut it short.
//
// A save is whole once every one of its n pieces is there, all saying the same n (1 to SLOTS): those are joined into
// the doc, whatever relays they came from, as member data with the origin 'relays' (each is a whole state, so
// joining them all is the CRDT join); pieces of a save that isn't whole are skipped, so a save torn mid-write never
// mixes with another (the slots of two saves hold unrelated entries).
//
// Saving (see cycle()): one member saves at a time. After the doc changes, a member waits QUIET_MS without a change,
// SAVE_EVERY_MS since its last save and the end of any deferral, then a random JITTER_MS more; it reads and joins
// (a save follows only a read that was complete on some relay), and, on each relay whose read was complete, writes
// only if the doc holds something that relay's newest whole save (by created_at, never by `c`, which anyone can set)
// lacks, or that save expires within RENEW_S, there being none counting as lacking. Then, unless a member said it
// was saving meanwhile (we wait out that deferral instead), it tells the members { d, t: 5, s } ("saving"), only
// now that it knows it writes (one that says so and then doesn't would be ignored, below), and writes, each relay
// on its own:
// - into the bank that doesn't hold that relay's newest whole save, so one whole save always stands while the other
//   bank is written (none written yet, bank 0); a bank with a piece of another VERSION in it is left alone, and the
//   other one written, or nothing if both have one;
// - pieces 0 to n-1 in slots 0 to n-1, then empty pieces in the bank's other slots that held a piece of another save;
// - each event created_at the later of now and one more than that slot's newest on that relay, ignoring events more
//   than AHEAD_S past our clock (relays take none so far ahead: a relay that serves one can't stall our saves to the
//   others); a slot holding one of those can't be written, so the save fails on that relay;
// - one piece after another, through nostr.js (behind signalling, STORE_EVERY_MS apart); one refused ends the save
//   on that relay.
// A save that no relay took whole is tried again, SAVE_EVERY_MS on, as is one whose read was complete on no relay
// (offline, or the sockets reconnecting). The last member online also saves when the page hides, if the doc changed
// since its last save that a relay took (the same way: a read, then t: 5 and the pieces), in place of one waiting,
// past SAVE_EVERY_MS but not more than once each HIDE_EVERY_MS.
// A member's { t: 5 } defers our next save (one is then due, changed or not) to DEFER_MS after the first that
// deferred it: later ones don't push it further until that save has run, and we honour one per member per
// DEFER_MS. That save reads as ever, and a member whose save `s` wasn't whole on any relay whose read was complete
// is ignored until it joins again (it announced a save it never made, as one would to hold the others' back after
// wiping the slots: a save one relay refused for its rate is still whole on another); then we save if the doc
// needs it. A t: 5 naming a save already made (whole at any read since the latest that some relay completed, or
// our own) announces nothing new: its member is ignored at once. A doc of more than SLOTS pieces (or with an entry
// past a piece on its own) keeps no relay copy, with a warning; its members still serve it.

import { toHex } from './peerwire.js';
import { b64, unb64 } from './nostr.js';
import { MAX_STORED } from './store.js';
import { deflate, inflate } from './zip.js';

export const KIND = 30078;
export const VERSION = 1;
export const BANKS = 2;
export const SLOTS = 8;
export const PIECE_DEFLATED = 44 * 1024;
export const PIECE_INFLATED = 1 << 20;
export const MAX_PIECE_CONTENT = 60 * 1024;
export const READ_MS = 10000;
export const QUIET_MS = 10000;
export const JITTER_MS = 30000;
export const SAVE_EVERY_MS = 5 * 60000;
export const DEFER_MS = 5 * 60000;
// The last member online saves when the page hides, past SAVE_EVERY_MS but not more than once each HIDE_EVERY_MS.
export const HIDE_EVERY_MS = 60000;
export const EXPIRY_S = 30 * 86400;
export const RENEW_S = 7 * 86400;
export const AHEAD_S = 15 * 60;
const EPOCH = new Uint8Array(4);
const IV_BYTES = 12;
const TAG_BYTES = 16;
// Grouping pieces aims this far under PIECE_DEFLATED, as a piece is deflated again once n is known.
const MARGIN = 256;
// Frames a piece may hold: what PIECE_INFLATED takes, cut by doc.js's pack(), is a few. pieces() holds to it too.
const PIECE_FRAMES = 16;
const SAVE_ID = /^[0-9a-f]{16}$/;
const EVENT_ID = /^[0-9a-f]{64}$/;

const enc = new TextEncoder();
const dec = new TextDecoder();
const sha256 = async (bytes) => new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
const randomHex = (n) => toHex(crypto.getRandomValues(new Uint8Array(n)));
const concat = (...parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
};

// A piece as read, checked: { v: 1, s, i, n, c, f } with `i` the slot's (`at`), and f frames (doc.js checks them).
function checkPiece(p, at) {
  if (p === null || typeof p !== 'object' || Array.isArray(p) || p.v !== 1) return null;
  if (typeof p.s !== 'string' || !SAVE_ID.test(p.s) || p.i !== at || !Number.isInteger(p.n) || p.n < 0 || p.n > SLOTS) return null;
  if ((p.n > 0 && p.i >= p.n) || !Number.isSafeInteger(p.c) || p.c < 0 || !Array.isArray(p.f) || p.f.length > PIECE_FRAMES || (p.n === 0 && p.f.length)) return null;
  return p;
}

// An event's NIP-40 expiration, in seconds, or null.
function expiryOf(ev) {
  const t = ev.tags.find((x) => Array.isArray(x) && x[0] === 'expiration')?.[1];
  const n = typeof t === 'string' && /^\d{1,15}$/.test(t) ? Number(t) : NaN;
  return Number.isSafeInteger(n) ? n : null;
}

// Whole saves among `slots` (the slots of one or more relays): s -> { s, n, pieces: Map(i -> Set of pieces), at
// (seconds: the latest created_at of its pieces there), made (the same, of those not ahead of our clock, or 0),
// banks (the banks holding them), expires (the soonest) }.
function wholeSaves(slotLists) {
  const saves = new Map();
  for (const slots of slotLists) {
    slots.forEach((slot, j) => {
      const p = slot?.piece;
      if (!p || !p.n) return;
      let sv = saves.get(p.s);
      if (!sv) saves.set(p.s, (sv = { s: p.s, n: p.n, bad: false, pieces: new Map(), at: 0, made: 0, banks: new Set(), expires: Infinity }));
      if (p.n !== sv.n) sv.bad = true;
      let set = sv.pieces.get(p.i);
      if (!set) sv.pieces.set(p.i, (set = new Set()));
      set.add(p);
      sv.at = Math.max(sv.at, slot.at);
      if (!slot.ahead) sv.made = Math.max(sv.made, slot.at);
      sv.banks.add(Math.floor(j / SLOTS));
      sv.expires = Math.min(sv.expires, slot.expires ?? -Infinity);
    });
  }
  for (const [s, sv] of saves) if (sv.bad || sv.pieces.size !== sv.n) saves.delete(s);
  return saves;
}

// The doc's relay copy: `sig` the room's signalling (query(), store()), `pw` the Peerwire module (Nostr keys),
// `proof` the room's RoomProof, `name` the doc's. Throws RangeError for a name that isn't well-formed.
export class RelayCopy {
  constructor(sig, pw, proof, name) {
    if (/\p{Cs}/u.test(name)) throw new RangeError('a doc kept on the relays needs a well-formed name (no lone surrogate)');
    this.sig = sig;
    this.pw = pw;
    this.proof = proof;
    this.name = name;
    this.keyed = null; // the keys, once derived (see keys)
    this.doc = null;
    this.off = false; // closed, or forgotten: no more saves
    this.at = null; // ms: the time of the newest whole save joined, as its saver claims it (for doc.sync)
    this.pending = false; // a save is due: the doc changed, or a member's t: 5 is to be checked
    this.lastChange = -Infinity;
    this.lastCycle = -Infinity; // host.now() of the last save's start
    this.lastHidden = -Infinity; // and of the last made as the page hid
    this.deferUntil = null; // host.now() the next save is deferred to by members' t: 5s, or null
    this.announced = new Map(); // peer -> its save `s`, for the next save to check
    this.honoured = new Map(); // peer -> host.now() of its last t: 5 we honoured
    this.ignoring = new Set(); // peers whose t: 5s we ignore until they join again
    this.saved = new Set(); // saves made: whole at any read since the latest some relay completed, or ours since
    this.jitter = null;
    this.timer = null; // cancels the wait for the next save
    this.waiting = false; // a save came due while the doc was loading from a member
    this.busy = null; // the save (or wipe) under way, a promise
    this.warned = false;
    this.last = null; // what the last save did, for tests: { s, n, wrote: Map(url -> ok) }
  }

  wall() {
    return this.doc?.host.wall?.() ?? Date.now();
  }

  now() {
    return this.doc.now();
  }

  // The keys: { seal (AES-GCM), slots (hex d, BANKS * SLOTS), index (d -> slot), ad (additional data per slot),
  // secret and pubkey (hex) of the signer }.
  keys() {
    return (this.keyed ??= (async () => {
      const n = enc.encode(this.name);
      const seal = await crypto.subtle.importKey('raw', await this.proof.mac('pw-snap-key', [n, EPOCH]), 'AES-GCM', false, ['encrypt', 'decrypt']);
      const slots = [];
      for (let b = 0; b < BANKS; b++) for (let i = 0; i < SLOTS; i++) slots.push(toHex(await this.proof.mac('pw-snap-id', [n, EPOCH, [b], [i]])));
      let seed = await this.proof.mac('pw-snap-sk', [n, EPOCH]);
      let key;
      while (!(key = this.pw.nostrKeypair(seed))) {
        const k = await crypto.subtle.importKey('raw', seed, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
        seed = new Uint8Array(await crypto.subtle.sign('HMAC', k, enc.encode('pw-snap-sk')));
      }
      return {
        seal,
        slots,
        index: new Map(slots.map((d, j) => [d, j])),
        ad: slots.map((d) => concat([VERSION], enc.encode(d))),
        secret: key.secret,
        pubkey: toHex(key.pubkey),
      };
    })());
  }

  // The slot a relay's event goes in, or null: of kind 30078, by our signer, in one of our slots.
  slotOf(ev, k) {
    if (ev === null || typeof ev !== 'object' || ev.kind !== KIND || ev.pubkey !== k.pubkey || typeof ev.id !== 'string' || !EVENT_ID.test(ev.id)) return null;
    if (!Number.isSafeInteger(ev.created_at) || ev.created_at < 0 || typeof ev.content !== 'string' || !Array.isArray(ev.tags)) return null;
    const d = ev.tags.find((t) => Array.isArray(t) && t[0] === 'd')?.[1];
    return k.index.get(d) ?? null;
  }

  // Reads the slots from every relay: { now (seconds), relays: Map(url -> { complete, slots }) }, each slot null or
  // { id, at (created_at), ahead, expires, piece, other } (piece null when absent; other: another VERSION's).
  async read() {
    const k = await this.keys();
    const now = Math.floor(this.wall() / 1000);
    const got = await this.sig.query({ kinds: [KIND], authors: [k.pubkey], '#d': k.slots }, { ms: READ_MS, keep: (ev) => this.slotOf(ev, k) });
    const relays = new Map();
    const opened = new Map(); // slot + content -> what it opened to
    let budget = MAX_STORED;
    for (const [url, r] of got) {
      const rel = { complete: r.complete, slots: new Array(BANKS * SLOTS).fill(null) };
      relays.set(url, rel);
      for (const [j, ev] of r.events) {
        const slot = { id: ev.id, at: ev.created_at, ahead: ev.created_at > now + AHEAD_S, expires: expiryOf(ev), piece: null, other: false };
        rel.slots[j] = slot;
        const key = `${j}:${ev.content}`;
        let o = opened.get(key);
        if (!o) {
          o = await this.unseal(ev.content, j, k, budget);
          if (o.cut) {
            budget = 0;
            rel.complete = false;
            continue;
          }
          budget -= o.used;
          opened.set(key, o);
        }
        slot.piece = o.piece ?? null;
        slot.other = o.other === true;
      }
    }
    return { now, relays };
  }

  // The content of a slot-`j` event, opened: { piece } (null if absent), { other } for another VERSION's, or { cut }
  // if inflating it would pass what is left of the read's `budget`. `used`: the bytes inflated.
  async unseal(content, j, k, budget) {
    if (content.length > MAX_PIECE_CONTENT) return { used: 0 };
    let bytes;
    try {
      bytes = unb64(content);
    } catch {
      return { used: 0 };
    }
    if (bytes.length && bytes[0] !== VERSION) return { other: true, used: 0 };
    if (bytes.length < 1 + IV_BYTES + TAG_BYTES) return { used: 0 };
    let z;
    try {
      z = new Uint8Array(await crypto.subtle.decrypt({ name: 'AES-GCM', iv: bytes.subarray(1, 1 + IV_BYTES), additionalData: k.ad[j] }, k.seal, bytes.subarray(1 + IV_BYTES)));
    } catch {
      return { used: 0 };
    }
    if (budget < 1) return { cut: true };
    const max = Math.min(PIECE_INFLATED, budget);
    const json = await inflate(z, max);
    if (!json) return max < PIECE_INFLATED ? { cut: true } : { used: max };
    let p;
    try {
      p = JSON.parse(dec.decode(json));
    } catch {
      return { used: json.length };
    }
    return { piece: checkPiece(p, j % SLOTS), used: json.length };
  }

  // The doc's side.

  // The doc opened: reads the relays and joins the whole saves found. A save is due if a relay's copy lacks what
  // the doc holds or expires soon (opening a doc renews it).
  open(doc) {
    this.doc = doc;
    this.busy = this.load().catch(() => {}).finally(() => {
      this.busy = null;
      this.arm();
    });
  }

  async load() {
    const read = await this.read();
    if (this.off) return;
    const valid = this.join(read);
    if (this.needs(read, valid).size) this.due();
  }

  // Joins every whole save found into the doc: the set of those it took (doc.js checks their frames).
  join(read) {
    const valid = new Set();
    let newest = 0;
    for (const sv of wholeSaves([...read.relays.values()].map((r) => r.slots)).values()) {
      const pieces = [...sv.pieces.values()].flatMap((set) => [...set]);
      if (!this.doc.joinRelays(pieces)) continue;
      valid.add(sv.s);
      // A relay can rewrite a piece's created_at (nothing here checks the signature): one more than AHEAD_S past our
      // clock says only that the save was made by now.
      newest = Math.max(newest, sv.made || read.now);
    }
    if (valid.size && newest * 1000 > (this.at ?? -Infinity)) {
      this.at = newest * 1000;
      this.doc.emitSync();
    }
    // A read no relay completed says nothing of the saves already made.
    if ([...read.relays.values()].some((r) => r.complete)) this.saved = valid;
    else for (const s of valid) this.saved.add(s);
    return valid;
  }

  // The relays a save should go to, each with its newest whole save (null if none): those whose read was complete,
  // and whose copy lacks something the doc holds or expires within RENEW_S. None while the doc holds nothing.
  needs(read, valid) {
    const out = new Map();
    if (!this.doc.ctx.size) return out;
    const lacks = new Map(); // s -> whether the doc holds something it lacks
    for (const [url, rel] of read.relays) {
      if (!rel.complete) continue;
      let newest = null;
      for (const sv of wholeSaves([rel.slots]).values()) {
        if (valid.has(sv.s) && (!newest || sv.at > newest.at || (sv.at === newest.at && sv.s > newest.s))) newest = sv;
      }
      if (newest && !lacks.has(newest.s)) lacks.set(newest.s, this.doc.lacks([...newest.pieces.values()].flatMap((set) => [...set])));
      if (!newest || lacks.get(newest.s) || newest.expires - read.now < RENEW_S) out.set(url, newest);
    }
    return out;
  }

  // The doc changed (not by joining the relay copy).
  changed() {
    if (!this.doc) return; // before it opened (doc.log() trims, say)
    this.lastChange = this.now();
    this.due();
  }

  due() {
    this.pending = true;
    this.arm();
  }

  // A member's { t: 5, s }: it is saving.
  saving(peer, s) {
    if (this.off || this.ignoring.has(peer)) return;
    // A save already made isn't one being made now.
    if (this.saved.has(s)) return void this.ignoring.add(peer);
    const now = this.now();
    if (now - (this.honoured.get(peer) ?? -Infinity) < DEFER_MS) return;
    for (const [p, t] of this.honoured) if (now - t >= DEFER_MS) this.honoured.delete(p);
    this.honoured.set(peer, now);
    this.deferUntil ??= now + DEFER_MS;
    this.announced.set(peer, s);
    this.due();
  }

  // A member joined (again): its t: 5s count again.
  joined(peer) {
    this.ignoring.delete(peer);
  }

  // A member left: it can't send t: 5 before it joins again, which clears it anyway.
  left(peer) {
    this.ignoring.delete(peer);
  }

  // The doc's sync state changed: a save that waited for a catch-up to end goes now.
  synced() {
    if (this.waiting && this.doc.sync.state !== 'loading') {
      this.waiting = false;
      this.fire();
    }
  }

  // The page was hidden: the last member online saves now, if the doc changed since its last save (a deferral
  // waits on members, and none is online).
  hidden() {
    if (this.off || !this.pending || this.busy || this.doc.host.peers().length) return;
    const now = this.now();
    if (now - this.lastHidden < HIDE_EVERY_MS) return;
    this.lastHidden = now;
    this.timer?.();
    this.timer = null;
    this.run();
  }

  // Waits for the next save, if one is due (see the top of this file).
  arm() {
    if (!this.pending || this.busy || this.off || this.waiting) return;
    this.jitter ??= Math.random() * JITTER_MS;
    const due = Math.max(this.lastChange + QUIET_MS, this.lastCycle + SAVE_EVERY_MS, this.deferUntil ?? -Infinity) + this.jitter;
    this.timer?.();
    this.timer = this.doc.host.after(Math.max(0, Math.ceil(due - this.now())), () => {
      this.timer = null;
      this.fire();
    });
  }

  fire() {
    if (this.off || this.busy) return;
    if (this.doc.sync.state === 'loading') return void (this.waiting = true);
    this.run();
  }

  run() {
    this.busy = this.cycle().catch(() => {}).finally(() => {
      this.busy = null;
      this.arm();
    });
  }

  // One save: read, join, check the members that announced saves, then write to the relays that need it.
  async cycle() {
    const now = this.now();
    this.pending = false;
    this.jitter = null;
    this.lastCycle = now;
    const announced = this.announced;
    this.announced = new Map();
    this.deferUntil = null;
    const read = await this.read();
    if (this.off) return;
    const valid = this.join(read);
    const answered = [...read.relays.values()].filter((r) => r.complete).map((r) => wholeSaves([r.slots]));
    // No relay answered (offline, reconnecting): the save, and the check of who announced one, wait for the next.
    if (!answered.length) {
      this.pending = true;
      for (const [peer, s] of announced) if (!this.announced.has(peer)) this.announced.set(peer, s);
      return;
    }
    for (const [peer, s] of announced) if (!valid.has(s) || !answered.some((w) => w.has(s))) this.ignoring.add(peer);
    // A deferral set during our read by members now ignored holds nothing back.
    for (const peer of this.announced.keys()) if (this.ignoring.has(peer)) this.announced.delete(peer);
    if (!this.announced.size) this.deferUntil = null;
    const needs = this.needs(read, valid);
    if (!needs.size) return;
    const s = randomHex(8);
    const zs = await this.pieces(s);
    if (!zs || this.off) return;
    const plans = new Map();
    for (const [url, newest] of needs) {
      const plan = this.plan(read.relays.get(url), newest, zs.length, read.now);
      if (plan) plans.set(url, plan);
    }
    // A member said it was saving while we read: it saves, and ours waits out the deferral.
    if (!plans.size || this.deferUntil !== null) return;
    for (const p of this.doc.host.peers()) this.doc.host.send(p, { d: this.name, t: 5, s }, this.doc.members.get(p)?.z);
    this.saved.add(s);
    const wrote = await this.write(plans, zs, s, read.now);
    this.last = { s, n: zs.length, wrote };
    if (![...wrote.values()].some(Boolean)) this.pending = true;
  }

  // The doc as pieces of save `s`: the deflated JSON of each, or null (with a warning, once) if it takes more than
  // SLOTS. Its entries, then its dead ranges, in order, each piece taking as many as keep its JSON within
  // PIECE_INFLATED, its frames within PIECE_FRAMES and its deflated bytes within PIECE_DEFLATED.
  async pieces(s) {
    const { c, entries, ranges, pack } = this.doc.relayItems();
    const list = [...entries.map((e) => [e, null]), ...ranges.map((r) => [null, r])];
    const piece = (part, i, n) => ({ v: 1, s, i, n, c, f: pack(part.flatMap(([e]) => (e ? [e] : [])), part.flatMap(([, r]) => (r ? [r] : []))) });
    const parts = [];
    let guess = list.length;
    for (let at = 0; at < list.length;) {
      if (parts.length === SLOTS) return this.tooLarge(`more than ${SLOTS} pieces`);
      let k = Math.min(guess, list.length - at);
      for (;;) {
        const part = list.slice(at, at + k);
        const p = piece(part, parts.length, SLOTS);
        const json = enc.encode(JSON.stringify(p));
        let over = Math.max(json.length / PIECE_INFLATED, p.f.length / PIECE_FRAMES);
        if (over <= 1) over = (await deflate(json)).length / (PIECE_DEFLATED - MARGIN);
        if (over <= 1) {
          parts.push(part);
          at += k;
          guess = k;
          break;
        }
        if (k === 1) return this.tooLarge('an entry too large for a piece');
        k = Math.max(1, Math.floor((k / over) * 0.9));
      }
    }
    const zs = [];
    for (let i = 0; i < parts.length; i++) {
      const z = await deflate(enc.encode(JSON.stringify(piece(parts[i], i, parts.length))));
      if (z.length > PIECE_DEFLATED) return this.tooLarge('a piece past its size');
      zs.push(z);
    }
    return zs;
  }

  tooLarge(why) {
    if (!this.warned) {
      this.warned = true;
      console.warn(`@peerwire/js: doc ${JSON.stringify(this.name)} is too large to keep on the relays (${why}); its members still serve it`);
    }
    return null;
  }

  // What to write to one relay (`rel` its read, `newest` its newest whole save): [{ j (slot), at (created_at),
  // piece (index, or -1 for an empty one) }], or null if it can't be written (see the top of this file).
  plan(rel, newest, n, now) {
    let b = 0;
    if (newest) {
      if (newest.banks.size > 1) return null;
      b = 1 - [...newest.banks][0];
    }
    const other = (x) => rel.slots.slice(x * SLOTS, (x + 1) * SLOTS).some((slot) => slot?.other);
    if (other(b)) b = 1 - b;
    if (other(b)) return null;
    const writes = [];
    for (let i = 0; i < SLOTS; i++) {
      const j = b * SLOTS + i;
      const slot = rel.slots[j];
      if (i >= n && !(slot?.piece?.n > 0)) continue;
      if (slot?.ahead) {
        if (i < n) return null;
        continue;
      }
      writes.push({ j, at: Math.max(now, (slot?.at ?? -1) + 1), piece: i < n ? i : -1 });
    }
    return writes;
  }

  // Writes `plans` (url -> writes) for save `s` of pieces `zs`, each relay one event after another: url -> whether
  // all of it was taken. A piece sealed for one slot, and an event made for one slot and time, serve every relay.
  async write(plans, zs, s, now) {
    const k = await this.keys();
    const sealed = new Map();
    const events = new Map();
    const expires = String(now + EXPIRY_S);
    const c = this.doc.clock;
    const event = (j, at, piece) => {
      const key = `${j}:${at}:${piece}`;
      let ev = events.get(key);
      if (!ev) {
        let content = sealed.get(`${j}:${piece}`);
        if (!content) {
          const z = piece >= 0 ? Promise.resolve(zs[piece]) : deflate(enc.encode(JSON.stringify({ v: 1, s, i: j % SLOTS, n: 0, c, f: [] })));
          sealed.set(`${j}:${piece}`, (content = z.then((bytes) => this.seal(bytes, j, k))));
        }
        events.set(key, (ev = content.then((text) => this.event(KIND, at, [['d', k.slots[j]], ['expiration', expires]], text, k))));
      }
      return ev;
    };
    const wrote = new Map();
    await Promise.all([...plans].map(async ([url, writes]) => {
      let ok = true;
      for (const w of writes) {
        if (this.off) ok = false;
        if (!ok) break;
        ok = await this.sig.store(await event(w.j, w.at, w.piece), [url]).get(url);
      }
      wrote.set(url, ok);
    }));
    return wrote;
  }

  // Deflated piece `z`, sealed for slot `j`: the event's content.
  async seal(z, j, k) {
    const iv = crypto.getRandomValues(new Uint8Array(IV_BYTES));
    const ct = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: k.ad[j] }, k.seal, z));
    return b64(concat([VERSION], iv, ct));
  }

  // A Nostr event (NIP-01) by the signer.
  async event(kind, created_at, tags, content, k) {
    const id = await sha256(enc.encode(JSON.stringify([0, k.pubkey, created_at, kind, tags, content])));
    return { id: toHex(id), pubkey: k.pubkey, created_at, kind, tags, content, sig: toHex(this.pw.nostrSign(k.secret, id)) };
  }

  close() {
    this.off = true;
    this.timer?.();
    this.timer = null;
  }

  // Wipes the relay copy, best effort, and saves no more: an empty piece in every slot that holds an event (every
  // slot, on a relay whose read wasn't complete), then a NIP-09 deletion of all the slots and the events read and
  // written. Never rejects.
  async forget() {
    this.close();
    await this.busy;
    try {
      const k = await this.keys();
      const read = await this.read();
      const s = randomHex(8);
      const expires = String(read.now + EXPIRY_S);
      await Promise.all([...read.relays].map(async ([url, rel]) => {
        const ids = [];
        let last = read.now;
        for (let j = 0; j < BANKS * SLOTS; j++) {
          const slot = rel.slots[j];
          if ((rel.complete && !slot) || slot?.ahead) continue;
          if (slot) ids.push(slot.id);
          const at = Math.max(read.now, (slot?.at ?? -1) + 1);
          const z = await deflate(enc.encode(JSON.stringify({ v: 1, s, i: j % SLOTS, n: 0, c: 0, f: [] })));
          const ev = await this.event(KIND, at, [['d', k.slots[j]], ['expiration', expires]], await this.seal(z, j, k), k);
          if (await this.sig.store(ev, [url]).get(url)) {
            ids.push(ev.id);
            last = Math.max(last, at);
          }
        }
        const tags = [['k', String(KIND)], ...k.slots.map((d) => ['a', `${KIND}:${k.pubkey}:${d}`]), ...ids.map((id) => ['e', id])];
        await this.sig.store(await this.event(5, last, tags, '', k), [url]).get(url);
      }));
    } catch {
      // Best effort: the relays may be gone.
    }
  }
}
