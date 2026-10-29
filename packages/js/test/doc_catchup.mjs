// room.doc() catch-up for a late joiner on the routes a field test meets (NB-006d). Member A draws a
// whiteboard-sized board, as the whiteboard demo stores it: room.doc('board') with 40 pen strokes of 1000 points
// in its set 'strokes' and a title and background in its map 'meta'. Then C joins and opens the doc, five ways:
// (a) a direct link: a fake RTCPeerConnection whose DataChannel opens, as in room_fallback.mjs;
// (b) the fallback route: ICE never completes, so the link goes over the signalling pipe, here the real
//     NostrSignaling over fake NIP-01 relays in this process (no network) that take RELAY_RATE events a second
//     from each connection (burst RELAY_RATE) and RELAY_CONTENT characters of content an event, refuse what is
//     past the rate with 'rate-limited:' as public relays do (nostr.js then slows down and sends it again), and
//     take RELAY_MS each way;
// (c) a background tab answering: A in a worker whose timers fire no sooner than 1000 ms, as in
//     room_throttled.mjs, over LocalTransport;
// (d) A, which keeps the board on its device (persist), leaves before C joins, and comes back later;
// (e) (b) and (c) at once: A in the background on the fallback route, where the pipe's batching and rate waits
//     are timers too;
// (f) two members hold the board, B linked to C directly and A over the pipe: C asks B first and takes one whole
//     answer, not one from each;
// (g) (b) with relays that take 3 events a second, then 2: the join's burst draws a refusal from every relay, and
//     the room must neither stop signalling nor lose what was refused;
// (h) (b) with C deaf to one of the three relays: it delivers C nothing, so the pipe batches A sends through it
//     (each goes through one relay) are lost, and the core sends what they carried again.
// For each it prints how long after the link C held every entry (or what it held at the deadline), the doc
// frames both sides sent (asks, answers, deltas and their bytes), what went on the wire (datagrams, core
// packets and retransmits, pipe batches, relay events) and when C's ask of A was pending or timed out. C must
// hold the whole doc within DEADLINE_MS of the link in (a) to (d), (f) and (g), within BACKGROUND_MS in (e) and
// DEAF_MS in (h); its first entries within FIRST_MS in (b), (g) and (h), and FIRST_BACKGROUND_MS in (e). In (d) C
// holds nothing while alone, and A comes back to its whole board from its device.
//
// It failed on main at 45c7cfd (NB-006d): (b) took 32 s and (e) held 26 of 42 entries after 120 s. (g) failed at
// fe2a4e9: one refusal from each relay held all three for 30 s, and C never linked. At 9f26bd1, with each batch
// sent through every relay, (g) at 2 a second took 33 to 39 s.
//
//   node packages/js/test/doc_catchup.mjs [--runs=abcdefgh] [--watch=60] [--relay-rate=10]
//
// --watch keeps watching a run that missed the deadline until that many seconds after the link, to see where
// it ends up; --relay-rate sets RELAY_RATE, the rate of every run's relays but (g)'s.
import { Worker, MessageChannel, isMainThread, parentPort, workerData, threadId } from 'node:worker_threads';
import { joinRoom, Room, memoryStore } from '../src/index.js';
import { WebRtcTransport, BroadcastSignaling } from '../src/webrtc.js';

const APP = 'peerwire-test';
const STROKES = 40;
const POINTS = 1000;
const META = { title: 'Field test board', bg: 'grid' };
const ENTRIES = STROKES + Object.keys(META).length;
const DEADLINE_MS = 15000;
const BACKGROUND_MS = 30000; // (e)
const FIRST_MS = 3000; // (b): C's first entries
const FIRST_BACKGROUND_MS = 5000; // (e)
// (h): a third of A's batches are lost, so the core sends what they carried again. It backs each lost fragment off
// (to 10 s), and the answer, in order, waits behind one lost three or four times: 27 s alone, more beside other tests.
const DEAF_MS = 60000;
const LINK_MS = 30000; // for C to link to A at all
const ALONE_MS = 2000; // (d): how long C waits alone
const CLAMP_MS = 1000; // (c): a background tab's timers
const now = () => performance.timeOrigin + performance.now(); // comparable across threads
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The board as whiteboard.js stores it: { k, c, w, p }, p the points [x0, y0, x1, y1, ...] in board units
// rounded to 0.1, strokes that wander like a hand's. Seeded, so every run draws the same board.
function board() {
  let seed = 6;
  const rnd = () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
  const round = (v) => Math.round(v * 10) / 10;
  const colors = ['#1e1f24', '#e5484d', '#f76b15', '#f5a524', '#30a46c', '#0090ff', '#8e4ec6', '#8b8d98'];
  return Array.from({ length: STROKES }, (_, i) => {
    let x = rnd() * 3000 - 1500;
    let y = rnd() * 1800 - 900;
    let a = rnd() * 2 * Math.PI;
    const p = [];
    for (let j = 0; j < POINTS; j++) {
      a += (rnd() - 0.5) * 0.6;
      x += Math.cos(a) * 3;
      y += Math.sin(a) * 3;
      p.push(round(x), round(y));
    }
    return { k: 'pen', c: colors[i % colors.length], w: [2, 4, 8, 16][i % 4], p };
  });
}
const BOARD = board();

function draw(doc) {
  const strokes = doc.set('strokes');
  for (const s of BOARD) if (strokes.add(s) === null) throw new Error('the doc refused a stroke');
  for (const [k, v] of Object.entries(META)) doc.map('meta').set(k, v);
}

// The entries of the board a doc holds.
const held = (doc) => doc.set('strokes').entries().length + Object.entries(META).filter(([k, v]) => doc.map('meta').get(k) === v).length;

// Doc frames each room sends and receives, and the datagrams each endpoint sends, by peer id.
const docLog = []; // { self, peer, dir: 'out' | 'in', t, end, entries, bytes, at }
const writeDoc = Room.prototype.writeDoc;
Room.prototype.writeDoc = function (peer, m, ...rest) {
  docLog.push({ self: this.self, peer, dir: 'out', t: m.t, end: m.end === 1, entries: m.e?.length ?? 0, bytes: JSON.stringify(m).length, at: now() });
  return writeDoc.call(this, peer, m, ...rest);
};
const dec = new TextDecoder();
// A doc frame's JSON text as it arrives (inflated, if it came deflated).
const docFrame = Room.prototype.docFrame;
Room.prototype.docFrame = function (peer, text) {
  const t = Number(/"t":(\d)/.exec(dec.decode(text.subarray(0, 64)))?.[1]);
  docLog.push({ self: this.self, peer, dir: 'in', t, end: /"end":1[,}]/.test(dec.decode(text.subarray(-64))), bytes: text.length, at: now() });
  return docFrame.call(this, peer, text);
};
const wire = new Map(); // peer id -> { dgs, bytes }
const sent = (id, n) => {
  let w = wire.get(id);
  if (!w) wire.set(id, (w = { dgs: 0, bytes: 0 }));
  w.dgs++;
  w.bytes += n;
};
for (const k of ['dcSend', 'pipeSend']) {
  const f = WebRtcTransport.prototype[k];
  WebRtcTransport.prototype[k] = function (x, bytes) {
    sent(this.ep.id, bytes.length);
    return f.call(this, x, bytes);
  };
}
const bcSend = BroadcastSignaling.prototype.send;
BroadcastSignaling.prototype.send = function (to, body) {
  if (body?.kind === 'dg') sent(this.id, body.bytes.length); // LocalTransport's datagrams
  return bcSend.call(this, to, body);
};

// A fake RTCPeerConnection, as in room_fallback.mjs: two of them in one thread link once the offerer has the
// answer, unless `iceHangs`, when ICE never completes (and the far one may be in another thread), or the pair
// of owners is in `hangs` ('a|c').
let iceHangs = false;
const hangs = new Set();
const pcs = new Map();
let tokens = 0;
class FakeDc {
  constructor() {
    this.readyState = 'connecting';
    this.bufferedAmount = 0;
    this.other = null;
  }
  send(bytes) {
    const o = this.other;
    const copy = bytes.slice().buffer;
    setTimeout(() => o.readyState === 'open' && o.onmessage?.({ data: copy }));
  }
  open() {
    this.readyState = 'open';
    this.onopen?.();
  }
}
const fakePc = (owner) => class FakePc {
  constructor() {
    this.owner = owner;
    this.token = `${threadId}.${++tokens}`;
    this.connectionState = 'new';
    this.iceGatheringState = 'complete';
    this.localDescription = null;
    pcs.set(this.token, this);
  }
  createDataChannel() {
    this.dc = new FakeDc();
    return this.dc;
  }
  async createOffer() {
    return { type: 'offer', sdp: `fake:${this.token}` };
  }
  async createAnswer() {
    return { type: 'answer', sdp: `fake:${this.token}` };
  }
  async setLocalDescription(d) {
    this.localDescription = { ...d, toJSON: () => ({ type: d.type, sdp: d.sdp }) };
  }
  async setRemoteDescription(d) {
    if (iceHangs) return;
    const remote = pcs.get(d.sdp.slice(5));
    if (!remote) throw new Error('unknown remote');
    if (hangs.has(`${this.owner}|${remote.owner}`) || hangs.has(`${remote.owner}|${this.owner}`)) return;
    this.remote = remote;
    if (d.type === 'answer') setTimeout(() => this.connect(remote), 5);
  }
  connect(r) {
    this.dc.other = r.dc;
    r.dc.other = this.dc;
    for (const pc of [this, r]) pc.connectionState = 'connected';
    this.dc.open();
    r.dc.open();
  }
  addEventListener() {}
  async addIceCandidate() {}
  close() {
    this.connectionState = 'closed';
    pcs.delete(this.token);
  }
};

if (!isMainThread) {
  // (c) and (e): A, its timers clamped as in a background tab, over LocalTransport or, for (e), the pipe
  // through the main thread's relays (`relayPort` carries its sockets).
  const { roomId, clamp, relays, relayPort } = workerData;
  for (const k of ['setTimeout', 'setInterval']) {
    const real = globalThis[k];
    globalThis[k] = (f, ms = 0, ...rest) => real(f, Math.max(ms, clamp), ...rest);
  }
  let opts = {};
  if (relays) {
    const open = new Map(); // id -> socket
    let ids = 0;
    globalThis.WebSocket = class RelaySocket {
      static CONNECTING = 0;
      static OPEN = 1;
      static CLOSING = 2;
      static CLOSED = 3;
      constructor(url) {
        this.url = url;
        this.readyState = 0;
        this.id = ++ids;
        open.set(this.id, this);
        relayPort.postMessage({ ws: this.id, url });
      }
      send(raw) {
        if (this.readyState === 1) relayPort.postMessage({ ws: this.id, raw });
      }
      close() {
        if (this.readyState === 3) return;
        this.readyState = 3;
        open.delete(this.id);
        relayPort.postMessage({ ws: this.id, close: 1 });
        setTimeout(() => this.onclose?.());
      }
    };
    relayPort.on('message', (m) => {
      const ws = open.get(m.ws);
      if (m.open && ws) (ws.readyState = 1), ws.onopen?.();
      else if (m.data && ws?.readyState === 1) ws.onmessage?.({ data: m.data });
    });
    iceHangs = true;
    opts = { signaling: 'nostr', relays, fallbackMs: 300, RTCPeerConnection: fakePc('a') };
  }
  const a = await joinRoom({ appId: APP, roomId, presence: { name: 'a' }, ...opts });
  draw(a.doc('board'));
  parentPort.on('message', async (m) => {
    if (m === 'stats') parentPort.postMessage({ docLog, wire: wire.get(a.self), core: a.stats(), piped: a.transport.piped, nostr: a.signaling.stats });
    if (m === 'leave') {
      await a.leave();
      process.exit(0);
    }
  });
  parentPort.postMessage({ ready: a.self });
} else {
  const arg = (name, dflt) => process.argv.find((a) => a.startsWith(`--${name}=`))?.slice(name.length + 3) ?? dflt;
  const RUNS = arg('runs', 'abcdefgh');
  const WATCH_MS = Math.max(DEADLINE_MS, Number(arg('watch', 0)) * 1000);
  const RELAY_RATE = Number(arg('relay-rate', 10));
  let relayRate = RELAY_RATE; // the relays made from now on: (g) sets its own
  let deafTo = null; // (h): a connection made to this relay from now on is delivered no events
  const RELAY_CONTENT = 64 * 1024;
  const RELAY_MS = 50;
  const RELAYS = ['wss://relay-1.test', 'wss://relay-2.test', 'wss://relay-3.test'];
  process.on('unhandledRejection', (e) => {
    console.error(`FAIL unhandled: ${e?.stack ?? e}`);
    process.exit(1);
  });

  // Fake NIP-01 relays: EVENT gets an OK, REQ subscribes by kind, '#p' and since. Each connection may publish
  // relayRate events a second (burst relayRate) of up to RELAY_CONTENT characters of content. A deaf one is
  // delivered none (it counts them in `missed`).
  const sockets = new Set(); // open ones, every relay's
  class FakeRelaySocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSING = 2;
    static CLOSED = 3;
    constructor(url) {
      this.url = url;
      this.readyState = 0;
      this.subs = new Map();
      this.rate = relayRate;
      this.tokens = this.rate;
      this.refilled = Date.now();
      this.deaf = url === deafTo;
      this.stats = { accepted: 0, limited: 0, tooLarge: 0, chars: 0, missed: 0 };
      setTimeout(() => {
        if (this.readyState !== 0) return;
        this.readyState = 1;
        sockets.add(this);
        this.onopen?.();
      }, 5);
    }
    deliver(m) {
      setTimeout(() => this.readyState === 1 && this.onmessage?.({ data: JSON.stringify(m) }), RELAY_MS);
    }
    send(raw) {
      if (this.readyState !== 1) return;
      const m = JSON.parse(raw);
      if (m[0] === 'REQ') return void this.subs.set(m[1], m[2]);
      if (m[0] === 'CLOSE') return void this.subs.delete(m[1]);
      if (m[0] !== 'EVENT') return;
      const ev = m[1];
      const t = Date.now();
      this.tokens = Math.min(this.rate, this.tokens + ((t - this.refilled) / 1000) * this.rate);
      this.refilled = t;
      if (ev.content.length > RELAY_CONTENT) {
        this.stats.tooLarge++;
        return this.deliver(['OK', ev.id, false, 'invalid: event too large']);
      }
      if (this.tokens < 1) {
        this.stats.limited++;
        return this.deliver(['OK', ev.id, false, 'rate-limited: slow down']);
      }
      this.tokens--;
      this.stats.accepted++;
      this.stats.chars += ev.content.length;
      this.deliver(['OK', ev.id, true, '']);
      const p = ev.tags.find((x) => x[0] === 'p')?.[1];
      for (const s of sockets) {
        if (s === this || s.url !== this.url) continue;
        const sub = [...s.subs].find(([, f]) => f.kinds?.includes(ev.kind) && f['#p']?.includes(p) && ev.created_at >= (f.since ?? 0));
        if (sub && s.deaf) s.stats.missed++;
        else if (sub) s.deliver(['EVENT', sub[0], ev]);
      }
    }
    close() {
      if (this.readyState === 3) return;
      this.readyState = 3;
      sockets.delete(this);
      setTimeout(() => this.onclose?.());
    }
  }
  globalThis.WebSocket = FakeRelaySocket;

  // Watches C's doc from C's join (t0): when C links to A, then what C holds and its ask of A, until C holds
  // the whole board or WATCH_MS after the link.
  async function watch(c, dc, aId, t0, until = WATCH_MS) {
    const w = { link: null, whole: null, held: 0, series: [], states: [] };
    while (!c.peers.includes(aId)) {
      if (now() - t0 > LINK_MS) return w;
      await sleep(10);
    }
    // The link is when C's doc first sends to A (its ask, on A's join), or when this loop saw A join.
    w.at = Math.min(now(), docLog.find((f) => f.self === c.self && f.peer === aId && f.dir === 'out')?.at ?? Infinity);
    w.link = w.at - t0;
    let last = -1;
    let state = '';
    for (;;) {
      const t = now() - w.at;
      w.held = held(dc);
      if (w.held !== last) w.series.push([t, (last = w.held)]);
      // Doc internals, for the report only: the member asked, and whether its ask timed out (ASK_WAIT_MS).
      const s = dc.asking === aId ? 'asking A' : dc.members?.get(aId)?.stalled ? 'ask of A timed out' : 'not asking';
      if (s !== state) w.states.push([t, (state = s)]);
      if (w.held === ENTRIES) {
        w.whole = t;
        w.same = JSON.stringify(dc.set('strokes').entries().map((e) => e.value)) === JSON.stringify(BOARD);
        return w;
      }
      if (t > until) return w;
      await sleep(25);
    }
  }

  const s = (ms) => `${(ms / 1000).toFixed(1)} s`;
  const kb = (n) => `${(n / 1024).toFixed(0)} KB`;
  // Frames of type t that `log` holds from `self` (out) or to it (in), from `peer`, summed.
  function frames(log, self, dir, t, w) {
    const l = log.filter((f) => f.self === self && f.dir === dir && f.t === t);
    if (!l.length) return '0';
    const ends = l.filter((f) => f.end).map((f) => s(f.at - w.at));
    const entries = dir === 'out' ? `, ${l.reduce((n, f) => n + f.entries, 0)} entries` : '';
    return `${l.length} (${kb(l.reduce((n, f) => n + f.bytes, 0))}${entries}${ends.length ? `, end at ${ends.join(', ')}` : ''}; first at ${s(l[0].at - w.at)})`;
  }

  const limits = (rate) => `relays: ${rate} events/s per connection, ${kb(RELAY_CONTENT)} an event, ${RELAY_MS} ms`;
  const RELAY_LIMITS = limits(RELAY_RATE);
  // A member's pipe and Nostr counters, and what one of its relay connections took and refused.
  const pipe = (p, st, relay = {}) => `${p.sent} pipe batches, ${p.dropped} datagrams dropped from a full pipe queue; ${st.published} Nostr events, relay-1 accepted ${relay.accepted} (${kb(relay.chars ?? 0)} of content), rate-limited ${relay.limited}, too large ${relay.tooLarge}; backoffs ${st.backoffs ?? 0}, sent again ${st.resent ?? 0}, unsent ${st.unsent ?? 0}`;

  const results = [];
  // Prints run `key` and records whether it met its bar: the whole board within `deadline` of the link, and
  // its first entries within `first` (none with `bar` false: printed only).
  function report(key, what, w, { aId, cId, aLog = docLog, aWire, cWire, aCore, cCore, extra = [], bar = true, deadline = DEADLINE_MS, first = Infinity }) {
    w.first = w.series.find(([, n]) => n > 0)?.[0] ?? null;
    const ok = !bar || (w.whole !== null && w.whole <= deadline && w.same && w.first <= first);
    const head = w.link === null ? `C never linked to A within ${s(LINK_MS)}`
      : w.whole !== null ? `C linked ${s(w.link)} after joining and held the whole board ${s(w.whole)} after the link${w.same ? '' : ', BUT NOT THE SAME BOARD'}`
      : `C linked ${s(w.link)} after joining and held ${w.held} of ${ENTRIES} entries ${s(Math.max(WATCH_MS, deadline))} after the link`;
    console.log(`(${key}) ${what}: ${head}`);
    if (w.link !== null) {
      const series = w.series.length > 10 ? [...w.series.slice(0, 5), ...w.series.slice(-5)] : w.series;
      console.log(`    C held: ${series.map(([t, n]) => `${n} at ${s(t)}`).join(', ')}`);
      console.log(`    C's ask of A: ${w.states.map(([t, x]) => `${x} at ${s(t)}`).join(', ')}`);
      console.log(`    C sent: asks ${frames(docLog, cId, 'out', 2, w)}; answers ${frames(docLog, cId, 'out', 3, w)}; deltas ${frames(docLog, cId, 'out', 1, w)}`);
      console.log(`    A sent: asks ${frames(aLog, aId, 'out', 2, w)}; answers ${frames(aLog, aId, 'out', 3, w)}; deltas ${frames(aLog, aId, 'out', 1, w)}`);
      console.log(`    C received: asks ${frames(docLog, cId, 'in', 2, w)}; answers ${frames(docLog, cId, 'in', 3, w)}`);
    }
    {
      console.log(`    wire: A ${aWire?.dgs ?? 0} datagrams (${kb(aWire?.bytes ?? 0)}), core ${aCore.txPackets} packets, ${aCore.retransmits} retransmits; C ${cWire?.dgs ?? 0} datagrams (${kb(cWire?.bytes ?? 0)}), ${cCore.txPackets} packets, ${cCore.retransmits} retransmits`);
      for (const x of extra) console.log(`    ${x}`);
    }
    const firsts = first < Infinity ? `, first entries ${s(w.first)} (at most ${s(first)})` : '';
    const line = !bar ? `(${key}) printed only: ${head}` : ok ? `(${key}) whole board ${s(w.whole)} after the link (at most ${s(deadline)})${firsts}` : `(${key}) ${head} (deadline ${s(deadline)})${w.first !== null ? firsts : ''}`;
    results.push({ key, ok, line });
  }

  const docBytes = JSON.stringify(BOARD).length;
  console.log(`board: ${STROKES} strokes of ${POINTS} points and ${Object.keys(META).length} meta keys, ${ENTRIES} entries, ${kb(docBytes)} as JSON; deadline ${s(DEADLINE_MS)} after C links to A`);

  // (a) A direct link.
  if (RUNS.includes('a')) {
    const roomId = `catchup-a-${process.pid}`;
    const opts = (name) => ({ appId: APP, roomId, presence: { name }, RTCPeerConnection: fakePc(name) });
    const a = await joinRoom(opts('a'));
    draw(a.doc('board'));
    const t0 = now();
    const c = await joinRoom(opts('c'));
    const w = await watch(c, c.doc('board'), a.self, t0);
    report('a', 'direct link', w, { aId: a.self, cId: c.self, aWire: wire.get(a.self), cWire: wire.get(c.self), aCore: a.stats(), cCore: c.stats() });
    await Promise.all([a.leave(), c.leave()]);
  }

  // (b) The fallback route: ICE never completes, so the link goes over the signalling pipe through the relays.
  // (g) The same with relays that take `rate` events a second. (h) The same with C deaf to relay `deaf`.
  async function fallback(key, rate, bars, deaf = null) {
    iceHangs = true;
    relayRate = rate;
    const roomId = `catchup-${key}${rate}-${process.pid}`;
    const name = key === 'g' ? `${key} at ${rate}/s` : key;
    const opts = (name) => ({ appId: APP, roomId, presence: { name }, signaling: 'nostr', relays: RELAYS, fallbackMs: 300, RTCPeerConnection: fakePc(name) });
    const a = await joinRoom(opts('a'));
    draw(a.doc('board'));
    const t0 = now();
    deafTo = deaf;
    const c = await joinRoom(opts('c'));
    const w = await watch(c, c.doc('board'), a.self, t0, Math.max(WATCH_MS, bars.deadline));
    const route = c.route(a.self)?.kind;
    const lost = deaf && c.signaling.sockets.find((ws) => ws.url === deaf)?.stats.missed;
    const through = deaf && a.signaling.sockets.find((ws) => ws.url === deaf)?.stats.accepted;
    report(name, `fallback over the signalling pipe (${limits(rate)}${deaf ? `; C deaf to ${deaf}` : ''})`, w, {
      aId: a.self, cId: c.self, aWire: wire.get(a.self), cWire: wire.get(c.self), aCore: a.stats(), cCore: c.stats(), ...bars,
      extra: [`route C->A: ${route}`, `A: ${pipe(a.transport.piped, a.signaling.stats, a.signaling.sockets[0]?.stats)}`, `C: ${pipe(c.transport.piped, c.signaling.stats, c.signaling.sockets[0]?.stats)}`,
        ...(deaf ? [`${deaf} took ${through} events from A and kept ${lost} from C`] : [])],
    });
    if (w.link !== null && route !== 'signaling') results.push({ key, ok: false, line: `(${name}) C reached A over ${route}, not the pipe` });
    // Not vacuous: the relay C is deaf to carried some of what A sent it.
    if (deaf && !(lost > 0)) results.push({ key, ok: false, line: `(${name}) ${deaf} kept nothing from C` });
    deafTo = null;
    await Promise.all([a.leave(), c.leave()]);
    iceHangs = false;
    relayRate = RELAY_RATE;
  }
  if (RUNS.includes('b')) await fallback('b', RELAY_RATE, { deadline: DEADLINE_MS, first: FIRST_MS });

  // (c) A answers from a background tab: its timers fire no sooner than CLAMP_MS. (e) The same over the
  // fallback route of (b): the pipe's batching and rate waits are timers too. A runs in a worker; for (e) its
  // relay sockets are the main thread's, over a MessageChannel.
  async function background(key, what, pipeRoute) {
    const roomId = `catchup-${key}-${process.pid}`;
    const ch = pipeRoute ? new MessageChannel() : null;
    const port = ch?.port1;
    const aSockets = [];
    if (pipeRoute) {
      const open = new Map();
      port.on('message', (m) => {
        if (m.url) {
          const ws = new FakeRelaySocket(m.url);
          ws.onopen = () => port.postMessage({ ws: m.ws, open: 1 });
          ws.onmessage = (e) => port.postMessage({ ws: m.ws, data: e.data });
          open.set(m.ws, ws);
          aSockets.push(ws);
        } else if (m.raw) open.get(m.ws)?.send(m.raw);
        else if (m.close) open.get(m.ws)?.close();
      });
      iceHangs = true;
    }
    const data = pipeRoute ? { roomId, clamp: CLAMP_MS, relays: RELAYS, relayPort: ch.port2 } : { roomId, clamp: CLAMP_MS };
    const worker = new Worker(new URL(import.meta.url), { workerData: data, transferList: pipeRoute ? [ch.port2] : [] });
    const reply = () => new Promise((r) => worker.once('message', r));
    const aId = (await reply()).ready;
    const t0 = now();
    const extra = pipeRoute ? { signaling: 'nostr', relays: RELAYS, fallbackMs: 300, RTCPeerConnection: fakePc('c') } : {};
    const c = await joinRoom({ appId: APP, roomId, presence: { name: 'c' }, ...extra });
    const w = await watch(c, c.doc('board'), aId, t0, key === 'e' ? Math.max(WATCH_MS, BACKGROUND_MS) : WATCH_MS);
    worker.postMessage('stats');
    const st = await reply();
    const lines = [];
    if (pipeRoute) {
      const route = c.route(aId)?.kind;
      lines.push(`route C->A: ${route}`, `A: ${pipe(st.piped, st.nostr, aSockets[0]?.stats)}`, `C: ${pipe(c.transport.piped, c.signaling.stats, c.signaling.sockets[0]?.stats)}`);
    }
    const bars = key === 'e' ? { deadline: BACKGROUND_MS, first: FIRST_BACKGROUND_MS } : {};
    report(key, what, w, { aId, cId: c.self, aLog: st.docLog, aWire: st.wire, cWire: wire.get(c.self), aCore: st.core, cCore: c.stats(), extra: lines, ...bars });
    const gone = new Promise((r) => worker.once('exit', r));
    worker.postMessage('leave');
    await Promise.all([c.leave(), gone]);
    port?.close();
    iceHangs = false;
  }
  if (RUNS.includes('c')) await background('c', `A's timers clamped to ${CLAMP_MS} ms (a background tab), LocalTransport`, false);

  // (d) A leaves before C joins: nobody online holds the doc, so C alone gets nothing (a copy held while nobody
  // is online is NB-006g). A kept the board on its device (persist), so when A comes back the board is back from
  // that copy, and C gets it from A.
  if (RUNS.includes('d')) {
    const roomId = `catchup-d-${process.pid}`;
    const memory = memoryStore();
    let kept = 0;
    const store = { ...memory, set: (key, bytes) => ((kept = bytes.length), memory.set(key, bytes)) };
    const a = await joinRoom({ appId: APP, roomId, presence: { name: 'a' } });
    draw(a.doc('board', { persist: store }));
    await a.leave();
    console.log(`(d) A kept the board on its device in ${kb(kept)}, sealed`);
    const c = await joinRoom({ appId: APP, roomId, presence: { name: 'c' } });
    const dc = c.doc('board');
    await sleep(ALONE_MS);
    const n = held(dc);
    const asks = docLog.filter((f) => f.self === c.self && f.dir === 'out').length;
    console.log(`(d) A left before C joined: C alone held ${n} of ${ENTRIES} entries ${s(ALONE_MS)} after opening the doc, with ${c.peers.length} members to ask and ${asks} doc frames sent`);
    const t0 = now();
    const a2 = await joinRoom({ appId: APP, roomId, presence: { name: 'a' } });
    const da = a2.doc('board', { persist: store });
    const found = await da.loaded;
    const back = held(da);
    const w = await watch(c, dc, a2.self, t0);
    report('d', `A came back, its board ${found} from its device (${back} of ${ENTRIES} entries ${s(now() - t0)} after joining), and C caught up from it`, w, { aId: a2.self, cId: c.self, aWire: wire.get(a2.self), cWire: wire.get(c.self), aCore: a2.stats(), cCore: c.stats() });
    if (n !== 0 || found !== 'loaded' || back !== ENTRIES) results.push({ key: 'd', ok: false, line: `(d) C alone held ${n} entries; A came back to a board ${found} with ${back} entries` });
    await Promise.all([a2.leave(), c.leave()]);
  }

  if (RUNS.includes('e')) await background('e', `A's timers clamped to ${CLAMP_MS} ms on the fallback route of (b) (${RELAY_LIMITS})`, true);

  // (f) Two members hold the board: B, linked to C directly, and A, over the pipe as in (b), to B too (so B
  // can't relay A for C). C asks B first, the better route, and takes the board from it: the entries A and B
  // send C in answers come to one whole board, not two.
  if (RUNS.includes('f')) {
    hangs.add('a|c').add('a|b');
    const roomId = `catchup-f-${process.pid}`;
    const opts = (name) => ({ appId: APP, roomId, presence: { name }, signaling: 'nostr', relays: RELAYS, fallbackMs: 300, RTCPeerConnection: fakePc(name) });
    const a = await joinRoom(opts('a'));
    draw(a.doc('board'));
    const b = await joinRoom(opts('b'));
    const db = b.doc('board');
    const t = now();
    while (held(db) < ENTRIES && now() - t < LINK_MS) await sleep(25);
    const t0 = now();
    const c = await joinRoom(opts('c'));
    const w = await watch(c, c.doc('board'), b.self, t0);
    await sleep(2000); // A's answer, over the pipe, after B's
    const sent = (x) => docLog.filter((f) => f.self === x.self && f.peer === c.self && f.dir === 'out' && f.t === 3).reduce((n, f) => n + f.entries, 0);
    const [fromA, fromB] = [sent(a), sent(b)];
    const routes = `routes C->A: ${c.route(a.self)?.kind}, C->B: ${c.route(b.self)?.kind}`;
    report('f', `two holders, B direct and A over the pipe (${RELAY_LIMITS})`, w, {
      aId: b.self, cId: c.self, aWire: wire.get(b.self), cWire: wire.get(c.self), aCore: b.stats(), cCore: c.stats(),
      extra: [routes, `answers to C: ${fromB} entries from B, ${fromA} from A`],
    });
    if (fromA + fromB > ENTRIES + 8 || c.route(a.self)?.kind !== 'signaling' || c.route(b.self)?.kind !== 'direct') results.push({ key: 'f', ok: false, line: `(f) ${routes}; C was sent ${fromB} entries by B and ${fromA} by A, for ${ENTRIES}` });
    await Promise.all([a.leave(), b.leave(), c.leave()]);
    hangs.clear();
  }

  if (RUNS.includes('g')) {
    await fallback('g', 3, { deadline: DEADLINE_MS, first: FIRST_MS });
    await fallback('g', 2, { deadline: DEADLINE_MS, first: FIRST_MS });
  }

  if (RUNS.includes('h')) await fallback('h', RELAY_RATE, { deadline: DEAF_MS, first: FIRST_MS }, RELAYS[2]);

  const failed = results.filter((r) => !r.ok);
  for (const r of results) console.log(`${r.ok ? 'ok  ' : 'FAIL'} ${r.line}`);
  console.log(failed.length ? `doc_catchup: ${failed.length} of ${results.length} runs FAILED` : 'doc_catchup: ok');
  process.exit(failed.length ? 1 : 0);
}
