// NB-126: RelayTransport (relay_link.js) between bare endpoints, over two Nostr relays on loopback (nostr_relay.mjs),
// each endpoint saying hello as a native peer does. Checks:
// - its hello is Rust's exact JSON (traversal.rs `parse_hello`), sealed for the room, with its proof under the room
//   key; it goes on start and every helloEveryMs, and in answer to a peer newly heard: once (no ping-pong between two
//   transports), and at most one each greetMs for many newcomers at once;
// - a native peer's hello that proves the room key lists it ('seen' once, `peers`); a joinRoom member's (no meta.nb),
//   a wrong proof, a stale one and another room's don't;
// - dial() links over the pipe alone and resolves to the PeerId ('open'); messages go both ways, 6 KB and 100 small
//   ones too; every batch to the peer carries NATIVE_BATCH bytes of datagrams at most, and every event so made fits a
//   4 KB relay frame as a board reads it;
// - a pipe batch from a peer that said no hello is refused (no datagram taken, no link), and an offer goes unanswered;
// - drop() says goodbye at once, and a dial again links; a dial nobody answers rejects with 'timeout', one under way
//   at close() with 'closed', and close() leaves the relays; TypeErrors for a bad PeerId, a missing room, bad relays
//   and something that isn't an endpoint; the endpoint's datagrams to another transport's handles still go there; an
//   onPeer that throws breaks nothing.
//
//   node packages/js/test/relay_link.mjs [path/to/peerwire_ffi.wasm]
import { loadPeerwire } from '../src/peerwire.js';
import { RelayTransport, NATIVE_BATCH } from '../src/relay_link.js';
import { NostrSignaling, roomSealKey, b64, unb64 } from '../src/nostr.js';
import { RoomProof } from '../src/proof.js';
import { WebRtcTransport } from '../src/webrtc.js';
import { startRelay } from './nostr_relay.mjs';

const pw = await loadPeerwire(process.argv[2] ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url).pathname);
let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const until = async (what, cond, ms = 10000) => {
  const t = Date.now();
  while (!(await cond())) {
    if (Date.now() - t > ms) {
      check(false, `timed out waiting for ${what}`);
      return false;
    }
    await new Promise((r) => setTimeout(r, 10));
  }
  return true;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const settled = (p) => p.then(() => null, (e) => e); // the error, or null
const outcome = (p) => p.then((v) => ({ v }), (e) => ({ e }));
const enc = new TextEncoder();
const dec = new TextDecoder();
const APP = 'peerwire-test';
const ROOM = `relay-${process.pid}`;
const NAME = `${APP}/${ROOM}`;
// The shape Rust's traversal.rs `parse_hello` takes, and nothing else.
const RUST_HELLO = /^\{"t":"hello","from":"([0-9a-f]{64})","meta":\{"nb":1,"role":"peer","ts":(\d{1,19}),"p":"[0-9a-f]{64}"\}\}$/;
// What an ESP32 board's relay socket reads at most (its carrier's Limits::SMALL.message).
const BOARD_FRAME = 4096;
// An event as a board's socket reads it: under a subscription id of 64 characters, more than any client's.
const frame = (ev) => JSON.stringify(['EVENT', 'x'.repeat(64), ev]).length;
const sha = async (s) => Buffer.from(await crypto.subtle.digest('SHA-256', enc.encode(s))).toString('hex');
const ROOM_TAG = await sha(`peerwire/room/${NAME}`);
const sealKey = await roomSealKey(NAME);
const proof = await RoomProof.create(NAME);
const tagOf = (ev) => ev.tags.find((t) => t[0] === 'p')?.[1];
const relays = [await startRelay(), await startRelay()];
const urls = relays.map((r) => r.url);

// The room hellos from `id` so far, in clear (each goes through every relay: relay 0's are all of them).
async function hellosOf(id) {
  const out = [];
  for (const ev of relays[0].events) {
    if (tagOf(ev) !== ROOM_TAG) continue;
    const buf = unb64(ev.content);
    try {
      const text = dec.decode(await crypto.subtle.decrypt({ name: 'AES-GCM', iv: buf.subarray(0, 12) }, sealKey, buf.subarray(12)));
      if (JSON.parse(text).from === id) out.push(text);
    } catch {
      // not a hello of this room
    }
  }
  return out;
}

// A bare endpoint with a RelayTransport in the room: what it got, and what onPeer said.
function node(opts = {}) {
  const ep = pw.endpoint();
  const n = { ep, id: ep.id, events: [], got: [], told: [] };
  ep.onEvent = (e) => {
    n.events.push(e);
    if (e.type === 'message') n.got.push(dec.decode(e.data));
  };
  n.t = new RelayTransport(ep, { appId: APP, roomId: ROOM, relays: urls, onPeer: (p, s) => n.told.push([p, s]), ...opts });
  return n;
}
const end = (...ns) => ns.forEach((n) => (n.t.close(), n.ep.close()));

// A bare signalling client in the room, saying what hellos it is told to.
async function client() {
  const ep = pw.endpoint();
  const sig = new NostrSignaling(pw, ep, { room: NAME, relays: urls });
  await sig.ready;
  return { ep, id: ep.id, sig, close: () => (sig.close(), ep.close()) };
}

// 1. TypeErrors.
{
  const ep = pw.endpoint();
  const threw = (f, re = /./) => {
    try {
      f();
      return false;
    } catch (e) {
      return e instanceof TypeError && re.test(e.message);
    }
  };
  check(threw(() => new RelayTransport(ep, { roomId: 'r' })) && threw(() => new RelayTransport(ep, { appId: 'a', roomId: '' })) &&
    threw(() => new RelayTransport(ep, { appId: 'a', roomId: 'r', relays: ['https://relay.test'] }), /relays\[0\]/) &&
    threw(() => new RelayTransport(ep, { appId: 'a', roomId: 'r', relays: [] })) &&
    threw(() => new RelayTransport({}, { appId: 'a', roomId: 'r' }), /Endpoint/),
  "TypeError: no appId or roomId, relays that aren't ws:// or wss:// urls, something that isn't an endpoint");
  ep.close();
}

// 2. The hello: Rust's shape, on start and every helloEveryMs.
{
  RelayTransport.helloEveryMs = 600;
  const a = node();
  await until("a's first hello", async () => (await hellosOf(a.id)).length >= 1);
  const [first] = await hellosOf(a.id);
  const m = RUST_HELLO.exec(first);
  check(!!m && m[1] === a.id && Math.abs(Number(m[2]) - Date.now()) < 5000 && (await proof.checkHello(a.id, JSON.parse(first).meta)),
    `its hello is Rust's exact JSON, sealed for the room, with its proof under the room key: ${first}`);
  await sleep(2000);
  const n = (await hellosOf(a.id)).length;
  check(n >= 3 && n <= 5, `a hello every helloEveryMs: ${n} in about 2 s at 600 ms`);
  end(a);
  RelayTransport.helloEveryMs = 60000;
}

// 3. Native peers hear each other: each lists the other once, and answers its hello once.
RelayTransport.greetMs = 300;
const a = node();
await until("a's hello", async () => (await hellosOf(a.id)).length === 1);
const b = node();
await until('a and b list each other', () => a.t.peers.includes(b.id) && b.t.peers.includes(a.id));
await sleep(1500);
const counts = [(await hellosOf(a.id)).length, (await hellosOf(b.id)).length];
check(JSON.stringify(a.told) === JSON.stringify([[b.id, 'seen']]) && JSON.stringify(b.told) === JSON.stringify([[a.id, 'seen']]) &&
  counts.join() === '2,2', `a and b list each other ('seen' once each), and each answered the other's hello once: hellos ${counts}`);

// 4. Five newcomers at once: at most a hello in answer each greetMs.
{
  const before = (await hellosOf(a.id)).length;
  const cs = await Promise.all([1, 2, 3, 4, 5].map(() => client()));
  await Promise.all(cs.map(async (c) => c.sig.hello({ nb: 1, ...(await proof.hello(c.id)) })));
  await until('a lists the five', () => cs.every((c) => a.t.peers.includes(c.id)));
  await sleep(RelayTransport.greetMs + 500);
  const n = (await hellosOf(a.id)).length - before;
  check(n >= 1 && n <= 2, `five newcomers at once got ${n} hello(s) in answer (at most one each greetMs)`);
  cs.forEach((c) => c.close());
}

// 5. Hellos that don't list a peer.
{
  const c = await client();
  const good = await proof.hello(c.id);
  const otherRoom = await (await RoomProof.create(`${APP}/elsewhere`)).hello(c.id);
  for (const meta of [good, { nb: 1, ...good, p: '0'.repeat(64) }, { nb: 1, ...(await proof.hello(c.id, 'peer', Date.now() - 120000)) }, { nb: 1, ...otherRoom }]) {
    await c.sig.hello(meta);
  }
  const d = await client();
  await d.sig.hello({ nb: 1, ...(await proof.hello(d.id)) });
  await until('a lists d, whose hello went after', () => a.t.peers.includes(d.id));
  await sleep(300);
  check(!a.t.peers.includes(c.id) && !a.told.some(([p]) => p === c.id),
    "not listed: a joinRoom member's hello (no meta.nb), a wrong proof, a stale one, another room's");
  c.close();
  d.close();
}

// 6. A dial over the pipe, and messages both ways.
const batches = []; // the datagrams' sizes in each pipe batch a sends
{
  const send = a.t.signaling.send.bind(a.t.signaling);
  a.t.signaling.send = (to, body, o) => {
    if (body?.kind === 'dg') batches.push(body.d.map((s) => unb64(s).length));
    return send(to, body, o);
  };
}
{
  const t0 = Date.now();
  const linked = await a.t.dial(b.id);
  const ms = Date.now() - t0;
  // b is linked once a's first packet after the handshake comes.
  await until('b connected', () => b.events.some((e) => e.type === 'connected' && e.peer === a.id));
  check(linked === b.id && a.t.has(b.id) && a.told.some(([p, s]) => p === b.id && s === 'open') && a.t.rtc.links.size === 0,
    `a dials b over the pipe alone (no WebRTC): resolves to its PeerId in ${ms} ms, 'open', b connected`);
  a.ep.send(b.id, 'led on');
  await until('b gets it', () => b.got.includes('led on'));
  b.ep.send(a.id, 'led: on');
  await until("a gets b's answer", () => a.got.includes('led: on'));
  const big = 'x'.repeat(6000);
  a.ep.send(b.id, big);
  for (let i = 0; i < 100; i++) a.ep.send(b.id, `m${i}`);
  await until('b gets 6 KB and 100 small ones', () => b.got.includes(big) && b.got.includes('m99'), 30000);
  check(b.got.includes(big) && Array.from({ length: 100 }, (_, i) => `m${i}`).every((m) => b.got.includes(m)), 'messages go both ways: 6 KB, and 100 small ones');
  const sums = batches.map((s) => s.reduce((x, y) => x + y, 0));
  check(batches.length > 5 && Math.max(...sums) <= NATIVE_BATCH && batches.every((s) => s.length <= 64) && batches.some((s) => s.length > 1),
    `every batch to b carries ${NATIVE_BATCH} bytes of datagrams at most: ${batches.length} batches, the biggest ${Math.max(...sums)} B, up to ${Math.max(...batches.map((s) => s.length))} datagrams`);
  const tag = await sha(`peerwire/peer/${b.id}`);
  const frames = relays.flatMap((r) => r.events).filter((ev) => tagOf(ev) === tag).map(frame);
  check(frames.length >= batches.length && Math.max(...frames) <= BOARD_FRAME,
    `every event to b fits a ${BOARD_FRAME} B relay frame as a board reads it (a 64-character subscription): the biggest ${Math.max(...frames)} B`);
}

// 6b. The biggest batches to a native peer, through the relay (J4): 64 datagrams of 19 bytes (1,216 B, each one's
// base64 padded with two '=', the most per byte), and one of 1,232 B. Junk to b's endpoint, which drops it.
{
  const h = a.t.rtc.pipeByPeer.get(b.id);
  const tag = await sha(`peerwire/peer/${b.id}`);
  for (const [n, size] of [[64, 19], [1, NATIVE_BATCH]]) {
    await sleep(1500); // the pipes' budget whole, what was queued sent
    const marks = relays.map((r) => r.events.length);
    const from = batches.length;
    for (let i = 0; i < n; i++) a.ep.onTransmit(h, crypto.getRandomValues(new Uint8Array(size)));
    const ours = () => batches.slice(from).flat().filter((x) => x === size).length;
    await until(`${n} x ${size} B sent`, () => ours() >= n);
    await sleep(300);
    const sent = batches.slice(from);
    const sizes = relays.flatMap((r, i) => r.events.slice(marks[i])).filter((ev) => tagOf(ev) === tag).map(frame);
    const most = Math.max(...sent.map((d) => d.length));
    check(sent.every((d) => d.length <= 64 && d.reduce((x, y) => x + y, 0) <= NATIVE_BATCH) && most >= Math.min(n, 60) &&
      sizes.length && Math.max(...sizes) <= BOARD_FRAME,
    `${n} datagram(s) of ${size} B queued at once go in batches of up to ${most}: the biggest event is ${Math.max(...sizes)} B ` +
      `as a board reads it, under ${BOARD_FRAME}`);
  }
}

// 7. Pipe batches from a peer that said no hello, and an offer.
{
  const e = await client();
  let hs = null;
  e.ep.onTransmit = (_addr, bytes) => void (hs ??= bytes);
  e.ep.connect(1, a.id);
  await until('e makes a handshake', () => hs);
  const bad = a.t.rtc.badSignals;
  await e.sig.send(a.id, { kind: 'dg', d: [b64(hs)] });
  await until('a refuses it', () => a.t.rtc.badSignals > bad);
  await e.sig.send(a.id, { kind: 'offer', sdp: { type: 'offer', sdp: 'v=0\r\n' } });
  await sleep(500);
  check(!a.t.rtc.pipeByPeer.has(e.id) && !a.ep.isConnected(e.id) && a.t.rtc.links.size === 0,
    'a pipe batch from a peer that said no hello is refused (no pipe, no datagram taken, no link), and an offer goes unanswered');
  e.close();
}

// 7b. A batch is taken whole or not at all (J3): 1 to 64 strings, each the base64 of 1 to 1,232 bytes, from a peer
// allowed one; each datagram goes to the endpoint as from its sender alone.
{
  const got = [];
  const stub = { receive: (h, bytes, from) => got.push([bytes.length, from]), isConnected: () => false, set onTransmit(_) {} };
  const t = new WebRtcTransport(stub, { onMessage() {}, send() {}, close() {} }, { pipes: true, RTCPeerConnection: null });
  const p = 'ab'.repeat(32);
  t.allowPipe(p);
  const one = b64(Uint8Array.of(7));
  const most = b64(new Uint8Array(NATIVE_BATCH));
  const bad = [[], Array(65).fill(one), [one, 7], [one, ''], [one, b64(new Uint8Array(NATIVE_BATCH + 1))], [one, '!!!!'],
    [one, `${most}AAAA`], 'x', null];
  for (const d of bad) t.pipeReceive(p, d);
  const refused = t.badSignals;
  t.pipeReceive('cd'.repeat(32), [one]); // from a peer not allowed one
  t.pipeReceive(p, [one, most]);
  check(refused === bad.length && t.badSignals === bad.length + 1 && JSON.stringify(got) === JSON.stringify([[1, p], [NATIVE_BATCH, p]]),
    'a batch is taken whole or not at all: none of [], 65 items, a number, an empty datagram, 1,233 B, bad base64, ' +
    'base64 too long, or from a peer not allowed; 1 B and 1,232 B taken, as from their sender');
  t.close();
}

// 8. drop() says goodbye at once; a dial again links.
{
  b.events.length = 0;
  a.t.drop(b.id);
  await until('b sees a go', () => b.events.some((e) => e.type === 'disconnected' && e.peer === a.id), 5000);
  check(!a.t.has(b.id) && !a.ep.isConnected(b.id), 'drop(): b sees a go at once, a has no link to b');
  const again = await outcome(a.t.dial(b.id));
  check(again.v === b.id && a.t.has(b.id), `a dial after drop() links again: ${again.v ?? again.e}`);
}

// 9. Dials that fail, and close().
{
  RelayTransport.dialMs = 1500;
  const t0 = Date.now();
  const nobody = pw.endpoint(); // a PeerId nobody in the room has
  const lost = await settled(a.t.dial(nobody.id));
  check(lost?.code === 'timeout' && Date.now() - t0 >= 1400 && !a.t.rtc.pipeAllowed.has(nobody.id),
    `a dial nobody answers rejects with 'timeout' after dialMs (${lost?.code}), and a dial allows no pipe batches from a peer not heard`);
  RelayTransport.dialMs = 15000;
  const bad = await settled(a.t.dial('not a peer'));
  check(bad instanceof TypeError, `dial('not a peer'): ${bad}`);
  // An onPeer that throws: the peer is still listed, and its dial still resolves.
  const y = node({ onPeer: () => {
    throw new Error('the app');
  } });
  await until('y lists a and b despite its onPeer', () => y.t.peers.includes(a.id) && y.t.peers.includes(b.id));
  const yd = await outcome(y.t.dial(b.id));
  check(yd.v === b.id, `an onPeer that throws breaks nothing: y lists a and b, and its dial resolves: ${yd.v ?? yd.e}`);
  end(y);
  const under = settled(a.t.dial(nobody.id));
  a.t.close();
  const [u, after] = [await under, await settled(a.t.dial(b.id))];
  await until("a's relay sockets close", () => a.t.signaling.sockets.every((ws) => ws.readyState === WebSocket.CLOSED));
  check(u?.code === 'closed' && after?.code === 'closed', `close(): a dial under way rejects with 'closed', and one after (${u?.code}, ${after?.code}); a leaves the relays`);
  a.ep.close();
  nobody.close();
}

// 10. The endpoint's datagrams to another transport's handles still go there.
{
  const x = pw.endpoint();
  const other = [];
  x.onTransmit = (addr) => other.push(addr);
  const tx = new RelayTransport(x, { appId: APP, roomId: ROOM, relays: urls });
  const z = pw.endpoint();
  x.connect(7777, z.id);
  await until('the other transport gets its datagram', () => other.includes(7777), 3000);
  check(other.includes(7777), "the endpoint's datagrams to another transport's handle still go to it");
  tx.close();
  x.close();
  z.close();
}

end(b);
relays.forEach((r) => r.close());
console.log(failed ? `relay link: ${failed} check(s) failed` : 'relay link: all passed');
process.exit(failed ? 1 : 0);
