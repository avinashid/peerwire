// PERF-4: a Room send larger than the reliable send window (4 MiB) waits in the
// room's outbox. The outbox is flushed when the endpoint handles incoming datagrams
// (ACKs free window: Endpoint.onDrain), with a 2 ms timer only as the fallback.
// Sends 6 MiB in one go between two rooms in this process (BroadcastChannel and
// LocalTransport), checks every message arrives intact and in order, and that the
// drain hook, not the timer, moved the frames. Then the fallback alone (no hook)
// must still deliver another 6 MiB.
// OUT-1: a message over its lane's limit throws RangeError even while the outbox holds
// frames (the core refused it only later, in flush, and the frame was lost unseen), a
// message of exactly the limit is queued and arrives, and a broadcast throws before any
// peer's outbox takes the frame. A lane other than 0, 1 or 2 throws TypeError (OUT-1a).
// A presence over the limit is refused before it is stored (OUT-1b): setPresence keeps
// the old one, joinRoom rejects, and a room that joins later still lists us both ways.
// So is one JSON gives no text for (OUT-1c): undefined, a function or a Symbol went out
// as an empty frame, which no peer could parse, so a room joining later never listed us.
//
//   node packages/js/test/room_drain.mjs
import { joinRoom } from '../src/index.js';

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (cond, ms) => {
  for (const t = Date.now(); !cond() && Date.now() - t < ms;) await sleep(10);
  return cond();
};

const opts = { appId: 'peerwire-test', roomId: `drain-${process.pid}` };
const [a, b] = await Promise.all([joinRoom(opts), joinRoom(opts)]);
check(await until(() => a.peers.length === 1 && b.peers.length === 1, 10000), 'two rooms joined');

// Frames each flush moved out of a's outbox, by who asked for the flush.
const moved = { drain: 0, timer: 0 };
let timerLeft = 0; // drain flushes that emptied the outbox but left the fallback timer armed
const queued = () => [...a.outbox.values()].reduce((n, q) => n + q.length, 0);
const flush = a.flush.bind(a);
a.flush = (from) => {
  const before = queued();
  flush(from);
  moved[from] += before - queued();
  if (from === 'drain' && before && !queued() && a.flushTimer) timerLeft++;
};

const got = [];
b.on('message', (data) => got.push(data));
const SIZE = 128 << 10;
const COUNT = 48; // 6 MiB, over the 4 MiB send window
const msg = (i) => new Uint8Array(SIZE).fill(i & 0xff).fill(0xa5, 0, 4);

async function burst(first, what) {
  got.length = 0;
  const t0 = performance.now();
  for (let i = 0; i < COUNT; i++) a.send(msg(first + i));
  const waited = queued();
  const done = await until(() => got.length === COUNT, 60000);
  const ms = performance.now() - t0;
  check(waited > 0, `${what}: the send window filled, ${waited} of ${COUNT} messages waited in the outbox`);
  check(done && got.every((d, i) => d.length === SIZE && d[4] === ((first + i) & 0xff) && d[SIZE - 1] === ((first + i) & 0xff)),
    `${what}: ${got.length} of ${COUNT} messages (${(COUNT * SIZE) / 2 ** 20} MiB) arrived intact and in order in ${ms.toFixed(0)} ms`);
  check(a.outbox.size === 0 && !a.flushTimer, `${what}: the outbox is empty and the fallback timer stopped`);
  return waited;
}

const waited = await burst(0, 'drain hook');
check(moved.drain === waited && moved.timer === 0, `the drain hook moved the waiting frames (drain ${moved.drain}, timer ${moved.timer})`);
check(timerLeft === 0, 'the drain that emptied the outbox stopped the fallback timer');

// Without the hook the 2 ms timer still empties the outbox, just later.
moved.drain = moved.timer = 0;
const onDrain = a.endpoint.onDrain;
a.endpoint.onDrain = null;
const waited2 = await burst(100, 'timer fallback');
check(moved.timer === waited2 && moved.drain === 0, `with no drain hook the timer moved them (drain ${moved.drain}, timer ${moved.timer})`);

// OUT-1. The limits are the core's (Config::max_message, MAX_URGENT, MAX_UNRELIABLE) on
// the encoded frame, a tag byte and the data: `max` bytes of data are one byte over.
// Every frame the core refuses is counted, as flush() used to drop those unseen.
a.endpoint.onDrain = onDrain;
const refused = [];
const sendRetry = a.endpoint.sendRetry.bind(a.endpoint);
a.endpoint.sendRetry = (peer, f, lane) => {
  const r = sendRetry(peer, f, lane);
  if (r === -2) refused.push(f.length);
  return r;
};
const c = await joinRoom(opts);
check(await until(() => a.peers.length === 2 && c.peers.length === 2, 10000), 'a third room joined');
const tried = (fn) => {
  try {
    fn();
    return 'no throw';
  } catch (e) {
    return `${e.name}: ${e.message}`;
  }
};
const LIMITS = { reliable: 1 << 20, urgent: 64 << 10, unreliable: 1100 };
got.length = 0;
for (let i = 0; i < 40; i++) a.send(msg(i), { to: b.self });
const held = queued();
check(held > 0 && !a.outbox.has(c.self), `5 MiB to b: ${held} frames wait in b's outbox, none in c's`);
for (const [lane, max] of Object.entries(LIMITS)) {
  const n = queued();
  const r = tried(() => a.send(new Uint8Array(max), { to: b.self, lane }));
  check(r.startsWith('RangeError') && queued() === n, `${lane}: ${max} B with ${n} frames queued -> ${r}; not queued`);
  const fits = tried(() => a.send(new Uint8Array(max - 1).fill(9), { to: b.self, lane }));
  check(fits === 'no throw' && queued() === n + 1, `${lane}: ${max - 1} B, the lane's limit -> ${fits}; queued`);
}
const n = queued();
const r = tried(() => a.send(new Uint8Array(1 << 20)));
check(a.peers[0] === b.self && r.startsWith('RangeError') && queued() === n && !a.outbox.has(c.self),
  `a broadcast of 1 MiB to b (frames queued), then c (none) -> ${r}; queued for neither`);
a.send(Uint8Array.of(7), { to: b.self });
const sizes = Object.values(LIMITS).map((max) => max - 1);
const arrived = await until(() => got.length === 40 + sizes.length + 1, 60000);
await sleep(200);
check(arrived && got.length === 44 && sizes.every((s) => got.some((d) => d.length === s && d.every((x) => x === 9))) &&
  got.some((d) => d.length === 1), `b got ${got.length} of 44 messages: the 40, each lane's limit intact, and the 1 B sent last`);
check(refused.length === 0 && a.outbox.size === 0, `the core refused no queued frame (refused: ${refused.join(', ') || 'none'})`);

// OUT-1a: only lanes 0, 1 and 2 (or their names); the core would send any other number
// as unreliable, past MAX_FRAME, and a name off Object.prototype would be read as one.
const before = got.length;
const odd = [3, -1, NaN, 1.5, 2 ** 32, 'toString'].map((lane) => tried(() => a.send(new Uint8Array(1200), { to: b.self, lane })));
await sleep(200);
check(odd.every((r) => r.startsWith('TypeError')) && queued() === 0 && got.length === before,
  `lanes 3, -1, NaN, 1.5, 2**32 and 'toString' -> ${[...new Set(odd.map((r) => r.split(':')[0]))].join(', ')}; nothing queued or sent`);

// OUT-1b: a presence that can't go out is refused before it is stored.
const unhandled = [];
process.on('unhandledRejection', (e) => unhandled.push(String(e)));
const huge = { blob: 'x'.repeat(1 << 20) };
const mine = a.getPresence();
const set = [huge, { n: 1n }].map((p) => tried(() => a.setPresence(p)));
check(set[0].startsWith('RangeError') && set[1].startsWith('TypeError') && a.getPresence() === mine,
  `setPresence of 1 MiB -> ${set[0]}; of a BigInt -> ${set[1].split(':')[0]}; getPresence() unchanged`);
const join = await joinRoom({ ...opts, presence: huge }).then((r) => (r.leave(), 'joined'), (e) => `${e.name}: ${e.message}`);
check(join.startsWith('RangeError'), `joinRoom({ presence: 1 MiB }) -> ${join}`);
const empty = [undefined, () => 0, Symbol('p')].map((p) => tried(() => a.setPresence(p)));
check(empty.every((r) => r.startsWith('TypeError')) && a.getPresence() === mine,
  `setPresence of undefined, a function, a Symbol -> ${[...new Set(empty.map((r) => r.split(':')[0]))].join(', ')}; getPresence() unchanged`);
const asNull = tried(() => a.setPresence(null));
check(asNull === 'no throw' && a.getPresence() === null, `setPresence(null) -> ${asNull}`);
a.setPresence(mine);
const joinFn = await joinRoom({ ...opts, presence: () => 0 }).then((r) => (r.leave(), 'joined'), (e) => `${e.name}: ${e.message}`);
check(joinFn.startsWith('TypeError'), `joinRoom({ presence: () => 0 }) -> ${joinFn}`);
const d = await joinRoom({ ...opts, presence: undefined });
const listed = await until(() => a.peers.includes(d.self) && d.peers.includes(a.self), 15000);
check(listed && !unhandled.length, `a room joined later: a and it list each other (${listed}); unhandled rejections: ${unhandled.join('; ') || 'none'}`);
const seen = [JSON.stringify(d.getPresence(a.self)), JSON.stringify(a.getPresence(d.self))];
check(seen[0] === JSON.stringify(mine) && seen[1] === '{}', `it sees a's presence ${seen[0]}; a sees its presence: undefined gave the default ${seen[1]}`);

await Promise.all([a.leave(), b.leave(), c.leave(), d.leave()]);
console.log(failed ? `${failed} FAILED` : 'room drain: all passed');
process.exit(failed ? 1 : 0);
