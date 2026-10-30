// @peerwire/react's RoomStore on fake rooms in one process: what members send each other goes through a queue
// that counts it and can lose a message, as room.js can lose the first one to a newcomer. Checks the caps
// (values, shared state in all, each member's per-peer quota, keys a hook reads pushing out keys none reads),
// the hybrid clock (a clock far ahead is ignored, a key can't be stamped out of reach), and catching up (a
// join costs each value once, a lost first message, a member that asks over and over, writes that cross), and
// values nested past MAX_VALUE_DEPTH. Needs the build: `npm run build` (packages/react).
//
//   node packages/react/test/store.mjs
import { existsSync } from 'node:fs';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
if (!existsSync(new URL('../dist/store.js', import.meta.url))) fail('no dist/: run `npm run build` in packages/react');
const { RoomStore, MAX_KEYS, MAX_VALUE_LENGTH, MAX_STORE_LENGTH, MAX_PEER_KEYS, MAX_PEER_LENGTH, MAX_CLOCK_DRIFT, MAX_VALUE_DEPTH } = await import('../dist/store.js');

const MAX_MESSAGE = 1 << 20; // room.send's limit for a reliable message
const id = (c) => c.repeat(64);
const full = 'x'.repeat(MAX_VALUE_LENGTH - 2); // MAX_VALUE_LENGTH as JSON, with its quotes

// Members of one room. `lose(from, to, m)` true drops a message; `sent` counts what each sends each.
function net() {
  const nodes = new Map();
  let queue = [];
  // `outbox`: a reliable message over MAX_MESSAGE bytes is dropped without a throw, as room.js does when the
  // peer's outbox holds frames (OUT-1).
  const n = { lose: null, outbox: false, sent: new Map(), log: [], largest: 0 };
  n.node = (c, { start = true } = {}) => {
    const self = id(c);
    const on = {};
    const room = {
      self,
      peers: [],
      on: (e, fn) => ((on[e] ??= []).push(fn), () => {}),
      getPresence() {},
      send(m, o) {
        const text = JSON.stringify(m);
        const bytes = Buffer.byteLength(text);
        if (o?.lane !== 'unreliable') n.largest = Math.max(n.largest, bytes);
        if (bytes > (o?.lane === 'unreliable' ? 1200 : MAX_MESSAGE)) {
          if (n.outbox && o?.lane !== 'unreliable') return 0;
          throw new RangeError('too large');
        }
        const to = o?.to ? [o.to] : room.peers;
        for (const t of to) queue.push([self, t, text]);
        return to.length;
      },
    };
    const node = { self, room, emit: (e, ...a) => (on[e] ?? []).forEach((f) => f(...a)), st: new RoomStore(room) };
    if (start) node.st.start();
    nodes.set(self, node);
    return node;
  };
  // Raw: a member with no store, whose messages the test sends with `n.from(raw, to, m)`.
  n.raw = (c) => {
    const self = id(c);
    const raw = { self, got: [], room: { peers: [] }, emit: (e, m) => e === 'message' && raw.got.push(m) };
    nodes.set(self, raw);
    return raw;
  };
  // `m` as JSON text when it is a string, so the test can send a value too deep for JSON.stringify.
  n.from = (a, b, m) => queue.push([a.self, b.self, typeof m === 'string' ? m : JSON.stringify(m)]);
  n.join = (a, b) => {
    a.room.peers.push(b.self);
    b.room.peers.push(a.self);
    a.emit('peer-join', b.self);
    b.emit('peer-join', a.self);
  };
  n.leave = (a, b) => {
    a.room.peers.splice(a.room.peers.indexOf(b.self), 1);
    b.room.peers.splice(b.room.peers.indexOf(a.self), 1);
    a.emit('peer-leave', b.self);
    b.emit('peer-leave', a.self);
  };
  n.flush = () => {
    while (queue.length) {
      const q = queue;
      queue = [];
      for (const [f, t, text] of q) {
        const m = JSON.parse(text);
        if (n.lose?.(f, t, m)) continue;
        n.sent.set(`${f}>${t}`, (n.sent.get(`${f}>${t}`) ?? 0) + text.length);
        n.log.push([f, t, m]);
        nodes.get(t).emit('message', m, f);
      }
    }
  };
  n.bytes = (a, b) => n.sent.get(`${a.self}>${b.self}`) ?? 0;
  // Shared values a sent b in catch-up messages.
  n.values = (a, b) => n.log.filter(([f, t, m]) => f === a.self && t === b.self && m.$peerwire === 'sync').reduce((k, [, , m]) => k + (m.s?.length ?? 0), 0);
  return n;
}
const read = (node, k) => node.st.getShared(k)?.v;
const throws = (fn) => {
  try {
    fn();
    return null;
  } catch (e) {
    return e;
  }
};

// The caps on shared values: a value over MAX_VALUE_LENGTH is ignored, MAX_STORE_LENGTH in all, replacing a
// value frees room, and a local write past them throws.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  n.from(x, a, { $peerwire: 'set', k: 'over', v: `${full}x`, c: 1 });
  const fit = MAX_STORE_LENGTH / MAX_VALUE_LENGTH;
  for (let i = 0; i <= fit; i++) n.from(x, a, { $peerwire: 'set', k: `b${i}`, v: full, c: 1 });
  n.flush();
  if (read(a, 'over') !== undefined) fail('a value over MAX_VALUE_LENGTH was kept');
  const kept = () => Array.from({ length: fit + 2 }, (_, i) => i).filter((i) => read(a, `b${i}`) !== undefined).length;
  if (kept() !== fit || read(a, `b${fit}`) !== undefined) fail(`kept ${kept()} values of ${MAX_VALUE_LENGTH}, not the first ${fit}`);
  for (const k of ['b0', 'b1']) n.from(x, a, { $peerwire: 'set', k, v: 'small', c: 2 });
  n.from(x, a, { $peerwire: 'set', k: `b${fit}`, v: full, c: 1 });
  n.flush();
  if (read(a, `b${fit}`) === undefined || kept() !== fit + 1) fail('replacing a value did not free room');
  const e = throws(() => a.st.setShared('mine', `${full}x`));
  if (!(e instanceof RangeError)) fail(`a local value over MAX_VALUE_LENGTH: ${e}`);
  console.log(`caps: values over ${MAX_VALUE_LENGTH} ignored, ${fit} of that size fill ${MAX_STORE_LENGTH}, replacing frees room`);
}

// RH-3: a member that fills the store can't keep out the keys the app reads (a hook is subscribed) or writes:
// those push out keys no hook reads, oldest first. Keys nobody reads still wait for room.
{
  const n = net();
  const [a, x, y] = [n.node('a'), n.raw('b'), n.raw('c')];
  const unsub = a.st.subscribe('s:count', () => {});
  const now = Date.now();
  for (let i = 0; i < MAX_KEYS; i += 64) {
    n.from(x, a, { $peerwire: 'sync', s: Array.from({ length: 64 }, (_, j) => [`j${i + j}`, 0, now, x.self]), p: [] });
  }
  n.flush();
  if (read(a, `j${MAX_KEYS - 1}`) !== 0) fail('the junk keys did not fill the store');
  n.from(y, a, { $peerwire: 'set', k: 'count', v: 5, c: now + 1 });
  n.from(y, a, { $peerwire: 'set', k: 'unread', v: 5, c: now + 1 });
  n.flush();
  if (read(a, 'count') !== 5) fail('a key a hook reads was kept out by a full store');
  if (read(a, 'unread') !== undefined) fail('a key no hook reads pushed out another');
  if (read(a, 'j0') !== undefined || read(a, 'j1') !== 0) fail('the oldest unread key was not the one pushed out');
  a.st.setShared('mine', 1);
  if (read(a, 'mine') !== 1 || read(a, 'j1') !== undefined) fail('a local write to a full store');
  // RH-5: a store full of keys hooks read, filled by members: a local write past the caps is dropped with a
  // warning, not thrown, and nothing is sent.
  const m = net();
  const [b, z] = [m.node('d'), m.raw('e')];
  m.join(b, z);
  for (let i = 0; i < MAX_KEYS; i++) {
    b.st.subscribe(`s:k${i}`, () => {});
    m.from(z, b, { $peerwire: 'set', k: `k${i}`, v: i, c: now });
  }
  m.flush();
  z.got.length = 0;
  const warned = [];
  const warn = console.warn;
  console.warn = (w) => warned.push(w);
  let err, wrote;
  try {
    err = throws(() => (wrote = b.st.setShared('one-more', 1)));
  } finally {
    console.warn = warn;
  }
  m.flush();
  if (err || wrote !== false || read(b, 'one-more') !== undefined || z.got.length) fail(`RH-5: a local write past MAX_KEYS: ${err}, ${wrote}, sent ${z.got.length}`);
  if (warned.length !== 1 || !/full/.test(warned[0])) fail(`RH-5: warnings ${JSON.stringify(warned)}`);
  if (b.st.setShared('k0', 'replaced') !== true || read(b, 'k0') !== 'replaced') fail('RH-5: replacing a key in a full store');
  unsub();
  console.log(`RH-3 shared: ${MAX_KEYS} junk keys can't keep out a key a hook reads or a local write`);
  console.log('RH-5: past the caps a local write is dropped with one warning; the setter does not throw');
}

// RH-3: per-peer values have a quota per member, MAX_PEER_KEYS keys and MAX_PEER_LENGTH in all, so one member
// can't crowd out another; ours has the same quota.
{
  const n = net();
  const [a, x, y] = [n.node('a'), n.raw('b'), n.raw('c')];
  for (let i = 0; i <= MAX_PEER_KEYS; i++) n.from(x, a, { $peerwire: 'live', k: `j${i}`, v: 0, n: 1 });
  n.from(y, a, { $peerwire: 'live', k: 'cursor', v: { x: 1 }, n: 1 });
  n.flush();
  if (a.st.getLive(`j${MAX_PEER_KEYS - 1}`)[x.self] !== 0 || a.st.getLive(`j${MAX_PEER_KEYS}`)[x.self] !== undefined) fail('MAX_PEER_KEYS');
  if (a.st.getLive('cursor')[y.self]?.x !== 1) fail("one member's keys kept out another's");
  const z = n.raw('e');
  const fit = MAX_PEER_LENGTH / MAX_VALUE_LENGTH;
  for (let i = 0; i <= fit; i++) n.from(z, a, { $peerwire: 'live', k: `big${i}`, v: full, n: 1 + i });
  n.from(y, a, { $peerwire: 'live', k: 'big0', v: 'small', n: 2 });
  n.flush();
  if (a.st.getLive(`big${fit - 1}`)[z.self] !== full || a.st.getLive(`big${fit}`)[z.self] !== undefined) fail('MAX_PEER_LENGTH');
  if (a.st.getLive('big0')[y.self] !== 'small') fail("one member's values kept out another's");
  // Replacing a value frees its room.
  for (const k of ['big0', 'big1']) n.from(z, a, { $peerwire: 'live', k, v: 'small', n: 99 });
  n.from(z, a, { $peerwire: 'live', k: `big${fit}`, v: full, n: 100 });
  n.flush();
  if (a.st.getLive(`big${fit}`)[z.self] !== full) fail('replacing a per-peer value did not free room');
  const b = net().node('d');
  for (let i = 0; i < MAX_PEER_KEYS; i++) b.st.setLive(`k${i}`, i);
  if (!(throws(() => b.st.setLive('one-more', 1)) instanceof RangeError)) fail('a local per-peer key past MAX_PEER_KEYS');
  const c = net().node('f');
  for (let i = 0; i < fit; i++) c.st.setLive(`k${i}`, full);
  if (!(throws(() => c.st.setLive('one-more', 1)) instanceof RangeError)) fail('a local per-peer value past MAX_PEER_LENGTH');
  c.st.setLive('k0', 1);
  c.st.setLive('one-more', 1);
  console.log(`RH-3 per-peer: ${MAX_PEER_KEYS} keys and ${MAX_PEER_LENGTH} per member, ours too; one member can't crowd out another`);
}

// RH-1: the clock is per key, wall time or one past the value it replaces. A clock further ahead than
// MAX_CLOCK_DRIFT is ignored, so nothing can stamp a key out of reach, and no write throws for its clock.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  n.join(a, x);
  n.from(x, a, { $peerwire: 'set', k: 'zz', v: 'max', c: Number.MAX_SAFE_INTEGER });
  n.from(x, a, { $peerwire: 'set', k: 'zz', v: 'far', c: Date.now() + MAX_CLOCK_DRIFT + 60_000 });
  n.flush();
  if (read(a, 'zz') !== undefined) fail(`a clock past MAX_CLOCK_DRIFT took: ${read(a, 'zz')}`);
  const pin = Date.now() + MAX_CLOCK_DRIFT - 1000;
  n.from(x, a, { $peerwire: 'set', k: 'zz', v: 'ahead', c: pin });
  n.flush();
  if (read(a, 'zz') !== 'ahead') fail('a clock within MAX_CLOCK_DRIFT was refused');
  const before = Date.now();
  a.st.setShared('count', 1);
  a.st.setShared('count', 2);
  const sets = n.log.length;
  n.flush();
  const stamps = n.log.slice(sets).filter(([f, , m]) => f === a.self && m.$peerwire === 'set').map(([, , m]) => m.c);
  if (stamps.length !== 2 || stamps[0] < before || stamps[1] <= stamps[0] || stamps[1] > Date.now() + 1) fail(`wall-time stamps: ${stamps}`);
  a.st.setShared('zz', 'ours');
  n.flush();
  const over = n.log.findLast(([f, , m]) => f === a.self && m.k === 'zz');
  if (read(a, 'zz') !== 'ours' || over?.[2].c !== pin + 1) fail(`a write over a clock ahead: ${JSON.stringify(over?.[2])}`);
  // A joiner catches up on the key stamped ahead, from a, and writes over it.
  const y = n.node('c');
  n.join(a, y);
  n.flush();
  if (read(y, 'zz') !== 'ours' || read(y, 'count') !== 2) fail('catch-up of a key stamped ahead');
  y.st.setShared('zz', 'y');
  y.st.setShared('new', 1);
  n.flush();
  if (read(a, 'zz') !== 'y' || read(a, 'new') !== 1) fail("a joiner's writes after the catch-up");
  // A member whose clock is behind still writes over the current value: one past it.
  n.from(x, a, { $peerwire: 'set', k: 'new', v: 'behind', c: 5 });
  n.from(x, a, { $peerwire: 'set', k: 'other', v: 'behind', c: 5 });
  n.flush();
  if (read(a, 'new') !== 1 || read(a, 'other') !== 'behind') fail('a write stamped behind');
  console.log(`RH-1: clocks past now + ${MAX_CLOCK_DRIFT} ms ignored (MAX_SAFE_INTEGER too), a key stamped ahead is written over, no throw`);
}

// RH-4: a join costs each value once, both ways at once, with a digest of versions first.
{
  const n = net();
  const [a, b] = [n.node('a'), n.node('b')];
  for (let i = 0; i < 100; i++) a.st.setShared(`k${i}`, 'v'.repeat(1000));
  b.st.setShared('mine', 'b');
  b.st.setLive('cursor', { x: 1 });
  a.st.setLive('cursor', { x: 2 });
  n.flush();
  n.join(a, b);
  n.flush();
  const state = 100 * 1002;
  if (read(b, 'k99') !== 'v'.repeat(1000) || read(a, 'mine') !== 'b') fail('the join did not catch up both ways');
  if (b.st.getLive('cursor')[a.self]?.x !== 2 || a.st.getLive('cursor')[b.self]?.x !== 1) fail('per-peer values on the join');
  if (n.values(a, b) !== 100 || n.values(b, a) !== 1) fail(`values sent: ${n.values(a, b)} and ${n.values(b, a)}, not 100 and 1`);
  const ratio = n.bytes(a, b) / state;
  // Each value once, with its version (a clock and a 64-hex writer id) in the digest and again beside it.
  if (ratio > 1.25) fail(`a sent ${n.bytes(a, b)} B for ${state} B of state (${ratio.toFixed(2)}x)`);
  console.log(`RH-4: a join sent ${n.bytes(a, b)} B for ${state} B of shared state (${ratio.toFixed(2)}x), each value once`);

  // The first message a sends the newcomer is lost (room.js lists it before our presence goes): both still
  // catch up, from the other's ask and the ask back. With per-peer values alone, nothing in a digest says
  // what is missing: the answer asks back because the other hasn't answered.
  for (const [losing, shared] of [['a', true], ['c', true], ['a', false], ['c', false]]) {
    const m = net();
    const [p, q] = [m.node('a'), m.node('c')];
    if (shared) p.st.setShared('from-a', 1);
    if (shared) q.st.setShared('from-c', 2);
    p.st.setLive('cursor', 'a');
    q.st.setLive('cursor', 'c');
    m.flush();
    let first = true;
    m.lose = (f) => f === id(losing) && first && !(first = false);
    m.join(p, q);
    m.flush();
    if (first) fail('nothing was lost');
    if (shared && (read(p, 'from-c') !== 2 || read(q, 'from-a') !== 1)) fail(`lost first message from ${losing}: shared state`);
    if (p.st.getLive('cursor')[q.self] !== 'c' || q.st.getLive('cursor')[p.self] !== 'a') fail(`lost first message from ${losing}: per-peer`);
  }
  console.log("RH-4: a newcomer's lost first message, either way, with and without shared state: both catch up");
}

// RH-4: a member that asks over and over gets each value once, and at most 4 answers.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  for (let i = 0; i < 100; i++) a.st.setShared(`k${i}`, 'v'.repeat(1000));
  n.join(a, x);
  n.flush();
  x.got.length = 0;
  for (let i = 0; i < 20; i++) n.from(x, a, { $peerwire: 'sync', ask: true, h: [['wanted', Date.now(), x.self]] });
  n.flush();
  const answers = x.got.filter((m) => m.$peerwire === 'sync' && m.ask === true).length;
  if (n.values(a, x) !== 100 || answers > 4) fail(`20 asks: ${n.values(a, x)} values in ${answers} answers`);
  console.log(`RH-4: 20 asks: 100 values sent once, ${answers} answers`);
}

// Catch-up messages are cut to CHUNK_BYTES, so none reaches room.send's 1 MiB even at 3 bytes a character:
// with room.js queueing an oversize message and dropping it unthrown (OUT-1), a joiner still gets every value.
{
  const n = net();
  n.outbox = true;
  const [a, b] = [n.node('a'), n.node('b')];
  const fit = MAX_STORE_LENGTH / MAX_VALUE_LENGTH;
  const wide = '\u20ac'.repeat(MAX_VALUE_LENGTH - 2); // MAX_VALUE_LENGTH as JSON, 3 bytes a character
  for (let i = 0; i < fit; i++) a.st.setShared(`w${i}`, wide);
  n.join(a, b);
  n.flush();
  const got = Array.from({ length: fit }, (_, i) => read(b, `w${i}`) === wide).filter(Boolean).length;
  if (got !== fit || n.largest > 512 * 1024 + 1024) fail(`chunk bytes: the joiner got ${got} of ${fit} values; largest message ${n.largest} B`);
  console.log(`chunks: ${fit} values of ${MAX_VALUE_LENGTH} three-byte characters all arrive, largest message ${n.largest} B`);
}

// N1: an asker whose digest holds a newer version of a key doesn't get our older value.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  a.st.setShared('doc', 'old');
  a.st.setShared('other', 1);
  n.join(a, x);
  n.flush();
  x.got.length = 0;
  n.from(x, a, { $peerwire: 'sync', ask: true, h: [['doc', Date.now() + 1000, x.self]] });
  n.flush();
  const sent = x.got.filter((m) => m.$peerwire === 'sync').flatMap((m) => m.s ?? []).map((e) => e[0]);
  if (sent.includes('doc') || !sent.includes('other')) fail(`N1: the answer carried ${JSON.stringify(sent)}`);
  console.log("N1: a key the asker holds newer is left out of the answer");
}

// Writes that cross: both members hold different versions of the same keys, and settle on the same ones.
// A member whose store starts late catches up when it starts.
{
  const n = net();
  const [a, b] = [n.node('a'), n.node('b', { start: false })];
  a.st.setShared('k', 'a');
  b.st.setShared('k', 'b');
  b.st.setShared('j', 'b');
  for (const t = Date.now() + 2; Date.now() < t; );
  a.st.setShared('j', 'a');
  n.flush();
  n.join(a, b);
  n.flush();
  if (read(a, 'k') !== 'a' || read(b, 'k') !== 'b') fail('a store that has not started answered');
  b.st.start();
  n.flush();
  for (const k of ['k', 'j']) if (read(a, k) !== read(b, k)) fail(`${k}: ${read(a, k)} and ${read(b, k)}`);
  if (read(a, 'j') !== 'a') fail('the later write did not win');
  console.log('crossing writes settle alike; a store that starts late catches up');
}

// Malformed catch-ups are ignored whole or entry by entry.
{
  const n = net();
  const [a, x] = [n.node('a'), n.raw('b')];
  n.join(a, x);
  const now = Date.now();
  for (const m of [
    { $peerwire: 'sync', s: 'nope' },
    { $peerwire: 'sync', s: null },
    { $peerwire: 'sync', h: new Array(MAX_KEYS + 1).fill(['k', now, x.self]) },
    { $peerwire: 'sync', s: [[1, 2, 3], null, ['k', 1, now, 'not a peer id'], ['k', 1, 'x', x.self], ['k', 1, now + MAX_CLOCK_DRIFT * 2, x.self]] },
    { $peerwire: 'sync', h: [null, ['k'], ['k', 'x', x.self], [{}, now, x.self]], p: [null, [1], ['cursor', { x: 9 }, 0]] },
  ]) {
    n.from(x, a, m);
  }
  n.flush();
  if (read(a, 'k') !== undefined || a.st.getLive('cursor')[x.self] !== undefined) fail('a malformed entry took');
  n.from(x, a, { $peerwire: 'sync', s: [['k', 1, now, x.self]], p: [['cursor', { x: 9 }, 1]] });
  n.flush();
  if (read(a, 'k') !== 1 || a.st.getLive('cursor')[x.self]?.x !== 9) fail('a valid catch-up after malformed ones');
  console.log('malformed catch-ups ignored');
}

// NB-054: a member's value nested past MAX_VALUE_DEPTH (64) is ignored before anything stringifies it, so it throws
// nowhere: Node 22's JSON.stringify recurses and throws a few thousand levels down, and JSON.parse takes 100000. In
// a catch-up, the entries after it still take and the ask is answered; an entry whose listener throws costs none
// of the others either. Ours past it throws. JSON.stringify throws past 4000 levels here, as Node 22's does, so
// this bites on engines whose JSON.stringify doesn't recurse.
{
  if (MAX_VALUE_DEPTH !== 64) fail(`MAX_VALUE_DEPTH is ${MAX_VALUE_DEPTH}, not 64 as doc.js has it`);
  // JSON text of `depth` objects and arrays, alternating (an array outermost with `array`), nested around 0.
  const nest = (depth, array = false) => {
    const open = [];
    const close = [];
    for (let i = +array; i < depth + array; i++) {
      open.push(i % 2 ? '[' : '{"k":');
      close.push(i % 2 ? ']' : '}');
    }
    return `${open.join('')}0${close.reverse().join('')}`;
  };
  const stringify = JSON.stringify;
  JSON.stringify = function (v, ...rest) {
    for (const stack = [[v, 1]]; stack.length; ) {
      const [o, depth] = stack.pop();
      if (o === null || typeof o !== 'object') continue;
      if (depth > 4000) throw new RangeError('Maximum call stack size exceeded');
      for (const k in o) stack.push([o[k], depth + 1]);
    }
    return stringify.call(this, v, ...rest);
  };
  try {
    if (!throws(() => JSON.stringify(JSON.parse(nest(4001))))) fail('the JSON.stringify stub let a value 4001 deep through');
    const n = net();
    const [a, x] = [n.node('a'), n.raw('b')];
    const now = Date.now();
    const deep = nest(100000);
    n.from(x, a, `{"$peerwire":"set","k":"deep","v":${deep},"c":${now}}`);
    n.from(x, a, { $peerwire: 'set', k: 'after', v: 1, c: now });
    n.from(x, a, `{"$peerwire":"live","k":"deep","v":${deep},"n":1}`);
    n.from(x, a, { $peerwire: 'live', k: 'after', v: 1, n: 2 });
    let e = throws(() => n.flush());
    if (e) fail(`a member's set or live value 100000 deep threw: ${e}`);
    if (read(a, 'deep') !== undefined || a.st.getLive('deep')[x.self] !== undefined) fail('a set or live value 100000 deep was kept');
    if (read(a, 'after') !== 1 || a.st.getLive('after')[x.self] !== 1) fail('the set or live after a value 100000 deep was lost');
    // A value 65 deep down a middle branch, beside shallow ones: every branch is walked. One a level shorter is
    // kept: the bound is on depth, not on the containers walked.
    n.from(x, a, `{"$peerwire":"set","k":"mid","v":[[],${nest(64, true)},[]],"c":${now}}`);
    n.from(x, a, `{"$peerwire":"set","k":"midobj","v":{"a":{},"b":${nest(64)},"c":{}},"c":${now}}`);
    n.from(x, a, `{"$peerwire":"set","k":"mid64","v":[[],${nest(63, true)},[]],"c":${now}}`);
    n.from(x, a, `{"$peerwire":"set","k":"midobj64","v":{"a":{},"b":${nest(63)},"c":{}},"c":${now}}`);
    n.flush();
    if (read(a, 'mid') !== undefined || read(a, 'midobj') !== undefined) fail('a value 65 deep in a middle branch was kept');
    if (read(a, 'mid64') === undefined || read(a, 'midobj64') === undefined) fail('a value 64 deep in a middle branch was ignored');

    const s = (k, v, w = x.self) => `["${k}",${v},${now},"${w}"]`;
    n.from(x, a, `{"$peerwire":"sync","ask":true,"s":[${s('sdeep', deep)},${s('safter', 2)}],"p":[["pdeep",${deep},3],["pafter",2,4]]}`);
    e = throws(() => n.flush());
    if (e) fail(`a catch-up with values 100000 deep threw: ${e}`);
    if (read(a, 'sdeep') !== undefined || a.st.getLive('pdeep')[x.self] !== undefined) fail('a catch-up value 100000 deep was kept');
    if (read(a, 'safter') !== 2 || a.st.getLive('pafter')[x.self] !== 2) fail('the entries after a value 100000 deep in a catch-up were lost');
    if (!x.got.some((m) => m.$peerwire === 'sync' && m.re === true)) fail('a catch-up with a value 100000 deep went unanswered');

    // The bound, from both sides, for shared and per-peer values, a member's and ours, an object or an array
    // outermost.
    let seq = 10;
    for (const array of [false, true]) {
      for (const depth of [64, 65]) {
        n.from(x, a, `{"$peerwire":"set","k":"d${depth}${array}","v":${nest(depth, array)},"c":${now}}`);
        n.from(x, a, `{"$peerwire":"live","k":"d${depth}${array}","v":${nest(depth, array)},"n":${seq++}}`);
      }
      n.flush();
      const k = `d64${array}`;
      if (read(a, k) === undefined || a.st.getLive(k)[x.self] === undefined) fail(`a member's value 64 deep was ignored (${k})`);
      if (!Array.isArray(read(a, k)) !== !array) fail(`${k} is not the value sent`);
      if (read(a, `d65${array}`) !== undefined || a.st.getLive(`d65${array}`)[x.self] !== undefined) fail(`a member's value 65 deep was kept (d65${array})`);
      for (const set of [(k, v) => a.st.setShared(k, v), (k, v) => a.st.setLive(k, v)]) {
        if (throws(() => set('mine64', JSON.parse(nest(64, array))))) fail('setting a value 64 deep threw');
        if (!(throws(() => set('mine65', JSON.parse(nest(65, array)))) instanceof RangeError)) fail(`setting a value 65 deep did not throw a RangeError (array outermost: ${array})`);
      }
    }
    if (read(a, 'mine64') === undefined || read(a, 'mine65') !== undefined) fail('setShared kept the wrong value');

    // A listener that throws for one entry: the entries after it take and the ask is answered, then its error is
    // thrown.
    const off = a.st.subscribe('s:boom', () => {
      throw new Error('boom');
    });
    const poff = a.st.subscribe('p:pboom', () => {
      throw new Error('pboom');
    });
    const answers = x.got.length;
    n.from(x, a, { $peerwire: 'sync', ask: true, s: [['boom', 1, now + 1, x.self], ['next', 3, now + 1, x.self]], p: [['pboom', 1, 8], ['plive', 3, 9]] });
    e = throws(() => n.flush());
    n.flush();
    off();
    poff();
    if (e?.message !== 'boom') fail(`a listener's error: ${e}`);
    if (read(a, 'boom') !== 1 || read(a, 'next') !== 3 || a.st.getLive('pboom')[x.self] !== 1 || a.st.getLive('plive')[x.self] !== 3) fail("the entries after one whose listener threw were lost");
    const answer = x.got.slice(answers).filter((m) => m.$peerwire === 'sync' && m.re === true);
    if (!answer.length) fail('a catch-up whose listener threw went unanswered');
    if (answer.some((m) => m.s?.some(([k]) => k === 'boom' || k === 'next'))) fail('the answer sent back values the asker had just sent');
  } finally {
    JSON.stringify = stringify;
  }

  // NB-056: the walk takes own keys, as JSON.stringify does. With an object on Object.prototype, enumerable as a
  // page's assignment makes it, every object would nest past the bound through it: members' objects and arrays are
  // kept all the same, ours set, and 65 deep still dropped or thrown. Nulls inside are passed over, not walked.
  Object.prototype.polluted = {};
  try {
    const n = net();
    const [a, x] = [n.node('a'), n.raw('b')];
    const now = Date.now();
    n.from(x, a, `{"$peerwire":"set","k":"obj","v":{"a":{"b":[null]},"n":null},"c":${now}}`);
    n.from(x, a, `{"$peerwire":"set","k":"arr","v":[],"c":${now}}`);
    n.from(x, a, `{"$peerwire":"live","k":"obj","v":{},"n":1}`);
    for (const array of [false, true]) {
      for (const depth of [64, 65]) n.from(x, a, `{"$peerwire":"set","k":"d${depth}${array}","v":${nest(depth, array)},"c":${now}}`);
    }
    let e = throws(() => n.flush());
    if (e) fail(`a member's value threw with Object.prototype.polluted: ${e}`);
    if (JSON.stringify(read(a, 'obj')) !== '{"a":{"b":[null]},"n":null}' || JSON.stringify(read(a, 'arr')) !== '[]') fail("a member's object or array was ignored with Object.prototype.polluted");
    if (JSON.stringify(a.st.getLive('obj')[x.self]) !== '{}') fail("a member's per-peer object was ignored with Object.prototype.polluted");
    if (read(a, 'd64false') === undefined || read(a, 'd64true') === undefined) fail("a member's value 64 deep was ignored with Object.prototype.polluted");
    if (read(a, 'd65false') !== undefined || read(a, 'd65true') !== undefined) fail("a member's value 65 deep was kept with Object.prototype.polluted");
    e = throws(() => (a.st.setShared('mine', { a: [null] }), a.st.setLive('mine', { n: null })));
    if (e) fail(`setting an object threw with Object.prototype.polluted: ${e}`);
    if (!(throws(() => a.st.setShared('mine65', JSON.parse(nest(65)))) instanceof RangeError)) fail('setting a value 65 deep did not throw with Object.prototype.polluted');
  } finally {
    delete Object.prototype.polluted;
  }
  console.log(`values nested past ${MAX_VALUE_DEPTH} ignored, the rest of a catch-up kept, ours past it throw; own keys only`);
}

console.log('ok');
