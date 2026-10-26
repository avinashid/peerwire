// Shared documents (src/doc.js) on fake rooms in one process, seeded. Members write at random (map sets and
// deletes, set adds and deletes, log appends), and what they send each other goes through a network that can
// deliver frames in any order, twice, or not at all, cut a link (its frames in flight are lost, as with a
// session) and link again (a join, so a catch-up). Checks that every member ends up holding the same doc:
// after random order and duplicates with no catch-up, after drops healed by joining again, after a partition
// heals, and for a late joiner, which gets each entry about once. Then intent (a later write wins, a set
// beats a delete it didn't see, a log keeps causal order), the checks on member data (malformed frames,
// caps, a frame's size, answers per member, stamps far ahead, claims on our future dots) and 'change'.
//
//   node packages/js/test/doc_crdt.mjs
import { Doc, MAX_VALUE_LENGTH, MAX_VALUE_DEPTH, MAX_ENTRIES, MAX_DOC_LENGTH, MAX_WRITERS, MAX_RANGES, MAX_KEY_LENGTH, MAX_NAME_LENGTH, MAX_CLOCK_STEP, MESSAGE_BYTES } from '../src/doc.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
const id = (c) => c.repeat(64);
const KEEP = 6;

// mulberry32
function rng(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const int = (r, n) => Math.floor(r() * n);

// Members of one room. `order`: 'fifo' delivers each link's frames in order (the reliable lane), 'random'
// any frame in flight; `dup` and `drop`: the chance a delivered frame comes again, or is lost.
class Net {
  constructor(seed, { order = 'fifo', dup = 0, drop = 0 } = {}) {
    this.r = rng(seed);
    this.order = order;
    this.dup = dup;
    this.drop = drop;
    this.nodes = new Map();
    this.flight = []; // [from, to, text, epoch]
    this.epochs = new Map(); // 'a|b' -> epoch of the link, odd while up
    this.now = 0;
    this.timers = [];
    this.got = new Map(); // peer -> entries it was sent in answers
    this.largest = 0;
    this.lost = 0;
  }

  key(a, b) {
    return a < b ? `${a}|${b}` : `${b}|${a}`;
  }

  up(a, b) {
    return (this.epochs.get(this.key(a, b)) ?? 0) % 2 === 1;
  }

  node(c, { keep = KEEP, open = true } = {}) {
    const self = id(c);
    const n = { self, peers: new Set(), doc: null, keep };
    n.host = {
      self,
      peers: () => [...n.peers],
      send: (to, m) => {
        const text = JSON.stringify(m);
        const bytes = Buffer.byteLength(text) + 1;
        if (bytes > this.largest) this.largest = bytes;
        if (m.t === 3) this.got.set(to, (this.got.get(to) ?? 0) + (m.e?.length ?? 0));
        this.flight.push([self, to, text, this.epochs.get(this.key(self, to))]);
      },
      after: (ms, fn) => {
        const t = { at: this.now + ms, fn };
        this.timers.push(t);
        return () => (t.fn = null);
      },
      incarnation: () => int(this.r, 2 ** 32).toString(16).padStart(8, '0'),
    };
    if (open) this.open(n);
    this.nodes.set(self, n);
    return n;
  }

  open(n) {
    n.doc = new Doc('board', n.host);
    n.doc.log('l', { keep: n.keep });
    n.doc.open();
  }

  link(a, b) {
    if (this.up(a.self, b.self)) return;
    const k = this.key(a.self, b.self);
    this.epochs.set(k, (this.epochs.get(k) ?? 0) + 1);
    a.peers.add(b.self);
    b.peers.add(a.self);
    a.doc?.joined(b.self);
    b.doc?.joined(a.self);
  }

  // The session ends: what was in flight on it is lost.
  unlink(a, b) {
    if (!this.up(a.self, b.self)) return;
    const k = this.key(a.self, b.self);
    this.epochs.set(k, this.epochs.get(k) + 1);
    a.peers.delete(b.self);
    b.peers.delete(a.self);
    a.doc?.left(b.self);
    b.doc?.left(a.self);
  }

  mesh(nodes) {
    for (const a of nodes) for (const b of nodes) if (a.self < b.self) this.link(a, b);
  }

  // Delivers one frame, if any is in flight.
  step() {
    if (!this.flight.length) return false;
    let i = 0;
    if (this.order === 'random') i = int(this.r, this.flight.length);
    else {
      // A random link's oldest frame.
      const j = int(this.r, this.flight.length);
      const [f, t] = this.flight[j];
      i = this.flight.findIndex(([f2, t2]) => f2 === f && t2 === t);
    }
    const [from, to, text, epoch] = this.flight[i];
    const again = this.r() < this.dup;
    if (!again) this.flight.splice(i, 1);
    if (this.epochs.get(this.key(from, to)) !== epoch) return true;
    if (this.r() < this.drop) {
      this.lost++;
      return true;
    }
    this.nodes.get(to).doc?.receive(from, JSON.parse(text));
    return true;
  }

  // Delivers everything and fires timers that come due, until nothing is left. Bounded.
  run(limit = 1e6) {
    for (let n = 0; n < limit; n++) {
      if (this.step()) continue;
      const live = this.timers.filter((t) => t.fn);
      if (!live.length) return;
      const t = live.reduce((a, b) => (b.at < a.at ? b : a));
      this.now = Math.max(this.now, t.at);
      const fn = t.fn;
      t.fn = null;
      fn();
    }
    fail('the network never went quiet');
  }

  quiet() {
    this.dup = 0;
    this.drop = 0;
  }
}

// A member's whole doc: every map key with the dots it holds (not only the value shown), every set item and
// log entry in order.
function state(doc) {
  const out = [];
  for (const [k, coll] of [...doc.colls].sort(([a], [b]) => (a < b ? -1 : 1))) {
    if (coll.t === 'm') {
      const keys = [...coll.keys].sort(([a], [b]) => (a < b ? -1 : 1));
      out.push([k, keys.map(([key, list]) => [key, list.map((e) => e.id).sort(), Doc.shown(list).v])]);
    } else {
      out.push([k, doc.list(coll.t, coll.name).map((e) => [e.id, e.value])]);
    }
  }
  return JSON.stringify(out);
}

function same(nodes, what) {
  const s = nodes.map((n) => state(n.doc));
  for (let i = 1; i < s.length; i++) {
    if (s[i] !== s[0]) fail(`${what}: members differ\n  ${nodes[0].self.slice(0, 4)} ${s[0].slice(0, 600)}\n  ${nodes[i].self.slice(0, 4)} ${s[i].slice(0, 600)}`);
  }
  return s[0];
}

function randomWrite(r, n) {
  const d = n.doc;
  const x = r();
  if (x < 0.3) d.map('m').set(`k${int(r, 5)}`, int(r, 100));
  else if (x < 0.4) d.map('m').delete(`k${int(r, 5)}`);
  else if (x < 0.6) d.set('s').add(int(r, 1000));
  else if (x < 0.75) {
    const items = d.set('s').entries();
    if (items.length) d.set('s').delete(items[int(r, items.length)].id);
  } else d.log('l', { keep: n.keep }).append(int(r, 1000));
}

// Writes `ops` times at random members, delivering a few frames between writes.
function churn(net, nodes, ops) {
  for (let i = 0; i < ops; i++) {
    randomWrite(net.r, nodes[int(net.r, nodes.length)]);
    for (let k = int(net.r, 4); k > 0; k--) net.step();
  }
}

const SEEDS = 25;
let t0 = performance.now();

// Random order and duplicates: every frame is a piece of state joined into the doc, so delivering them all
// in any order, some twice, converges with no catch-up at all.
for (let seed = 1; seed <= SEEDS; seed++) {
  const net = new Net(seed, { order: 'random', dup: 0.3 });
  const nodes = ['a', 'b', 'c'].map((c) => net.node(c));
  net.mesh(nodes);
  churn(net, nodes, 150);
  net.run();
  const s = same(nodes, `random order and duplicates, seed ${seed}`);
  if (s.length < 20) fail(`seed ${seed}: an empty doc proves nothing`);
}
console.log(`random order and duplicates: ${SEEDS} seeds converged with no catch-up (${(performance.now() - t0).toFixed(0)} ms)`);

// Drops: frames lost without the link ending. Members converge once they join again (a catch-up each way).
t0 = performance.now();
let lost = 0;
for (let seed = 1; seed <= SEEDS; seed++) {
  const net = new Net(seed, { order: 'random', dup: 0.1, drop: 0.25 });
  const nodes = ['a', 'b', 'c', 'd'].map((c) => net.node(c));
  net.mesh(nodes);
  churn(net, nodes, 150);
  net.run();
  net.quiet();
  lost += net.lost;
  for (const a of nodes) for (const b of nodes) if (a.self < b.self) net.unlink(a, b);
  net.mesh(nodes);
  net.run();
  same(nodes, `drops then joining again, seed ${seed}`);
}
console.log(`drops: ${lost} frames lost over ${SEEDS} seeds, healed by joining again (${(performance.now() - t0).toFixed(0)} ms)`);

// A partition: {a, b} and {c, d} write apart, links cut mid-flight, then heal. Both sides' writes survive
// unless deleted, and an item one side deleted stays deleted after the heal.
t0 = performance.now();
for (let seed = 1; seed <= SEEDS; seed++) {
  const net = new Net(seed, { order: 'fifo' });
  const nodes = ['a', 'b', 'c', 'd'].map((c) => net.node(c));
  const [a, b, c, d] = nodes;
  net.mesh(nodes);
  churn(net, nodes, 60);
  net.run();
  const shared = a.doc.set('s').add('shared');
  net.run();
  if (!d.doc.set('s').has(shared)) fail(`seed ${seed}: an item didn't reach every member`);
  churn(net, nodes, 20); // some still in flight when the links go
  for (const x of [a, b]) for (const y of [c, d]) net.unlink(x, y);
  churn(net, [a, b], 40);
  churn(net, [c, d], 40);
  const left = a.doc.set('s').add('left side');
  const right = d.doc.set('s').add('right side');
  c.doc.set('s').delete(shared);
  b.doc.map('m').set('side', 'left');
  net.run();
  if (a.doc.set('s').has(right) || d.doc.set('s').has(left)) fail(`seed ${seed}: a write crossed the partition`);
  net.mesh(nodes);
  net.run();
  same(nodes, `a partition healed, seed ${seed}`);
  if (!c.doc.set('s').has(left) || !a.doc.set('s').has(right)) fail(`seed ${seed}: a side's write was lost in the heal`);
  if (a.doc.set('s').has(shared)) fail(`seed ${seed}: an item deleted in a partition came back in the heal`);
  if (d.doc.map('m').get('side') !== 'left') fail(`seed ${seed}: a key set in a partition was lost in the heal`);
}
console.log(`partition: ${SEEDS} seeds healed; both sides' writes kept, a deletion kept (${(performance.now() - t0).toFixed(0)} ms)`);

// A late joiner: it asks one member at a time with its context, so it gets each entry about once, not once
// per member; and it holds what everyone holds.
t0 = performance.now();
let worst = 0;
for (let seed = 1; seed <= SEEDS; seed++) {
  const net = new Net(seed, { order: 'fifo' });
  const nodes = ['a', 'b', 'c', 'd'].map((c) => net.node(c, { keep: 50 }));
  net.mesh(nodes);
  churn(net, nodes, 200);
  net.run();
  const late = net.node('e', { keep: 50 });
  for (const n of nodes) net.link(late, n);
  net.run();
  same([...nodes, late], `a late joiner, seed ${seed}`);
  const held = late.doc.entries.size;
  const ratio = (net.got.get(late.self) ?? 0) / held;
  worst = Math.max(worst, ratio);
  if (ratio > 1.2) fail(`seed ${seed}: the joiner was sent ${net.got.get(late.self)} entries for ${held} (${ratio.toFixed(2)}x)`);
}
console.log(`late joiner: ${SEEDS} seeds caught up, sent at most ${worst.toFixed(2)}x the entries it holds (${(performance.now() - t0).toFixed(0)} ms)`);

// A member that opens the doc after the others wrote: it asks every member when it opens, and frames for a
// doc it hasn't opened were dropped meanwhile.
{
  const net = new Net(7, { order: 'fifo' });
  const [a, b] = ['a', 'b'].map((c) => net.node(c));
  const c = net.node('c', { open: false });
  net.mesh([a, b, c]);
  churn(net, [a, b], 50);
  net.run();
  net.open(c);
  net.run();
  same([a, b, c], 'a doc opened late');
}

// Intent.
{
  const net = new Net(1);
  const [a, b, c] = ['a', 'b', 'c'].map((x) => net.node(x));
  net.mesh([a, b, c]);
  // A write made after seeing another wins, whatever the writers' ids or stamps elsewhere.
  for (let i = 0; i < 20; i++) c.doc.log('l', { keep: KEEP }).append(i); // c's clock runs ahead
  net.run();
  a.doc.map('m').set('x', 'first');
  net.run();
  b.doc.map('m').set('x', 'second');
  net.run();
  if ([a, b, c].some((n) => n.doc.map('m').get('x') !== 'second')) fail('a later set lost to an earlier one');
  // A set that a delete hadn't seen wins over it (add-wins); of two sets at once, one shows everywhere.
  a.doc.map('m').delete('x');
  c.doc.map('m').set('x', 'concurrent');
  net.run();
  if ([a, b, c].some((n) => n.doc.map('m').get('x') !== 'concurrent')) fail('a delete removed a set it had not seen');
  a.doc.map('m').set('y', 'from a');
  b.doc.map('m').set('y', 'from b');
  net.run();
  const y = a.doc.map('m').get('y');
  if (![a, b, c].every((n) => n.doc.map('m').get('y') === y)) fail('two sets at once showed differently');
  // A log entry appended after seeing another sorts after it, even by a writer whose clock was behind.
  const before = c.doc.log('l', { keep: KEEP }).append('question');
  net.run();
  const reply = a.doc.log('l', { keep: KEEP }).append('reply');
  net.run();
  const ids = b.doc.log('l', { keep: KEEP }).entries().map((e) => e.id);
  if (!(ids.indexOf(before) < ids.indexOf(reply))) fail(`a reply sorts before the message it saw: ${ids.indexOf(before)}, ${ids.indexOf(reply)}`);
  // A log keeps its newest `keep`, the same on every member.
  if (ids.length !== KEEP || ids.at(-1) !== reply) fail(`log of ${ids.length}, last ${ids.at(-1)}`);
  same([a, b, c], 'intent');
}
console.log('intent: a later set wins, a set beats a delete it did not see, a reply sorts after what it saw');

// A log keeps its own length: a member keeping fewer entries tells the others they are dead, which trims no
// one else's log.
{
  const net = new Net(13);
  const a = net.node('a', { keep: 3 });
  const b = net.node('b', { keep: 10 });
  for (let i = 0; i < 10; i++) b.doc.log('l', { keep: 10 }).append(i);
  net.link(a, b);
  net.run();
  // Once more, so b asks a after a has trimmed: a's answer tells b of 7 dead entries.
  net.unlink(a, b);
  net.link(a, b);
  net.run();
  if (a.doc.log('l', { keep: 3 }).entries().length !== 3 || b.doc.log('l', { keep: 10 }).entries().length !== 10) fail('a shorter log trimmed a longer one');
}

// 'change': one event per change, with its fields; a set over a key is never seen as a delete on the way.
{
  const net = new Net(2);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  const local = [];
  const remote = [];
  a.doc.on('change', (e) => local.push(e));
  const off = b.doc.on('change', (e) => remote.push(e));
  a.doc.map('m').set('k', { v: 1 });
  a.doc.map('m').set('k', { v: 2 });
  const sid = a.doc.set('s').add([1, 2]);
  a.doc.set('s').delete(sid);
  a.doc.map('m').delete('k');
  net.run();
  const fmt = (e) => `${e.name}/${e.type}/${e.key ?? (e.id === sid ? 'sid' : e.id)}/${JSON.stringify(e.value) ?? ''}/${e.deleted ? 'deleted' : ''}/${e.from === a.self ? 'a' : e.from}/${e.local}`;
  const want = ['m/map/k/{"v":1}//a/', 'm/map/k/{"v":2}//a/', 's/set/sid/[1,2]//a/', 's/set/sid//deleted/a/', 'm/map/k//deleted/a/'];
  if (local.map(fmt).join(' ') !== want.map((w) => w + 'true').join(' ')) fail(`local events ${local.map(fmt).join(' ')}`);
  if (remote.map(fmt).join(' ') !== want.map((w) => w + 'false').join(' ')) fail(`remote events ${remote.map(fmt).join(' ')}`);
  // Values are frozen, entries too.
  a.doc.map('m').set('o', { deep: { x: [1] } });
  net.run();
  const o = b.doc.map('m').get('o');
  if (!Object.isFrozen(o.deep.x) || !Object.isFrozen(b.doc.map('m').entries()[0])) fail('values are not frozen');
  off();
  a.doc.map('m').set('k', 3);
  net.run();
  if (remote.length !== 6) fail(`off() left the handler on: ${remote.length}`);
  // A handler that throws doesn't stop the others or the doc.
  const seen = [];
  const thrown = [];
  const caught = (e) => thrown.push(e.message);
  process.on('uncaughtException', caught);
  b.doc.on('change', () => {
    throw new Error('app bug');
  });
  b.doc.on('change', (e) => seen.push(e.key));
  a.doc.map('m').set('k', 4);
  net.run();
  await new Promise((r) => setTimeout(r, 10));
  process.off('uncaughtException', caught);
  if (seen.join() !== 'k' || b.doc.map('m').get('k') !== 4 || thrown.join() !== 'app bug') fail(`throwing handler: ${seen} ${thrown}`);
}
console.log("'change': one event per change, local and remote alike; frozen values; a throwing handler stops nothing");

// Our own mistakes throw; member data never makes our writes throw.
{
  const net = new Net(3);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  const throws = (f, type, what) => {
    try {
      f();
    } catch (e) {
      if (e instanceof type) return;
      fail(`${what}: threw ${e}`);
    }
    fail(`${what}: did not throw`);
  };
  const big = 'x'.repeat(MAX_VALUE_LENGTH - 1); // MAX_VALUE_LENGTH + 1 as JSON
  throws(() => a.doc.map('m').set('k', big), RangeError, 'a value over the limit');
  throws(() => a.doc.map('m').set('k', undefined), TypeError, 'undefined');
  throws(() => a.doc.set('s').add(() => {}), TypeError, 'a function');
  throws(() => a.doc.map('m').set('k'.repeat(MAX_KEY_LENGTH + 1), 1), RangeError, 'a key over the limit');
  throws(() => a.doc.map('m'.repeat(MAX_NAME_LENGTH + 1)), RangeError, 'a collection name over the limit');
  throws(() => new Doc('', a.host), RangeError, 'an empty doc name');
  throws(() => a.doc.log('l', { keep: 0 }), RangeError, 'keep 0');
  let deep = 1;
  for (let i = 0; i <= MAX_VALUE_DEPTH; i++) deep = [deep]; // arrays nested MAX_VALUE_DEPTH + 1 deep
  throws(() => a.doc.set('s').add(deep), RangeError, 'a value nested too deep');
  if (!a.doc.set('s').add(deep[0])) fail('a value nested to the limit');
  a.doc.map('m').set('k', 'x'.repeat(MAX_VALUE_LENGTH - 2));
  net.run();
  if (b.doc.map('m').get('k')?.length !== MAX_VALUE_LENGTH - 2) fail('a value at the limit');

  // A full doc refuses writes with a warning, from us and from members, and frees room as entries go.
  const warn = console.warn;
  let warned = 0;
  console.warn = () => warned++;
  const s = a.doc.set('fill');
  for (let i = a.doc.entries.size; i < MAX_ENTRIES; i++) s.add(i);
  if (s.add('one more') !== null || a.doc.map('m').set('new', 1) !== false || !warned) fail('a full doc took a write');
  net.run();
  if (b.doc.entries.size !== MAX_ENTRIES) fail(`member holds ${b.doc.entries.size}`);
  b.doc.entries.size;
  const before = state(a.doc);
  // A member writes into a full doc (a forged frame: our writes are refused): dropped.
  b.doc.receive(a.self, { d: 'board', t: 1, ws: [b.doc.writer.replace(/^b+/, 'a'.repeat(64))], e: [[0, 999, 1, 's', 'fill', null, 'over']] });
  if (state(b.doc) !== before && b.doc.entries.size > MAX_ENTRIES) fail('a full doc took a member write');
  const first = s.entries()[0].id;
  if (!s.delete(first) || !s.add('room again')) fail('a deletion did not free room');
  console.warn = warn;
  // What a doc's values take in all.
  const net2 = new Net(4);
  const [c] = [net2.node('c')];
  const v = 'y'.repeat(MAX_VALUE_LENGTH - 100);
  let n = 0;
  console.warn = () => {};
  while (c.doc.set('big').add(v)) n++;
  console.warn = warn;
  if (c.doc.length > MAX_DOC_LENGTH || n < Math.floor(MAX_DOC_LENGTH / MAX_VALUE_LENGTH) - 1) fail(`doc length ${c.doc.length} after ${n} values`);
}
console.log('limits: our mistakes throw; a full doc refuses writes with a warning and takes them again as room frees');

// Frames: none over MESSAGE_BYTES as UTF-8, a catch-up of a full doc of 3-byte characters included, and an
// ask at the context's caps.
{
  const net = new Net(5);
  const a = net.node('a');
  const v = '€'.repeat(MAX_VALUE_LENGTH / 2 - 10); // 3 bytes each as UTF-8
  const warn = console.warn;
  console.warn = () => {};
  while (a.doc.set('s').add(v));
  console.warn = warn;
  const b = net.node('b');
  net.link(a, b);
  net.run();
  if (b.doc.entries.size !== a.doc.entries.size) fail('a full doc of large values did not catch up');
  if (net.largest > MESSAGE_BYTES) fail(`a frame of ${net.largest} bytes`);
  // An ask at MAX_WRITERS writers and MAX_RANGES ranges.
  const c = net.node('c');
  for (let i = 0; i < MAX_WRITERS - 1; i++) c.doc.ctx.add(i.toString(16).padStart(72, 'f'), 1, 1);
  for (let i = 0; c.doc.ctx.size < MAX_RANGES; i++) c.doc.ctx.add('f'.repeat(72), 2 * i + 2 ** 52, 2 * i + 2 ** 52);
  const ask = c.doc.askFrame();
  const bytes = Buffer.byteLength(JSON.stringify(ask)) + 1;
  if (ask.r.length !== MAX_RANGES || bytes > MESSAGE_BYTES) fail(`an ask of ${ask.r.length} ranges takes ${bytes} bytes`);
  console.log(`frames: a full doc's catch-up in frames of at most ${net.largest} bytes, an ask at the caps ${bytes} (limit ${MESSAGE_BYTES})`);
}

// Member data: malformed frames are dropped whole, never throw, and change nothing.
{
  const net = new Net(6);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  churn(net, [a, b], 40);
  net.run();
  const before = state(b.doc);
  const w = a.doc.writer;
  const good = { d: 'board', t: 3, ws: [w], e: [[0, 1e6, 5, 's', 's', null, 1]], x: [[0, 1e6 + 1, 1e6 + 1]] };
  // Each differs from `good` (a fresh write and a fresh dead dot) in one field, so a check left out lets it in.
  const E = (...x) => ({ ...good, e: [x] });
  const F = 2e6; // a dot b hasn't seen
  const bad = [
    null, 1, 'x', [], { t: 9 }, { ...good, ws: 'w' }, { ...good, ws: [w.toUpperCase()] }, { ...good, ws: [w + '0'] },
    E(1, F, 1, 's', 's', null, 1), E(0, 0, 1, 's', 's', null, 1), E(0, F + 0.5, 1, 's', 's', null, 1), E(0, F, 2 ** 53, 's', 's', null, 1),
    E(0, F, 1, 'q', 's', null, 1), E(0, F, 1, '__proto__', 's', null, 1), E(0, F, 1, 's', '', null, 1), E(0, F, 1, 's', 's', 'key', 1),
    E(0, F, 1, 'm', 's', null, 1), E(0, F, 1, 'm', 's', 'k'.repeat(MAX_KEY_LENGTH + 1), 1), E(0, F, 1, 's', 's', null, 'x'.repeat(MAX_VALUE_LENGTH)),
    E(0, F, 1, 's', 's', null), { ...good, e: {} }, { ...good, x: [[0, 5, 4]] }, { ...good, x: [[0, 0, 4]] }, { ...good, x: [[0, 1]] },
    { ...good, end: true }, { d: 'board', t: 2, ws: [w], r: [[0, 1, 1]] }, { d: 'board', t: 2, ws: [w], r: [[0, 1, 1]], i: b.doc.writer },
    { ...good, t: 1, ws: [id('c') + '00000000'] }, // a delta carrying another member's write
    E(0, F, 1, 's', 's', null, JSON.parse('['.repeat(MAX_VALUE_DEPTH + 1) + ']'.repeat(MAX_VALUE_DEPTH + 1))),
  ];
  // Nor does one make b send anything: an ask without a valid incarnation of its sender's is not answered.
  let sent = 0;
  const bsend = b.host.send;
  b.host.send = () => sent++;
  for (const m of bad) {
    try {
      b.doc.receive(a.self, m);
    } catch (e) {
      fail(`${JSON.stringify(m)?.slice(0, 80)} threw ${e}`);
    }
    if (state(b.doc) !== before || sent) fail(`${JSON.stringify(m).slice(0, 120)} changed the doc or was answered`);
  }
  b.host.send = bsend;
  b.doc.receive(a.self, good);
  if (state(b.doc) === before) fail('the frame the bad ones are made from was not taken');
  // Random mutations of real frames: never a throw.
  const r = rng(11);
  const frames = [];
  const send = a.host.send;
  a.host.send = (to, m) => frames.push(JSON.parse(JSON.stringify(m)));
  churn(net, [a], 30);
  a.doc.receive(b.self, b.doc.askFrame());
  a.host.send = send;
  const junk = [null, -1, 0, 2 ** 60, 'x', '', [], {}, [0], true, 1.5, '__proto__', 'f'.repeat(72)];
  for (let i = 0; i < 20000; i++) {
    const m = JSON.parse(JSON.stringify(frames[int(r, frames.length)]));
    for (let k = 1 + int(r, 3); k > 0; k--) {
      const paths = [];
      const walk = (o, p) => {
        if (o && typeof o === 'object') for (const key of Object.keys(o)) (paths.push([o, key]), walk(o[key], p));
      };
      walk(m, '');
      const [o, key] = paths[int(r, paths.length)];
      o[key] = structuredClone(junk[int(r, junk.length)]);
    }
    try {
      b.doc.receive(a.self, m);
    } catch (e) {
      fail(`a mutated frame threw ${e.stack}`);
    }
  }
}
console.log('member data: malformed frames dropped whole without a throw; 20000 mutated frames never threw');

// A member that asks over and over gets at most MAX_ANSWERS answers while it stays, and each entry once.
{
  const net = new Net(8);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  churn(net, [a], 100);
  net.run();
  const answers = [];
  b.host.send = (to, m) => m.t === 3 && answers.push(m);
  b.doc.left(a.self); // as if a joined again: the ask from its first join counted
  for (let i = 0; i < 50; i++) b.doc.receive(a.self, { d: 'board', t: 2, ws: [a.doc.writer], r: [], i: a.doc.writer });
  const ends = answers.filter((m) => m.end).length;
  const entries = answers.reduce((s, m) => s + (m.e?.length ?? 0), 0);
  if (ends !== 8 || entries !== b.doc.entries.size) fail(`50 asks got ${ends} answers carrying ${entries} entries for ${b.doc.entries.size}`);
  // Leaving and joining again starts afresh.
  answers.length = 0;
  b.doc.left(a.self);
  b.doc.receive(a.self, { d: 'board', t: 2, ws: [], r: [], i: a.doc.writer });
  if (answers.reduce((s, m) => s + (m.e?.length ?? 0), 0) !== b.doc.entries.size) fail('a member that joined again was not answered afresh');
}
console.log('answers: at most 8 per member while it stays, each entry sent once');

// Stamps: one more than MAX_CLOCK_STEP ahead of ours is ignored, so no frame can run the clock out.
{
  const net = new Net(9);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  const w = a.doc.writer;
  b.doc.receive(a.self, { d: 'board', t: 1, ws: [w], e: [[0, 50, b.doc.clock + MAX_CLOCK_STEP + 1, 's', 's', null, 'far']] });
  if (b.doc.entries.size || b.doc.ctx.has(w, 50)) fail('a stamp far ahead was taken');
  b.doc.receive(a.self, { d: 'board', t: 1, ws: [w], e: [[0, 50, b.doc.clock + MAX_CLOCK_STEP, 's', 's', null, 'near']] });
  if (b.doc.clock !== MAX_CLOCK_STEP) fail('a stamp at the step was not taken');
}
console.log('stamps: a frame may move the clock at most MAX_CLOCK_STEP');

// Claims on a member's future dots: a member says a's next dots are dead, so others would ignore a's writes.
// a sees the claim (it is in the ask that b sends a when a write of a's comes in already counted) and moves
// to a new incarnation; its writes from then on reach everyone.
for (const via of ['delta', 'answer', 'ask']) {
  const net = new Net(10);
  const [a, b, m] = ['a', 'b', 'f'].map((x) => net.node(x));
  net.mesh([a, b, m]);
  a.doc.map('m').set('k', 1);
  net.run();
  const w = a.doc.writer;
  const claim = { d: 'board', ws: [w], x: [[0, a.doc.n + 1, a.doc.n + 1000]] };
  if (via === 'delta') b.doc.receive(m.self, { ...claim, t: 1 });
  else if (via === 'answer') b.doc.receive(m.self, { ...claim, t: 3 });
  else a.doc.receive(m.self, { d: 'board', t: 2, ws: [w], r: [[0, 1, a.doc.n + 1000]], i: m.doc.writer });
  // A claim that reaches a itself moves it at once, before it writes.
  if (via !== 'ask') {
    const c = net.node('c');
    c.doc.map('m').set('k', 1);
    const cw = c.doc.writer;
    c.doc.receive(m.self, { d: 'board', t: via === 'delta' ? 1 : 3, ws: [cw], x: [[0, c.doc.n + 1, c.doc.n + 5]] });
    if (c.doc.writer === cw) fail(`${via}: a claim on our own next dots didn't move us to a new incarnation`);
  }
  a.doc.map('m').set('k', 2);
  net.run();
  a.doc.map('m').set('k', 3);
  net.run();
  if (a.doc.writer === w) fail(`${via}: a never moved to a new incarnation`);
  if (b.doc.map('m').get('k') !== 3) fail(`${via}: a's writes after a claim on its dots never reached b (b shows ${b.doc.map('m').get('k')})`);
  same([a, b, m], `claims via ${via}`);
}
console.log('claims on our future dots: the doc moves to a new incarnation and its writes reach everyone');

// A full context makes room: a member's claims for 2000 writers that hold nothing don't stop a new writer.
{
  const net = new Net(12);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  const ws = Array.from({ length: 2000 }, (_, i) => i.toString(16).padStart(72, '0'));
  b.doc.receive(a.self, { d: 'board', t: 1, ws, x: ws.map((_, i) => [i, 1, 1]) });
  if (b.doc.ctx.writers.size > MAX_WRITERS) fail(`context of ${b.doc.ctx.writers.size} writers`);
  // And claims fragmenting one writer's dots don't stop new writers either.
  b.doc.receive(a.self, { d: 'board', t: 1, ws: [ws[0]], x: Array.from({ length: MAX_RANGES }, (_, i) => [0, 3 * i + 10, 3 * i + 10]) });
  if (b.doc.ctx.size > MAX_RANGES) fail(`context of ${b.doc.ctx.size} ranges`);
  a.doc.set('s').add('still heard');
  const c = net.node('c');
  net.link(b, c);
  c.doc.set('s').add('new writer');
  net.run();
  if (b.doc.set('s').entries().length !== 2) fail(`a full context refused writes: ${JSON.stringify(b.doc.set('s').entries())}`);
}
console.log('context caps: writers and ranges stay capped and new writers are still heard');
console.log('all doc CRDT checks passed');
