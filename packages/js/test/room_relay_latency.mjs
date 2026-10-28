// Fast lane 3: a link relayed by a member takes another member that relays it clearly
// faster, and stays there. A fake in-process RTCPeerConnection links any two members except
// a and b, whose ICE fails, so a and b link over signalling, then through a member. Each
// DataChannel delivers after a delay set by the members it joins: c's take 60 ms, d's 2 ms.
// Checks:
// - c joins: a and b move their link to c; route(peer).rttMs, once data has been
//   acknowledged, is c's round trip (4 channel hops of 60 ms, near 240 ms);
// - d joins: the room offers d for the link on c too, the core races the two, and a and b
//   move to d (route { kind: 'peer', via: d }), once each, with no peer-leave or peer-join;
//   rttMs is then d's round trip, well under c's;
// - the link stays on d for the next 6 s (no flapping), and messages flow through d.
//
//   node packages/js/test/room_relay_latency.mjs
import { joinRoom } from '../src/index.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  process.exit(1);
};
process.on('unhandledRejection', (e) => fail(`unhandled: ${e?.stack ?? e}`));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const until = async (what, cond, ms = 15000) => {
  const t = Date.now();
  while (!cond()) {
    if (Date.now() - t > ms) fail(`timed out waiting for ${what}`);
    await sleep(10);
  }
  return Date.now() - t;
};

const blocked = new Set(['a|b', 'b|a']);
const lag = { c: 60, d: 2 }; // ms a DataChannel of this member's takes, each way
const pcs = new Map(); // token -> FakePc
let tokens = 0;

class FakeDc {
  constructor() {
    this.readyState = 'connecting';
    this.bufferedAmount = 0;
    this.other = null;
    this.ms = 0;
  }
  send(bytes) {
    const o = this.other;
    const copy = bytes.slice().buffer;
    setTimeout(() => o.readyState === 'open' && o.onmessage?.({ data: copy }), this.ms);
  }
  open() {
    this.readyState = 'open';
    this.onopen?.();
  }
}

// An RTCPeerConnection class for member `owner` (as in room_relay_move.mjs).
const fakePc = (owner) => class FakePc {
  constructor() {
    this.owner = owner;
    this.token = String(++tokens);
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
    const remote = pcs.get(d.sdp.slice(5));
    if (!remote) throw new Error('unknown remote');
    this.remote = remote;
    if (d.type === 'answer') setTimeout(() => this.connect(remote), 5);
  }
  connect(r) {
    if (blocked.has(`${this.owner}|${r.owner}`)) {
      for (const pc of [this, r]) {
        pc.connectionState = 'failed';
        pc.onconnectionstatechange?.();
      }
      return;
    }
    this.dc.other = r.dc;
    r.dc.other = this.dc;
    this.dc.ms = r.dc.ms = (lag[this.owner] ?? 0) + (lag[r.owner] ?? 0);
    for (const pc of [this, r]) pc.connectionState = 'connected';
    this.dc.open();
    r.dc.open();
  }
  addEventListener() {}
  async addIceCandidate() {}
  close() {
    this.connectionState = 'closed';
    if (this.dc) this.dc.readyState = 'closed';
    pcs.delete(this.token);
  }
};

const room = (name) => joinRoom({
  appId: 'peerwire-test',
  roomId: `relay-latency-${process.pid}`,
  presence: { name },
  fallbackMs: 300,
  RTCPeerConnection: fakePc(name),
});
const via = (r, peer) => (r.route(peer)?.kind === 'peer' ? r.route(peer).via : null);

const [a, b] = await Promise.all(['a', 'b'].map(room));
const log = [];
const moves = { a: [], b: [] };
for (const [name, r, other] of [['a', a, b], ['b', b, a]]) {
  r.on('peer-join', (p) => p === other.self && log.push(`${name} join`));
  r.on('peer-leave', (p) => p === other.self && log.push(`${name} leave`));
  const ev = r.endpoint.onEvent;
  r.endpoint.onEvent = (e) => {
    if (e.type === 'pathChanged' && e.peer === other.self) moves[name].push(e.path.via ?? e.path.direct);
    ev(e);
  };
}
await until('a and b linked over signalling', () => a.peers.length === 1 && b.peers.length === 1, 30000);
const joins = log.length;

// Messages each way between a and b; resolves once both arrived.
const both = async (tag) => {
  const got = [];
  const offA = a.on('message', (d, from) => from === b.self && d === `${tag} to a` && got.push('a'));
  const offB = b.on('message', (d, from) => from === a.self && d === `${tag} to b` && got.push('b'));
  a.send(`${tag} to b`, { to: b.self });
  b.send(`${tag} to a`, { to: a.self });
  await until(`messages both ways (${tag})`, () => got.length === 2, 15000);
  offA();
  offB();
};

// rttMs over a's route to b, once a message a sends there is acknowledged (one every 200 ms).
const timed = async (what) => {
  for (let i = 0; i < 25 && a.route(b.self).rttMs == null; i++) {
    a.send('tick', { to: b.self });
    await sleep(200);
  }
  const ms = a.route(b.self).rttMs;
  if (typeof ms !== 'number') fail(`no rttMs ${what}`);
  return ms;
};

const c = await room('c');
await until('a and b on c', () => via(a, b.self) === c.self && via(b, a.self) === c.self, 30000);
await both('through c');
const onC = await timed('through c');
if (onC < 200) fail(`rttMs through c is ${onC}, well under c's 4 hops of 60 ms`);
console.log(`c joins: a and b on c, rttMs ${onC}`);

moves.a.length = 0;
moves.b.length = 0;
const d = await room('d');
const took = await until('a and b on d', () => via(a, b.self) === d.self && via(b, a.self) === d.self, 30000);
if (moves.a.join() !== d.self || moves.b.join() !== d.self) fail(`not one move each, to d: ${JSON.stringify(moves)}`);
const relayed = d.stats().relayed;
await both('through d');
if (d.stats().relayed <= relayed) fail('the messages did not go through d');
const onD = await timed('through d');
if (!(onD < onC / 2)) fail(`rttMs through d is ${onD}, not well under c's ${onC}`);
console.log(`d joins: a and b take d ${took} ms after d's room resolved, once each, rttMs ${onD}`);

await sleep(6000);
if (moves.a.length !== 1 || moves.b.length !== 1 || via(a, b.self) !== d.self) fail(`the link moved again: ${JSON.stringify(moves)}`);
if (log.length !== joins) fail(`a and b saw each other leave or join: ${log.slice(joins)}`);
await both('still through d');
console.log('the link stays on d, the same session');
await Promise.all([a, b, c, d].map((r) => r.leave()));
console.log('room_relay_latency: ok');
process.exit(0);
