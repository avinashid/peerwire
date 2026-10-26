// Shared documents (src/doc.js) on fake rooms in one process, seeded. Members write at random (map sets and
// deletes, set adds and deletes, log appends), and what they send each other goes through a network that can
// deliver frames in any order, twice, or not at all, cut a link (its frames in flight are lost, as with a
// session) and link again (a join, so a catch-up). Checks that every member ends up holding the same doc:
// after random order and duplicates with no catch-up, after drops healed by joining again, after a partition
// heals, and for a late joiner, which gets each entry about once. Then intent (a later write wins, a set
// beats a delete it didn't see, a log keeps causal order), the checks on member data (malformed frames,
// caps, a frame's size, answers per member, stamps far ahead (all taken), claims on our future dots) and 'change'.
//
//   node packages/js/test/doc_crdt.mjs
import { Doc, MAX_VALUE_LENGTH, MAX_VALUE_DEPTH, MAX_ENTRIES, MAX_DOC_LENGTH, MAX_WRITERS, MAX_RANGES, MAX_KEY_LENGTH, MAX_NAME_LENGTH, MESSAGE_BYTES } from '../src/doc.js';

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
      now: () => this.now,
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
      if (!this.fire(Infinity)) return;
    }
    fail('the network never went quiet');
  }

  // Fires the first timer due by `end`, if any.
  fire(end) {
    this.timers = this.timers.filter((t) => t.fn);
    if (!this.timers.length) return false;
    const t = this.timers.reduce((a, b) => (b.at < a.at ? b : a));
    if (t.at > end) return false;
    this.now = Math.max(this.now, t.at);
    const fn = t.fn;
    t.fn = null;
    fn();
    return true;
  }

  // Lets `ms` pass, firing the timers due meanwhile; delivers nothing.
  wait(ms) {
    const end = this.now + ms;
    for (let n = 0; n < 1e5 && this.fire(end); n++);
    this.now = end;
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

// Whether a doc's indexes agree: every entry listed in a collection is held, and the counts and length add up.
function whole(doc, what) {
  let n = 0;
  let len = 0;
  for (const coll of doc.colls.values()) {
    for (const e of coll.t === 'm' ? [...coll.keys.values()].flat() : coll.items.values()) {
      n++;
      len += e.len;
      if (doc.entries.get(e.id) !== e || doc.byWriter.get(e.w)?.get(e.n) !== e) fail(`${what}: ${e.id} is listed but not held`);
    }
  }
  if (n !== doc.entries.size || len !== doc.length) fail(`${what}: ${doc.entries.size} entries held and ${n} listed; length ${doc.length}, listed ${len}`);
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
// A map whose name ends where another's key begins, map 'a' key 'b\0c' and map 'a\0b' key 'c', changed by one
// frame: two events.
{
  const net = new Net(17);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  net.run();
  const events = [];
  b.doc.on('change', (ev) => events.push(`${ev.name}|${ev.key}`));
  b.doc.receive(a.self, { d: 'board', t: 1, ws: [a.doc.writer], e: [[0, 100, 1, 'm', 'a', 'b\u0000c', 1], [0, 101, 1, 'm', 'a\u0000b', 'c', 2]] });
  if (events.length !== 2) fail(`two maps' keys changed in one frame gave ${events.length} events: ${JSON.stringify(events)}`);
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
    { ...good, end: true }, { d: 'board', t: 2, ws: [w], r: [[0, 2, 1]] }, { d: 'board', t: 2, ws: [w], r: [[1, 1, 1]] },
    { ...good, t: 1, ws: [id('c') + '00000000'] }, // a delta carrying another member's write
    E(0, F, 1, 's', 's', null, JSON.parse('['.repeat(MAX_VALUE_DEPTH + 1) + ']'.repeat(MAX_VALUE_DEPTH + 1))),
  ];
  // Nor does one make b send anything: a malformed ask is not answered.
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

// A member that asks over and over is answered 8 times at once, then once per 15 s, each time with all it
// lacks. An ask past the budget waits for it, and only the latest one waits.
{
  const net = new Net(8);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  churn(net, [a], 100);
  net.run();
  let answers = [];
  const asks = [];
  b.host.send = (to, m) => (m.t === 3 ? answers : asks).push(m);
  const ends = () => answers.filter((m) => m.end).length;
  const entries = () => answers.reduce((s, m) => s + (m.e?.length ?? 0), 0);
  const ask = (r = []) => b.doc.receive(a.self, { d: 'board', t: 2, ws: [a.doc.writer], r });
  const size = b.doc.entries.size;
  b.doc.left(a.self); // as if a joined again: the ask from its first join counted
  for (let i = 0; i < 50; i++) ask();
  if (ends() !== 8 || entries() !== 8 * size) fail(`50 asks got ${ends()} answers carrying ${entries()} entries for ${size}`);
  net.wait(14999);
  if (ends() !== 8) fail(`an answer past the budget went after ${net.now} ms`);
  net.wait(60000);
  if (ends() !== 9 || entries() !== 9 * size) fail(`the ask that waited got ${ends() - 8} answers carrying ${entries() - 8 * size} entries`);
  // An hour idle refills the budget to 8, not more.
  net.wait(3600e3);
  answers = [];
  for (let i = 0; i < 50; i++) ask();
  if (ends() !== 8) fail(`after an hour idle, 50 asks got ${ends()} answers`);
  // An ask every second for a minute: one answer each 15 s.
  answers = [];
  for (let i = 0; i < 60; i++) {
    ask();
    net.wait(1000);
  }
  if (ends() !== 4) fail(`an ask a second for 60 s got ${ends()} answers`);
  // An ask answered at once while an earlier one waits, the budget being back before the wait ran: the wait
  // finds nothing left to answer, and neither throws nor answers twice.
  b.doc.left(a.self);
  answers = [];
  for (let i = 0; i < 9; i++) ask();
  net.now += 15000;
  ask();
  net.wait(60000);
  if (ends() !== 9) fail(`10 asks, the last as the budget came back, got ${ends()} answers`);
  // Our asks of a member have the same budget: 12 asks of ours due at once, as a's asks show dots we lack, send
  // 8 and the rest wait, then go one at a time.
  b.doc.left(a.self);
  for (let i = 0; i < 12; i++) {
    ask([[0, 1, 1000 + i]]);
    b.doc.nextAsk(true); // as if a answered
  }
  if (asks.length !== 8) fail(`12 asks due at once sent ${asks.length}`);
  net.wait(15000);
  if (asks.length !== 9) fail(`an ask of ours that waited for the budget went ${asks.length - 8} times`);
  // Leaving and joining again starts afresh.
  answers = [];
  b.doc.left(a.self);
  b.doc.receive(a.self, { d: 'board', t: 2, ws: [], r: [] });
  if (entries() !== size) fail('a member that joined again was not answered afresh');
}
console.log('answers: 8 per member at once, then one per 15 s, each with all the member lacks; an ask past that waits');

// What answers carry is bounded as well, whatever the asker claims to lack: 8 Mi characters of JSON text at
// once (two full docs), then 64 Ki a second. A member asks with an empty context every second for a minute, of
// a doc of 2 Mi characters: it is sent the doc at once 5 times (the last takes the budget past empty), then
// about once every 32 s.
{
  const CHARS = 8 * 2 ** 20;
  const CHARS_PER_S = 64 * 1024;
  const net = new Net(14);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  for (let i = 0; i < 32; i++) b.doc.set('s').add('x'.repeat(MAX_VALUE_LENGTH - 2));
  net.run();
  let chars = 0;
  let answer = 0;
  let largest = 0;
  const times = [];
  b.host.send = (to, m) => {
    if (m.t !== 3) return;
    answer += JSON.stringify(m).length;
    if (!m.end) return;
    times.push(net.now);
    chars += answer;
    largest = Math.max(largest, answer);
    answer = 0;
  };
  b.doc.left(a.self);
  for (let i = 0; i < 60; i++) {
    b.doc.receive(a.self, { d: 'board', t: 2, ws: [], r: [] });
    net.wait(1000);
  }
  const bound = CHARS + 60 * CHARS_PER_S + largest;
  if (chars > bound || times.slice(0, 5).join() !== '0,1000,2000,3000,4000' || times.length !== 6) fail(`an asker of a ${b.doc.length}-character doc was sent ${chars} characters in 60 s (bound ${bound}), answers at ${times.join(', ')} ms`);
  console.log(`answers: a ${b.doc.length}-character doc asked for every second, sent ${times.length} times in 60 s (at ${times.join(', ')} ms), ${chars} characters of at most ${bound}`);
}

// A member that closes the doc and opens it again, over and over, while it stays (a component that remounts, a
// page switching views) catches up every time: 7 times at once (its first ask, from the join, counted), then
// each 15 s later.
{
  const net = new Net(13);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  net.mesh([a, b]);
  for (let i = 0; i < 5; i++) a.doc.set('s').add(i);
  a.doc.map('m').set('k', 'v');
  net.run();
  const late = [];
  for (let i = 1; i <= 12; i++) {
    b.doc.close();
    const t = net.now;
    net.open(b);
    net.run();
    if (b.doc.set('s').entries().length !== 5) fail(`reopen ${i}: b holds ${b.doc.set('s').entries().length} of 5 items`);
    same([a, b], `reopen ${i}`);
    late.push(net.now - t);
  }
  if (late.join() !== '0,0,0,0,0,0,0,15000,15000,15000,15000,15000') fail(`reopens caught up after ${late.join(', ')} ms`);
}
console.log('reopening a doc 12 times while a member stays: each catches up, 7 at once and then each after 15 s');

// A member that never answers holds our asks up once: asked when it joined, it had 5 s, and it isn't asked again
// until it answers, however often it prompts us to; once it does, the ask it was due goes.
{
  const net = new Net(16);
  const [a, b] = ['a', 'b'].map((x) => net.node(x));
  const m = net.node('f', { open: false });
  net.mesh([a, b, m]);
  net.run();
  let asked = 0;
  const send = b.host.send;
  b.host.send = (to, msg) => {
    if (to === m.self && msg.t === 2) asked++;
    send(to, msg);
  };
  for (let i = 1; i <= 10; i++) {
    b.doc.receive(m.self, { d: 'board', t: 2, ws: [m.self + '00000001'], r: [[0, 1, i]] });
    net.run();
  }
  if (asked) fail(`a member that never answered was asked ${asked} times more`);
  b.doc.receive(m.self, { d: 'board', t: 3, ws: [], end: 1 });
  net.run();
  if (asked !== 1) fail(`a member that answered at last was asked ${asked} times`);
}
console.log('a member that never answers is asked once, until it answers');

// Stamps far ahead: a member moves the stamps by 2^32 in each of k deltas of its own (the earlier ones deleted,
// or not), or to 2^53 - 1 at once, sent to a and b while c is cut off; then a, b and c write. Every member takes
// them, so c, back, and d, joining after, converge with a and b, writes made after the jump included.
for (const [k, kill] of [[1, false], [4, false], [4, true], [Infinity, false]]) {
  const net = new Net(9);
  const [a, b, c, m] = ['a', 'b', 'c', 'f'].map((x) => net.node(x));
  net.mesh([a, b, c, m]);
  a.doc.map('m').set('title', 'before');
  a.doc.set('s').add('a before');
  net.run();
  for (const p of [a, b, m]) net.unlink(c, p);
  const w = m.self + '0badc10c';
  for (let i = 1; i <= Math.min(k, 4); i++) {
    const t = k === Infinity ? Number.MAX_SAFE_INTEGER : Math.max(a.doc.clock, b.doc.clock) + 2 ** 32;
    const x = kill && i > 1 ? { x: [[0, i - 1, i - 1]] } : {};
    for (const p of [a, b]) p.doc.receive(m.self, { d: 'board', t: 1, ws: [w], e: [[0, i, t, 'm', 'z', 'k', i]], ...x });
    if (k === Infinity) break;
  }
  a.doc.map('m').set('title', 'after');
  a.doc.set('s').add('a after');
  b.doc.log('l', { keep: KEEP }).append('b after');
  c.doc.set('s').add('c, cut off');
  net.run();
  const d = net.node('d');
  net.mesh([a, b, c, d]);
  net.run();
  a.doc.log('l', { keep: KEEP }).append('a, after d joined');
  net.run();
  const what = `stamps moved ${k === Infinity ? 'to 2^53 - 1' : `${k} x 2^32`}${kill ? ', earlier ones deleted' : ''}`;
  same([a, b, c, d], what);
  if (d.doc.map('m').get('title') !== 'after' || d.doc.set('s').entries().length !== 3 || d.doc.log('l').entries().length !== 2) fail(`${what}: d holds ${state(d.doc).slice(0, 300)}`);
}
// And sent to a alone, so a's next writes carry stamps far past b's clock: b takes them.
{
  const net = new Net(9);
  const [a, b, m] = ['a', 'b', 'f'].map((x) => net.node(x));
  net.mesh([a, b, m]);
  net.run();
  const w = m.self + '0badc10c';
  for (let i = 1; i <= 2; i++) a.doc.receive(m.self, { d: 'board', t: 1, ws: [w], e: [[0, i, a.doc.clock + 2 ** 32, 'm', 'z', 'k', i]], ...(i > 1 && { x: [[0, i - 1, i - 1]] }) });
  for (let i = 0; i < 5; i++) a.doc.log('l', { keep: KEEP }).append(i);
  b.doc.log('l', { keep: KEEP }).append('b');
  net.run();
  if (b.doc.log('l').entries().length !== 6) fail(`a's writes after a jump sent to it alone: b holds ${b.doc.log('l').entries().length} of 6 log entries`);
}
console.log('stamps: taken however far ahead, so members cut off or joining after a jump converge with the rest');

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
  else a.doc.receive(m.self, { d: 'board', t: 2, ws: [w], r: [[0, 1, a.doc.n + 1000]] });
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

// Past the incarnation cap: a member claims a's next dots until a has moved on 16 times, then sends an entry on
// a's next dot. a never holds it (it isn't a's), writes after it, and its writes keep the doc whole and don't
// throw. With every dot claimed, its writes return false or null with a warning.
{
  const net = new Net(15);
  const [a, b, m] = ['a', 'b', 'f'].map((x) => net.node(x));
  net.mesh([a, b, m]);
  net.run();
  for (let i = 0; i < 20; i++) a.doc.receive(m.self, { d: 'board', t: 2, ws: [a.doc.writer], r: [[0, 1, a.doc.n + 5]] });
  if (a.doc.incarnations !== 16) fail(`20 claims moved a ${a.doc.incarnations} times`);
  net.run();
  const w = a.doc.writer;
  const next = a.doc.n + 1;
  // Sent to b as well, which takes it for a's: a tells b it is dead.
  const forged = { d: 'board', t: 3, ws: [w], e: [[0, next, a.doc.clock + 1, 's', 'x', null, 'forged']] };
  for (const p of [b, a]) p.doc.receive(m.self, forged);
  net.run();
  if (a.doc.entries.has(`${w}.${next}`) || a.doc.set('x').entries().length) fail('a holds an entry on its own next dot that it did not write');
  if (b.doc.entries.has(`${w}.${next}`)) fail("b kept an entry forged on a's next dot");
  const strokes = a.doc.set('s');
  const sid = strokes.add({ stroke: 1 });
  if (sid === `${w}.${next}`) fail('a wrote on a dot it had seen');
  a.doc.map('x').set('k', 'mine');
  a.doc.map('x').delete('k');
  strokes.delete(sid);
  whole(a.doc, 'writes after an entry forged on our next dot');
  if (strokes.entries().length || a.doc.entries.size) fail(`a holds ${a.doc.entries.size} entries after deleting its own`);
  // Every dot claimed, in an answer's dead ranges.
  a.doc.receive(m.self, { d: 'board', t: 3, ws: [w], x: [[0, 1, Number.MAX_SAFE_INTEGER]] });
  const warn = console.warn;
  let warned = 0;
  console.warn = () => warned++;
  try {
    const got = [strokes.add(1), a.doc.map('x').set('k', 1), a.doc.log('l', { keep: KEEP }).append(1)];
    if (got.join() !== ',false,' || warned !== 3) fail(`with every dot claimed, writes gave ${JSON.stringify(got)} and ${warned} warnings`);
  } finally {
    console.warn = warn;
  }
  whole(a.doc, 'every dot claimed');
}
console.log('claims past the 16 incarnations: an entry on our next dot is never held, writes skip claimed dots, and with none left warn');

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
  net.run(1e4); // were the writes refused, asks for them would go on every 15 s
  if (b.doc.set('s').entries().length !== 2) fail(`a full context refused writes: ${JSON.stringify(b.doc.set('s').entries())}`);
}
console.log('context caps: writers and ranges stay capped and new writers are still heard');

// A member's frame costs about one pass over what it names. Three shapes from the security review (DOC-3), each
// timed against taking a frame of 16384 set items into an empty doc (the best of 3 runs each, so the host's load
// counts alike), must stay within a few times it, where walking what the doc holds again for each item made them
// 6 to 60 times: 16384 copies of a dead range over a writer's 9000 entries (within 3 times), 16384 entries into
// a log that keeps 5000 (5, as each goes in and out), and dead ranges naming 16000 fresh writers into a context
// of 1000 writers with live entries (3).
{
  const peer = id('e');
  const w = peer + '0badc10c';
  const host = { self: id('a'), peers: () => [peer], send: () => {}, after: () => () => {}, now: () => 0 };
  const best = (make) => {
    let t = Infinity;
    for (let i = 0; i < 3; i++) {
      const [doc, frame] = make();
      const t0 = performance.now();
      doc.receive(peer, frame);
      t = Math.min(t, performance.now() - t0);
    }
    return t;
  };
  const items = (n, from, t = 's') => Array.from({ length: n }, (_, i) => [0, from + i, from + i, t, t, null, 0]);
  const base = best(() => [new Doc('board', host), { d: 'board', t: 1, ws: [w], e: items(16384, 1) }]);
  const shapes = {
    'dead ranges over held entries': [3, () => {
      const doc = new Doc('board', host);
      doc.receive(peer, { d: 'board', t: 1, ws: [w], e: items(9000, 1) });
      return [doc, { d: 'board', t: 1, ws: [w], x: Array.from({ length: 16384 }, () => [0, 9001, 17999]) }];
    }],
    'log entries past keep': [5, () => {
      const doc = new Doc('board', host);
      doc.log('l', { keep: 5000 });
      doc.receive(peer, { d: 'board', t: 1, ws: [w], e: items(5000, 1, 'l') });
      return [doc, { d: 'board', t: 1, ws: [w], e: items(16384, 5001, 'l') }];
    }],
    'fresh writers into a full context': [3, () => {
      const doc = new Doc('board', host);
      const ws = Array.from({ length: 1000 }, (_, i) => peer + i.toString(16).padStart(8, '0'));
      doc.receive(peer, { d: 'board', t: 1, ws, e: ws.map((_, i) => [i, 1, 1, 's', 's', null, 0]) });
      const fresh = Array.from({ length: 16000 }, (_, i) => 'd' + i.toString(16).padStart(71, '0'));
      return [doc, { d: 'board', t: 1, ws: fresh, x: fresh.map((_, i) => [i, 1, 1]) }];
    }],
  };
  const got = [];
  for (const [what, [limit, make]] of Object.entries(shapes)) {
    const t = best(make);
    got.push(`${what} ${t.toFixed(0)} ms`);
    if (t > limit * base) fail(`${what}: ${t.toFixed(0)} ms, ${(t / base).toFixed(1)} times a frame of 16384 items (${base.toFixed(0)} ms)`);
  }
  console.log(`frame cost: ${got.join(', ')}; a frame of 16384 items ${base.toFixed(0)} ms`);
}
console.log('all doc CRDT checks passed');
