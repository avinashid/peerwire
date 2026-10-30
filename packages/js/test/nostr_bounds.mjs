// PERF-2: Nostr signalling stays bounded under a flood. Anyone who knows the room
// name can publish events, so the de-duplication set must not grow without limit,
// and base64 must handle bodies far larger than a signalling message. No relays:
// events are fed straight into onRelay.
import { loadPeerwire } from '../src/peerwire.js';
import { BACKOFF_MS, MAX_BACKOFF_MS, RECONNECT_MS, NostrSignaling, MAX_SEEN, b64, unb64, RELAY_RATE, RELAY_BURST_S, RELAY_RAISE_MS, MIN_RELAY_RATE, HOLD_MS, RELAY_TRIES, RELAY_QUEUE, RELAY_QUEUE_MS, SILENT_MS, STORE_EVERY_MS, STORE_QUEUE_MS, STORE_OK_MS, QUERY_MS } from '../src/nostr.js';

const wasm = process.argv[2] ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname;
const pw = await loadPeerwire(wasm);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};

const other = pw.endpoint();
const sig = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/flood', relays: [] });
await sig.ready;
const hellos = [];
sig.onHello = (from) => hellos.push(from);
// Presence is encrypted to the room (SEC-2): build the hello the way hello() does.
const hello = await sig.sealRoom({ t: 'hello', from: other.id });
// NIP-01 ids are 64 lowercase hex characters.
const hid = (i) => i.toString(16).padStart(64, '0');
const feed = (id, extra = {}) =>
  sig.onRelay('test', JSON.stringify(['EVENT', 's', { id, kind: 25059, pubkey: 'x', tags: [['p', sig.roomTag]], content: 'e30=', ...extra }]));

// 1. A flood of distinct ids keeps the set at the cap, forgetting the oldest first.
const N = 12000;
let peak = 0;
for (let i = 0; i < N; i++) {
  feed(hid(i));
  peak = Math.max(peak, sig.seen.size);
}
check(MAX_SEEN === 4096, `MAX_SEEN is 4096`);
check(peak <= MAX_SEEN && sig.seen.size === MAX_SEEN, `${N} distinct events: seen.size peaked at ${peak} (cap ${MAX_SEEN})`);
check(!sig.seen.has(hid(0)) && sig.seen.has(hid(N - 1)) && sig.seen.has(hid(N - MAX_SEEN)), 'the oldest ids are evicted, the newest kept');
check(sig.stats.received === N, `every distinct event was counted once (${sig.stats.received})`);

// 2. A recent duplicate is still dropped; a hello still gets through after the flood.
feed(hid(N - 1));
check(sig.stats.received === N, 'a recent duplicate id is still dropped');
await feed(hid(1e9), { content: hello });
await feed(hid(1e9), { content: hello });
check(hellos.length === 1 && hellos[0] === other.id, 'a hello after the flood is delivered once');

// 2b. Security SEC-2: the published hello body does not carry the PeerId in the clear
// (the old plaintext format did), and a body sealed to another room's key is not read.
const bodyText = new TextDecoder('latin1').decode(unb64(hello));
check(!hello.includes(other.id) && !bodyText.includes(other.id) && !bodyText.includes('hello'),
  'the hello body is ciphertext: neither the long-term PeerId nor "hello" appears in it');
const foreign = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/other-room', relays: [] });
await foreign.ready;
const otherRoomHello = await foreign.sealRoom({ t: 'hello', from: other.id });
const before2b = hellos.length;
await feed(hid(2.1e9), { content: otherRoomHello });
check(hellos.length === before2b, "a hello sealed to another room's key is not delivered");
foreign.close();

// 3. Malformed events neither throw nor enter the set.
const size = sig.seen.size;
const before = sig.stats.received;
let threw = false;
try {
  for (const id of [undefined, null, 7, { a: 1 }, ['x']]) feed(id);
  feed(hid(2e9), { tags: 'p' });
  feed(hid(3e9), { tags: [null, 5, 'p'] });
} catch {
  threw = true;
}
check(!threw && sig.stats.received === before + 2 && sig.seen.size === size, 'non-string ids are ignored; bad tags do not throw');

// 3b. Security PERF2-1: only a NIP-01 id (64 lowercase hex) is remembered, so a relay
// can't pin megabytes of id strings in `seen`.
const bad = ['a'.repeat(64 * 1024), 'ab'.repeat(33), 'ab'.repeat(31), 'AB'.repeat(32), 'zz'.repeat(32), '', `flood1`];
const heap = process.memoryUsage().heapUsed;
for (let i = 0; i < 4096; i++) feed(`${i.toString(16)}`.padStart(64 * 1024, 'f'));
for (const id of bad) feed(id);
check(sig.stats.received === before + 2 && sig.seen.size === size && [...sig.seen].every((id) => id.length === 64),
  `4096 events with 64 KiB ids and ${bad.length} non-hex or wrong-length ids are dropped, never remembered`);
check(process.memoryUsage().heapUsed - heap < 64 << 20, `heap grew ${((process.memoryUsage().heapUsed - heap) / 2 ** 20).toFixed(1)} MiB (unbounded: ~256 MiB)`);

// 3c. NB-014 (found by fuzz.mjs): a relay message that is JSON but not an array
// (`null` threw a TypeError out of the WebSocket handler) is ignored.
let scalarThrew = false;
try {
  for (const raw of ['null', '0', '1', '"EVENT"', 'true', 'false', '{}', '{"0":"EVENT"}']) sig.onRelay('test', raw);
} catch {
  scalarThrew = true;
}
check(!scalarThrew && sig.stats.received === before + 2, 'relay messages that are JSON but not arrays are ignored');
sig.close();

// 4. base64 round-trips a 100 KB and a 1 MB body (spread into fromCharCode fails at that size on some engines).
for (const n of [100 * 1024, 1 << 20]) {
  const bytes = crypto.getRandomValues(new Uint8Array(65536));
  const data = new Uint8Array(n).map((_, i) => bytes[i % bytes.length] ^ (i >> 16));
  const back = unb64(b64(data));
  check(back.length === n && back.every((x, i) => x === data[i]), `base64 round-trip of ${n} bytes`);
}
check(b64(new Uint8Array(0)) === '' && b64(new Uint8Array([0, 255, 128])) === 'AP+A', 'base64 of empty and short inputs');

// 5. Each relay is paced by a token bucket of its own: a burst at once, then RELAY_RATE a second, the
//    rest waiting on its queue. A relay that refuses an event for its rate ('rate-limited:') is held for
//    HOLD_MS, its rate halved, and sent the event again in its place, while the other is still sent to;
//    refusals of events sent before its hold began count for nothing more. Refused again before it takes
//    one, the hold doubles; after it took one, it starts over. A relay isn't held while no other is free.
//    A ban ('blocked:', 'banned') stops a relay for BACKOFF_MS, doubling. A fake monotonic clock keeps time.
{
  let clock = 1e6;
  performance.now = () => clock;
  const s5 = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/backoff', relays: [] });
  await s5.ready;
  const sent = { a: [], b: [] };
  const sock = {};
  for (const name of ['a', 'b']) s5.sockets.push((sock[name] = { relayUrl: name, readyState: WebSocket.OPEN, send: (m) => sent[name].push(JSON.parse(m)[1].id), close() {} }));
  let n5 = 0; // events differ, as sealed ones do: the same content in the same second is the same event
  const ok = (url, id, taken, why = '') => s5.onRelay(url, JSON.stringify(['OK', id, taken, why]));
  const held = (url) => s5.paused.get(url)?.until > clock;
  const ids = (url) => sock[url].q.map((e) => e.id);
  const burst = RELAY_RATE * RELAY_BURST_S;
  for (let i = 0; i < burst + 4; i++) await s5.publish(s5.roomTag, `e${++n5}`);
  check(sent.a.length === burst && sent.b.length === burst && sock.a.q.length === 4, `a relay is sent a burst of ${burst} at once, the rest wait for its rate (${sent.a.length} sent, ${sock.a.q.length} waiting)`);
  check(s5.wait() > 0, `the pipe is told to wait while no relay can take one (${s5.wait().toFixed(0)} ms)`);
  clock += 500;
  s5.pump(sock.a);
  s5.pump(sock.b);
  check(sent.a.length === burst + RELAY_RATE / 2 && sent.b.length === burst + RELAY_RATE / 2, `half a second later, ${RELAY_RATE / 2} more go (${sent.a.length}, ${sent.b.length})`);
  // a refuses the last two it was sent, a moment later: the first refusal holds it, the second was on its way.
  clock += 1;
  const at = clock;
  const [x, y] = sent.a.slice(-2);
  ok('a', y, false, 'rate-limited: slow down');
  ok('a', x, false, 'rate-limited: slow down');
  check(s5.paused.get('a')?.ms === HOLD_MS && held('a') && !held('b') && s5.stats.backoffs === 1 && s5.rates.get('a').rate === RELAY_RATE / 2,
    `a relay that refuses for its rate is held ${HOLD_MS} ms, once (backoffs ${s5.stats.backoffs}), at half its rate (${s5.rates.get('a').rate}); the other isn't held`);
  check(ids('a').slice(0, 2).join() === `${x},${y}` && sock.a.q.length === 4 && s5.stats.resent === 2, 'what it refused goes back first on its queue, in order');
  ok('a', sent.a[0], true); // taken, but sent before the hold: it took none since
  ok('b', sent.b[0], false, 'invalid: bad signature');
  check(!s5.paused.has('b') && !s5.banned.has('b') && sock.b.q.length === 2, 'other refusals hold nothing and are not sent again');
  clock = at + HOLD_MS - 1;
  s5.pump(sock.a);
  check(sent.a.length === burst + RELAY_RATE / 2, 'nothing goes to it while held');
  clock = at + HOLD_MS;
  s5.pump(sock.a);
  check(sent.a.slice(-2).join() === `${x},${y}` && sent.a.length === burst + RELAY_RATE / 2 + 2, `once the hold is over, they go again at its new rate (${sent.a.length - burst - RELAY_RATE / 2} in ${HOLD_MS} ms)`);
  clock += 1;
  const rate2 = (RELAY_RATE / 2 + (clock - at) / RELAY_RAISE_MS) / 2;
  ok('a', y, false, 'rate-limited: still');
  check(s5.paused.get('a').ms === HOLD_MS * 2 && Math.abs(s5.rates.get('a').rate - rate2) < 1e-9 && ids('a')[0] === y,
    `refused again before it took one: the hold doubles, the rate halves again (${s5.rates.get('a').rate.toFixed(2)}), and it is sent again`);
  clock = s5.paused.get('a').until;
  const from = sent.a.length;
  s5.pump(sock.a);
  const w = sent.a.at(-1);
  ok('a', w, true);
  clock += 1;
  ok('a', y, false, 'rate-limited: once more');
  check(sent.a[from] === y && sent.a.length > from + 1 && s5.paused.get('a').ms === HOLD_MS && !ids('a').includes(y) && s5.stats.resent === 3,
    `after it took one it starts over at ${HOLD_MS} ms, and an event refused ${RELAY_TRIES} times is not sent again`);

  // b refuses too while a is held: of the two, the hold that ends first (a's) is cut short.
  clock += 1;
  ok('b', sent.b.at(-1), false, 'rate-limited: b too');
  check(held('b') && !held('a') && s5.stats.backoffs === 4, `with every relay held, the one whose hold ends first goes on (a ${held('a') ? 'held' : 'free'}, b ${held('b') ? 'held' : 'free'})`);
  // a, the one free, closes: b's hold is cut short.
  s5.dial = () => {};
  sock.a.readyState = WebSocket.CLOSED;
  s5.lostSocket(sock.a);
  check(!held('b'), 'the free relay closing frees the held one');
  sock.a.readyState = WebSocket.OPEN;

  // A ban, even one that speaks of a rate limit, stops a relay for BACKOFF_MS, doubling, and drops its queue.
  s5.rate('a', clock).tokens = 0;
  await s5.publish(s5.roomTag, `e${++n5}`);
  const queued = sock.a.q.length;
  ok('b', sent.b.at(-1), false, 'rate-limited: b again');
  ok('a', 'q', false, 'banned: too many rate-limit violations, try again later');
  ok('a', 'q', false, 'blocked: you are banned');
  check(s5.banned.get('a')?.ms === BACKOFF_MS && !held('a') && queued === 1 && sock.a.q.length === 0 && !held('b'),
    `a banned relay is stopped for ${BACKOFF_MS} ms, once, its queue dropped (${queued} waited); the held one is freed, as no other is`);
  const before5 = sent.a.length;
  await s5.publish(s5.roomTag, `e${++n5}`);
  check(sent.a.length === before5 && sock.a.q.length === 0, 'a banned relay is sent nothing and queues nothing');
  s5.banned.get('a').until = clock - 1;
  ok('a', 'r', false, 'blocked: again');
  check(s5.banned.get('a').ms === BACKOFF_MS * 2, 'a second ban doubles');
  s5.banned.get('a').until = clock - MAX_BACKOFF_MS - 1;
  ok('a', 's', false, 'blocked: much later');
  check(s5.banned.get('a').ms === BACKOFF_MS, 'a ban after a long quiet period starts over');

  // Bounds: RELAY_QUEUE events wait at most, for RELAY_QUEUE_MS at most.
  s5.paused.delete('b');
  sock.b.q.length = 0;
  s5.rate('b', clock).tokens = 0;
  const unsent = s5.stats.unsent;
  for (let i = 0; i < RELAY_QUEUE + 3; i++) await s5.publish(s5.roomTag, `e${++n5}`);
  check(sock.b.q.length === RELAY_QUEUE && s5.stats.unsent === unsent + 3, `past ${RELAY_QUEUE} waiting, the oldest is dropped (${sock.b.q.length} waiting)`);
  const sentB = sent.b.length;
  clock += RELAY_QUEUE_MS + 1;
  s5.pump(sock.b);
  check(sock.b.q.length === 0 && sent.b.length === sentB && s5.stats.unsent === unsent + 3 + RELAY_QUEUE, `what waited over ${RELAY_QUEUE_MS} ms is dropped, not sent`);
  const cap = Math.max(1, s5.rates.get('b').rate * RELAY_BURST_S);
  check(s5.rate('b', clock).tokens === cap, `idle for ${RELAY_QUEUE_MS} ms, a relay may still send only ${RELAY_BURST_S} s of its rate at once (${cap.toFixed(2)})`);
  check(s5.wait() === 0, 'and the pipe is told it may send');
  // close(): what waits goes at once, past the rate, but not to a banned relay.
  s5.rate('b', clock).tokens = 0;
  for (let i = 0; i < 3; i++) await s5.publish(s5.roomTag, `e${++n5}`);
  const waiting = sock.b.q.length;
  const sentA = sent.a.length;
  s5.close();
  check(waiting === 3 && sent.b.length === sentB + 3 && sent.a.length === sentA, `close() sends what waits at once (${sent.b.length - sentB} of ${waiting}), but not to a banned relay`);
  s5.sockets.length = 0;
  delete performance.now;
}

// 5b. The pacing runs on a monotonic clock (NB6H-1): the wall clock set back 60 s doesn't stall a relay. Should
//     even that clock step back, a relay's bucket runs into no debt and its rate stays at least MIN_RELAY_RATE.
//     Once closed, a relay held for its rate is sent nothing; the free one carries what waits.
{
  let clock = 1e6;
  performance.now = () => clock;
  const realNow = Date.now;
  const s6 = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/clock', relays: [] });
  await s6.ready;
  const sent = { a: [], b: [] };
  const sock = {};
  for (const name of ['a', 'b']) s6.sockets.push((sock[name] = { relayUrl: name, readyState: WebSocket.OPEN, send: (m) => sent[name].push(JSON.parse(m)[1].id), close() {} }));
  let n6 = 0;
  const burst = RELAY_RATE * RELAY_BURST_S;
  for (let i = 0; i < burst + 4; i++) await s6.publish(s6.roomTag, `c${++n6}`);
  Date.now = () => realNow() - 60000;
  clock += 500;
  s6.pump(sock.a);
  s6.pump(sock.b);
  Date.now = realNow;
  check(sent.a.length === burst + RELAY_RATE / 2 && sent.b.length === burst + RELAY_RATE / 2,
    `the wall clock set back 60 s, a relay still sends at its rate (${sent.a.length - burst} in 500 ms)`);
  clock -= 60000;
  s6.pump(sock.a);
  const r = s6.rates.get('a');
  check(r.rate >= MIN_RELAY_RATE && r.tokens >= 0 && s6.wait() >= 0 && s6.wait() <= 1000,
    `the monotonic clock stepped back 60 s: no debt (tokens ${r.tokens.toFixed(2)}), rate ${r.rate} >= ${MIN_RELAY_RATE}, wait ${s6.wait().toFixed(0)} ms`);
  clock += 500;
  s6.pump(sock.a);
  check(sent.a.length === burst + RELAY_RATE && sock.a.q.length === 0, `and its queue drains at its rate (${sent.a.length - burst - RELAY_RATE / 2} more in 500 ms)`);
  // a refuses for its rate: held, while b is free. Closing sends b what waits, and a nothing.
  await s6.onRelay('a', JSON.stringify(['OK', sent.a.at(-1), false, 'rate-limited: slow down']));
  s6.rate('b', clock).tokens = 0;
  for (let i = 0; i < 3; i++) await s6.publish(s6.roomTag, `c${++n6}`);
  const sentA = sent.a.length;
  const sentB = sent.b.length;
  const waitB = sock.b.q.length;
  check(s6.paused.get('a')?.until > clock && sock.a.q.length === 4 && waitB > 0, `a is held with ${sock.a.q.length} waiting, b free with ${waitB}`);
  s6.sending = 1; // a send() still under way keeps the sockets open after close()
  s6.close();
  check(sent.a.length === sentA && sent.b.length === sentB + waitB && !sock.a.pumpTimer,
    `close() sends the free relay what waits (${sent.b.length - sentB} of ${waitB}), and the held one nothing (${sent.a.length - sentA}), not even once its hold ends`);
  s6.sending = 0;
  s6.shut();
  s6.sockets.length = 0;
  delete performance.now;
}

// 5c. A lossy event (a pipe batch) goes through one relay, the relays taking turns; any other event goes through
//     each. A relay that can't take one yet but can within HOLD_MS still gets its turn (it waits on its queue), so
//     one that answers OK but delivers nothing gets no more than its share. One that holds us for its rate, has
//     banned us or isn't open is skipped, and so is one slowed behind its backlog (NB6I-1); with none left, the one
//     free soonest takes it. A lossy event refused for the rate goes again through the next relay, not the one
//     held. Lossy events sent as wait() allows move at the relays' rates together, others at one relay's; with
//     relays of different rates, about an equal share each.
{
  let clock = 1e6;
  performance.now = () => clock;
  const s = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/stripe', relays: [] });
  await s.ready;
  const names = ['a', 'b', 'c'];
  const sent = { a: [], b: [], c: [] };
  const sock = {};
  const order = [];
  for (const name of names) s.sockets.push((sock[name] = { relayUrl: name, readyState: WebSocket.OPEN, send: (m) => (sent[name].push(JSON.parse(m)[1].id), order.push(name)), close() {} }));
  let n = 0;
  const lossy = () => s.publish(s.roomTag, `l${++n}`, true);
  const counts = () => names.map((x) => sent[x].length).join();
  const tokens = (x, t) => (s.rate(x, clock).tokens = t);
  const ok = (url, id, taken, why = '') => s.onRelay(url, JSON.stringify(['OK', id, taken, why]));
  const through = (id) => names.filter((x) => sent[x].includes(id)).join();
  // Every relay open, free and empty again, its bucket full.
  const reset = () => names.forEach((x) => (s.paused.delete(x), s.banned.delete(x), s.rates.delete(x), (sock[x].q = []), (sock[x].readyState = WebSocket.OPEN)));

  for (let i = 0; i < 6; i++) await lossy();
  check(order.join('') === 'abcabc', `lossy events take turns, one relay each (${order.join('')})`);
  await s.publish(s.roomTag, `e${++n}`);
  check(counts() === '3,3,3' && sent.a.at(-1) === sent.b.at(-1) && sent.b.at(-1) === sent.c.at(-1), `any other event goes through each relay (${counts()})`);
  await s.send(other.id, { kind: 'dg', d: [] }, { lossy: true });
  check(counts() === '4,3,3', `send(to, body, { lossy: true }) goes through one (${counts()})`);
  await s.send(other.id, { kind: 'dg', d: [] });
  check(counts() === '5,4,4' && s.stats.published === 9, `send() without it, through each (${counts()}; ${s.stats.published} published)`);

  // b's turn, with one token; c has three, a none: b and c send theirs, a's waits on its queue, and wait() is how
  // long until b, next, can take one.
  tokens('a', 0);
  tokens('b', 1);
  tokens('c', 3);
  for (let i = 0; i < 3; i++) await lossy();
  check(counts() === '5,5,5' && sock.a.q.length === 1 && s.wait() === 1000 / RELAY_RATE,
    `every relay takes its turn, also one that can't send yet (a waits with ${sock.a.q.length}); wait() ${s.wait()} ms, until the next in turn can`);
  clock += 1000 / RELAY_RATE;
  s.pump(sock.a);
  check(counts() === '6,5,5', 'and it sends it at its rate');

  // c refuses the last two it was sent (the second was on its way when the first held it): both go again through
  // the next relay in turn, not c; while c is held, it is skipped.
  reset();
  for (let i = 0; i < 6; i++) await lossy();
  const [w, z] = sent.c.slice(-2);
  const resent = s.stats.resent;
  clock += 1;
  ok('c', z, false, 'rate-limited: slow down');
  ok('c', w, false, 'rate-limited: slow down');
  const again = (id) => ['a', 'b'].filter((x) => sent[x].includes(id)).length;
  check(s.paused.get('c')?.until > clock && s.stats.backoffs === 1 && s.stats.resent === resent + 2 && again(z) === 1 && again(w) === 1 && sent.c.filter((id) => id === z || id === w).length === 2,
    `a lossy event refused for the rate goes again through the next relay, not the one held (z through ${through(z)}, w through ${through(w)})`);
  const heldC = sent.c.length;
  for (let i = 0; i < 4; i++) await lossy();
  check(sent.c.length === heldC && !sock.c.q.length, 'a relay held for its rate is skipped');
  // Every relay held: the one free soonest (b, whose hold ends first) takes it, and wait() says when.
  s.paused.set('a', { at: clock, until: clock + 500, ms: 500, took: false });
  s.paused.set('b', { at: clock, until: clock + 300, ms: 300, took: false });
  s.turn = 0; // a's turn: it is held longer, so it is passed over
  const all = s.wait();
  await lossy();
  check(all === 300 && sock.b.q.length === 1 && !sock.a.q.length && !sock.c.q.length, `with every relay held, the hold that ends first takes it (wait() ${all} ms)`);

  // A banned relay, even one whose ban ends in a moment, and one that isn't open are skipped, whoever's turn.
  reset();
  s.banned.set('b', { until: clock + 1, ms: BACKOFF_MS });
  const before = sent.b.length;
  for (let i = 0; i < 3; i++) await lossy();
  check(sent.b.length === before && !sock.b.q.length, 'a banned relay is sent no lossy event, however soon its ban ends');
  reset();
  sock.a.readyState = WebSocket.CLOSED;
  const beforeA = sent.a.length;
  for (let i = 0; i < 3; i++) await lossy();
  check(sent.a.length === beforeA && !sock.a.q.length, 'nor is one that is not open');
  // Every open relay banned: no lossy event goes, and wait() is until the first ban ends.
  reset();
  names.forEach((x, i) => s.banned.set(x, { until: clock + 700 + 100 * i, ms: BACKOFF_MS }));
  const none = counts();
  await lossy();
  check(counts() === none && names.every((x) => !sock[x].q.length) && s.wait() === 700, `with every relay banned, none goes and wait() is until the first ban ends (${s.wait()} ms)`);

  // Pacing: send whenever wait() says one can go, for 10 s from empty buckets, the relays answering OK at once.
  // Lossy events take every relay's rate, about 3 * RELAY_RATE a second, in turns; others, one relay's. With a and b
  // slowed to MIN_RELAY_RATE (growing back) and c not (one that never refuses), each still takes about a third: c
  // only the odd turn a slowed one passes.
  const answer = () => names.forEach((x) => [...(sock[x].inflight?.keys() ?? [])].forEach((id) => ok(x, id, true)));
  const pace = async (isLossy, slow = []) => {
    reset();
    for (const x of names) tokens(x, 0);
    for (const x of slow) Object.assign(s.rates.get(x), { low: MIN_RELAY_RATE, lowAt: clock, rate: MIN_RELAY_RATE });
    const from = names.map((x) => sent[x].length);
    let m = 0;
    for (const end = clock + 10000; clock < end;) {
      answer();
      const ms = s.wait();
      if (ms > 0) clock += Math.ceil(ms);
      else (await s.publish(s.roomTag, `p${++n}`, isLossy), m++);
    }
    return { m, each: names.map((x, i) => sent[x].length - from[i]) };
  };
  const striped = await pace(true);
  const full = await pace(false);
  check(Math.abs(striped.m - 30 * RELAY_RATE) <= 3 && striped.each.every((k) => Math.abs(k - 10 * RELAY_RATE) <= 1) && Math.abs(full.m - 10 * RELAY_RATE) <= 1,
    `paced by wait() for 10 s, ${striped.m} lossy events go (${striped.each.join(', ')} per relay), ${full.m} others (each relay ${RELAY_RATE}/s)`);
  const uneven = await pace(true, ['a', 'b']);
  check(Math.max(...uneven.each) - Math.min(...uneven.each) <= 2 && uneven.each[2] === Math.max(...uneven.each) && uneven.m > 0,
    `with two relays slowed and one not, each takes its turn: ${uneven.each.join(', ')}`);

  // NB6I-1: a relay whose turn it is but which is slowed behind other events (at MIN_RELAY_RATE, 8 waiting) would
  // keep the pipe idle for seconds while the others are free: it is passed over, and wait() is the next one's.
  reset();
  for (let i = 0; i < 8; i++) await s.publish(s.roomTag, `e${++n}`);
  Object.assign(s.rate('a', clock), { low: MIN_RELAY_RATE, lowAt: clock, rate: MIN_RELAY_RATE, tokens: 0 });
  sock.a.q = Array.from({ length: 8 }, (_, i) => ({ id: `q${i}`, msg: '', at: clock, seq: 0, tries: 0, sent: 0 }));
  s.turn = 0;
  const backlog = s.wait();
  await lossy();
  const on = names.filter((x) => sock[x].q.some((e) => e.lossy));
  check(backlog <= HOLD_MS && on.join() === 'b' && sock.a.q.length === 8,
    `a relay slowed behind its backlog is passed over (it waits on ${on.join()}; wait() ${Math.round(backlog)} ms, not ${Math.round(s.free(sock.a, clock))})`);

  // close(): a lossy event waiting on a relay held for its rate is there alone, so it goes through a free one,
  // even one slowed behind its backlog (once closed, all of it goes at once), rather than a held one free sooner;
  // what else waits there went through each relay, and the held ones are sent nothing.
  reset();
  s.turn = 0;
  tokens('c', 0);
  for (let i = 0; i < 3; i++) await lossy();
  const late = sock.c.q[0]?.id;
  await s.publish(s.roomTag, `e${++n}`);
  const each = sent.a.at(-1);
  Object.assign(s.rate('a', clock), { low: MIN_RELAY_RATE, lowAt: clock, rate: MIN_RELAY_RATE, tokens: 0 });
  sock.a.q = Array.from({ length: 8 }, (_, i) => ({ id: `q${i}`, msg: '["EVENT",{"id":"q' + i + '"}]', at: clock, seq: 0, tries: 0, sent: 0 }));
  s.paused.set('b', { at: clock, until: clock + 300, ms: 300, took: false });
  s.paused.set('c', { at: clock, until: clock + 1000, ms: 1000, took: false });
  const heldAt = [sent.b.length, sent.c.length].join();
  s.turn = 1; // b's turn: held for less than HOLD_MS, it is passed over all the same
  s.close();
  const once = sent.a.filter((id) => id === each).length === 1;
  check(late && through(late) === 'a' && once && [sent.b.length, sent.c.length].join() === heldAt,
    `close() sends a lossy event waiting on a held relay through a free one (${through(late)}), no other event twice, and the held ones nothing`);
  delete performance.now;
}

// 5d. NB-006j: a relay that has answered nothing, taken or refused, since an event sent to it SILENT_MS ago is silent
//     (a half-open socket, still OPEN but dead, would take its turns and lose them until it closes). It is passed over
//     for lossy events and wait() isn't its, but it is still sent every other event; its events leaving `inflight`
//     unanswered leave it silent, and its next OK, taken or refused, gives it its turns back. One that answers each
//     event 4 s late keeps its turns. With only silent ones left, they take turns. A relay that answers but holds us
//     is waited for rather than a silent one sent to; once closed, a lossy event goes through one that answers and
//     isn't slowed behind its backlog, then one slowed, then a silent one, and a held one last.
{
  let clock = 1e6;
  performance.now = () => clock;
  const s = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/silent', relays: [] });
  await s.ready;
  const names = ['a', 'b', 'c'];
  const sent = { a: [], b: [], c: [] };
  const sentAt = new Map(); // event id -> when b was sent it
  const order = []; // [when, relay] of each event sent
  const sock = {};
  for (const name of names) {
    const send = (m) => {
      const id = JSON.parse(m)[1].id;
      sent[name].push(id);
      order.push([clock, name]);
      if (name === 'b') sentAt.set(id, clock);
    };
    s.sockets.push((sock[name] = { relayUrl: name, readyState: WebSocket.OPEN, send, close() {} }));
  }
  let n = 0;
  const lossy = () => s.publish(s.roomTag, `s${++n}`, true);
  const ok = (url, id, taken = true, why = '') => s.onRelay(url, JSON.stringify(['OK', id, taken, why]));
  const inflight = (x) => [...(sock[x].inflight?.keys() ?? [])];
  const turns = (x, from = 0) => order.filter(([t, y]) => y === x && t >= from).map(([t]) => t);
  const since = (k) => order.slice(k).map(([, x]) => x).join('');
  const LATE_MS = 4000;

  // A pipe sends a lossy event every 250 ms for 12 s: a answers each at once, b LATE_MS after it was sent, c never.
  // c's turns come every 750 ms: its last is 4.25 to 5 s after its first unanswered one (about 5 s; not SILENT_MS,
  // so that the constant itself is checked).
  const start = clock;
  s.turn = 0;
  for (; clock < start + 12000; clock += 250) {
    inflight('a').forEach((id) => ok('a', id));
    inflight('b').filter((id) => clock - sentAt.get(id) >= LATE_MS).forEach((id) => ok('b', id));
    await lossy();
  }
  const c = turns('c');
  const after = c[0] + SILENT_MS;
  const [toA, toB] = [turns('a', after).length, turns('b', after).length];
  check(c.at(-1) - c[0] < 5000 && c.at(-1) - c[0] >= 4250 && !turns('c', after).length && Math.abs(toA - toB) <= 1 && toA > 10 && s.silent(sock.c, clock) && !s.silent(sock.b, clock),
    `a relay that never answers takes its turns for ${c.at(-1) - c[0]} ms after its first, then none; a and b, which answers ${LATE_MS} ms late, take ${toA} and ${toB}`);

  // c's turn and its bucket full, a and b able to take one in 1000 / RELAY_RATE ms: the pipe waits for a, not c.
  s.turn = 2;
  s.rate('a', clock).tokens = 0;
  s.rate('b', clock).tokens = 0;
  const w = s.wait();
  await lossy();
  check(s.free(sock.c, clock) === 0 && w === 1000 / RELAY_RATE && sock.a.q.at(-1)?.lossy && !sock.c.q?.length,
    `wait() isn't a silent relay's (${w} ms, not ${s.free(sock.c, clock)}), and the event waits for the next in turn`);
  const toC = sent.c.length;
  await s.publish(s.roomTag, `e${++n}`);
  check(sent.c.length === toC + 1, 'a silent relay is still sent every other event');
  sock.c.inflight.clear(); // as when each is pushed out past RELAY_QUEUE, unanswered
  check(s.silent(sock.c, clock), 'its events leaving `inflight` unanswered leave it silent');
  ok('c', sent.c.at(-1), false, 'invalid: refused');
  s.turn = 2;
  let k = order.length;
  await lossy();
  check(!s.silent(sock.c, clock) && since(k) === 'c', `its next OK, here a refusal, gives it its turns back (${since(k)})`);

  // Every relay silent: they take turns, also a, which can't send yet (its event waits on its queue) while c could.
  names.forEach((x) => (s.rates.delete(x), (sock[x].q = [])));
  await s.publish(s.roomTag, `e${++n}`);
  clock += SILENT_MS;
  s.rate('a', clock).tokens = 0;
  s.turn = 0;
  k = order.length;
  for (let i = 0; i < 3; i++) await lossy();
  check(names.every((x) => s.silent(sock[x], clock)) && sock.a.q.length === 1 && since(k) === 'bc', `with only silent relays, they take turns (a waits with ${sock.a.q.length}, then ${since(k)})`);

  // a and b answer but hold us for their rate, c is silent: the event waits for the hold that ends first, not on c.
  for (const x of ['a', 'b']) inflight(x).forEach((id) => ok(x, id));
  names.forEach((x) => (s.rates.delete(x), (sock[x].q = [])));
  s.paused.set('a', { at: clock, until: clock + 300, ms: 300, took: false });
  s.paused.set('b', { at: clock, until: clock + 500, ms: 500, took: false });
  s.turn = 2;
  const held = s.wait();
  await lossy();
  const moved = sock.a.q.at(-1);
  check(held === 300 && moved?.lossy && !sock.c.q.length, `a relay that answers but holds us is waited for rather than a silent one sent to (wait() ${held} ms)`);

  // close(): the lossy event waiting on a, held, goes through b, which answers, though it is c's turn. Then, from a's
  // turn: b, which answers; a, slowed behind its backlog (it may well refuse more, and nothing is sent again once
  // closed); c, silent; and a held relay, which is sent nothing once closed, last.
  s.paused.delete('b');
  s.turn = 2;
  k = order.length;
  s.close();
  const ladder = [];
  s.turn = 0;
  s.paused.delete('a');
  Object.assign(s.rate('a', clock), { low: MIN_RELAY_RATE, lowAt: clock, rate: MIN_RELAY_RATE, tokens: 0 });
  sock.a.q = Array.from({ length: 8 }, (_, i) => ({ id: `q${i}`, msg: '', at: clock, seq: 0, tries: 0, sent: 0 }));
  ladder.push(s.next(clock));
  s.paused.set('b', { at: clock, until: clock + 500, ms: 500, took: false });
  ladder.push(s.next(clock));
  s.paused.set('a', { at: clock, until: clock + 300, ms: 300, took: false });
  ladder.push(s.next(clock));
  check(sent.b.includes(moved.id) && !sent.c.includes(moved.id) && ladder.map((i) => names[i]).join('') === 'bac',
    `once closed, a lossy event goes through a relay that answers (${since(k)}); then, held one by one: ${ladder.map((i) => names[i]).join(', ')}`);
  delete performance.now;
}

// 5e. NB-006l: the pacing of stored events, and of reads.
check(STORE_EVERY_MS === 1000 && STORE_QUEUE_MS === 120000 && STORE_OK_MS === 30000 && QUERY_MS === 10000, 'stored events go 1 s apart, wait 120 s at most and 30 s for an OK; a read takes 10 s at most');

// 5e. NB-006l: query() reads stored events from every open relay that hasn't banned us, each under a subscription of
//     its own. The relay's events for it go to `keep`, which names their slot (or null), and each relay keeps the
//     newest of each slot, by created_at and then the lower id; other events on the socket go on as ever. EOSE ends a
//     relay's read complete; CLOSED, the socket closing, or `ms` without EOSE end it incomplete; it is CLOSEd either
//     way. A relay that isn't open, or has banned us, is incomplete at once.
{
  let clock = 1e6;
  performance.now = () => clock;
  const s = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/query', relays: [] });
  await s.ready;
  const names = ['a', 'b', 'c', 'd', 'e'];
  const sent = Object.fromEntries(names.map((x) => [x, []]));
  const sock = {};
  for (const x of names) s.sockets.push((sock[x] = { relayUrl: x, readyState: WebSocket.OPEN, send: (m) => sent[x].push(JSON.parse(m)), close() {} }));
  sock.d.readyState = WebSocket.CLOSED;
  s.banned.set('e', { until: clock + BACKOFF_MS, ms: BACKOFF_MS });
  const keep = (ev) => (ev?.kind === 30078 && Number.isInteger(ev.created_at) ? ev.tags?.[0]?.[1] ?? null : null);
  const ev = (slot, at, id) => ({ id, kind: 30078, created_at: at, tags: [['d', slot]] });
  const tick = () => new Promise((r) => setImmediate(r));
  const t0 = Date.now();
  const read = s.query({ kinds: [30078], '#d': ['x', 'y'] }, { ms: 300, keep });
  await tick();
  const sub = (x) => sent[x].find((m) => m[0] === 'REQ')?.[1];
  check(['a', 'b', 'c'].every((x) => sent[x].length === 1 && sent[x][0][0] === 'REQ' && sent[x][0][2].kinds[0] === 30078) && !sent.d.length && !sent.e.length && new Set(['a', 'b', 'c'].map(sub)).size === 3,
    'a REQ under its own subscription to each open relay, none to one closed or banning us');
  const relay = (x, m) => s.onRelay(x, JSON.stringify(m));
  for (const e of [ev('x', 5, 'f1'), ev('x', 7, 'f2'), ev('x', 7, 'e9'), ev('x', 6, '01'), ev('y', 1, 'aa'), { kind: 1, created_at: 9, tags: [['d', 'x']] }, null, 'junk', 7]) await relay('a', ['EVENT', sub('a'), e]);
  const received = s.stats.received;
  await relay('a', ['EVENT', s.sub, { id: hid(7e9), kind: 25059, pubkey: 'x', tags: [['p', s.roomTag]], content: 'e30=' }]);
  check(s.stats.received === received + 1, "signalling's events on the same socket go on as ever");
  await relay('a', ['EOSE', sub('a')]);
  await relay('b', ['CLOSED', sub('b'), 'auth-required: we only serve our users']);
  const r = await read;
  const ms = Date.now() - t0;
  const ids = (x) => [...r.get(x).events].map(([k, e]) => `${k}:${e.id}`).join(' ');
  check(r.get('a').complete && ids('a') === 'x:e9 y:aa', `a sent EOSE: complete, the newest of each slot kept, by created_at then the lower id (${ids('a')})`);
  check(names.slice(1).every((x) => !r.get(x).complete && !r.get(x).events.size) && ms >= 290, `b refused it (CLOSED), c sent no EOSE in time (${ms} ms), d isn't open, e banned us: incomplete`);
  check(sent.a.at(-1)?.[0] === 'CLOSE' && sent.a.at(-1)[1] === sub('a') && sent.b.at(-1)?.[0] === 'CLOSE' && sent.c.at(-1)?.[0] === 'CLOSE' && sent.c.at(-1)[1] === sub('c'), 'each subscription CLOSEd: at EOSE, CLOSED, and the timeout');
  await relay('a', ['EVENT', sub('a'), ev('z', 99, 'ff')]);
  check(!r.get('a').events.has('z'), 'an event for an ended read is dropped');
  const t1 = Date.now();
  const read2 = s.query({ kinds: [30078] }, { ms: 60000, keep });
  await tick();
  const sub2 = (x) => sent[x].filter((m) => m[0] === 'REQ').at(-1)[1];
  await relay('b', ['EOSE', sub2('b')]);
  await relay('c', ['EOSE', sub2('c')]);
  s.forget(sock.a);
  const r2 = await read2;
  const forgot = Date.now() - t1;
  const read3 = s.query({ kinds: [30078] }, { ms: 60000, keep });
  await tick();
  s.close();
  const r3 = await read3;
  check(!r2.get('a').complete && r2.get('b').complete && forgot < 2000 && [...r3.values()].every((x) => !x.complete) && Date.now() - t1 < 2000, "the socket closing ends that relay's read incomplete at once, and close() every read");
  delete performance.now;
}

// 5f. NB-006l: store() keeps an event on the relays named, behind signalling: it waits while signalling's events wait
//     on that relay, goes one each STORE_EVERY_MS at most, and counts in the relay's bucket. It resolves, per relay,
//     true once the relay takes it, false once it refuses it (one refused for its rate is held and sent again, up to
//     RELAY_TRIES in all), bans us, closes, holds it past STORE_QUEUE_MS or doesn't answer within STORE_OK_MS, or the
//     signalling closes; a relay not open or banning us is false at once.
{
  let clock = 1e6;
  performance.now = () => clock;
  const oks = []; // STORE_OK_MS timers, fired by hand
  const realTimeout = globalThis.setTimeout;
  globalThis.setTimeout = (fn, ms, ...x) => (ms === STORE_OK_MS ? (oks.push(fn), { ok: true }) : realTimeout(fn, ms, ...x));
  const s = new NostrSignaling(pw, pw.endpoint(), { room: 'perf2/store', relays: [] });
  await s.ready;
  const names = ['a', 'b', 'c', 'd'];
  const order = { a: [], b: [], c: [], d: [] };
  const sock = {};
  for (const x of names) s.sockets.push((sock[x] = { relayUrl: x, readyState: WebSocket.OPEN, send: (m) => order[x].push(JSON.parse(m)[1].id), close() {} }));
  sock.c.readyState = WebSocket.CLOSED;
  s.banned.set('d', { until: clock + BACKOFF_MS, ms: BACKOFF_MS });
  const keepEv = (n) => ({ id: hid(8e9 + n), kind: 30078, pubkey: 'k', created_at: 1, tags: [], content: '', sig: '' });
  const ok = (x, id, taken = true, why = '') => s.onRelay(x, JSON.stringify(['OK', id, taken, why]));
  const settled = new Map();
  const watch = (m, n) => m.forEach((p, x) => p.then((v) => settled.set(`${n}${x}`, v)));
  const tick = () => new Promise((r) => setImmediate(r));
  // a's bucket is spent and two signalling events wait there (b closed meanwhile).
  sock.b.readyState = WebSocket.CLOSED;
  for (let i = 0; i < RELAY_RATE * RELAY_BURST_S + 2; i++) await s.publish(s.roomTag, `sig${i}`);
  sock.b.readyState = WebSocket.OPEN;
  const waiting = order.a.length;
  const e1 = keepEv(1);
  watch(s.store(e1, ['a', 'b', 'c', 'd']), 1);
  await tick();
  check(settled.get('1c') === false && settled.get('1d') === false && order.b.at(-1) === e1.id && !order.a.includes(e1.id) && waiting === RELAY_RATE * RELAY_BURST_S,
    'c closed and d banning us: false at once; b, with nothing waiting, sent it; a, with signalling waiting, holds it');
  clock += 1000;
  s.pump(sock.a);
  check(order.a.slice(waiting).join() === [...order.a.slice(waiting, waiting + 2), e1.id].join() && order.a.length === waiting + 3, 'a sends its waiting signalling first, then the event to keep');
  ok('a', e1.id);
  ok('b', e1.id, false, 'invalid: we keep no such events');
  await tick();
  check(settled.get('1a') === true && settled.get('1b') === false, 'taken on a (true), refused by b (false)');
  // Kept events go STORE_EVERY_MS apart, behind one another.
  const e2 = keepEv(2);
  const e3 = keepEv(3);
  watch(s.store(e2, ['b']), 2);
  watch(s.store(e3, ['b']), 3);
  const at = order.b.length - 1;
  const first = order.b.slice(at).join();
  clock += STORE_EVERY_MS - 1;
  s.pump(sock.b);
  const early = order.b.slice(at).join();
  clock += 1;
  s.pump(sock.b);
  check(first === e2.id && early === e2.id && order.b.slice(at).join() === [e2.id, e3.id].join(), `two more to keep on b, ${STORE_EVERY_MS} ms after the last: the first at once, the next ${STORE_EVERY_MS} ms after it`);
  // Refused for the rate: held, sent again, then taken.
  ok('b', e2.id, false, 'rate-limited: slow down');
  clock += HOLD_MS + STORE_EVERY_MS;
  s.pump(sock.b);
  const again = order.b.filter((id) => id === e2.id).length;
  ok('b', e2.id);
  await tick();
  check(again === 2 && settled.get('2b') === true && s.stats.resent >= 1, 'refused for its rate: held, sent again, then taken');
  // No answer within STORE_OK_MS: false. A ban: false, with what waits there.
  for (const fn of oks.splice(0)) fn();
  await tick();
  check(settled.get('3b') === false, `no answer within ${STORE_OK_MS / 1000} s: false`);
  const e4 = keepEv(4);
  const e5 = keepEv(5);
  watch(s.store(e4, ['b']), 4);
  watch(s.store(e5, ['b']), 5);
  clock += STORE_EVERY_MS;
  s.pump(sock.b);
  ok('b', e4.id, false, 'blocked: too many events');
  await tick();
  check(settled.get('4b') === false && settled.get('5b') === false, 'a ban: false, for it and for the one waiting behind it');
  // Held past STORE_QUEUE_MS: false.
  s.paused.set('a', { at: clock, until: clock + 10 * STORE_QUEUE_MS, ms: HOLD_MS, took: false });
  const e6 = keepEv(6);
  watch(s.store(e6, ['a']), 6);
  clock += STORE_QUEUE_MS + 1;
  s.pump(sock.a);
  await tick();
  check(settled.get('6a') === false && !order.a.includes(e6.id), `held past ${STORE_QUEUE_MS / 1000} s: false, never sent`);
  // close(): what waits and what waits for an OK, false.
  s.paused.delete('a');
  const e7 = keepEv(7);
  const e8 = keepEv(8);
  watch(s.store(e7, ['a']), 7);
  watch(s.store(e8, ['a']), 8);
  s.close();
  await tick();
  check(settled.get('7a') === false && settled.get('8a') === false && order.a.includes(e7.id) && !order.a.includes(e8.id), 'close(): one sent and one waiting, both false; nothing more is sent');
  globalThis.setTimeout = realTimeout;
  delete performance.now;
}

// 6. A relay socket that closes (phone hotspot on, network switch, tab frozen) is redialled,
//    subscribes again and says so; a network change recycles open sockets; close() stops it.
{
  const Real = globalThis.WebSocket;
  const made = [];
  class Fake {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    constructor(url) {
      this.url = url;
      this.readyState = Fake.CONNECTING;
      this.sent = [];
      made.push(this);
    }
    open() {
      this.readyState = Fake.OPEN;
      this.onopen?.();
    }
    send(m) {
      this.sent.push(m);
    }
    close() {
      this.readyState = Fake.CLOSED;
      this.onclose?.();
    }
  }
  globalThis.WebSocket = Fake;
  const tick = (ms) => new Promise((r) => setTimeout(r, ms));
  const s6 = new NostrSignaling(pw, pw.endpoint(), { room: 'hotspot/reconnect', relays: ['wss://a', 'wss://b'] });
  // start() dials only after its key derivation, which a loaded machine can hold up
  // well past a fixed tick: wait for both sockets, then open them.
  for (let i = 0; i < 1000 && made.length < 2; i++) await tick(5);
  made.forEach((w) => w.open());
  await s6.ready;
  const back = [];
  s6.onReconnect = (url) => back.push(url);
  check(s6.stats.relaysOpen === 2 && made.every((w) => w.sent.some((m) => m.startsWith('["REQ"'))), 'both relays open and subscribed');
  made[0].close(); // relay a drops
  check(s6.stats.relaysOpen === 1, `a closed relay is no longer counted open (${s6.stats.relaysOpen})`);
  await tick(RECONNECT_MS + 100);
  const a2 = made.at(-1);
  check(made.length === 3 && a2.url === 'wss://a', `relay a is redialled after ${RECONNECT_MS} ms (${made.length} sockets made)`);
  a2.close(); // fails again: the delay doubles
  await tick(RECONNECT_MS + 100);
  check(made.length === 3, 'a second failure waits longer');
  await tick(RECONNECT_MS + 100);
  const a3 = made.at(-1);
  a3.open();
  check(made.length === 4 && a3.sent.some((m) => m.startsWith('["REQ"')) && back.join() === 'wss://a' && s6.stats.reconnects === 1 && s6.stats.relaysOpen === 2,
    `the reopened relay subscribes again and onReconnect fires (${JSON.stringify(back)}, open ${s6.stats.relaysOpen})`);
  await s6.publish(s6.roomTag, 'e30=');
  check(a3.sent.some((m) => m.startsWith('["EVENT"')) && made[0].sent.every((m) => !m.startsWith('["EVENT"')), 'events go to the new socket, not the dead one');
  // Chrome's connection 'change' also fires on a new speed estimate: open sockets stay.
  s6.wake({ type: 'change' });
  s6.wake({ type: 'visibilitychange' });
  check(made.length === 4 && s6.stats.relaysOpen === 2 && s6.stats.reconnects === 1, `a connection change or the page showing leaves open relays alone (${made.length} sockets made)`);
  s6.wake({ type: 'online' }); // back online: open sockets may be on the old network
  check(made.length === 6 && s6.stats.relaysOpen === 0 && made.slice(-2).map((w) => w.url).sort().join() === 'wss://a,wss://b', 'going online redials every relay at once');
  made.slice(-2).forEach((w) => w.open());
  check(s6.stats.relaysOpen === 2 && s6.sockets.length === 2, `after it, both are open again with one socket each (${s6.sockets.length})`);
  s6.close();
  await tick(RECONNECT_MS * 2 + 100);
  check(made.length === 6 && s6.retry.size === 0, 'close() stops the redialling');
  globalThis.WebSocket = Real;
}

console.log(failed ? `${failed} FAILED` : 'nostr bounds: all passed');
process.exit(failed ? 1 : 0);
