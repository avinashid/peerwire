// Signalling over public Nostr relays (NIP-01), with no server of our own.
//
// - Ephemeral events (kind 25059): relays forward them to live subscribers
//   and don't store them.
// - A fresh secp256k1 key per session signs events, so nothing links them to
//   the Peerwire identity or to other sessions.
// - Messages to a peer are sealed to its PeerId (Noise X: encrypted, and the
//   sender's PeerId authenticated), because WebRTC offers carry IP addresses.
//   They're tagged with a hash of the recipient's PeerId, not the PeerId.
// - Presence ('hello') is tagged with a hash of the room name. Its body (which
//   carries our long-term PeerId) is encrypted with AES-GCM under a key derived
//   from the room name (sha256("peerwire/room-enc/<room>")), so a relay operator
//   or anyone who only sees the traffic learns nothing: only someone who already
//   knows the room name can read who is in it (SEC-2). Knowing the name is the
//   same bar as computing the topic tag, so this keeps presence confidential to
//   room members without changing who can join.
//
// Same interface as BroadcastSignaling: id, hello(meta), onHello, send(to, body, { lossy }),
// onMessage(fn), close().

import { isPeerId } from './peerwire.js';

const KIND = 25059;
const MAX_CONTENT = 64 * 1024; // event content we are willing to decode (base64 characters)
export const MAX_SEEN = 4096; // event ids remembered for de-duplication; the oldest is forgotten first
const EVENT_ID = /^[0-9a-f]{64}$/; // NIP-01: the event hash in lowercase hex, so `seen` stays ~256 KiB
// Three: a relay may rate-limit or ban an IP that several devices share (a phone hotspot).
export const DEFAULT_RELAYS = ['wss://nos.lol', 'wss://relay.primal.net', 'wss://nostr.mom'];
// Each relay connection publishes from a queue of its own, at most RELAY_RATE events a
// second (a burst of RELAY_BURST_S seconds' worth), so a join's signals and pipe batches
// aren't refused for a relay's rate limit. Relays don't publish one (NIP-11 has no field
// for it), so each relay's is learnt from its refusals. One that refuses an event for its
// rate (NIP-01 'rate-limited:'):
// - halves its rate, to no less than MIN_RELAY_RATE, which grows back by one event a
//   second each RELAY_RAISE_MS;
// - is held for HOLD_MS, doubling to MAX_HOLD_MS while it refuses again before it takes
//   one;
// - is sent the event again (RELAY_TRIES times in all).
// Events on their way when it refused are refused too: they go back on its queue and
// count for nothing more. A relay isn't held while no other open relay is free, so a join
// or a pipe always has one to go through, at its rate. One that bans us ('blocked:',
// 'banned') gets nothing for BACKOFF_MS, doubling to MAX_BACKOFF_MS. An event waits at
// most RELAY_QUEUE_MS, and a relay holds RELAY_QUEUE of them: the oldest go first, as from
// a full UDP buffer.
// Every event goes through each relay, but a lossy one (a pipe batch, whose datagrams the
// core resends if it is lost) goes through one, the relays taking turns. A pipe loads each
// with a share and moves at about their rates together while they keep pace, and a relay
// that answers OK but delivers nothing loses no more than its share. A relay that holds us
// for its rate, has banned us or isn't open is skipped, and so is one slowed behind what
// waits for it (see next()); with none left, the open, unbanned one free soonest takes it.
// One refused for its rate goes again through the next relay, seldom the one that refused,
// now held; one on a relay that closes or bans us is lost.
export const RELAY_RATE = 4;
export const RELAY_BURST_S = 2;
export const MIN_RELAY_RATE = 0.5;
export const RELAY_RAISE_MS = 5000;
export const HOLD_MS = 1000;
export const MAX_HOLD_MS = 30000;
export const RELAY_TRIES = 3;
export const RELAY_QUEUE = 64;
export const RELAY_QUEUE_MS = 10000;
export const BACKOFF_MS = 30000;
export const MAX_BACKOFF_MS = 600000;
// What a refusal's message asks: 'rate' (slow down), 'ban' (stop), or neither (null).
const refusal = (why) => (/^rate-limited/i.test(why) ? 'rate' : /^blocked|banned/i.test(why) ? 'ban' : /rate.?limit/i.test(why) ? 'rate' : null);
// A relay socket that closes (the phone switched networks or turned on its hotspot,
// Android froze the tab, the relay dropped us) is dialled again after this long,
// doubling to the cap; a network change or the page showing again redials it at
// once, and going online redials every relay.
export const RECONNECT_MS = 1000;
export const MAX_RECONNECT_MS = 30000;
// A socket still connecting this long when the network changes is given up and redialled.
const CONNECT_TIMEOUT_MS = 5000;
// The pacing (buckets, holds, bans, queue ages) runs on a monotonic clock: a wall clock
// set back (NTP, the phone's network time, a restored VM) would stall every relay that long.
const clock = () => performance.now();

const enc = new TextEncoder();
const dec = new TextDecoder();
const hex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
// Chunked: spreading a large array into fromCharCode overflows the engine's argument limit.
export const b64 = (b) => {
  let s = '';
  for (let i = 0; i < b.length; i += 8192) s += String.fromCharCode.apply(null, b.subarray(i, i + 8192));
  return btoa(s);
};
export const unb64 = (s) => Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
const sha256 = async (bytes) => new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));

export class NostrSignaling {
  constructor(pw, endpoint, { room, relays = DEFAULT_RELAYS } = {}) {
    this.pw = pw;
    this.ep = endpoint;
    this.id = endpoint.id;
    this.room = room;
    this.key = pw.nostrKeypair();
    this.pubkey = hex(this.key.pubkey);
    this.handlers = [];
    this.onHello = () => {};
    this.seen = new Set();
    this.sockets = [];
    this.paused = new Map(); // relay url -> { at, until, ms, took }: held for its rate limit
    this.banned = new Map(); // relay url -> { until, ms }: it banned us
    this.rates = new Map(); // relay url -> { tokens, at, rate, low, lowAt }: its token bucket
    this.seq = 0; // events published, numbering them so a refused one goes back in its place
    this.turn = 0; // the socket whose turn it is to take a lossy event (see next())
    this.stats = { published: 0, received: 0, rejected: 0, relaysOpen: 0, reconnects: 0, backoffs: 0, resent: 0, unsent: 0 };
    this.closed = false;
    this.sending = 0; // send()s not yet published: close() leaves the sockets open for them
    this.retry = new Map(); // relay url -> { ms, timer }: redial after a close
    this.onReconnect = () => {}; // (url): a relay opened again after a close
    this.ready = this.start(relays);
  }

  async start(relays) {
    this.myTag = hex(await sha256(enc.encode(`peerwire/peer/${this.id}`)));
    this.roomTag = hex(await sha256(enc.encode(`peerwire/room/${this.room}`)));
    // Presence bodies are encrypted under this key so only someone who knows the
    // room name can read them (SEC-2); the topic tag above is a hash of the name,
    // so the name itself can't be recovered from the traffic.
    this.encKey = await crypto.subtle.importKey(
      'raw', await sha256(enc.encode(`peerwire/room-enc/${this.room}`)),
      { name: 'AES-GCM' }, false, ['encrypt', 'decrypt']);
    this.sub = hex(crypto.getRandomValues(new Uint8Array(8)));
    // Only going online recycles open sockets: navigator.connection's 'change' also
    // fires when Chrome's speed estimate moves (rtt 450 -> 300 ms), and redialling every
    // relay on each of those got an IP banned for too many connections.
    this.wake = (e) => {
      if (e?.type === 'visibilitychange' && globalThis.document?.visibilityState !== 'visible') return;
      this.redial(e?.type === 'online');
    };
    globalThis.addEventListener?.('online', this.wake);
    globalThis.document?.addEventListener?.('visibilitychange', this.wake);
    globalThis.navigator?.connection?.addEventListener?.('change', this.wake);
    await Promise.all(relays.map((url) => new Promise((resolve) => {
      this.dial(url, resolve);
      setTimeout(resolve, 5000);
    })));
  }

  // Opens (or reopens) the socket to `url`; `opened` runs once it opens or fails.
  dial(url, opened = () => {}) {
    if (this.closed) return;
    const again = this.sockets.some((s) => s.relayUrl === url);
    for (const s of this.sockets) if (s.relayUrl === url) this.forget(s);
    this.sockets = this.sockets.filter((s) => s.relayUrl !== url);
    const ws = new WebSocket(url);
    ws.relayUrl = url;
    ws.onopen = () => {
      this.stats.relaysOpen++;
      ws.counted = true;
      this.retry.delete(url);
      // Since a little before now: what was published while we were away has gone.
      const filter = { kinds: [KIND], '#p': [this.myTag, this.roomTag], since: Math.floor(Date.now() / 1000) - 30 };
      ws.send(JSON.stringify(['REQ', this.sub, filter]));
      opened();
      if (again) {
        this.stats.reconnects++;
        this.onReconnect(url);
      }
    };
    ws.onerror = () => opened();
    ws.onclose = () => this.lostSocket(ws);
    ws.onmessage = (m) => void this.onRelay(url, m.data).catch(() => {});
    ws.dialled = clock();
    this.sockets.push(ws);
  }

  // `ws` carries nothing more: what waited on it is lost, as on a dead path.
  forget(ws) {
    clearTimeout(ws.pumpTimer);
    ws.q = [];
    ws.inflight?.clear();
  }

  lostSocket(ws) {
    if (ws.counted) this.stats.relaysOpen--;
    ws.counted = false;
    this.forget(ws);
    if (this.closed || !this.sockets.includes(ws)) return;
    this.release(); // it may have been the one relay free
    if (this.retry.get(ws.relayUrl)?.timer) return;
    const ms = this.retry.get(ws.relayUrl)?.ms ?? RECONNECT_MS;
    const timer = setTimeout(() => {
      this.retry.set(ws.relayUrl, { ms: Math.min(ms * 2, MAX_RECONNECT_MS), timer: null });
      this.dial(ws.relayUrl);
    }, ms);
    this.retry.set(ws.relayUrl, { ms, timer });
  }

  // The network came back or changed, or the page is showing again: every relay that
  // isn't open is redialled now. On going online (`all`), open sockets are too,
  // since their TCP connection may be on the old network and take minutes to time out.
  redial(all) {
    if (this.closed) return;
    for (const ws of [...this.sockets]) {
      const open = ws.readyState === WebSocket.OPEN;
      const stuck = ws.readyState === WebSocket.CONNECTING && clock() - ws.dialled < CONNECT_TIMEOUT_MS;
      if ((open && !all) || stuck) continue;
      clearTimeout(this.retry.get(ws.relayUrl)?.timer);
      this.retry.delete(ws.relayUrl);
      if (ws.counted) this.stats.relaysOpen--;
      ws.counted = false;
      ws.onclose = null;
      ws.onmessage = null;
      try {
        ws.close();
      } catch {}
      this.dial(ws.relayUrl);
    }
  }

  // `lossy`: through one relay, the relays taking turns (stripe), not each.
  async publish(tag, content, lossy = false) {
    await this.ready;
    const created_at = Math.floor(Date.now() / 1000);
    const tags = [['p', tag]];
    const id = await sha256(enc.encode(JSON.stringify([0, this.pubkey, created_at, KIND, tags, content])));
    const sig = this.pw.nostrSign(this.key.secret, id);
    const ev = { id: hex(id), pubkey: this.pubkey, created_at, kind: KIND, tags, content, sig: hex(sig) };
    const e = { id: ev.id, msg: JSON.stringify(['EVENT', ev]), at: clock(), seq: ++this.seq, tries: 0, sent: 0, lossy };
    this.stats.published++;
    if (lossy) return this.stripe(e);
    for (const ws of this.sockets) {
      if (ws.readyState !== WebSocket.OPEN || this.banned.get(ws.relayUrl)?.until > e.at) continue;
      this.queue(ws, { ...e });
      this.pump(ws);
    }
  }

  // The socket the next lossy event goes to, from `turn` on: the first open one that has
  // neither banned us nor holds us for its rate, and could take it within HOLD_MS (one slowed
  // by its own refusals, behind what else waits there, is passed over like a held one), else
  // the open, unbanned one free soonest; -1 if none. Once closed, what waits goes at once but
  // not to a held relay, so any relay not holding us will do.
  next(now) {
    let held = -1, soonest = Infinity;
    for (let i = 0; i < this.sockets.length; i++) {
      const k = (this.turn + i) % this.sockets.length;
      const url = this.sockets[k].relayUrl;
      if (this.sockets[k].readyState !== WebSocket.OPEN || this.banned.get(url)?.until > now) continue;
      const f = this.free(this.sockets[k], now);
      if (!(this.paused.get(url)?.until > now) && (this.closed || f <= HOLD_MS)) return k;
      if (f < soonest) (held = k), (soonest = f);
    }
    return held;
  }

  // Queues `e` on the relay whose turn it is (see next()), and sends it if its rate allows.
  stripe(e) {
    const k = this.next(clock());
    if (k < 0) return;
    this.turn = k + 1;
    this.queue(this.sockets[k], e);
    this.pump(this.sockets[k]);
  }

  // Puts `e` on `ws`'s queue in its place (an event sent again goes before those published
  // after it), and drops the oldest past RELAY_QUEUE.
  queue(ws, e) {
    const q = (ws.q ??= []);
    let i = q.length;
    while (i > 0 && q[i - 1].seq > e.seq) i--;
    q.splice(i, 0, e);
    if (q.length > RELAY_QUEUE) {
      q.shift();
      this.stats.unsent++;
    }
  }

  // Sends what waits on `ws` as its relay's rate allows, and comes back when it allows more.
  // Once closed, all of it goes at once (a leaving room's last messages), but not to a relay
  // that banned us or holds us for its rate.
  pump(ws) {
    clearTimeout(ws.pumpTimer);
    ws.pumpTimer = null;
    const q = ws.q;
    if (!q?.length || ws.readyState !== WebSocket.OPEN) return;
    const now = clock();
    while (q.length && now - q[0].at > RELAY_QUEUE_MS) {
      q.shift();
      this.stats.unsent++;
    }
    const url = ws.relayUrl;
    const hold = Math.max(this.banned.get(url)?.until ?? 0, this.paused.get(url)?.until ?? 0) - now;
    // Once closed, a held relay is sent nothing: what waits goes through the free ones
    // (close() moves lossy events there), and there is always one (see release()).
    if (hold > 0) return this.closed ? undefined : this.pumpIn(ws, hold);
    const r = this.rate(url, now);
    const inflight = (ws.inflight ??= new Map());
    while (q.length && (r.tokens >= 1 || this.closed)) {
      const e = q.shift();
      r.tokens--;
      e.sent = now;
      // Kept until the relay answers OK, to send it again if refused for the rate.
      inflight.set(e.id, e);
      if (inflight.size > RELAY_QUEUE) inflight.delete(inflight.keys().next().value);
      ws.send(e.msg);
    }
    if (q.length) this.pumpIn(ws, ((1 - r.tokens) * 1000) / r.rate);
  }

  pumpIn(ws, ms) {
    clearTimeout(ws.pumpTimer);
    ws.pumpTimer = setTimeout(() => this.pump(ws), Math.max(1, Math.ceil(ms)));
  }

  // `url`'s token bucket, filled to now: at its rate, which grows back from `low`, where a
  // refusal left it, by one event a second each RELAY_RAISE_MS, to RELAY_RATE.
  rate(url, now) {
    let r = this.rates.get(url);
    if (!r) this.rates.set(url, (r = { tokens: RELAY_RATE * RELAY_BURST_S, at: now, rate: RELAY_RATE, low: RELAY_RATE, lowAt: now }));
    // Clamped too, should the clock ever step back: no debt, and never below MIN_RELAY_RATE.
    r.rate = Math.max(MIN_RELAY_RATE, Math.min(RELAY_RATE, r.low + Math.max(0, now - r.lowAt) / RELAY_RAISE_MS));
    r.tokens = Math.min(Math.max(1, r.rate * RELAY_BURST_S), r.tokens + (Math.max(0, now - r.at) / 1000) * r.rate);
    r.at = now;
    return r;
  }

  // While no open relay is free (none held or banned), the hold that ends first is cut
  // short: a join or a pipe always has a relay to go through, at that relay's rate.
  release() {
    const now = clock();
    const open = this.sockets.filter((ws) => ws.readyState === WebSocket.OPEN && !(this.banned.get(ws.relayUrl)?.until > now));
    if (!open.length || open.some((ws) => !(this.paused.get(ws.relayUrl)?.until > now))) return;
    const ws = open.reduce((a, b) => (this.paused.get(b.relayUrl).until < this.paused.get(a.relayUrl).until ? b : a));
    this.paused.get(ws.relayUrl).until = now;
    this.pump(ws);
  }

  // How long until `ws`'s relay could take one more event: its hold or ban, then its queue
  // at its rate.
  free(ws, now) {
    const url = ws.relayUrl;
    const hold = Math.max(this.banned.get(url)?.until ?? 0, this.paused.get(url)?.until ?? 0, now) - now;
    const r = this.rate(url, now);
    return hold + (Math.max(0, (ws.q?.length ?? 0) + 1 - r.tokens) * 1000) / r.rate;
  }

  // How long until the relay the next lossy event goes to (see next()) could take it (0: it
  // can now, or no relay is open); with every open relay banned, until the first ban ends.
  // The pipe waits this long before its next batch, so its batches fill rather than queue
  // here; with the relays taking turns, it moves at about their rates together while they
  // keep pace, and one slowed behind its backlog gives up its turns (see next()).
  wait() {
    const now = clock();
    const k = this.next(now);
    if (k >= 0) return this.free(this.sockets[k], now);
    let ms = Infinity;
    for (const ws of this.sockets) if (ws.readyState === WebSocket.OPEN) ms = Math.min(ms, this.free(ws, now));
    return ms === Infinity ? 0 : ms;
  }

  async onRelay(url, raw) {
    let m;
    try {
      m = JSON.parse(raw);
    } catch {
      return;
    }
    // Relay messages are arrays; `null` and other JSON values are ignored, not dereferenced.
    if (!Array.isArray(m)) return;
    if (m[0] === 'OK') return this.answered(url, m[1], m[2], String(m[3]));
    if (m[0] !== 'EVENT') return;
    const ev = m[2];
    if (!ev || ev.kind !== KIND || typeof ev.id !== 'string' || !EVENT_ID.test(ev.id) || this.seen.has(ev.id) || ev.pubkey === this.pubkey) return;
    // Anyone who knows the room name can publish, so the set is bounded (a Set iterates in insertion order).
    this.seen.add(ev.id);
    if (this.seen.size > MAX_SEEN) this.seen.delete(this.seen.values().next().value);
    this.stats.received++;
    if (!Array.isArray(ev.tags)) return;
    const tag = ev.tags.find((t) => Array.isArray(t) && t[0] === 'p')?.[1];
    // Signalling bodies are small; a huge event would only cost us wasm memory.
    if (typeof ev.content !== 'string' || ev.content.length > MAX_CONTENT) return;
    try {
      if (tag === this.roomTag) {
        // Presence is encrypted to the room: a body we can't open isn't ours to read.
        const h = await this.openRoom(ev.content);
        // Anyone who knows the room name can publish a hello: check `from` is a PeerId.
        if (h && h.t === 'hello' && isPeerId(h.from) && h.from !== this.id) this.onHello(h.from, h.meta);
      } else if (tag === this.myTag) {
        const opened = this.ep.open(unb64(ev.content));
        if (!opened) return; // not for us, or tampered: ignore
        const body = JSON.parse(dec.decode(opened.data));
        for (const h of this.handlers) h(opened.from, body);
      }
    } catch {
      // malformed content from someone else's client: ignore
    }
  }

  // A relay's OK for event `id`: taken (`ok` true) or refused, and why (NIP-01).
  answered(url, id, ok, why) {
    const ws = this.sockets.find((s) => s.relayUrl === url);
    const e = ws?.inflight?.get(id);
    ws?.inflight?.delete(id);
    const now = clock();
    const p = this.paused.get(url);
    if (ok === true) {
      // It took one sent since its last hold began: a refusal from now on starts over.
      if (p && e?.sent >= p.at) p.took = true;
      return;
    }
    if (ok !== false) return;
    this.stats.rejected++;
    this.stats.lastRejection = `${url}: ${why}`;
    const kind = refusal(why);
    if (kind === 'ban') {
      const b = this.banned.get(url);
      if (b?.until > now) return;
      // A relay quiet for MAX_BACKOFF_MS since its last ban starts over at BACKOFF_MS.
      const ms = b && now - b.until < MAX_BACKOFF_MS ? Math.min(b.ms * 2, MAX_BACKOFF_MS) : BACKOFF_MS;
      this.banned.set(url, { until: now + ms, ms });
      this.stats.backoffs++;
      if (ws) this.forget(ws);
      return void this.release();
    }
    if (kind !== 'rate') return;
    const again = e && ws && e.tries < RELAY_TRIES - 1 && !this.closed;
    if (again) {
      e.tries++;
      this.stats.resent++;
    }
    // Events sent before its hold began were on their way: refused, they count for nothing
    // more (one we don't know counts unless a hold runs now).
    if (!(e ? e.sent < p?.at : p?.until > now)) {
      const ms = p && !p.took ? Math.min(p.ms * 2, MAX_HOLD_MS) : HOLD_MS;
      this.paused.set(url, { at: now, until: now + ms, ms, took: false });
      const r = this.rate(url, now);
      r.low = Math.max(MIN_RELAY_RATE, r.rate / 2);
      r.lowAt = now;
      r.tokens = 0;
      r.rate = r.low;
      this.stats.backoffs++;
      this.release();
    }
    // A lossy one goes again through the next relay, now that this one is held.
    if (again) e.lossy ? this.stripe(e) : this.queue(ws, e);
    if (ws) this.pump(ws);
  }

  // Encrypts an object for the room: base64 of nonce (12 bytes) || AES-GCM ciphertext.
  async sealRoom(obj) {
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const ct = new Uint8Array(await crypto.subtle.encrypt({ name: 'AES-GCM', iv }, this.encKey, enc.encode(JSON.stringify(obj))));
    const out = new Uint8Array(iv.length + ct.length);
    out.set(iv);
    out.set(ct, iv.length);
    return b64(out);
  }

  // Decrypts a room-sealed body back to its object, or null if it isn't one for
  // this room (another room's tag collision, old plaintext, or a forgery).
  async openRoom(content) {
    if (typeof content !== 'string') return null;
    let buf;
    try {
      buf = unb64(content);
    } catch {
      return null;
    }
    if (buf.length < 12 + 16) return null; // too short for a nonce and a GCM tag
    try {
      const pt = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: buf.subarray(0, 12) }, this.encKey, buf.subarray(12));
      return JSON.parse(dec.decode(new Uint8Array(pt)));
    } catch {
      return null;
    }
  }

  async hello(meta) {
    await this.ready; // the room tag and key are computed asynchronously
    this.publish(this.roomTag, await this.sealRoom({ t: 'hello', from: this.id, meta }));
  }

  // `lossy` (a pipe batch: the core resends what it carries if it is lost): through one relay.
  async send(to, body, opts) {
    if (!isPeerId(to)) throw new TypeError('send: `to` must be a peer id (64 lowercase hex characters)');
    if (this.closed) return;
    const sealed = this.ep.seal(to, enc.encode(JSON.stringify(body)));
    if (!sealed) return;
    this.sending++;
    try {
      await this.publish(hex(await sha256(enc.encode(`peerwire/peer/${to}`))), b64(sealed), opts?.lossy === true);
    } finally {
      if (!--this.sending && this.closed) this.shut();
    }
  }

  onMessage(fn) {
    this.handlers.push(fn);
  }

  // Nothing is read from now on. A message handed to send() before (a leaving room's last
  // pipe batch, its Bye) is still being hashed and signed, which is asynchronous: the
  // sockets close once it is published.
  close() {
    this.closed = true;
    for (const r of this.retry.values()) clearTimeout(r.timer);
    this.retry.clear();
    globalThis.removeEventListener?.('online', this.wake);
    globalThis.document?.removeEventListener?.('visibilitychange', this.wake);
    globalThis.navigator?.connection?.removeEventListener?.('change', this.wake);
    // A lossy event waiting on a relay held for its rate is there alone: it goes through a
    // free one instead, as the others waiting there already do.
    const now = clock();
    for (const ws of this.sockets) {
      if (!(this.paused.get(ws.relayUrl)?.until > now) || !ws.q?.some((e) => e.lossy)) continue;
      const moved = ws.q.filter((e) => e.lossy);
      ws.q = ws.q.filter((e) => !e.lossy);
      for (const e of moved) this.stripe(e);
    }
    for (const ws of this.sockets) {
      ws.onmessage = null;
      this.pump(ws); // what waits for its relay's rate goes now
    }
    if (!this.sending) this.shut();
  }

  shut() {
    for (const ws of this.sockets) {
      this.forget(ws);
      ws.close();
    }
  }
}

