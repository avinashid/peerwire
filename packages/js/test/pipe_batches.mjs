// The batches a pipe (src/webrtc.js) sends over signalling, on a fake clock: datagrams queued in a moment go in one
// batch after PIPE_FLUSH_MS, and a batch's worth goes at once; all pipes keep to PIPE_RATE batches a second, burst
// PIPE_BURST. A background tab fires timers once a second at most: a timer that fires that late finds the batches
// of the time it missed and sends them together, and until a timer fires on time again, a pipe sends what a task
// queued at its end rather than PIPE_FLUSH_MS later (NB-006e). Signalling that paces its sends (NostrSignaling's
// wait()) holds a batch while it can take none, and the batch fills meanwhile. Batches go lossy (NostrSignaling sends
// each through one relay), but not a last one at close(), which nothing can send again.
//
//   node packages/js/test/pipe_batches.mjs
import { PIPE_BATCH, PIPE_BURST, PIPE_FLUSH_MS, PIPE_RATE, WebRtcTransport } from '../src/webrtc.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};

// The clock: timers fire as `advance` passes their time, or `clamp` ms after they were set at the soonest.
let now = 1e12;
let clamp = 0;
let timers = [];
globalThis.setTimeout = (fn, ms = 0) => {
  const t = { at: now + Math.max(ms, clamp), fn };
  timers.push(t);
  return t;
};
globalThis.clearTimeout = (t) => {
  if (t) t.fn = null;
};
Date.now = () => now;
performance.now = () => now;
const tick = () => new Promise((r) => setImmediate(r)); // the end of a task, which no clock holds back
async function advance(ms) {
  const end = now + ms;
  for (;;) {
    timers = timers.filter((t) => t.fn);
    const t = timers.reduce((a, b) => (!a || b.at < a.at ? b : a), null);
    if (!t || t.at > end) break;
    now = Math.max(now, t.at);
    const fn = t.fn;
    t.fn = null;
    fn();
    await tick();
  }
  now = end;
  await tick();
}

const out = []; // [time, datagrams, bytes, lossy]
let free = 0; // when the signalling can take a message again
const sig = { send: (to, m, o) => m.kind === 'dg' && out.push([now, m.d.length, m.d.reduce((n, s) => n + Math.floor((s.length * 3) / 4), 0), o?.lossy]), wait: () => Math.max(0, free - now), onMessage() {}, close() {} };
const ep = { connect() {}, receive() {}, setRelayOnly() {}, isConnected: () => false };
const t = new WebRtcTransport(ep, sig, { pipes: true });
const h = t.pipe('ab'.repeat(32));
const send = (n, bytes = 1000) => {
  for (let i = 0; i < n; i++) ep.onTransmit(h, new Uint8Array(bytes));
};
const batches = (from) => out.slice(from).map(([, n]) => n).join();
const per = Math.floor(PIPE_BATCH / 1000); // datagrams of 1000 bytes in a batch
const idle = () => advance(10000); // the rate budget whole again

// Datagrams queued in a moment go in one batch, PIPE_FLUSH_MS later.
send(1, 100);
send(2, 100);
await advance(PIPE_FLUSH_MS - 1);
if (out.length) fail(`datagrams went before ${PIPE_FLUSH_MS} ms: ${batches(0)}`);
await advance(1);
if (batches(0) !== '3') fail(`three datagrams went as ${batches(0)}`);

// A batch's worth goes at once; the rest PIPE_FLUSH_MS later.
await idle();
let from = out.length;
send(per + 1);
if (batches(from) !== `${per}`) fail(`a batch's worth queued went as ${batches(from) || 'nothing'} at once`);
await advance(PIPE_FLUSH_MS);
if (batches(from) !== `${per},1`) fail(`then ${batches(from)}`);
console.log(`batches: datagrams queued together go ${PIPE_FLUSH_MS} ms later as one, and a batch's worth at once`);

// The rate: PIPE_BURST batches at once, then PIPE_RATE a second.
await idle();
from = out.length;
const at = now;
send(per * PIPE_BURST + 2 * per);
if (out.length - from !== PIPE_BURST) fail(`${out.length - from} batches at once, burst ${PIPE_BURST}`);
await advance(10000);
const sent = out.slice(from).filter(([time]) => time - at <= 2000).length;
if (sent > PIPE_BURST + 2 * PIPE_RATE) fail(`${sent} batches in 2 s, at most ${PIPE_BURST + 2 * PIPE_RATE}`);
console.log(`rate: ${PIPE_BURST} batches at once, ${sent} in 2 s`);

// A background tab: timers fire 1000 ms after they were set at the soonest. The budget spent, the wait for it
// fires a second on, and the batches of that second go together, PIPE_RATE of them.
clamp = 1000;
await idle();
from = out.length;
send(per * PIPE_BURST + PIPE_RATE * per);
await advance(999);
if (out.length - from !== PIPE_BURST) fail(`a background tab sent ${out.length - from} batches before its timer`);
await advance(1);
for (let i = 0; i < PIPE_RATE; i++) await tick(); // late timers: a batch a task, back to back
const late = out.slice(from + PIPE_BURST);
if (late.length !== PIPE_RATE || late.some(([time]) => time !== late[0][0])) fail(`its timer a second late sent ${late.map(([time, n]) => `${n} at ${time - at}`).join(', ')}`);
// Its timers are late now: a datagram goes at the end of the task, as the budget allows, not a second later.
await idle();
from = out.length;
send(1, 100);
await tick();
if (batches(from) !== '1') fail(`with late timers, a datagram went as ${batches(from) || 'nothing'} by the end of the task`);
console.log(`background: a timer a second late sent ${late.length} batches together; then a datagram went at the end of its task`);

// Timers on time again: once one fires on time, datagrams wait PIPE_FLUSH_MS again to be batched.
clamp = 0;
send(per * (PIPE_BURST + 2)); // past the budget in whole batches: the wait for it is a timer, on time
await advance(1000);
await idle();
from = out.length;
send(1, 100);
await tick();
if (out.length !== from) fail(`timers on time again, a datagram went at the end of its task`);
await advance(PIPE_FLUSH_MS);
if (batches(from) !== '1') fail(`then ${batches(from)}`);
console.log(`on time again: datagrams wait ${PIPE_FLUSH_MS} ms to be batched`);

// Signalling that can take nothing for 500 ms: the batch waits for it, gathering what is queued meanwhile, and
// goes whole once it can, not in small batches that would wait there.
await idle();
from = out.length;
free = now + 500;
send(2, 100);
await advance(PIPE_FLUSH_MS);
send(3, 100);
await advance(499 - PIPE_FLUSH_MS);
if (out.length !== from) fail(`with signalling busy for 500 ms, ${batches(from)} went before it could take one`);
await advance(1);
if (batches(from) !== '5') fail(`once it could, ${batches(from) || 'nothing'} went, not one batch of 5`);
free = 0;
console.log('paced signalling: a batch waits while the signalling can take none, then goes whole');

// Signalling that says 30 s but comes free sooner (a relay opened, a hold was cut short): the pipe asks again at
// least once a second, so the batch goes within a second of that, not 30 s later.
await idle();
from = out.length;
free = now + 30000;
send(2, 100);
await advance(PIPE_FLUSH_MS);
free = 0;
await advance(1000);
if (batches(from) !== '2') fail(`with signalling free again early, ${batches(from) || 'nothing'} went within a second, not one batch of 2`);
console.log('paced signalling: the pipe asks the signalling again at least once a second');

// The pipes' budget runs on a monotonic clock (NB6H-1): the wall clock set back 60 s doesn't stall them.
await idle();
await advance(2000);
from = out.length;
Date.now = () => now - 60000;
send(2, 100);
await advance(PIPE_FLUSH_MS);
Date.now = () => now;
if (batches(from) !== '2') fail(`with the wall clock set back 60 s, ${batches(from) || 'nothing'} went, not one batch of 2`);
console.log('wall clock set back: the pipes still send');

// Every batch so far went lossy; a last one at close() doesn't.
if (out.some(([, , , lossy]) => lossy !== true)) fail(`${out.filter(([, , , lossy]) => lossy !== true).length} of ${out.length} batches went without lossy`);
from = out.length;
send(1, 100);
t.close();
if (batches(from) !== '1' || out.at(-1)[3] !== false) fail(`close() sent ${batches(from) || 'nothing'}, lossy ${out.at(-1)?.[3]}`);
console.log(`lossy: ${from} batches went lossy, the last at close() not`);
console.log('pipe batches: all passed');
process.exit(0);
