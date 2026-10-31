// Offline messages to one peer, outside any room (NB-019; docs/STORE-DESIGN.md, "NB-019 (offline delivery), scoped
// down"): a message sealed to a PeerId and left on the Nostr relays, which its recipient reads the next time it is
// online.
//
// A drop is a kind-30078 event (NIP-78) sent to every relay, with two tags only: ['d', ns], where ns =
// hex(SHA-256('peerwire/drop/' + appId)) is the same for every drop of the app, and ['expiration', exp] (NIP-40). A
// fresh Nostr key signs each drop, so none replaces another (an addressable event is replaced only by its signer's),
// and its content is base64 of endpoint.seal(to, envelope): Noise X to the recipient's PeerId, with a fresh ephemeral
// key, the sender's PeerId inside and authenticated. So nothing on a drop names its recipient or its sender, or links
// two drops. A recipient finds its own by opening every drop of the app (one X25519 each, as signalling opens its
// messages): a tag it could ask the relays for would be one that anyone who knows its PeerId could compute and watch
// too (SEC-5).
//
// The envelope, big-endian: VERSION (1 byte), kind (1 byte: 0 bytes, 1 text, 2 JSON), id (16 random bytes), at and
// exp (u32 seconds: when it was sent, and when it expires), len (u32), the body (len bytes), then zeros to a multiple
// of PAD bytes, so a drop's size says little of its message. Its first byte keeps it apart from signalling's sealed
// bodies (JSON text), both ways.
//
// Reading (check()): each relay is asked for { kinds: [30078], '#d': [ns], since, limit: MAX_READ }, `since` being
// SKEW_S before the start of the last read of that relay that reached the end (MAX_TTL_S + SKEW_S ago at first). A
// relay sends the newest first, so one that sends MAX_READ may hold more: the next check asks it for those up to the
// oldest it sent (until), and so on down, and the mark moves only once a read reaches the end. So a check opens at
// most MAX_READ drops a relay, and a flood delays messages rather than costing more each check; a second holding more
// than MAX_READ drops is read in part. Only events of kind 30078 with our `d`, in the range asked for, count, and
// content over MAX_CONTENT characters isn't decoded. A drop holds a message if it opens and its envelope checks
// (VERSION, a kind we know, len within it), sent at most AHEAD_S past our clock, for MAX_TTL_S at most, and not yet
// expired. A message is delivered once: SHA-256(from || id) is kept until it expires (MAX_KEPT at most, the soonest
// to expire going first), and with `persist` across sessions, saved before the listeners hear it.
//
// The saved state, { v: 1, kept: [[hash, exp]], marks: { url: seconds } } as JSON, is sealed as store.js seals a
// doc's copy, under keys from our identity's secret rather than a room key: kept under hex(HMAC-SHA256(secret,
// 'pw-inbox-id' || appId)), as VERSION || iv || AES-256-GCM(HMAC-SHA256(secret, 'pw-inbox-key' || appId), iv, the
// state, additional data VERSION || that id).

import { toHex, fromHex, isPeerId } from './peerwire.js';
import { NostrSignaling, DEFAULT_RELAYS, b64, unb64 } from './nostr.js';
import { storeOf } from './store.js';
import { peerwireModule } from './room.js';

export const KIND = 30078;
export const VERSION = 1;
export const MAX_MESSAGE = 16 * 1024; // a message's body, in bytes
export const PAD = 256;
const HEAD = 30;
const SEAL_OVERHEAD = 128; // peerwire_core::sealed::SEAL_OVERHEAD
// The most a drop's content takes: base64 of a MAX_MESSAGE body's envelope, sealed.
export const MAX_CONTENT = 4 * Math.ceil((SEAL_OVERHEAD + Math.ceil((HEAD + MAX_MESSAGE) / PAD) * PAD) / 3);
export const MAX_TTL_S = 7 * 86400;
export const AHEAD_S = 15 * 60;
export const SKEW_S = 15 * 60;
export const MAX_READ = 100;
export const MAX_KEPT = 4096;
export const CHECK_MS = 60000;
const KINDS = ['bytes', 'text', 'json'];
const IV_BYTES = 12;
const EVENT_ID = /^[0-9a-f]{64}$/;

const enc = new TextEncoder();
const dec = new TextDecoder();
const sha256 = async (bytes) => new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
const concat = (...parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
};

// A message's kind and body, as room.send() takes data: bytes, text, or a JSON value.
function bodyOf(data) {
  if (typeof data === 'string') return [1, enc.encode(data)];
  if (data instanceof Uint8Array) return [0, data];
  if (data instanceof ArrayBuffer) return [0, new Uint8Array(data)];
  if (ArrayBuffer.isView(data)) return [0, new Uint8Array(data.buffer, data.byteOffset, data.byteLength)];
  const text = JSON.stringify(data);
  if (text === undefined) throw new TypeError('inbox.send: data is bytes, text or a JSON value');
  return [2, enc.encode(text)];
}

// The envelope of a message (see above).
export function envelope(kind, id, at, exp, body) {
  const out = new Uint8Array(Math.ceil((HEAD + body.length) / PAD) * PAD);
  const v = new DataView(out.buffer);
  out[0] = VERSION;
  out[1] = kind;
  out.set(id, 2);
  v.setUint32(18, at);
  v.setUint32(22, exp);
  v.setUint32(26, body.length);
  out.set(body, HEAD);
  return out;
}

// An envelope's fields, or null if `b` isn't one.
function readEnvelope(b) {
  if (b.length < HEAD || b[0] !== VERSION || b[1] >= KINDS.length) return null;
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const len = v.getUint32(26);
  if (HEAD + len > b.length) return null;
  return { kind: b[1], id: b.subarray(2, 18), at: v.getUint32(18), exp: v.getUint32(22), body: b.subarray(HEAD, HEAD + len) };
}

// The inbox of `ep`'s identity in app `appId`: `sig` reads and keeps events on the relays at `urls` (query(), store():
// nostr.js), `pw` is the Peerwire module (Nostr keys), `store` where its state is saved (null: nowhere), `wall` the
// clock (ms). `own`: close() closes `sig` and `ep` too.
export class Inbox {
  constructor(sig, pw, ep, { appId, urls, store = null, wall = () => Date.now(), own = false }) {
    this.sig = sig;
    this.pw = pw;
    this.ep = ep;
    this.id = ep.id;
    this.urls = [...urls];
    this.store = store;
    this.wall = wall;
    this.own = own;
    this.handlers = new Set();
    this.kept = new Map(); // SHA-256(from || id), first 16 bytes in hex -> exp: messages delivered
    this.marks = new Map(); // relay url -> seconds: when its last read that reached the end began
    this.below = new Map(); // relay url -> { top, until }: reading down a relay that held more than MAX_READ
    this.dirty = false; // the state changed since it was saved
    this.saving = true; // false while the store holds another VERSION's state, left as it is
    this.checking = null;
    this.timer = null;
    this.closed = false;
    this.ready = this.load(appId);
  }

  // Our namespace, and with a store, the state's keys and what it saved.
  async load(appId) {
    const ns = toHex(await sha256(enc.encode(`peerwire/drop/${appId}`)));
    if (!this.store) return { ns };
    const hk = await crypto.subtle.importKey('raw', this.ep.secret(), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
    const mac = async (label) => new Uint8Array(await crypto.subtle.sign('HMAC', hk, enc.encode(label + appId)));
    const id = toHex(await mac('pw-inbox-id'));
    const key = await crypto.subtle.importKey('raw', await mac('pw-inbox-key'), 'AES-GCM', false, ['encrypt', 'decrypt']);
    const ad = Uint8Array.of(VERSION, ...enc.encode(id));
    // Ours (it opened under our key), so as we saved it.
    const s = await this.loadState(id, key, ad);
    for (const [k, exp] of s?.kept ?? []) this.kept.set(k, exp);
    for (const [url, t] of Object.entries(s?.marks ?? {})) if (this.urls.includes(url)) this.marks.set(url, t);
    return { ns, id, key, ad };
  }

  async loadState(id, key, ad) {
    const refused = (why) => {
      console.warn(`@peerwire/js: inbox: the state on this device ${why}; messages still on the relays may come again`);
      return null;
    };
    let got;
    try {
      got = await this.store.get(id);
    } catch (e) {
      return refused(`didn't load (${e?.message ?? e})`);
    }
    if (got === undefined || got === null) return null;
    const bytes = got instanceof Uint8Array ? got : got instanceof ArrayBuffer ? new Uint8Array(got) : null;
    if (!bytes) return refused("isn't bytes");
    if (bytes.length && bytes[0] !== VERSION) {
      this.saving = false;
      return refused(`is of another version (${bytes[0]}), likely a newer release's: it is left as it is, and none kept`);
    }
    try {
      const text = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: bytes.subarray(1, 1 + IV_BYTES), additionalData: ad }, key, bytes.subarray(1 + IV_BYTES));
      return JSON.parse(dec.decode(text));
    } catch {
      return refused("didn't open under this identity's key");
    }
  }

  async save() {
    this.dirty = false;
    if (!this.store || !this.saving) return;
    const { id, key, ad } = await this.ready;
    const state = { v: 1, kept: [...this.kept], marks: Object.fromEntries(this.marks) };
    const iv = crypto.getRandomValues(new Uint8Array(IV_BYTES));
    const sealed = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: ad }, key, enc.encode(JSON.stringify(state))));
    try {
      await this.store.set(id, concat([VERSION], iv, sealed));
    } catch (e) {
      console.warn(`@peerwire/js: inbox: the state wasn't saved (${e?.message ?? e}); messages still on the relays may come again`);
    }
  }

  // `fn(data, from, { kind, at })` for each message: `data` as sent (bytes, text or a JSON value), `from` the sender's
  // PeerId (authenticated), `kind` 'bytes', 'text' or 'json', `at` when it was sent (ms, by the sender's clock). The
  // first listener starts the reading: now, then every CHECK_MS. Returns a function that removes it.
  on(event, fn) {
    if (event !== 'message') throw new TypeError(`inbox.on: the one event is 'message', not ${event}`);
    if (typeof fn !== 'function') throw new TypeError('inbox.on: a listener is a function');
    const first = !this.handlers.size;
    this.handlers.add(fn);
    if (first) this.check().catch(() => {});
    return () => this.off(event, fn);
  }

  off(event, fn) {
    if (event === 'message') this.handlers.delete(fn);
  }

  // Reads the relays now, and again CHECK_MS after: resolves to the number of messages delivered. Nothing is read
  // while nobody listens, so no message is marked as delivered unheard.
  check() {
    if (this.closed || !this.handlers.size) return Promise.resolve(0);
    return (this.checking ??= this.read().finally(() => {
      this.checking = null;
      clearTimeout(this.timer);
      if (!this.closed && this.handlers.size) this.timer = setTimeout(() => this.check().catch(() => {}), CHECK_MS);
    }));
  }

  async read() {
    const { ns } = await this.ready;
    const now = Math.floor(this.wall() / 1000);
    const asked = new Map(); // relay url -> { since, until, n, oldest }: its read, and what it sent that counts
    const filter = (url) => {
      const since = Math.max((this.marks.get(url) ?? 0) - SKEW_S, now - MAX_TTL_S - SKEW_S);
      const until = this.below.get(url)?.until;
      asked.set(url, { since, until: until ?? Infinity, n: 0, oldest: Infinity });
      return until === undefined ? { kinds: [KIND], '#d': [ns], since, limit: MAX_READ } : { kinds: [KIND], '#d': [ns], since, until, limit: MAX_READ };
    };
    const keep = (ev, url) => {
      const a = asked.get(url);
      if (!a || a.n >= MAX_READ || ev === null || typeof ev !== 'object' || ev.kind !== KIND || typeof ev.id !== 'string' || !EVENT_ID.test(ev.id)) return null;
      if (!Number.isSafeInteger(ev.created_at) || ev.created_at < a.since || ev.created_at > a.until) return null;
      if (!Array.isArray(ev.tags) || !ev.tags.some((t) => Array.isArray(t) && t[0] === 'd' && t[1] === ns)) return null;
      a.n++;
      a.oldest = Math.min(a.oldest, ev.created_at);
      return ev.id;
    };
    // Closed meanwhile, `ep` opens nothing and nobody listens.
    const res = await this.sig.query(filter, { keep });
    const found = [];
    const opened = new Set();
    for (const [url, r] of res) {
      for (const ev of r.events.values()) {
        if (opened.has(ev.id)) continue;
        opened.add(ev.id);
        const m = this.open(ev, now);
        if (m) found.push(m);
      }
      if (r.complete && asked.has(url)) this.advance(url, asked.get(url), now);
    }
    for (const [k, exp] of this.kept) if (exp <= now) this.kept.delete(k);
    const out = [];
    for (const m of found.sort((a, b) => a.at - b.at)) {
      const k = toHex((await sha256(concat(fromHex(m.from), m.id))).subarray(0, 16));
      if (this.kept.has(k)) continue;
      this.keep(k, m.exp);
      out.push(m);
    }
    if (this.dirty) await this.save();
    for (const m of out) this.emit(m);
    return out.length;
  }

  // After a read of `url` that reached the end, `a` what it counted: one that sent MAX_READ goes on below the oldest it
  // sent at the next check (below the second it was asked to end at, if it sent nothing older), and one that sent
  // less moves the mark to when the read, or the first of those down to it, began.
  advance(url, a, now) {
    const below = this.below.get(url);
    if (a.n >= MAX_READ) {
      this.below.set(url, { top: below?.top ?? now, until: below && a.oldest >= below.until ? below.until - 1 : a.oldest });
    } else {
      this.below.delete(url);
      this.marks.set(url, below?.top ?? now);
      this.dirty = true;
    }
  }

  // The message drop `ev` holds for us, or null: { from, id, at, exp, kind, data }.
  open(ev, now) {
    if (typeof ev.content !== 'string' || ev.content.length > MAX_CONTENT) return null;
    let sealed;
    try {
      sealed = unb64(ev.content);
    } catch {
      return null;
    }
    const o = this.ep.open(sealed);
    const e = o && readEnvelope(o.data);
    if (!e || e.at > now + AHEAD_S || e.exp <= now || e.exp - e.at > MAX_TTL_S) return null;
    let data;
    if (e.kind === 0) data = e.body.slice();
    else if (e.kind === 1) data = dec.decode(e.body);
    else {
      try {
        data = JSON.parse(dec.decode(e.body));
      } catch {
        return null;
      }
    }
    return { from: o.from, id: e.id, at: e.at, exp: e.exp, kind: KINDS[e.kind], data };
  }

  // Marks message `k` delivered until `exp`: past MAX_KEPT, the one that expires soonest is forgotten.
  keep(k, exp) {
    if (this.kept.size >= MAX_KEPT) {
      let soonest = null;
      for (const [j, x] of this.kept) if (soonest === null || x < this.kept.get(soonest)) soonest = j;
      this.kept.delete(soonest);
    }
    this.kept.set(k, exp);
    this.dirty = true;
  }

  emit(m) {
    for (const fn of this.handlers) {
      try {
        fn(m.data, m.from, { kind: m.kind, at: m.at * 1000 });
      } catch (e) {
        // A throwing listener must not keep the message from the others: report it on its own.
        setTimeout(() => {
          throw e;
        });
      }
    }
  }

  // Leaves `data` (bytes, text or a JSON value, MAX_MESSAGE bytes at most) for `to`, a PeerId, on the relays, for `ttl`
  // seconds (MAX_TTL_S at most): resolves to the number of relays that took it, once each has answered.
  async send(to, data, { ttl = MAX_TTL_S } = {}) {
    if (this.closed) throw new Error('inbox.send: the inbox is closed');
    if (!isPeerId(to)) throw new TypeError('inbox.send: `to` must be a peer id (64 lowercase hex characters)');
    if (!Number.isInteger(ttl) || ttl < 1 || ttl > MAX_TTL_S) throw new RangeError(`inbox.send: ttl is whole seconds, 1 to ${MAX_TTL_S}`);
    const [kind, body] = bodyOf(data);
    if (body.length > MAX_MESSAGE) throw new RangeError(`inbox.send: a message takes ${MAX_MESSAGE} bytes at most, not ${body.length}`);
    const at = Math.floor(this.wall() / 1000);
    return this.post(to, envelope(kind, crypto.getRandomValues(new Uint8Array(16)), at, at + ttl, body), at, at + ttl);
  }

  // Leaves envelope `env` sealed to `to` on the relays, sent `at`, expiring `exp`.
  async post(to, env, at, exp) {
    const sealed = this.ep.seal(to, env);
    if (!sealed) throw new TypeError('inbox.send: `to` is no peer id (not a point on the curve)');
    const { ns } = await this.ready;
    const k = this.pw.nostrKeypair();
    const pubkey = toHex(k.pubkey);
    const tags = [['d', ns], ['expiration', String(exp)]];
    const content = b64(sealed);
    const id = await sha256(enc.encode(JSON.stringify([0, pubkey, at, KIND, tags, content])));
    const ev = { id: toHex(id), pubkey, created_at: at, kind: KIND, tags, content, sig: toHex(this.pw.nostrSign(k.secret, id)) };
    // Closed meanwhile, the signalling takes nothing: 0.
    const took = await Promise.all(this.sig.store(ev, this.urls).values());
    return took.filter(Boolean).length;
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    clearTimeout(this.timer);
    this.handlers.clear();
    if (!this.own) return;
    this.sig.close();
    this.ep.close();
  }
}

// Opens the inbox of the identity `secret` (32 bytes, as endpoint.secret() gives it; a new identity without one) in
// app `appId`, on `relays`. `persist` (true for IndexedDB in a browser, or a store as room.doc() takes) keeps what was
// delivered across sessions, so a message still on the relays isn't delivered again; it needs `secret`.
export async function openInbox({ appId, secret, relays = DEFAULT_RELAYS, persist, wasm } = {}) {
  if (typeof appId !== 'string' || !appId) throw new TypeError('openInbox needs an appId');
  if (persist && !secret) throw new TypeError('openInbox: persist needs `secret`, the identity whose inbox it is');
  const store = persist ? storeOf(persist) : null;
  const pw = await peerwireModule(wasm);
  const ep = pw.endpoint({ secret });
  // No live subscription: a relay never sees our peer tag (SEC-5).
  const sig = new NostrSignaling(pw, ep, { relays, listen: false });
  const inbox = new Inbox(sig, pw, ep, { appId, urls: relays, store, own: true });
  await Promise.all([sig.ready, inbox.ready]);
  return inbox;
}
