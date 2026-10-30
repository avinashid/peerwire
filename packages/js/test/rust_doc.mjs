// NB-006b: golden vectors for peerwire-sync (crates/peerwire-sync), the Rust port of doc.js, so the two stay one
// protocol. It makes the vectors from doc.js and checks they are the ones committed in
// crates/peerwire-sync/tests/vectors/ (`--write` writes them instead; run `cargo test -p peerwire-sync` after),
// and applies to doc.js the frames a Rust session sent (rust.json, which golden.rs writes). golden.rs replays them:
// - session.json: doc.js members in fake rooms in one process, scripted and then seeded at random. Frames go
//   through per-link queues, delivered in rounds (each round every link's queue, links in name order) until none
//   is left; timers fire in time order, member by member. It records the frames delivered on each link, round by
//   round, each op's result, and at checkpoints each member's change and sync events and each doc's snapshot,
//   entries and sync. Rust replays the members marked `rust` and must send the same frames, byte for byte, and hold the same
//   docs. The others are doc.js only (`e` reads deflated frames, so its asks carry z; `f` writes keys and names
//   with lone surrogates): Rust is handed what they sent here, so it applies doc.js's frames, and the frames doc.js
//   applied from the Rust members are the ones Rust must send. A text longer than 96 bytes that Rust only compares
//   is kept as `sha256:<its first 16 bytes in hex>:<its length in bytes>` (PW_FULL_VECTORS=1 keeps it whole, to debug).
// - frames.json: doc frames, valid and mutated, each with what doc.js's parseFrame made of it (null: dropped), as
//   short() keeps it. A long one is given as parts, each a text or [text, times].
// - json.json: JSON texts and bytes, each with JSON.stringify(JSON.parse(new TextDecoder().decode(bytes))), null
//   where that throws.
//
//   node packages/js/test/rust_doc.mjs [--write]
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { Doc, unopened, DEFAULT_KEEP } from '../src/doc.js';

const DIR = new URL('../../../crates/peerwire-sync/tests/vectors/', import.meta.url);
const WRITE = process.argv.includes('--write');
const FULL = process.env.PW_FULL_VECTORS === '1';
const enc = new TextEncoder();
const dec = new TextDecoder();
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
// parseFrame isn't exported: the same source, with it exported (doc.js imports nothing).
const SRC = readFileSync(new URL('../src/doc.js', import.meta.url), 'utf8');
const { parseFrame } = await import(`data:text/javascript;base64,${Buffer.from(`${SRC}\nexport { parseFrame };`).toString('base64')}`);

// mulberry32
function rng(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const R = rng(0x6e62_0006);
const int = (n) => Math.floor(R() * n);
const pick = (xs) => xs[int(xs.length)];

// A text Rust only compares: itself if short, else its digest.
const short = (s) => {
  const b = Buffer.from(s);
  return FULL || b.length <= 96 ? s : `sha256:${createHash('sha256').update(b).digest('hex').slice(0, 32)}:${b.length}`;
};

// A frame of many writers or many ranges, to take a doc to its caps, made from its parameters (golden.rs makes
// the same): `writers`, n writers with an item each; `dead`, their dots dead; `late`, a dot of each they haven't
// written, dead; `ranges`, one writer's odd dots; `unranges`, that writer's dots dead; `big`, one writer's n map
// keys of 1000 characters; `unbig`, those dead.
const floodWriter = (i) => `${i.toString(16).padStart(64, '0')}0000000f`;
function flood(what, n, offset) {
  const ws = Array.from({ length: ['ranges', 'unranges', 'big', 'unbig'].includes(what) ? 1 : n }, (_, i) => floodWriter(offset + i));
  if (what === 'writers') return JSON.stringify({ d: 'board', t: 3, ws, e: ws.map((_, i) => [i, 1, 1, 's', 'fw', null, offset + i]) });
  if (what === 'dead') return JSON.stringify({ d: 'board', t: 3, ws, x: ws.map((_, i) => [i, 1, 1]) });
  if (what === 'late') return JSON.stringify({ d: 'board', t: 3, ws, x: ws.map((_, i) => [i, 5, 5]) });
  if (what === 'unranges') return JSON.stringify({ d: 'board', t: 3, ws, x: [[0, 1, 2 * n]] });
  if (what === 'unbig') return JSON.stringify({ d: 'board', t: 3, ws, x: [[0, 1, n]] });
  if (what === 'big') return JSON.stringify({ d: 'board', t: 3, ws, e: Array.from({ length: n }, (_, k) => [0, k + 1, 1, 'm', 'fb', `k${k}`, 'x'.repeat(1000)]) });
  return JSON.stringify({ d: 'board', t: 3, ws, e: Array.from({ length: n }, (_, k) => [0, 2 * k + 1, 1, 's', 'fr', null, k]) });
}

// The session.

class Net {
  constructor() {
    this.now = 0;
    this.nodes = new Map(); // name -> node
    this.byId = new Map();
    this.links = new Map(); // 'a>b' -> frames queued, as JSON text
    this.held = new Set(); // links whose frames wait
    this.steps = [];
    this.delivered = []; // every frame delivered, whole
    this.snapshots = new Map(); // node -> its last snapshot step's copy
  }

  node(name, { rust = true, deflate = false } = {}) {
    const incs = Array.from({ length: 64 }, () => int(2 ** 32));
    const n = { name, id: name.repeat(64), rust, deflate, incs, used: 0, peers: new Set(), routes: new Map(), docs: new Map(), timers: [], seq: 0, events: [] };
    this.nodes.set(name, n);
    this.byId.set(n.id, n);
    return n;
  }

  host(n, name) {
    return {
      self: n.id,
      peers: () => [...n.peers],
      send: (to, m) => this.queue(n, this.byId.get(to), JSON.stringify(m)),
      after: (ms, fn) => {
        const t = { at: this.now + ms, seq: ++n.seq, fn };
        n.timers.push(t);
        return () => (t.fn = null);
      },
      now: () => this.now,
      closed: (doc) => n.docs.get(name) === doc && n.docs.delete(name),
      incarnation: () => n.incs[n.used++].toString(16).padStart(8, '0'),
      route: (p) => n.routes.get(p) ?? null,
      deflate: n.deflate,
    };
  }

  queue(from, to, text) {
    if (!from.peers.has(to.id)) return;
    const k = `${from.name}>${to.name}`;
    if (!this.links.has(k)) this.links.set(k, []);
    this.links.get(k).push(text);
  }

  // room.js docFrame: a frame for an open doc, an answer to an ask for one not open, and nothing else.
  receive(n, from, bytes) {
    if (!n.peers.has(from.id)) return;
    try {
      const m = JSON.parse(dec.decode(bytes));
      const d = typeof m?.d === 'string' ? n.docs.get(m.d) : undefined;
      if (d) return d.receive(from.id, m);
      const none = unopened(m);
      if (none) this.queue(n, from, JSON.stringify(none));
    } catch {
      // Malformed: dropped.
    }
  }

  settle() {
    const rounds = [];
    for (;;) {
      const round = []; // [from, to, the frames on that link]
      for (const k of [...this.links.keys()].sort()) {
        if (this.held.has(k)) continue;
        const [f, t] = k.split('>');
        const texts = this.links.get(k).splice(0);
        if (texts.length) round.push([f, t, texts]);
      }
      if (!round.length) return rounds;
      if (rounds.length > 2000) throw new Error('the network never settles');
      // A Rust member's frames on a link, which Rust only compares, as one text; a doc.js-only member's, whole.
      rounds.push(round.map(([f, t, texts]) => [f, t, this.nodes.get(f).rust ? short(JSON.stringify(texts)) : texts]));
      for (const [f, t, texts] of round) {
        for (const text of texts) {
          this.delivered.push([f, t, text]);
          this.receive(this.nodes.get(t), this.nodes.get(f), enc.encode(text));
        }
      }
    }
  }

  advance(ms) {
    const target = this.now + ms;
    const ticks = [];
    for (;;) {
      let at = Infinity;
      for (const n of this.nodes.values()) for (const t of n.timers) if (t.fn && t.at < at) at = t.at;
      if (at > target) break;
      this.now = at;
      for (const n of this.nodes.values()) {
        for (;;) {
          let next = null;
          for (const t of n.timers) if (t.fn && t.at <= at && (!next || t.at < next.at || (t.at === next.at && t.seq < next.seq))) next = t;
          if (!next) break;
          const fn = next.fn;
          next.fn = null;
          fn();
        }
        n.timers = n.timers.filter((t) => t.fn);
      }
      ticks.push({ at, rounds: this.settle() });
    }
    this.now = target;
    return ticks;
  }

  open(n, name, copy) {
    const d = new Doc(name, this.host(n, name));
    d.on('change', (ev) => n.events.push(['change', name, JSON.stringify(ev)]));
    d.on('sync', (s) => n.events.push(['sync', name, JSON.stringify(s)]));
    if (copy !== null) {
      let ok = false;
      try {
        ok = d.restore(JSON.parse(copy));
      } catch {
        // not JSON
      }
      if (!ok) return false;
    }
    n.docs.set(name, d);
    d.open();
    return true;
  }

  state() {
    const out = {};
    for (const n of this.nodes.values()) {
      const docs = {};
      for (const [name, d] of n.docs) {
        const colls = [...d.colls.values()].filter((c) => c.name.isWellFormed()).map((c) => [c.t, c.name]).sort();
        docs[name] = {
          snapshot: short(JSON.stringify(d.snapshot())),
          sync: JSON.stringify(d.sync),
          entries: colls.map(([t, c]) => [t, c, short(JSON.stringify(d.list(t, c)))]),
        };
      }
      out[n.name] = { docs, events: short(JSON.stringify(n.events.splice(0))) };
    }
    return out;
  }

  // Runs step `s` and records it with what it gave. Ops take their arguments as `args`, JSON text, so that the
  // vector holds keys and names with lone surrogates as escapes.
  run(s) {
    const n = this.nodes.get(s.node);
    const args = s.args === undefined ? [] : JSON.parse(s.args);
    const d = n?.docs.get(s.doc);
    const call = (fn) => {
      try {
        return fn() ?? null;
      } catch {
        return 'throw';
      }
    };
    switch (s.op) {
      // A copy from a Rust member is named (`copyOf`): Rust takes its own, which the snapshot step checked.
      case 'open':
        s.result = this.open(n, s.doc, s.copyOf ? this.snapshots.get(s.copyOf) : (s.copy ?? null));
        break;
      case 'close':
        d.close();
        break;
      case 'link': {
        const [a, b] = [this.nodes.get(s.a), this.nodes.get(s.b)];
        a.peers.add(b.id);
        b.peers.add(a.id);
        for (const x of [...a.docs.values()]) x.joined(b.id);
        for (const x of [...b.docs.values()]) x.joined(a.id);
        break;
      }
      case 'unlink': {
        const [a, b] = [this.nodes.get(s.a), this.nodes.get(s.b)];
        a.peers.delete(b.id);
        b.peers.delete(a.id);
        this.links.delete(`${a.name}>${b.name}`);
        this.links.delete(`${b.name}>${a.name}`);
        for (const x of [...a.docs.values()]) x.left(b.id);
        for (const x of [...b.docs.values()]) x.left(a.id);
        break;
      }
      case 'route': {
        const [a, b] = [this.nodes.get(s.a), this.nodes.get(s.b)];
        if (!a.peers.has(b.id)) throw new Error('route a linked pair only');
        a.routes.set(b.id, s.kind);
        b.routes.set(a.id, s.kind);
        for (const x of [...a.docs.values()]) x.rerouted();
        for (const x of [...b.docs.values()]) x.rerouted();
        break;
      }
      case 'hold':
        this.held.add(s.link);
        break;
      case 'release':
        this.held.delete(s.link);
        break;
      case 'set':
        s.result = call(() => d.map(args[0]).set(args[1], JSON.parse(args[2])));
        break;
      case 'del':
        s.result = call(() => d.map(args[0]).delete(args[1]));
        break;
      case 'add':
        s.result = call(() => d.set(args[0]).add(JSON.parse(args[1])));
        break;
      case 'rem':
        s.result = call(() => d.set(args[0]).delete(args[1]));
        break;
      case 'append':
        s.result = call(() => d.log(args[0], { keep: d.keeps.get(args[0]) ?? DEFAULT_KEEP }).append(JSON.parse(args[1])));
        break;
      case 'keep':
        s.result = call(() => (d.log(args[0], { keep: args[1] }), true));
        break;
      // n keys k0, k1 ... of map args[0], each set to 'v' args[2] times and its number, or deleted: how many took.
      case 'fill':
        s.result = Array.from({ length: args[1] }, (_, i) => d.map(args[0]).set(`k${i}`, 'v'.repeat(args[2]) + i)).filter(Boolean).length;
        break;
      case 'unfill':
        s.result = Array.from({ length: args[1] }, (_, i) => d.map(args[0]).delete(`k${i}`)).filter(Boolean).length;
        break;
      case 'raw':
        this.receive(this.nodes.get(s.to), this.nodes.get(s.from), Buffer.from(s.hex, 'hex'));
        break;
      case 'flood':
        this.receive(this.nodes.get(s.to), this.nodes.get(s.from), enc.encode(flood(s.what, s.n, s.offset)));
        break;
      case 'snapshot': {
        // The copy goes back to the script, for an open step to hold whole; the vector holds its digest.
        const text = JSON.stringify(d.snapshot());
        s.result = short(text);
        this.snapshots.set(s.node, text);
        this.steps.push(s);
        return { ...s, text };
      }
      case 'settle':
        s.rounds = this.settle();
        break;
      case 'advance':
        s.ticks = this.advance(s.ms);
        break;
      case 'check':
        s.state = this.state();
        break;
      default:
        throw new Error(`no op ${s.op}`);
    }
    this.steps.push(s);
    return s;
  }
}

// doc.js warns once per write a full doc refuses: counted, not printed.
const warned = [];
const warn = console.warn;
console.warn = (m) => warned.push(String(m));
const net = new Net();
const nodes = { a: net.node('a'), b: net.node('b'), c: net.node('c'), d: net.node('d'), e: net.node('e', { rust: false, deflate: true }), f: net.node('f', { rust: false }) };
const step = (s) => net.run(s);
const op = (node, o, ...args) => step({ op: o, node, doc: 'board', args: JSON.stringify(args) });
const link = (a, b) => step({ op: 'link', a, b });
const unlink = (a, b) => step({ op: 'unlink', a, b });
const settle = () => step({ op: 'settle' });
const advance = (ms) => step({ op: 'advance', ms });
const checkpoint = () => step({ op: 'check' });
const writerOf = (n) => nodes[n].docs.get('board').writer;
const raw = (from, to, m) => step({ op: 'raw', from, to, hex: Buffer.from(typeof m === 'string' ? m : JSON.stringify(m)).toString('hex') });

// 1. One member writes alone: every kind of write, odd values, a log trimmed, deletes.
step({ op: 'open', node: 'a', doc: 'board' });
const firstA = writerOf('a');
op('a', 'keep', 'chat', 3);
op('a', 'set', 'meta', 'title', '"Plans"');
op('a', 'set', 'meta', 'n', '[-0, 1E21, 1e-7, 5e-324, 0.000001, 12345678901234567890, 0.1e1]');
op('a', 'set', 'meta', 'o', '{"b":2,"a":1,"10":0,"2":0,"b":3,"__proto__":{"x":1}}');
op('a', 'set', 'meta', 's', '"\\ud83d\\ude00 \\u2028\\u2029 \\u0000\\u001f\\u007f \\"\\\\/"');
op('a', 'set', 'meta', '', 'null');
const s0 = op('a', 'add', 'strokes', '{"pts":[1,2,3]}').result;
op('a', 'add', 'strokes', '{"pts":[4]}');
for (let i = 1; i <= 5; i++) op('a', 'append', 'chat', JSON.stringify({ text: `m${i}` }));
op('a', 'del', 'meta', 'n');
op('a', 'rem', 'strokes', s0);
op('a', 'rem', 'strokes', s0);
op('a', 'set', 'meta', 'x'.repeat(257), '1');
op('a', 'set', '', 'k', '1');
op('a', 'set', 'meta', 'k', '{"a":');
op('a', 'set', 'meta', 'deep', `${'['.repeat(65)}${']'.repeat(65)}`);
op('a', 'keep', 'chat', 0);
// Enough to answer in 6 pieces or more over a relayed route (32K characters each), WINDOW (4) of them unacknowledged.
op('a', 'fill', 'bulk', 200, 900);
checkpoint();

// 2. A member links and opens the doc: an ask for a doc it hasn't open is answered with nothing, then it catches
//    up in pieces; then writes at once on both sides.
link('a', 'b');
settle();
step({ op: 'route', a: 'a', b: 'b', kind: 'peer' });
step({ op: 'open', node: 'b', doc: 'board' });
settle();
op('a', 'unfill', 'bulk', 200);
settle();
op('a', 'set', 'meta', 'title', '"Ours"');
op('b', 'set', 'meta', 'title', '"Mine"');
op('b', 'del', 'meta', 'o');
op('a', 'set', 'meta', 'o', '[1]');
settle();
checkpoint();

// 3. A member that wrote alone joins later, through b; and a second doc b doesn't have.
step({ op: 'open', node: 'c', doc: 'board' });
op('c', 'set', 'meta', 'c', 'true');
op('c', 'append', 'chat', '"from c"');
step({ op: 'open', node: 'c', doc: 'notes' });
link('b', 'c');
settle();
checkpoint();

// 4. doc.js-only members: e reads deflated frames (its asks carry z: answered plain); f writes keys and names with
//    lone surrogates, and line separators.
step({ op: 'open', node: 'e', doc: 'board' });
link('a', 'e');
settle();
op('e', 'set', 'meta', 'from e', '{"z":[1,2]}');
step({ op: 'open', node: 'f', doc: 'board' });
link('a', 'f');
op('f', 'set', 'meta', '\ud800', '"\udc00 lone"');
op('f', 'set', '\udc00x', '\u2028', '1');
op('f', 'set', 'meta', 'k\ud83d', '2');
op('f', 'add', 'strokes', '"\ud800"');
op('f', 'append', 'chat', '"from f"');
settle();
checkpoint();

// 5. A partition: both sides write, then link again and catch up (dead dots go on to the others).
unlink('a', 'b');
op('a', 'del', 'meta', 'title');
op('a', 'set', 'meta', 'p', '"a side"');
op('b', 'set', 'meta', 'p', '"b side"');
op('b', 'set', 'bulk2', 'k1', '"b"');
op('c', 'del', 'meta', 'c');
settle();
link('a', 'b');
settle();
advance(1000);
checkpoint();

// 6. Frames doc.js makes by hand: claims on a's next dots (an answer's entry, an ask's range), a MORE for no answer,
//    a delta of another's writes, then malformed ones; every one judged as doc.js judges it.
const wa = writerOf('a');
raw('e', 'a', { d: 'board', t: 3, ws: [wa], e: [[0, 1000, 5, 'm', 'meta', 'claimed', 1]] });
settle();
raw('e', 'a', { d: 'board', t: 2, ws: [writerOf('a')], r: [[0, 1, 2000]], q: 77, z: 1 });
settle();
raw('e', 'a', { d: 'board', t: 4, q: 1, i: 0 });
raw('e', 'a', { d: 'board', t: 1, ws: [writerOf('b')], e: [[0, 9, 9, 'm', 'meta', 'stolen', 1]] });
raw('e', 'a', { d: 'board', t: 1, ws: [nodes.e.docs.get('board').writer], e: [[0, 500, 500, 's', 'x', null, 1]], x: [[0, 1, 2]], end: 1, s: 2 });
raw('e', 'a', '\ufeff{"d":"board","t":4,"q":1,"i":0}');
raw('e', 'a', '{"d":"board","t":1,"ws":[]');
raw('e', 'a', '{"d":"nope","t":2,"ws":[],"r":[],"q":3}');
raw('e', 'a', '{"d":"nope","t":2,"q":0}');
settle();
// A write of e's that a counts as dead (the delta above named it), sent again: a asks e.
raw('e', 'a', { d: 'board', t: 1, ws: [nodes.e.docs.get('board').writer], e: [[0, 1, 1, 'm', 'meta', 'again', 1]] });
settle();
// Claims on a's next dots, past the incarnations it may move through (16): then they pile up on its last writer,
// as one span, and a writes after them; once every dot is claimed, its writes are refused.
for (let i = 0; i < 18; i++) raw('e', 'a', { d: 'board', t: 3, ws: [writerOf('a')], e: [[0, 100 + 10 * i, 1, 'm', 'meta', 'c', 1]] });
op('a', 'set', 'meta', 'after claims', '1');
settle();
checkpoint();
raw('e', 'a', { d: 'board', t: 3, ws: [writerOf('a')], x: [[0, 1, 2 ** 53 - 1]] });
op('a', 'set', 'meta', 'all claimed', '1');
settle();
checkpoint();

// 7. Catching up under budgets and turns: a member opens the doc again and again (an answer per opening, 8 at once
//    then one per 15 s), and a member asked that doesn't answer loses its turn after 5 s to one waiting.
for (let i = 0; i < 10; i++) {
  step({ op: 'close', node: 'b', doc: 'board' });
  step({ op: 'open', node: 'b', doc: 'board' });
  settle();
}
advance(31000);
step({ op: 'hold', link: 'a>d' });
link('a', 'd');
link('c', 'd');
step({ op: 'open', node: 'd', doc: 'board' });
settle();
advance(6000);
step({ op: 'release', link: 'a>d' });
settle();
advance(70000);
checkpoint();

// 8. The copy: a's, opened again from it (its writer is in it, so it draws another); f's, made by doc.js, opened
//    by a Rust member; and copies that are off.
// a asks its members by route when it opens the doc again: linked directly (f, d), then relayed (b), then over
// signaling (e, queued before b). It draws the incarnation of its first writer, whose entries are in its copy, so
// it draws again (else it would take them for claims on its own dots).
step({ op: 'route', a: 'a', b: 'b', kind: 'peer' });
step({ op: 'route', a: 'a', b: 'e', kind: 'signaling' });
step({ op: 'snapshot', node: 'a', doc: 'board' });
nodes.a.incs[nodes.a.used] = Number.parseInt(firstA.slice(64), 16);
step({ op: 'close', node: 'a', doc: 'board' });
step({ op: 'open', node: 'a', doc: 'board', copyOf: 'a' });
const copyF = step({ op: 'snapshot', node: 'f', doc: 'board' }).text;
step({ op: 'close', node: 'd', doc: 'board' });
step({ op: 'open', node: 'd', doc: 'board', copy: copyF });
step({ op: 'open', node: 'c', doc: 'other', copyOf: 'a' });
step({ op: 'open', node: 'c', doc: 'other', copy: '{"v":2,"c":0,"f":[]}' });
step({ op: 'open', node: 'c', doc: 'other', copy: `{"v":1,"c":0,"f":[{"d":"other","t":3,"ws":["${'b'.repeat(72)}"],"e":[[0,1,1,["m"],"c","k",1]]}]}` });
step({ op: 'open', node: 'c', doc: 'other', copy: '{"v":1,"c":0,"f":[{"d":"other","t":3,"ws":[],"q":1}]}' });
step({ op: 'open', node: 'c', doc: 'other', copy: '{"v":1,"c":-0,"f":[{"d":"other","t":3,"ws":[]}]}' });
settle();
advance(20000);
checkpoint();

// 9. The caps: a doc that is full by length, then by entries, refuses our writes; three answers of a full doc
//    to one member take its chars budget past empty (the third waits for it); a doc that holds MAX_WRITERS
//    writers forgets idle ones for new ones, and one past MAX_RANGES forgets the highest ranges.
step({ op: 'route', a: 'a', b: 'b', kind: 'direct' });
step({ op: 'route', a: 'b', b: 'c', kind: 'signaling' });
op('a', 'fill', 'huge', 4140, 1000);
step({ op: 'flood', from: 'e', to: 'a', what: 'big', n: 5, offset: 9000 });
settle();
// e asks twice for all of it: two full answers take e's chars budget at a; then deletions e's answer names don't
// go on from a to the others (a's own dots: the last three it wrote).
for (const q of [900, 901]) {
  raw('e', 'a', { d: 'board', t: 2, ws: [], r: [], q });
  for (let i = 3; i < 160; i += 4) raw('e', 'a', { d: 'board', t: 4, q, i });
}
settle();
const na = nodes.a.docs.get('board').n;
raw('e', 'a', { d: 'board', t: 3, ws: [writerOf('a')], x: [[0, na - 2, na]] });
settle();
// b opens the doc three times: the third answer of the full doc waits on b's chars budget, a piece every 2 s, so
// it takes TURN_MS and the turn passes on to c, waiting (over signaling, c is asked after a).
for (let i = 0; i < 3; i++) {
  step({ op: 'close', node: 'b', doc: 'board' });
  step({ op: 'open', node: 'b', doc: 'board' });
  settle();
}
advance(70000);
checkpoint();
op('a', 'unfill', 'huge', 4140);
settle();
op('a', 'fill', 'many', 10010, 1);
settle();
checkpoint();
op('a', 'unfill', 'many', 10010);
settle();
// The context's caps: a writer with a few ranges, then one with 8500 (past MAX_RANGES: the highest ranges of the
// writers with the most go); a copy of that opened elsewhere; then 1100 writers (past MAX_WRITERS), their dots
// dead (idle), 1100 more (idle ones go for them), and new dead dots for the idle ones.
step({ op: 'flood', from: 'e', to: 'a', what: 'ranges', n: 3, offset: 4000 });
step({ op: 'flood', from: 'e', to: 'a', what: 'ranges', n: 8500, offset: 5000 });
settle();
checkpoint();
// A writer whose dots are all dead (idle: the first a may forget, its own aside), the context full to the range,
// then a new dead dot of that writer: the context makes room without forgetting it.
{
  const d = nodes.a.docs.get('board');
  step({ op: 'flood', from: 'e', to: 'a', what: 'writers', n: 1, offset: 7000 });
  step({ op: 'flood', from: 'e', to: 'a', what: 'dead', n: 1, offset: 7000 });
  step({ op: 'flood', from: 'e', to: 'a', what: 'ranges', n: 8192 - d.ctx.size, offset: 4500 });
  const idle = floodWriter(7000);
  check(d.ctx.size === 8192 && [...d.idle].filter((w) => w !== d.writer)[0] === idle, 'a holds MAX_RANGES ranges, and one idle writer not its own');
  raw('e', 'a', { d: 'board', t: 3, ws: [idle], x: [[0, 9999, 9999]] });
  check(d.ctx.writers.get(idle)?.length === 2, "a made room and kept the idle writer's ranges");
  settle();
  checkpoint();
}
step({ op: 'snapshot', node: 'a', doc: 'board' });
step({ op: 'close', node: 'd', doc: 'board' });
step({ op: 'open', node: 'd', doc: 'board', copyOf: 'a' });
settle();
checkpoint();
step({ op: 'close', node: 'd', doc: 'board' });
step({ op: 'open', node: 'd', doc: 'board' });
step({ op: 'flood', from: 'e', to: 'a', what: 'writers', n: 1100, offset: 0 });
settle();
checkpoint();
step({ op: 'flood', from: 'e', to: 'a', what: 'dead', n: 1100, offset: 0 });
step({ op: 'flood', from: 'e', to: 'a', what: 'writers', n: 1100, offset: 2000 });
settle();
checkpoint();
step({ op: 'flood', from: 'e', to: 'a', what: 'late', n: 1100, offset: 0 });
settle();
checkpoint();
// What the floods left goes, so the members converge again from here.
step({ op: 'flood', from: 'e', to: 'a', what: 'unranges', n: 3, offset: 4000 });
step({ op: 'flood', from: 'e', to: 'a', what: 'unranges', n: 8192, offset: 4500 });
step({ op: 'flood', from: 'e', to: 'a', what: 'unranges', n: 8500, offset: 5000 });
step({ op: 'flood', from: 'e', to: 'a', what: 'dead', n: 1100, offset: 2000 });
step({ op: 'flood', from: 'e', to: 'a', what: 'unbig', n: 5, offset: 9000 });
settle();
advance(70000);
checkpoint();

// 10. At random: writes, links cut and made, routes, and time.
const RUST = ['a', 'b', 'c', 'd'];
const KEYS = ['k1', 'k2', 'k3', 'ключ', '😀', '', 'a"b', '\u2028'];
const COLLS = ['meta', 'm2'];
const value = () => pick([
  () => String(int(1000)),
  () => pick(['-0', '1E21', '0.10', '1e400', '-1.5e-300', '123456789012345678901234567890', '0e5']),
  () => JSON.stringify(pick(['', 'é', '😀', '\u2028x', 'a\nb', '"q"', String.fromCharCode(int(0x80))])),
  () => `[${int(10)},{"b":1,"a":${int(9)},"7":0}]`,
  () => pick(['true', 'false', 'null', '{}', '[]']),
])();
let items = [];
for (let k = 0; k < 400; k++) {
  const n = pick(RUST);
  const x = R();
  if (x < 0.3) op(n, 'set', pick(COLLS), pick(KEYS), value());
  else if (x < 0.4) op(n, 'del', pick(COLLS), pick(KEYS));
  else if (x < 0.5) {
    const id = op(n, 'add', 'strokes', value()).result;
    if (typeof id === 'string') items.push(id);
  } else if (x < 0.55 && items.length) op(n, 'rem', 'strokes', items.splice(int(items.length), 1)[0]);
  else if (x < 0.62) op(n, 'append', 'chat', value());
  else if (x < 0.75) settle();
  else if (x < 0.8) advance(int(20000));
  else if (x < 0.88) {
    const [a, b] = [pick(RUST), pick(RUST)];
    if (a === b) continue;
    if (nodes[a].peers.has(nodes[b].id)) unlink(a, b);
    else link(a, b);
  } else if (x < 0.92) {
    const [a, b] = [pick(RUST), pick(RUST)];
    if (a !== b && nodes[a].peers.has(nodes[b].id)) step({ op: 'route', a, b, kind: pick(['direct', 'peer', 'signaling']) });
  } else if (x < 0.94) {
    step({ op: 'close', node: n, doc: 'board' });
    step({ op: 'open', node: n, doc: 'board' });
  } else if (x < 0.95) op(n, 'keep', 'chat', 1 + int(5));
  else if (x < 0.97) checkpoint();
}
for (const a of RUST) for (const b of RUST) if (a < b && !nodes[a].peers.has(nodes[b].id)) link(a, b);
settle();
advance(120000);
checkpoint();

const docs = (s) => Object.values(s.state ?? {}).flatMap((n) => Object.values(n.docs));
const last = net.steps.at(-1).state;
// Logs aside: each member keeps its own length of one.
const held = RUST.map((n) => JSON.stringify(last[n].docs.board.entries.filter(([t]) => t !== 'l')));
check(held.every((s) => s === held[0]), 'the Rust members end the session holding the same maps and sets');
const frames = net.delivered;
const kinds = new Set(frames.map(([, , t]) => {
  const m = JSON.parse(t);
  return `${m.t}${m.q !== undefined ? 'q' : ''}${m.end ? 'e' : ''}${m.z ? 'z' : ''}${m.s ? 's' : ''}`;
}));
check(['1', '2q', '2qz', '3q', '3qe', '3qes', '4q'].every((k) => kinds.has(k)), `the session sends every kind of frame (${[...kinds].sort()})`);
check(docs(net.steps.at(-1)).length >= 8, 'every member holds the doc at the end');
const session = { nodes: Object.fromEntries(Object.values(nodes).map((n) => [n.name, { id: n.id, rust: n.rust, incarnations: n.incs }])), steps: net.steps };

console.warn = warn;
check(warned.some((m) => m.includes('is full')) && warned.some((m) => m.includes('claimed every write')) && warned.every((m) => /is full|claimed every write/.test(m)),
  `doc.js refused ${warned.length} writes to a full doc or one with every dot claimed`);

// The frame corpus: frames the session sent, the shortest of each kind, and those mutated, each judged by
// parseFrame. A frame is given as parts, each a text or [text, times], so that long ones stay short here.
const norm = (f) => f && JSON.stringify([f.t, f.q ?? null, f.i ?? null, f.n ?? null, f.s ?? null, f.z ?? null, f.end ?? null,
  f.ranges ?? [], (f.entries ?? []).map((e) => [e.w, e.n, e.c, e.t, e.coll, e.k, JSON.stringify(e.v), e.len])]);
const judge = (bytes, peer) => {
  let m;
  try {
    m = JSON.parse(dec.decode(bytes));
  } catch {
    return null;
  }
  const f = norm(parseFrame(m, peer));
  return f && short(f);
};
const corpus = [];
// One whose entry type isn't a string: doc.js drops it once NB-051 is in (until then it may keep it), Rust always.
const typeConfused = (m) => Array.isArray(m?.e) && m.e.some((x) => Array.isArray(x) && x.length === 7 && typeof x[3] !== 'string');
let confused = 0;
const textOf = (parts) => parts.map((p) => (typeof p === 'string' ? p : p[0].repeat(p[1]))).join('');
const add = (parts, peer) => {
  if (typeof parts === 'string') parts = [parts];
  const text = textOf(parts);
  let m = null;
  try {
    m = JSON.parse(text);
  } catch {
    // judged as text
  }
  const verdict = judge(Buffer.from(text), peer);
  if (typeConfused(m)) confused += verdict === null ? 0 : 1;
  corpus.push({ parts, peer, verdict: typeConfused(m) ? null : verdict });
};
const byKind = new Map();
for (const [f, , text] of frames) {
  const m = JSON.parse(text);
  const k = `${m.t}${m.e ? 'e' : ''}${m.x ? 'x' : ''}${m.q !== undefined ? 'q' : ''}${m.end ? 'n' : ''}${m.z ? 'z' : ''}`;
  if (text.length < 700 && !(byKind.get(k)?.[1].length <= text.length)) byKind.set(k, [nodes[f].id, text]);
}
const WEIRD = [null, -1, 0, 1, 1.5, '1', [1], {}, 2 ** 53, 2 ** 53 - 1, true, 'm', ['m'], 'x'.repeat(129), 'x'.repeat(257), '\ud800'];
const FIELDS = ['d', 't', 'ws', 'q', 'i', 'n', 'e', 'x', 'r', 'end', 's', 'z'];
for (const [peer, text] of byKind.values()) {
  add(text, peer);
  add(text, 'f'.repeat(64));
  const m = JSON.parse(text);
  for (const k of FIELDS) {
    const c = structuredClone(m);
    delete c[k];
    add(JSON.stringify(c), peer);
    for (let j = 0; j < 2; j++) add(JSON.stringify({ ...m, [k]: pick(WEIRD) }), peer);
  }
  for (let j = 0; j < 8; j++) {
    const c = structuredClone(m);
    if (c.e?.length) {
      const x = pick(c.e);
      x[int(7)] = pick(WEIRD);
      if (R() < 0.2) x.push(0);
    } else if ((c.x ?? c.r)?.length) {
      const r = pick(c.x ?? c.r);
      r[int(3)] = pick(WEIRD);
    } else if (c.ws?.length) {
      c.ws[int(c.ws.length)] = pick([c.ws[0].toUpperCase(), c.ws[0].slice(1), 1, c.ws[0] + '0']);
    }
    add(JSON.stringify(c), peer);
  }
  // As text: a key given twice (the last wins, at the first's place), __proto__, numbers as V8 reads them, -0, a
  // BOM, whitespace, trailing bytes; and bytes flipped, invalid UTF-8 among them.
  add(text.replace('{', '{"t":9,'), peer);
  add(text.replace('{', '{"__proto__":{"t":4},'), peer);
  add(text.replace(/"t":(\d)/, '"t":$1.0e0'), peer);
  add(text.replace(/\]\]/, ',-0]]'), peer);
  add(`\ufeff${text}`, peer);
  add(` ${text}\n`, peer);
  add(`${text}x`, peer);
  for (let j = 0; j < 4; j++) {
    const b = Buffer.from(text);
    for (let k = 0; k < 1 + int(3); k++) b[int(b.length)] = pick([0x22, 0x5c, 0x7b, 0x5d, 0x2c, 0x30, 0x80, 0xff, 0xed, int(256)]);
    corpus.push({ hex: b.toString('hex'), peer, verdict: judge(b, peer) });
  }
}
check(confused === 0, 'doc.js drops every frame whose entry type isn\'t a string (NB-051), as Rust does');
// Values, keys and names at the edges of the caps; an ask with more ranges than a context holds.
const w0 = 'a'.repeat(72);
const entry = (v, k = ['"k"'], coll = ['"c"']) => ['{"d":"b","t":1,"ws":["', w0, '"],"e":[[0,1,1,"m",', ...coll, ',', ...k, ',', ...v, ']]}'];
const q = (s) => JSON.stringify(s);
for (const v of [[['[', 64], [']', 64]], [['[', 65], [']', 65]], ['"', ['x', 65534], '"'], ['"', ['x', 65535], '"'], ['"', ['\\u0001', 10922], '"'],
  ['"', ['\\u0001', 10923], '"'], ['1e400'], ['-0'], ['[1e21,5e-324]'], ['{"a":1,"a":2}']]) add(entry(v), 'a'.repeat(64));
for (const k of [['"', ['x', 256], '"'], ['"', ['x', 257], '"'], ['"', ['\\ud800', 256], '"'], ['""'], [q('__proto__')]]) add(entry(['1'], k), 'a'.repeat(64));
for (const c of [['"', ['x', 128], '"'], ['"', ['x', 129], '"'], [q('\udfff')], ['""']]) add(entry(['1'], ['"k"'], c), 'a'.repeat(64));
// DOC-6 (NB-051): an entry typed ["m"], all else valid, drops its frame (doc.js once took it for "m"), in a delta
// and in an answer piece; the session opens a copy holding one (refused).
const DOC6 = ['["m"]', '["l"]', '{"m":1}', '1', 'null', '"M"', '""'];
for (const ty of DOC6) add([`{"d":"b","t":1,"ws":["${w0}"],"e":[[0,1,1,${ty},"c","k",1]]}`], 'a'.repeat(64));
for (const ty of DOC6) add([`{"d":"b","t":3,"ws":["${w0}"],"e":[[0,1,1,${ty},"c","k",1]],"q":5,"i":0,"n":1}`], 'f'.repeat(64));
add([`{"d":"b","t":3,"ws":["${w0}"],"e":[[0,1,1,"m","c","k",1]],"q":5,"i":0,"n":1}`], 'f'.repeat(64));
check(corpus.slice(-2 * DOC6.length - 1, -1).every((c) => c.verdict === null) && corpus.at(-1).verdict !== null,
  'an entry whose type is ["m"], or any other than "m", "s" or "l", drops its frame; "m" keeps it');
add([`{"d":"b","t":2,"ws":["${w0}"],"r":[`, ['[0,1,1],', 8192], '[0,1,1]],"q":1}'], w0.slice(0, 64));
add([`{"d":"b","t":2,"ws":["${w0}"],"r":[`, ['[0,1,1],', 8191], '[0,1,1]],"q":1}'], w0.slice(0, 64));
// A deep unknown field is ignored (the frame is kept); a deep value drops it.
const DEEP = 100000;
add(['{"d":"board","t":1,"ws":[],"u":', ['[', DEEP], '0', [']', DEEP], '}'], 'a'.repeat(64));
add(['{"d":"board","t":1,"ws":["', w0, '"],"e":[[0,1,1,"s","c",null,', ['[', DEEP], ']', [']', DEEP - 1], ']]}'], 'a'.repeat(64));
check(corpus.at(-2).verdict !== null && corpus.at(-1).verdict === null, `doc.js keeps a frame with an unknown field ${DEEP} deep, and drops one with a value that deep`);
const frameVectors = corpus;

// The JSON corpus: texts, and what JSON.stringify(JSON.parse(text)) makes of them.
const texts = [
  '{"b":1,"4294967294":2,"4294967295":3,"1":4,"01":5,"0":6,"-1":7,"1.5":8,"a":9,"b":10}', '{"__proto__":1,"x":2}',
  '{"a":1,"a":{"b":2},"a":3}', '[-0, 1e21, 1e-7, 5e-324, 0.000001, 1e400, -1e400]', '1e21', '1e-7', '5e-324', '0.000001',
  '"\\ud800"', '{"\\ud800":"\\udc00"}', '"\\u2028\\u2029"', '"\u2028\u2029"', '"\\ud83d\\ude00"', '"\\uD83D\\uDE00"',
  '123456789012345678901234567890', '0.1', '1e-6', '1e20', '1.7976931348623157e308', '2.2250738585072014e-308',
  '4.9406564584124654e-324', '9007199254740993', '0.30000000000000004', '100', '1E+2', '-0.0', '0e-10', '1e0001',
];
for (let i = 0; i < 600; i++) {
  const b = new DataView(new ArrayBuffer(8));
  b.setUint32(0, int(2 ** 32));
  b.setUint32(4, int(2 ** 32));
  const x = b.getFloat64(0);
  if (!Number.isFinite(x)) continue;
  texts.push(pick([String(x), x.toExponential(), x.toExponential(int(17)), x.toPrecision(1 + int(21))]));
}
for (let i = 0; i < 200; i++) texts.push(`${pick(['', '-'])}${int(10 ** (1 + int(15)))}${pick(['', `.${int(1000)}`])}${pick(['', `e${pick(['', '+', '-'])}${int(400)}`, `E${int(30)}`])}`);
const units = [0x22, 0x5c, 0x2f, 0x08, 0x0c, 0x0a, 0x0d, 0x09, 0x00, 0x1f, 0x20, 0x7f, 0x80, 0xe9, 0x2028, 0x2029, 0xd800, 0xdbff, 0xdc00, 0xdfff, 0xfeff, 0xfffd, 0xffff];
for (let i = 0; i < 300; i++) {
  const s = Array.from({ length: int(6) }, () => String.fromCharCode(pick(units))).join('');
  texts.push(JSON.stringify(s));
  texts.push(`"${[...s].map((c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`).join('')}"`);
}
const keyish = () => pick(['0', '1', '10', '01', '4294967294', '4294967295', 'a', 'b', '__proto__', '-1', '1.0', '\\ud800', 'é']);
const randomJson = (depth) => {
  const x = R();
  if (depth > 3 || x < 0.4) return pick(['0', '-0', '1.5', '"s"', 'null', 'true', '1e400', '[]', '{}']);
  if (x < 0.7) return `[${Array.from({ length: int(4) }, () => randomJson(depth + 1)).join(',')}]`;
  return `{${Array.from({ length: int(5) }, () => `"${keyish()}":${randomJson(depth + 1)}`).join(',')}}`;
};
for (let i = 0; i < 300; i++) texts.push(randomJson(0));
for (const bad of ['', ' ', '{', '[1,]', '[,1]', '{"a"}', '{"a":1,}', '{a:1}', '01', '1.', '.5', '+1', '-', '1e', '1e+', '"\\x"',
  '"\\u12g4"', '"a\nb"', 'tru', 'nulll', '[1] 2', "'a'", 'NaN', 'Infinity', '[1 2]', '{"a" 1}', '\u00a01', '\ufeff1', '[\ufeff1]', ' \ufeff1', '"\t"', '[1,2']) texts.push(bad);
// As a doc frame's bytes are read: TextDecoder (a leading BOM dropped), then JSON.parse.
const jsonCorpus = texts.map((t) => {
  let out = null;
  try {
    out = JSON.stringify(JSON.parse(dec.decode(Buffer.from(t))));
  } catch {
    // null
  }
  return { text: t, out };
});
for (let i = 0; i < 200; i++) {
  const b = Buffer.from(pick(texts.filter((t) => t.startsWith('"') || t.startsWith('{'))));
  const bytes = Buffer.concat([pick([Buffer.alloc(0), Buffer.from([0xef, 0xbb, 0xbf])]), b.subarray(0, 1), Buffer.from(Array.from({ length: 1 + int(4) }, () => pick([0x80, 0xbf, 0xc0, 0xc2, 0xe2, 0xed, 0xa0, 0xf0, 0xf4, 0x90, 0xff, 0x41]))), b.subarray(1)]);
  let out = null;
  try {
    out = JSON.stringify(JSON.parse(dec.decode(bytes)));
  } catch {
    // null
  }
  jsonCorpus.push({ hex: bytes.toString('hex'), out });
}

// Written, or checked against what is committed.
const files = { 'session.json': session, 'frames.json': frameVectors, 'json.json': jsonCorpus };
for (const [name, v] of Object.entries(files)) {
  const text = `${JSON.stringify(v)}\n`;
  const url = new URL(name, DIR);
  if (WRITE) {
    writeFileSync(url, text);
    console.log(`wrote ${name}: ${text.length} bytes`);
  } else {
    check(existsSync(url) && readFileSync(url, 'utf8') === text, `${name} is what doc.js makes now (else: node packages/js/test/rust_doc.mjs --write)`);
  }
}

// What a Rust session sent, applied to doc.js: a member that only listens ends holding what Rust's held.
const rustUrl = new URL('rust.json', DIR);
if (existsSync(rustUrl)) {
  const v = JSON.parse(readFileSync(rustUrl, 'utf8'));
  const incs = [...v.observer.incarnations];
  const peers = [...new Set(v.frames.map(([p]) => p))];
  const d = new Doc('board', {
    self: v.observer.id,
    peers: () => peers,
    send: () => {},
    after: () => () => {},
    now: () => 0,
    incarnation: () => incs.shift().toString(16).padStart(8, '0'),
  });
  d.open();
  for (const [p, text] of v.frames) d.receive(p, JSON.parse(text));
  check(JSON.stringify(d.snapshot()) === v.snapshot, `doc.js applies the ${v.frames.length} frames a Rust session sent and holds what Rust held`);
  check(v.entries.every(([t, c, json]) => JSON.stringify(d.list(t, c)) === json), "doc.js lists the Rust session's entries as Rust does");
} else {
  check(WRITE, 'rust.json is there (cargo test -p peerwire-sync writes it)');
}
console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
