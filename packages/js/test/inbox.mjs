// openInbox (src/inbox.js): offline messages to one PeerId, sealed and left on Nostr relays (NB-019). All in this
// process: relays that keep events as strfry does (an addressable event replaced only by a newer one, none over 64 KiB
// or more than 15 min ahead, NIP-40 expiry, the newest first up to `limit`), a fake clock, and the real wasm. Checks:
// - a recipient offline when a message was left comes online and gets it once, from three relays, and not again in a
//   check or a session after; bytes, text and JSON come back as sent;
// - a non-recipient opens every drop and reads none;
// - nothing in clear on the relays or in a read: no PeerId (hex, or its bytes in a decoded drop), no peer tag (SEC-5),
//   no `p` tag; one fresh signer per drop; sizes padded;
// - expiry: NIP-40 set; a drop expired, sent too far ahead or for too long is dropped, whatever the relay serves;
// - envelopes that don't check (version, kind, length, JSON, a signalling body) and content that isn't ours;
// - duplicates: the same drop on three relays, its content signed again, a relay serving it with another created_at;
// - caps: MAX_MESSAGE to send, MAX_CONTENT to decode, MAX_READ opens a relay per check under a flood (the message
//   still delivered, reading down), a second with more than MAX_READ drops, relays that ignore `limit` or the filter,
//   MAX_KEPT (the soonest to expire forgotten first);
// - the saved state: sealed, per identity, and one that doesn't open, of another version or failing store handled;
// - send()'s checks, listeners, the check every CHECK_MS;
// - end to end over NostrSignaling and fake relay sockets: no live subscription, every socket closed.
//
//   node packages/js/test/inbox.mjs

import { loadPeerwire, toHex, fromHex } from '../src/peerwire.js';
import { Inbox, envelope, KIND, VERSION, PAD, MAX_MESSAGE, MAX_CONTENT, MAX_TTL_S, AHEAD_S, SKEW_S, MAX_READ, MAX_KEPT, CHECK_MS } from '../src/inbox.js';
import { openInbox, memoryStore } from '../src/index.js';
import { b64, unb64 } from '../src/nostr.js';

let failed = 0;
const fail = (m) => {
  console.error(`FAIL ${m}`);
  failed++;
};
const ok = (m) => console.log(`ok   ${m}`);
const check = (cond, m) => (cond ? ok(m) : fail(m));
const enc = new TextEncoder();
const warnings = [];
console.warn = (...a) => warnings.push(a.join(' '));
const WALL0 = Date.UTC(2026, 9, 31, 12);
const APP = 'peerwire-test';
const sha256 = async (b) => new Uint8Array(await crypto.subtle.digest('SHA-256', b));
const NS = toHex(await sha256(enc.encode(`peerwire/drop/${APP}`)));
const rand = (n) => crypto.getRandomValues(new Uint8Array(n));
const S = (ms) => Math.floor(ms / 1000);
const has = (hay, needle) => {
  for (let i = 0; i + needle.length <= hay.length; i++) if (needle.every((x, j) => hay[i + j] === x)) return true;
  return false;
};

const pw = await loadPeerwire(new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname);
const A = rand(32), B = rand(32), C = rand(32);

// A relay as strfry keeps events, on clock `wall` (ms). `keepsExpired`: serves events past their NIP-40 expiry.
// `ignoresLimit`, `ignoresFilter`: serves all it holds, newest first (and `junk` first, for the filter). `reqs`: the
// filters it was asked.
class Relay {
  constructor(url, wall) {
    this.url = url;
    this.wall = wall;
    this.held = new Map();
    this.reqs = [];
    this.junk = [];
  }

  now() {
    return S(this.wall());
  }

  publish(ev) {
    if (this.down) return [false, 'error: closed'];
    if (JSON.stringify(ev).length > 65536) return [false, 'invalid: event too large'];
    if (ev.created_at > this.now() + 900) return [false, 'invalid: created_at too late'];
    const d = ev.tags.find((t) => t[0] === 'd')?.[1] ?? '';
    const key = `${ev.kind}:${ev.pubkey}:${d}`;
    const old = this.held.get(key);
    if (old && (old.created_at > ev.created_at || (old.created_at === ev.created_at && old.id <= ev.id))) return [true, 'duplicate: have a newer event'];
    this.held.set(key, ev);
    return [true, ''];
  }

  // Holds `ev` whatever the rules.
  put(ev) {
    this.held.set(ev.id, ev);
  }

  serve(f) {
    this.reqs.push(structuredClone(f));
    const now = this.now();
    const exp = (ev) => Number((Array.isArray(ev.tags) && ev.tags.find((t) => t[0] === 'expiration'))?.[1] ?? Infinity);
    let evs = [...this.held.values()].filter((ev) => this.keepsExpired || !(exp(ev) <= now));
    if (!this.ignoresFilter) {
      evs = evs.filter((ev) => (!f.kinds || f.kinds.includes(ev.kind)) && (!f['#d'] || ev.tags.some((t) => t[0] === 'd' && f['#d'].includes(t[1])))
        && (f.since === undefined || ev.created_at >= f.since) && (f.until === undefined || ev.created_at <= f.until));
    }
    evs.sort((a, b) => b.created_at - a.created_at || (a.id < b.id ? -1 : 1));
    if (this.ignoresFilter) return [...this.junk, ...evs];
    return this.ignoresLimit ? evs : evs.slice(0, f.limit ?? Infinity);
  }
}

// What inbox.js uses of nostr.js (query(), store()), over `relays`, at once.
class Sig {
  constructor(relays) {
    this.relays = relays;
  }

  async query(filter, { keep }) {
    const out = new Map();
    for (const r of this.relays) {
      const res = { complete: false, events: new Map() };
      out.set(r.url, res);
      await null;
      if (r.down) continue;
      for (const ev of r.serve(typeof filter === 'function' ? filter(r.url) : filter)) {
        const slot = keep(structuredClone(ev), r.url);
        if (slot !== null && slot !== undefined) res.events.set(slot, ev);
      }
      res.complete = !r.noEose;
    }
    return out;
  }

  store(ev, urls) {
    return new Map(urls.map((url) => [url, (async () => {
      await null;
      const r = this.relays.find((x) => x.url === url);
      return !!r && r.publish(structuredClone(ev))[0];
    })()]));
  }
}

// Relays, a clock, and inboxes over them: each counts the drops it opened (`opens`), and keeps what it heard (`got`).
function world(n = 3) {
  const w = { now: WALL0, all: [] };
  w.wall = () => w.now;
  w.relays = Array.from({ length: n }, (_, i) => new Relay(`wss://r${i + 1}.test`, w.wall));
  w.sig = new Sig(w.relays);
  w.inbox = (secret, { store = null, listen = true } = {}) => {
    const ep = pw.endpoint({ secret });
    const ib = new Inbox(w.sig, pw, ep, { appId: APP, urls: w.relays.map((r) => r.url), store, wall: w.wall });
    ib.opens = 0;
    const open = ep.open.bind(ep);
    ep.open = (b) => (ib.opens++, open(b));
    ib.got = [];
    if (listen) ib.on('message', (data, from, info) => ib.got.push({ data, from, ...info }));
    w.all.push(ib);
    return ib;
  };
  w.done = () => w.all.forEach((ib) => ib.close());
  return w;
}

// A drop as a sender's inbox makes it, but whatever we like: envelope `env` sealed by endpoint `from` to `to` (or
// `content` as given), created at `at` (s), expiring at `exp`, with a made-up id, signer and signature (nothing here
// checks them). Not yet on any relay.
function craft({ env, from, to, at, exp = at + 86400, content, d = NS }) {
  content ??= b64(from.seal(to, env));
  return { id: toHex(rand(32)), pubkey: toHex(rand(32)), created_at: at, kind: KIND, tags: [['d', d], ['expiration', String(exp)]], content, sig: '0'.repeat(128) };
}
const text = (s) => enc.encode(s);
const all = (w) => w.relays.flatMap((r) => [...r.held.values()]);

// Offline delivery, once; kinds; a non-recipient; nothing in clear.
{
  const w = world();
  const alice = w.inbox(A, { listen: false });
  const sentAt = S(w.now);
  const took = await alice.send(pw.endpoint({ secret: B }).id, 'hello bob');
  check(took === 3 && w.relays.every((r) => r.held.size === 1), `left on ${took} relays while bob is offline`);
  w.now += 3600_000;
  const store = memoryStore();
  const bob = w.inbox(B, { store });
  await bob.check();
  const m = bob.got[0];
  check(bob.got.length === 1 && m.data === 'hello bob' && m.from === alice.id && m.kind === 'text' && m.at === sentAt * 1000, `bob comes online an hour later: ${JSON.stringify(bob.got)}`);
  check(bob.opens === 1, `the copies on three relays opened once (${bob.opens})`);
  const q = w.relays[0].reqs[0];
  check(JSON.stringify(q) === JSON.stringify({ kinds: [KIND], '#d': [NS], since: S(w.now) - MAX_TTL_S - SKEW_S, limit: MAX_READ }), `the first read: ${JSON.stringify(q)}`);
  check((await bob.check()) === 0 && bob.got.length === 1, 'a second check delivers nothing again');
  bob.close();
  const bob2 = w.inbox(B, { store });
  check((await bob2.check()) === 0 && bob2.got.length === 0, 'nor does a new session with the same store');
  check(w.relays[0].reqs.at(-1).since === bob.marks.get(w.relays[0].url) - SKEW_S, 'which reads from where the last left off, less SKEW_S');
  bob2.close();
  const bytes = Uint16Array.of(1, 2, 0xffff);
  await alice.send(bob.id, { n: 1, s: ['x'] });
  await alice.send(bob.id, bytes);
  await alice.send(bob.id, rand(8).buffer);
  w.now += 60_000;
  const bob3 = w.inbox(B, { store });
  await bob3.check();
  const j = bob3.got.find((g) => g.kind === 'json');
  const b = bob3.got.find((g) => g.data.length === 6);
  const ab = bob3.got.find((g) => g.data.length === 8);
  check(bob3.got.length === 3 && j.kind === 'json' && JSON.stringify(j.data) === '{"n":1,"s":["x"]}' && b.kind === 'bytes' && b.data instanceof Uint8Array
    && toHex(b.data) === toHex(new Uint8Array(bytes.buffer)) && b.data.buffer.byteLength === 6 && ab.kind === 'bytes' && ab.data.length === 8,
  `JSON, a typed array and an ArrayBuffer come back as sent, bytes in a buffer of their own (${bob3.got.map((g) => g.kind)})`);
  const carol = w.inbox(C);
  await carol.check();
  check(carol.got.length === 0 && carol.opens === 4, `carol, not the recipient, opens all ${carol.opens} drops and reads none`);

  const ids = [alice.id, bob.id, carol.id];
  const peerTags = await Promise.all(ids.map(async (id) => toHex(await sha256(enc.encode(`peerwire/peer/${id}`)))));
  const said = JSON.stringify([all(w), w.relays.map((r) => r.reqs)]);
  check(ids.every((id) => !said.includes(id)) && peerTags.every((t) => !said.includes(t)), 'no PeerId in hex nor peer tag on any relay or in any read');
  check(all(w).every((ev) => !ids.some((id) => has(unb64(ev.content), [...fromHex(id)]))), "no PeerId's bytes in any drop's content, decoded");
  const shapes = all(w).map((ev) => ev.tags.map((t) => t[0]).join());
  check(shapes.every((s) => s === 'd,expiration') && all(w).every((ev) => ev.tags[0][1] === NS && ev.kind === KIND), `tags d (the app's) and expiration only: ${[...new Set(shapes)]}`);
  check(new Set(all(w).map((ev) => ev.pubkey)).size === 4, 'a fresh signer for each of the 4 drops');
  w.done();
}

// Padding, the NIP-40 tag, expiry, and drops sent too far ahead or for too long.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const at = S(w.now);
  await alice.send(bob.id, 'a', { ttl: 60 });
  await alice.send(bob.id, 'a'.repeat(PAD - 30));
  await alice.send(bob.id, 'a'.repeat(PAD - 29));
  const lens = [...w.relays[0].held.values()].map((ev) => ev.content.length);
  check(lens[0] === lens[1] && lens[2] > lens[1], `sizes padded to ${PAD} bytes: ${lens}`);
  const first = [...w.relays[0].held.values()][0];
  check(first.tags[1][1] === String(at + 60) && first.created_at === at, `NIP-40 expiration at + ttl (${first.tags[1][1]})`);
  w.relays[0].held.clear();
  const env = (sent, exp, s = 'm') => envelope(1, rand(16), sent, exp, text(s));
  const put = (sent, exp, s) => w.relays[0].put(craft({ env: env(sent, exp, s), from: alice.ep, to: bob.id, at: S(w.now), exp }));
  const now = S(w.now);
  put(now - 60, now, 'expired');
  put(now - 60, now + 1, 'one second left');
  put(now + AHEAD_S + 1, now + 3600, 'too far ahead');
  put(now + AHEAD_S, now + 3600, 'just ahead enough');
  put(now - 10, now - 10 + MAX_TTL_S + 1, 'too long');
  put(now - 10, now - 10 + MAX_TTL_S, 'long enough');
  w.relays[0].keepsExpired = true;
  bob.on('message', (data) => bob.got.push(data));
  await bob.check();
  check(bob.got.join() === 'one second left,long enough,just ahead enough', `expired, too far ahead, too long dropped; the rest oldest first: ${bob.got}`);
  const kept = bob.kept.size;
  w.relays[0].held.clear();
  w.now += 1000;
  await bob.check();
  check(kept === 3 && bob.kept.size === 2, `one expiring now is forgotten (${kept} kept, then ${bob.kept.size})`);
  w.done();
}

// The same id from two senders: two messages.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const carol = w.inbox(C, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const now = S(w.now);
  const id = rand(16);
  for (const [from, s] of [[alice, 'from alice'], [carol, 'from carol']]) w.relays[0].put(craft({ env: envelope(1, id, now, now + 60, text(s)), from: from.ep, to: bob.id, at: now }));
  bob.on('message', (data) => bob.got.push(data));
  await bob.check();
  check(bob.got.length === 2, `one id, two senders: ${bob.got}`);
  w.done();
}

// A read that doesn't reach the relay's end delivers what came, and its mark stays.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  await alice.send(pw.endpoint({ secret: B }).id, 'cut short');
  w.relays[0].noEose = true;
  const bob = w.inbox(B);
  await bob.check();
  const marks = bob.marks.size;
  w.relays[0].noEose = false;
  await bob.check();
  check(bob.got.length === 1 && marks === 0 && bob.marks.size === 1, `delivered; the mark set only by a read that ended (${marks}, then ${bob.marks.size})`);
  w.done();
}

// Envelopes and content that aren't ours.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const carol = w.inbox(C, { listen: false });
  const now = S(w.now);
  const good = () => envelope(1, rand(16), now, now + 60, text('fine'));
  const variants = {
    version: (e) => (e[0] = VERSION + 1),
    kind: (e) => (e[1] = 3),
    length: (e) => new DataView(e.buffer).setUint32(26, e.length - 29),
  };
  for (const [name, edit] of Object.entries(variants)) {
    const e = good();
    edit(e);
    w.relays[0].put(craft({ env: e, from: alice.ep, to: bob.id, at: now }));
  }
  const lengthOk = good();
  new DataView(lengthOk.buffer).setUint32(26, lengthOk.length - 30);
  w.relays[0].put(craft({ env: lengthOk, from: alice.ep, to: bob.id, at: now }));
  const put = (o) => w.relays[0].put(craft({ from: alice.ep, to: bob.id, at: now, ...o }));
  put({ env: envelope(2, rand(16), now, now + 60, text('{"no json')) });
  put({ env: text('{"t":"offer","sdp":"v=0"}') });
  put({ env: good().subarray(0, 29) });
  put({ env: good(), to: carol.id });
  put({ content: b64(rand(300)) });
  put({ content: 'not base64!' });
  put({ content: 42 });
  put({ env: envelope(2, rand(16), now, now + 60, text('{"ok":true}')) });
  bob.on('message', (data, from, info) => bob.got.push([data, info.kind]));
  await bob.check();
  const t = bob.got.find(([, k]) => k === 'text')?.[0];
  check(bob.got.length === 2 && t?.length === lengthOk.length - 30 && t.startsWith('fine\0') && bob.got.some(([d, k]) => k === 'json' && d.ok === true),
    `of 12 drops only the JSON one and one whose length runs to its end are read: ${JSON.stringify(bob.got.map(([d, k]) => [k, typeof d === 'string' ? d.length : d]))}`);
  check(bob.opens === 10, `content that isn't base64 or a string isn't opened (${bob.opens} of 12 opened)`);
  w.done();
}

// Duplicates: content signed again, a relay serving a drop with another created_at, across sessions.
{
  const w = world();
  const alice = w.inbox(A, { listen: false });
  const store = memoryStore();
  const bob = w.inbox(B, { store });
  await bob.check();
  await alice.send(bob.id, 'once');
  w.now += 10_000;
  await bob.check();
  const ev = [...w.relays[0].held.values()][0];
  w.now += 3600_000;
  const again = { ...ev, id: toHex(rand(32)), pubkey: toHex(rand(32)), created_at: S(w.now) };
  w.relays[1].put(again);
  w.relays[2].put({ ...ev, created_at: S(w.now) - 5 });
  await bob.check();
  check(bob.got.length === 1 && bob.opens === 3, `its content signed again, or served as newer, isn't delivered again (${bob.got.length}, ${bob.opens} opens)`);
  bob.close();
  const bob2 = w.inbox(B, { store });
  w.relays[0].put({ ...again, id: toHex(rand(32)) });
  await bob2.check();
  check(bob2.got.length === 0, 'nor in the next session');
  w.done();
}

// Sizes.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const errs = [];
  for (const data of ['x'.repeat(MAX_MESSAGE + 1), new Uint8Array(MAX_MESSAGE + 1)]) {
    try {
      await alice.send(bob.id, data);
    } catch (e) {
      errs.push(e);
    }
  }
  check(errs.length === 2 && errs.every((e) => e instanceof RangeError), `a message over ${MAX_MESSAGE} bytes throws: ${errs[0]?.message}`);
  await alice.send(bob.id, 'x'.repeat(MAX_MESSAGE));
  const big = [...w.relays[0].held.values()][0];
  check(big.content.length === MAX_CONTENT, `the largest message's drop is MAX_CONTENT (${big.content.length}) characters`);
  const now = S(w.now);
  w.relays[0].put(craft({ env: envelope(1, rand(16), now, now + 60, text('y'.repeat(MAX_MESSAGE + 3 * PAD))), from: alice.ep, to: bob.id, at: now }));
  bob.on('message', (data) => bob.got.push(data));
  await bob.check();
  check(bob.got.length === 1 && bob.got[0].length === MAX_MESSAGE && bob.opens === 1, `a drop over MAX_CONTENT isn't opened (${bob.opens} opened, ${bob.got.length} read)`);
  w.done();
}

// A flood: MAX_READ opens a relay per check, the message still delivered by reading down; then nothing re-read.
{
  const w = world();
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const old = S(w.now) - 7200;
  w.relays[0].put(craft({ env: envelope(1, rand(16), old, old + 86400, text('under the flood')), from: alice.ep, to: bob.id, at: old }));
  for (const r of w.relays) for (let i = 1; i <= 250; i++) r.put(craft({ content: b64(rand(200)), at: old + i }));
  bob.on('message', (data) => bob.got.push(data));
  // 20 minutes between checks, and a message left after the first: past the reads down, read from where they began.
  const opens = [];
  for (let i = 0; i < 8 && bob.got.length < 2; i++) {
    const o = bob.opens;
    await bob.check();
    opens.push(bob.opens - o);
    w.now += 1200_000;
    if (!i) await alice.send(bob.id, 'late');
  }
  check(bob.got.join() === 'under the flood,late' && opens.length === 4 && opens.every((n) => n <= 3 * MAX_READ),
    `750 junk drops: delivered at check 3, one left meanwhile at check 4; opens per check ${opens}`);
  const o = bob.opens;
  const mark = bob.marks.get(w.relays[0].url);
  await bob.check();
  check(bob.opens === o && bob.below.size === 0 && w.relays[0].reqs.at(-1).since === mark - SKEW_S, `then the marks moved: the next check opens nothing (${bob.opens - o})`);
  w.done();
}

// More than MAX_READ drops in one second; relays that ignore `limit` or the filter.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const s = S(w.now) - 600;
  w.relays[0].put(craft({ env: envelope(1, rand(16), s - 10, s + 3600, text('below a full second')), from: alice.ep, to: bob.id, at: s - 10 }));
  for (let i = 0; i < 150; i++) w.relays[0].put(craft({ content: b64(rand(200)), at: s }));
  bob.on('message', (data) => bob.got.push(data));
  const opens = [];
  for (let i = 0; i < 5 && !bob.got.length; i++) {
    const o = bob.opens;
    await bob.check();
    opens.push(bob.opens - o);
  }
  check(bob.got.length === 1 && opens.length === 3 && opens.every((n) => n <= MAX_READ), `150 drops in one second: read past it at check ${opens.length} (${opens})`);
  w.done();
}
{
  const w = world(1);
  const bob = w.inbox(B, { listen: false });
  w.relays[0].ignoresLimit = true;
  for (let i = 0; i < 250; i++) w.relays[0].put(craft({ content: b64(rand(200)), at: S(w.now) - 1000 + i }));
  bob.on('message', () => {});
  await bob.check();
  check(bob.opens === MAX_READ && bob.below.size === 1, `a relay that ignores limit: ${bob.opens} opened, and the rest read later`);
  w.done();
}
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const now = S(w.now);
  const r = w.relays[0];
  r.ignoresFilter = true;
  r.junk = [null, 7, 'x', [], { kind: 1 }];
  for (let i = 0; i < 60; i++) {
    r.put({ ...craft({ content: b64(rand(200)), at: now - i }), kind: 1 });
    r.put(craft({ content: b64(rand(200)), at: now - i, d: 'f'.repeat(64) }));
    r.put({ ...craft({ content: b64(rand(200)), at: now - i }), id: `X${i}` });
    r.put({ ...craft({ content: b64(rand(200)), at: now - i }), created_at: String(now - i) });
    r.put({ ...craft({ content: b64(rand(200)), at: now - i }), tags: {} });
  }
  const real = craft({ env: envelope(1, rand(16), now, now + 60, text('among the junk')), from: alice.ep, to: bob.id, at: now });
  r.put(real);
  bob.on('message', (data) => bob.got.push(data));
  await bob.check();
  check(bob.got[0] === 'among the junk' && bob.opens === 1 && bob.below.size === 0, `a relay that ignores the filter: 305 events not ours, 1 opened, the read complete (${bob.opens})`);
  r.held.delete(real.id);
  for (let i = 0; i < 150; i++) r.put(craft({ content: b64(rand(200)), at: now - SKEW_S - 100 - i }));
  w.now += 60_000;
  const o = bob.opens;
  await bob.check();
  check(bob.opens === o && bob.below.size === 0, `nor events older than the read asked for (${bob.opens - o} opened)`);
  w.done();
}

{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const now = S(w.now);
  w.relays[0].ignoresFilter = true;
  w.relays[0].put(craft({ env: envelope(1, rand(16), now - 300, now + 60, text('at the bottom')), from: alice.ep, to: bob.id, at: now - 300 }));
  for (let i = 0; i < 250; i++) w.relays[0].put(craft({ content: b64(rand(200)), at: now - 250 + i }));
  bob.on('message', (data) => bob.got.push(data));
  const opens = [];
  for (let i = 0; i < 6 && !bob.got.length; i++) {
    const o = bob.opens;
    await bob.check();
    opens.push(bob.opens - o);
  }
  check(bob.got.length === 1 && opens.length === 3 && opens.every((n) => n <= MAX_READ), `a relay that ignores until and limit is still read down (${opens})`);
  w.done();
}

// MAX_KEPT: the soonest to expire is forgotten first.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const now = S(w.now);
  // Messages 1 to MAX_KEPT, one a second, the later the sooner to expire; then message 0, the last to expire.
  const evs = [];
  for (let i = 0; i <= MAX_KEPT; i++) {
    const at = i ? now - 6000 + i : now;
    const exp = now + 3600 + (i ? MAX_KEPT - i : MAX_KEPT + 1);
    evs.push(craft({ env: envelope(1, rand(16), at, exp, text(String(i))), from: alice.ep, to: bob.id, at, exp }));
  }
  for (const ev of evs.slice(1)) w.relays[0].put(ev);
  bob.on('message', (data) => bob.got.push(data));
  let checks = 0;
  do await bob.check();
  while (bob.below.size && ++checks < 100);
  check(bob.got.length === MAX_KEPT && bob.kept.size === MAX_KEPT, `${bob.got.length} delivered by reading down over ${checks + 1} checks, ${bob.kept.size} kept`);
  w.relays[0].put(evs[0]);
  await bob.check();
  check(bob.got.length === MAX_KEPT + 1 && bob.kept.size === MAX_KEPT, `one more: ${bob.kept.size} kept`);
  w.relays[0].held.clear();
  for (const i of [1, MAX_KEPT]) w.relays[0].put({ ...evs[i], id: toHex(rand(32)), created_at: S(w.now) });
  const seen = bob.got.length;
  await bob.check();
  check(JSON.stringify(bob.got.slice(seen)) === `["${MAX_KEPT}"]`, `only the one that expires soonest was forgotten, and comes again: ${JSON.stringify(bob.got.slice(seen))}`);
  w.done();
}

// The saved state.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const store = memoryStore();
  const keys = [];
  const spy = { get: (k) => store.get(k), set: (k, v) => (keys.push(k), store.set(k, v)), delete: (k) => store.delete(k) };
  await alice.send(pw.endpoint({ secret: B }).id, 'kept');
  const bob = w.inbox(B, { store: spy });
  await bob.check();
  const bytes = await store.get(keys[0]);
  const plain = new TextDecoder().decode(bytes);
  check(new Set(keys).size === 1 && bytes[0] === VERSION && ![bob.id, alice.id, 'kept', 'marks', w.relays[0].url].some((s) => plain.includes(s)) && !has(bytes, [...fromHex(bob.id)]), `sealed: ${bytes.length} bytes, nothing in clear`);
  const hk = await crypto.subtle.importKey('raw', B, { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
  const want = toHex(new Uint8Array(await crypto.subtle.sign('HMAC', hk, enc.encode(`pw-inbox-id${APP}`))));
  check(keys[0] === want, 'kept under hex(HMAC(secret, "pw-inbox-id" || appId))');
  const carol = w.inbox(C, { store: spy });
  await carol.check();
  const other = new Inbox(w.sig, pw, pw.endpoint({ secret: B }), { appId: 'another-app', urls: [w.relays[0].url], store: spy, wall: w.wall });
  other.on('message', () => {});
  await other.check();
  other.close();
  check(new Set(keys).size === 3 && toHex(await store.get(keys[0])) === toHex(bytes), "another identity's state, and another app's, are kept apart");
  const moved = new Inbox(w.sig, pw, pw.endpoint({ secret: B }), { appId: APP, urls: ['wss://elsewhere.test'], store, wall: w.wall });
  await moved.ready;
  check(bob.marks.size === 1 && moved.marks.size === 0 && moved.kept.size === 1, 'marks of relays no longer used are dropped on load');
  bob.close();
  const flipped = bytes.slice();
  flipped[flipped.length - 1] ^= 1;
  await store.set(keys[0], flipped);
  warnings.length = 0;
  const bob2 = w.inbox(B, { store });
  await bob2.check();
  check(bob2.got.length === 1 && /didn't open/.test(warnings[0]), `a state that doesn't open: warned, and the message comes again (${warnings[0]})`);
  bob2.close();
  const newer = Uint8Array.of(VERSION + 1, 1, 2, 3);
  await store.set(keys[0], newer);
  warnings.length = 0;
  const bob3 = w.inbox(B, { store });
  await bob3.check();
  check(bob3.got.length === 1 && /another version/.test(warnings[0]) && toHex(await store.get(keys[0])) === toHex(newer), "another version's state: left as it is");
  bob3.close();
  await store.set(keys[0], 'text');
  warnings.length = 0;
  const bob4 = w.inbox(B, { store });
  await bob4.check();
  check(bob4.got.length === 1 && /isn't bytes/.test(warnings[0]), `a state that isn't bytes (${warnings[0]})`);
  bob4.close();
  warnings.length = 0;
  const broken = { get: async () => { throw new Error('gone'); }, set: async () => { throw new Error('full'); }, delete: async () => {} };
  const bob5 = w.inbox(B, { store: broken });
  await bob5.check();
  check(bob5.got.length === 1 && /didn't load \(gone\)/.test(warnings[0]) && /wasn't saved \(full\)/.test(warnings[1]), `a store that fails: warned, the message still delivered (${warnings})`);
  bob5.close();
  await store.set(keys[0], new Uint8Array(bytes).buffer);
  const bob6 = w.inbox(B, { store });
  await bob6.check();
  check(bob6.got.length === 0, 'a state stored as an ArrayBuffer is read');
  let threw = null;
  try {
    await openInbox({ appId: APP, persist: store, relays: [] });
  } catch (e) {
    threw = e;
  }
  check(threw instanceof TypeError && /secret/.test(threw.message), `persist without a secret throws: ${threw?.message}`);
  w.done();
}

// send()'s checks; listeners; the check every CHECK_MS.
{
  const w = world(1);
  const alice = w.inbox(A, { listen: false });
  const bob = w.inbox(B, { listen: false });
  const tries = [[() => alice.send('ab', 'x'), TypeError], [() => alice.send(bob.id, undefined), TypeError], [() => alice.send(bob.id, () => {}), TypeError],
    [() => alice.send(bob.id, 'x', { ttl: 0 }), RangeError], [() => alice.send(bob.id, 'x', { ttl: MAX_TTL_S + 1 }), RangeError], [() => alice.send(bob.id, 'x', { ttl: 1.5 }), RangeError]];
  const errs = [];
  for (const [f, T] of tries) {
    try {
      await f();
      errs.push(null);
    } catch (e) {
      errs.push(e instanceof T);
    }
  }
  check(errs.every(Boolean), `a bad id, data or ttl throws (${errs})`);
  check((await alice.send(bob.id, 'x', { ttl: MAX_TTL_S })) === 1 && (await alice.send(bob.id, 'y', { ttl: 1 })) === 1, `ttl 1 and ${MAX_TTL_S} are taken`);
  let off = null;
  for (let i = 0; i < 64 && !off; i++) {
    const id = toHex(rand(32));
    if (!alice.ep.seal(id, text('x'))) off = id;
  }
  let curve = null;
  try {
    await alice.send(off, 'x');
  } catch (e) {
    curve = e;
  }
  check(off && curve instanceof TypeError, `an id off the curve throws: ${curve?.message}`);
  const n = w.relays[0].reqs.length;
  check((await bob.check()) === 0 && w.relays[0].reqs.length === n, 'nothing is read while nobody listens');
  const bad = [];
  for (const f of [() => bob.on('messages', () => {}), () => bob.on('message', 'fn')]) {
    try {
      f();
    } catch (e) {
      bad.push(e instanceof TypeError);
    }
  }
  check(bad.length === 2 && bad.every(Boolean), 'on() takes the one event and a function');
  const heard = [];
  const timers = [];
  const real = globalThis.setTimeout;
  const errors = [];
  globalThis.setTimeout = (fn, ms) => (timers.push([fn, ms]), 0);
  const stop = bob.on('message', () => {
    throw new Error('listener');
  });
  bob.on('message', (data) => heard.push(data));
  await bob.check();
  globalThis.setTimeout = real;
  for (const [fn, ms] of timers) {
    if (ms !== undefined) continue;
    try {
      fn();
    } catch (e) {
      errors.push(e.message);
    }
  }
  check(heard.length === 2 && errors.join() === 'listener,listener', `a listener that throws: the next still hears both, the error reported on its own (${errors})`);
  const next = timers.find(([, ms]) => ms === CHECK_MS);
  check(next, `the next check is due in CHECK_MS (${timers.map(([, ms]) => ms)})`);
  stop();
  await alice.send(bob.id, 'later');
  next[0]();
  await bob.checking;
  check(heard.at(-1) === 'later', 'which reads');
  bob.off('message', bob.handlers.values().next().value);
  const r = w.relays[0].reqs.length;
  check((await bob.check()) === 0 && w.relays[0].reqs.length === r, 'with every listener gone, nothing more is read');
  bob.close();
  bob.on('message', () => {});
  check((await bob.check()) === 0, 'nor once closed');
  let closed = null;
  try {
    await bob.send(alice.id, 'x');
  } catch (e) {
    closed = e;
  }
  check(closed instanceof Error && /closed/.test(closed.message), `send() once closed throws: ${closed?.message}`);
  w.done();
}

// End to end over NostrSignaling and fake relay sockets.
{
  const relays = ['wss://e2e-1.test', 'wss://e2e-2.test'].map((u) => new Relay(u, () => Date.now()));
  const sockets = new Set();
  const said = [];
  globalThis.WebSocket = class FakeSocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    constructor(url) {
      this.relay = relays.find((r) => r.url === url);
      this.readyState = 0;
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
      said.push(raw);
      const m = JSON.parse(raw);
      if (m[0] === 'REQ') {
        for (const ev of this.relay.serve(m[2])) this.deliver(['EVENT', m[1], ev]);
        this.deliver(['EOSE', m[1]]);
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
  const urls = relays.map((r) => r.url);
  const alice = await openInbox({ appId: APP, secret: A, relays: urls });
  const bobId = pw.endpoint({ secret: B }).id;
  check((await alice.send(bobId, 'over sockets')) === 2, 'left on both relays over sockets');
  alice.close();
  const bob = await openInbox({ appId: APP, secret: B, relays: urls, persist: memoryStore() });
  const got = [];
  bob.on('message', (data, from) => got.push([data, from]));
  for (let i = 0; i < 100 && !got.length; i++) await new Promise((r) => setTimeout(r, 20));
  check(bob.id === bobId && got.length === 1 && got[0][0] === 'over sockets' && got[0][1] === alice.id, `bob reads it: ${JSON.stringify(got)}`);
  const reqs = said.filter((s) => s.startsWith('["REQ"'));
  check(reqs.length === 2 && reqs.every((s) => !s.includes('#p')) && !said.some((s) => s.includes(alice.id) || s.includes(bobId)), `no live subscription, no PeerId on a socket (${reqs.length} reads)`);
  bob.close();
  await new Promise((r) => setTimeout(r, 20));
  check(sockets.size === 0, 'every socket closed');
}

if (failed) {
  console.error(`${failed} FAILED`);
  process.exit(1);
}
console.log('all inbox checks passed');
process.exit(0);
