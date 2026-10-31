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
// onMessage(fn), close(). A doc's copy on the relays (relays.js) and an inbox (inbox.js) also
// read stored events with query(filter) and write them with store(ev, urls), behind signalling.
// An inbox's own signalling (`listen: false`) subscribes to nothing, so no relay sees our peer tag.

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
// waits for it (see next()), and one silent: it has answered nothing since an event sent to
// it SILENT_MS ago (a half-open socket, still OPEN but dead, would take its turns and lose
// them until it closes). With none left, the open, unbanned one free soonest takes it, silent
// ones last; with only silent ones, they take turns the same way. A silent relay is still
// sent every other event, and its next OK, taken or refused, gives it its turns back. One
// refused for its rate goes again through the next relay, seldom the one that refused, now
// held; one on a relay that closes or bans us is lost.
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
// NIP-01 relays answer each event with an OK within a round trip.
export const SILENT_MS = 5000;
// Our own socket may be half-open too, its subscription hearing nothing, so what peers send us through that relay
// is lost. A socket that has heard nothing at all (no OK, no event) since an event sent through every relay (not a
// pipe batch, nor one to keep) REDIAL_SILENT_MS ago is deaf: it is closed and its relay dialled again, and the new
// socket is silent (see silent()) until its relay answers. Each next redial of that relay waits twice as long, to
// MAX_RECONNECT_MS, and after REDIAL_TRIES there are none until it answers an event within SILENT_MS on a socket open
// MAX_RECONNECT_MS or more: one that never answers, answers late, or answers each new socket only to go deaf again,
// isn't dialled again and again (relays ban an IP for too many connections, see start()).
export const REDIAL_SILENT_MS = 2 * SILENT_MS;
export const REDIAL_TRIES = 3;
// Events to keep (store(): a doc's copy on the relays) go to each relay one every STORE_EVERY_MS
// at most, and only while no other event waits there, so they never hold up a join or a pipe.
// One waits STORE_QUEUE_MS at most to be sent, and STORE_OK_MS for its relay's OK, then counts as
// refused. A read of stored events (query()) ends at the relay's EOSE, or after QUERY_MS.
export const STORE_EVERY_MS = 1000;
export const STORE_QUEUE_MS = 120000;
export const STORE_OK_MS = 30000;
export const QUERY_MS = 10000;
// What a refusal's message asks: 'rate' (slow down), 'ban' (stop), or neither (null).
const refusal = (why) => (/^rate-limited/i.test(why) ? 'rate' : /^blocked|banned/i.test(why) ? 'ban' : /rate.?limit/i.test(why) ? 'rate' : null);
// A relay socket that closes (the phone switched networks or turned on its hotspot,
// Android froze the tab, the relay dropped us) is dialled again after this long,
// doubling to the cap; a network change or the page showing again redials it at
// once, and going online redials every relay. The wait starts over only once a socket
// has stayed open the cap: one that takes each connection and drops it (overloaded,
// dropping an IP it bans, wanting auth) would otherwise be dialled every second, and
// each reopen says hello again (see onReconnect).
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
  constructor(pw, endpoint, { room, relays = DEFAULT_RELAYS, listen = true } = {}) {
    this.pw = pw;
    this.ep = endpoint;
    this.id = endpoint.id;
    this.room = room;
    this.listen = listen;
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
    this.stats = { published: 0, received: 0, rejected: 0, relaysOpen: 0, reconnects: 0, backoffs: 0, resent: 0, unsent: 0, redials: 0 };
    this.closed = false;
    this.sending = 0; // send()s not yet published: close() leaves the sockets open for them
    this.retry = new Map(); // relay url -> { ms, timer }: redial after a close
    this.redialled = new Map(); // relay url -> its deaf sockets redialled since it last answered in time (see deaf())
    this.onReconnect = () => {}; // (url): a relay opened again after a close, or after its socket went deaf (see deaf())
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

  // Opens (or reopens) the socket to `url`; `opened` runs once it opens or fails. One dialled for a `deaf` one (see
  // deaf()) is silent from the start: it takes no pipe batch until its relay answers.
  dial(url, opened = () => {}, deaf = false) {
    if (this.closed) return;
    const again = this.sockets.some((s) => s.relayUrl === url);
    for (const s of this.sockets) if (s.relayUrl === url) this.forget(s);
    this.sockets = this.sockets.filter((s) => s.relayUrl !== url);
    const ws = new WebSocket(url);
    ws.relayUrl = url;
    ws.onopen = () => {
      this.stats.relaysOpen++;
      ws.counted = true;
      // Its relay's wait (`retry`) stays until this socket has stayed open a while (see lostSocket()); the timer
      // that dialled it has run.
      ws.opened = clock();
      // Since a little before now: what was published while we were away has gone.
      const filter = { kinds: [KIND], '#p': [this.myTag, this.roomTag], since: Math.floor(Date.now() / 1000) - 30 };
      if (this.listen) ws.send(JSON.stringify(['REQ', this.sub, filter]));
      opened();
      if (again) {
        this.stats.reconnects++;
        this.onReconnect(url);
      }
    };
    ws.onerror = () => opened();
    ws.onclose = () => this.lostSocket(ws);
    ws.onmessage = (m) => void this.onRelay(url, m.data, ws).catch(() => {});
    ws.dialled = clock();
    if (deaf) ws.asked = -Infinity;
    this.sockets.push(ws);
  }

  // `ws` carries nothing more: what waited on it is lost, as on a dead path. Events to keep on it
  // count as refused, and its reads as cut short.
  forget(ws) {
    clearTimeout(ws.pumpTimer);
    clearTimeout(ws.deafTimer);
    ws.q = [];
    for (const e of ws.low ?? []) e.done(false);
    ws.low = [];
    for (const e of ws.inflight?.values() ?? []) e.done?.(false);
    ws.inflight?.clear();
    for (const r of [...(ws.queries?.values() ?? [])]) r.end(false);
  }

  lostSocket(ws) {
    if (ws.counted) this.stats.relaysOpen--;
    ws.counted = false;
    this.forget(ws);
    if (this.closed || !this.sockets.includes(ws)) return;
    this.release(); // it may have been the one relay free
    if (this.retry.get(ws.relayUrl)?.timer) return;
    // Back to RECONNECT_MS after a socket open MAX_RECONNECT_MS or more; after one that closed sooner, or never
    // opened (no `opened`), the wait goes on doubling.
    const ms = clock() - ws.opened >= MAX_RECONNECT_MS ? RECONNECT_MS : this.retry.get(ws.relayUrl)?.ms ?? RECONNECT_MS;
    const timer = setTimeout(() => {
      this.retry.set(ws.relayUrl, { ms: Math.min(ms * 2, MAX_RECONNECT_MS), timer: null });
      this.dial(ws.relayUrl);
    }, ms);
    this.retry.set(ws.relayUrl, { ms, timer });
  }

  // The network came back or changed, or the page is showing again: every relay that
  // isn't open is redialled now, its wait starting over. On going online (`all`), open
  // sockets are too, since their TCP connection may be on the old network and take
  // minutes to time out.
  redial(all) {
    if (this.closed) return;
    for (const ws of [...this.sockets]) {
      const open = ws.readyState === WebSocket.OPEN;
      const stuck = ws.readyState === WebSocket.CONNECTING && clock() - ws.dialled < CONNECT_TIMEOUT_MS;
      if ((open && !all) || stuck) continue;
      clearTimeout(this.retry.get(ws.relayUrl)?.timer);
      this.retry.delete(ws.relayUrl);
      this.swap(ws);
    }
  }

  // Closes `ws`, which isn't lost (its onclose doesn't run), and dials its relay again (see dial()).
  swap(ws, deaf = false) {
    if (ws.counted) this.stats.relaysOpen--;
    ws.counted = false;
    ws.onclose = null;
    ws.onmessage = null;
    try {
      ws.close();
    } catch {}
    this.dial(ws.relayUrl, undefined, deaf);
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

  // The socket the next lossy event goes to, from `turn` on, of the open ones that haven't
  // banned us: the first that neither holds us for its rate nor is silent (see silent()) and
  // could take it within HOLD_MS (one slowed by its own refusals, behind what else waits there,
  // is passed over like a held one), else the one of those free soonest; with only silent ones
  // left, the same of them; -1 if none. Once closed, what waits goes at once, but a held relay
  // is sent nothing: it comes last.
  next(now) {
    let best = -1, rank = Infinity, soonest = Infinity;
    for (let i = 0; i < this.sockets.length; i++) {
      const k = (this.turn + i) % this.sockets.length;
      const ws = this.sockets[k];
      if (ws.readyState !== WebSocket.OPEN || this.banned.get(ws.relayUrl)?.until > now) continue;
      const f = this.free(ws, now);
      const held = this.paused.get(ws.relayUrl)?.until > now;
      const ready = !held && f <= HOLD_MS;
      const r = (this.closed && held ? 4 : 0) + (this.silent(ws, now) ? 2 : 0) + (ready ? 0 : 1);
      if (r === 0) return k;
      // The first ready one in turn, else the one free soonest.
      if (r < rank || (r === rank && !ready && f < soonest)) (best = k), (rank = r), (soonest = f);
    }
    return best;
  }

  // Whether `ws`'s relay has answered nothing, taken or refused, since an event sent to it
  // SILENT_MS or more ago: its socket may be half-open, or the relay stalled. Its next OK ends
  // it. The mark is the time of the first event sent since its last OK, kept beside `inflight`,
  // so one pushed out of `inflight` unanswered (past RELAY_QUEUE) doesn't end it.
  silent(ws, now) {
    return now - (ws.asked ?? now) >= SILENT_MS;
  }

  // Whether `ws` has heard nothing at all from its relay since an event sent through every relay deafAfter() or more
  // ago: our socket may be half-open (see REDIAL_SILENT_MS). The mark is the time of the first such event sent since
  // anything came on it; whatever comes clears it.
  deaf(ws, now) {
    return now - (ws.unheard ?? now) >= this.deafAfter(ws.relayUrl);
  }

  // How long a socket to `url` may hear nothing before it is deaf: REDIAL_SILENT_MS, doubling with each of the relay's
  // redials since it last answered in time, to MAX_RECONNECT_MS; Infinity after REDIAL_TRIES of them.
  deafAfter(url) {
    const n = this.redialled.get(url) ?? 0;
    return n < REDIAL_TRIES ? Math.min(REDIAL_SILENT_MS * 2 ** n, MAX_RECONNECT_MS) : Infinity;
  }

  // Redials `ws`'s relay once `ws` is deaf, and until then looks again when it may be. Its timer goes when anything
  // comes on it, and when it is replaced or lost (see forget()); nothing is dialled once closed (see dial()).
  revive(ws) {
    const now = clock();
    const ms = this.deafAfter(ws.relayUrl);
    if (!this.deaf(ws, now)) return void (ms < Infinity && (ws.deafTimer = setTimeout(() => this.revive(ws), Math.ceil(ws.unheard + ms - now))));
    this.redialled.set(ws.relayUrl, (this.redialled.get(ws.relayUrl) ?? 0) + 1);
    this.stats.redials++;
    this.swap(ws, true);
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
  // that banned us or holds us for its rate. Events to keep (`low`, see store()) go after, one
  // each STORE_EVERY_MS at most, and not once closed.
  pump(ws) {
    clearTimeout(ws.pumpTimer);
    ws.pumpTimer = null;
    const q = (ws.q ??= []);
    const low = ws.low;
    if ((!q.length && !low?.length) || ws.readyState !== WebSocket.OPEN) return;
    const now = clock();
    while (q.length && now - q[0].at > RELAY_QUEUE_MS) {
      q.shift();
      this.stats.unsent++;
    }
    while (low?.length && now - low[0].at > STORE_QUEUE_MS) low.shift().done(false);
    const url = ws.relayUrl;
    const hold = Math.max(this.banned.get(url)?.until ?? 0, this.paused.get(url)?.until ?? 0) - now;
    // Once closed, a held relay is sent nothing: what waits goes through the free ones
    // (close() moves lossy events there), and there is always one (see release()).
    if (hold > 0) return this.closed ? undefined : this.pumpIn(ws, hold);
    const r = this.rate(url, now);
    const inflight = (ws.inflight ??= new Map());
    const send = (e) => {
      r.tokens--;
      e.sent = now;
      // Kept until the relay answers OK, to send it again if refused for the rate.
      inflight.set(e.id, e);
      if (inflight.size > RELAY_QUEUE) {
        const [id, old] = inflight.entries().next().value;
        inflight.delete(id);
        old.done?.(false);
      }
      ws.asked ??= now;
      // One sent through every relay starts the wait for the socket to hear something (see deaf()).
      if (!e.lossy && !e.stored && ws.unheard == null) {
        ws.unheard = now;
        this.revive(ws);
      }
      ws.send(e.msg);
    };
    while (q.length && (r.tokens >= 1 || this.closed)) send(q.shift());
    if (q.length) return this.pumpIn(ws, ((1 - r.tokens) * 1000) / r.rate);
    if (!low?.length || this.closed) return;
    const due = Math.max(((1 - r.tokens) * 1000) / r.rate, (ws.lowAt ?? -Infinity) + STORE_EVERY_MS - now);
    if (due > 0) return this.pumpIn(ws, due);
    ws.lowAt = now;
    const e = low.shift();
    send(e);
    e.timer = setTimeout(() => {
      if (inflight.get(e.id) === e) inflight.delete(e.id);
      e.done(false);
    }, STORE_OK_MS);
    if (low.length) this.pumpIn(ws, STORE_EVERY_MS);
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
  // keep pace, and one slowed behind its backlog or silent gives up its turns (see next()).
  wait() {
    const now = clock();
    const k = this.next(now);
    if (k >= 0) return this.free(this.sockets[k], now);
    let ms = Infinity;
    for (const ws of this.sockets) if (ws.readyState === WebSocket.OPEN) ms = Math.min(ms, this.free(ws, now));
    return ms === Infinity ? 0 : ms;
  }

  // A message from relay `url`, on socket `ws` (that relay's, if not given).
  async onRelay(url, raw, ws = this.sockets.find((s) => s.relayUrl === url)) {
    // Anything at all: the socket hears (see deaf()).
    if (ws) {
      ws.unheard = null;
      clearTimeout(ws.deafTimer);
    }
    let m;
    try {
      m = JSON.parse(raw);
    } catch {
      return;
    }
    // Relay messages are arrays; `null` and other JSON values are ignored, not dereferenced.
    if (!Array.isArray(m)) return;
    if (m[0] === 'OK') return this.answered(url, m[1], m[2], String(m[3]));
    // A read of stored events (see query()): its events, and its end.
    const read = typeof m[1] === 'string' ? ws?.queries?.get(m[1]) : undefined;
    if (read) {
      if (m[0] === 'EVENT') read.take(m[2]);
      else if (m[0] === 'EOSE' || m[0] === 'CLOSED') read.end(m[0] === 'EOSE');
      return;
    }
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
    if (ws) ws.asked = null; // it answers (see silent())
    const now = clock();
    // In time, on a socket open MAX_RECONNECT_MS or more: its redials start over (see deafAfter()). A relay that answers
    // each new socket at first, then goes deaf again, only ever answers on young ones.
    if (e && now - e.sent < SILENT_MS && now - ws.dialled >= MAX_RECONNECT_MS) this.redialled.delete(url);
    const p = this.paused.get(url);
    if (ok === true) {
      // It took one sent since its last hold began: a refusal from now on starts over.
      if (p && e?.sent >= p.at) p.took = true;
      return void e?.done?.(true);
    }
    if (ok !== false) return void e?.done?.(false);
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
      e?.done?.(false);
      if (ws) this.forget(ws);
      return void this.release();
    }
    if (kind !== 'rate') return void e?.done?.(false);
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
    // A lossy one goes again through the next relay, now that this one is held; one to keep, at
    // the head of what waits to be kept there.
    if (again) e.stored ? (clearTimeout(e.timer), (ws.low ??= []).unshift(e)) : e.lossy ? this.stripe(e) : this.queue(ws, e);
    else e?.done?.(false);
    if (ws) this.pump(ws);
  }

  // Reads stored events: `filter` (NIP-01; or a function of the relay's url giving that relay's) to
  // every open relay that hasn't banned us, under a subscription of its own, closed at the relay's EOSE
  // or after `ms`. `keep(ev, url)` names the slot an event from the relay at `url` goes in, or null to
  // drop it (it gets the event as the relay sent it: any JSON value), and
  // each relay keeps the newest event of each slot, by created_at and then the lower id, as relays
  // replace them: so a relay that sends more than asked costs a check each, not memory. Resolves to
  // a Map: relay url -> { complete, events: Map(slot -> event) }, `complete` once that relay sent
  // EOSE in time; false for a relay that wasn't open, refused the read (CLOSED), closed or banned
  // us meanwhile.
  async query(filter, { ms = QUERY_MS, keep }) {
    await this.ready;
    const out = new Map();
    const reads = [];
    for (const ws of this.sockets) {
      const r = { complete: false, events: new Map() };
      out.set(ws.relayUrl, r);
      if (this.closed || ws.readyState !== WebSocket.OPEN || this.banned.get(ws.relayUrl)?.until > clock()) continue;
      const sub = hex(crypto.getRandomValues(new Uint8Array(8)));
      reads.push(new Promise((resolve) => {
        const timer = setTimeout(() => read.end(false), ms);
        const read = {
          take: (ev) => {
            const slot = keep(ev, ws.relayUrl);
            if (slot === null || slot === undefined) return;
            const old = r.events.get(slot);
            if (!old || ev.created_at > old.created_at || (ev.created_at === old.created_at && ev.id < old.id)) r.events.set(slot, ev);
          },
          end: (complete) => {
            if (ws.queries?.get(sub) !== read) return;
            ws.queries.delete(sub);
            clearTimeout(timer);
            r.complete = complete;
            try {
              if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(['CLOSE', sub]));
            } catch {}
            resolve();
          },
        };
        (ws.queries ??= new Map()).set(sub, read);
        ws.send(JSON.stringify(['REQ', sub, typeof filter === 'function' ? filter(ws.relayUrl) : filter]));
      }));
    }
    await Promise.all(reads);
    return out;
  }

  // Keeps `ev`, a signed event (a piece of a doc's copy), on the relays at `urls`: queued on each,
  // behind what signalling sends there (see STORE_EVERY_MS). A Map: url -> a promise of true once
  // that relay took it (OK true), or false once it refused it (one refused for its rate is sent
  // again, RELAY_TRIES times in all), banned us, closed, held it STORE_QUEUE_MS or didn't answer
  // within STORE_OK_MS, or the signalling closed.
  store(ev, urls) {
    const msg = JSON.stringify(['EVENT', ev]);
    const out = new Map();
    for (const url of urls) {
      out.set(url, new Promise((resolve) => {
        const ws = this.sockets.find((s) => s.relayUrl === url);
        const now = clock();
        if (this.closed || !ws || ws.readyState !== WebSocket.OPEN || this.banned.get(url)?.until > now) return resolve(false);
        let settled = false;
        const e = {
          id: ev.id, msg, at: now, seq: ++this.seq, tries: 0, sent: 0, lossy: false, stored: true, timer: null,
          done: (ok) => {
            if (settled) return;
            settled = true;
            clearTimeout(e.timer);
            resolve(ok);
          },
        };
        (ws.low ??= []).push(e);
        this.pump(ws);
      }));
    }
    return out;
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
  // sockets close once it is published, and with them reads of stored events end and events
  // to keep count as refused (see forget()).
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

