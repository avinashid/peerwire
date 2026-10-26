// joinRoom's fallback when WebRTC can't link two members (both behind symmetric
// NATs, no TURN). A fake in-process RTCPeerConnection links any two members
// except the pairs in `blocked`, whose ICE fails. Checks:
// - three members, one pair blocked: the pair links through the third member, and
//   signalling never joins the dial before Room.peerFirstMs, even when WebRTC fails
//   before the third has proved the key; messages and presence flow;
// - three members, one pair whose ICE never ends (its link fails at fallbackMs), and a
//   third that never answers: the dialler asks it at once, signalling joins the dial
//   Room.peerFirstMs after the failure, not before, and the pair links over it;
// - two members, blocked: with no member to ask, the dialler dials the signalling
//   channel (a pipe) as soon as WebRTC fails, and a 100 KB reliable message arrives
//   whole despite the pipe's rate cap;
// - fallback 'peers' with no one to relay, and 'off': no link, as before.
//
//   node packages/js/test/room_fallback.mjs
import { joinRoom, Room, Endpoint } from '../src/index.js';
import { MAX_PIPE_PEERS, WebRtcTransport } from '../src/webrtc.js';

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
};

const blocked = new Set(); // 'a|b', both orders
const hung = new Set(); // the same, for pairs whose ICE never ends: their link fails at fallbackMs
const pcs = new Map(); // token -> FakePc
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

// An RTCPeerConnection class for member `owner`.
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
    // The offerer applying the answer completes the pair.
    if (d.type === 'answer') setTimeout(() => this.connect(remote), 5);
  }
  connect(r) {
    if (hung.has(`${this.owner}|${r.owner}`)) return;
    if (blocked.has(`${this.owner}|${r.owner}`)) {
      for (const pc of [this, r]) {
        pc.connectionState = 'failed';
        pc.onconnectionstatechange?.();
      }
      return;
    }
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
const block = (x, y) => (blocked.add(`${x}|${y}`), blocked.add(`${y}|${x}`));
const hang = (x, y) => (hung.add(`${x}|${y}`), hung.add(`${y}|${x}`));

// Every fallback step, timed: rtcFailed (with how many members it could ask), and the
// endpoint dialling a peer through a member (connectVia) or by address (connect: for a
// blocked pair, only ever its pipe). `muteVia` drops dials through a member, as if
// none answered.
const steps = [];
let muteVia = false;
const rtcFailed = Room.prototype.rtcFailed;
Room.prototype.rtcFailed = function (peer) {
  const vias = [...this.routes].filter(([p, r]) => p !== peer && r.kind === 'direct' && this.proven.has(p)).length;
  steps.push({ kind: 'failed', from: this.self, peer, vias, at: performance.now() });
  return rtcFailed.call(this, peer);
};
for (const kind of ['connect', 'connectVia']) {
  const dial = Endpoint.prototype[kind];
  Endpoint.prototype[kind] = function (x, peer) {
    steps.push({ kind, from: this.id, peer, at: performance.now() });
    if (!(kind === 'connectVia' && muteVia)) return dial.call(this, x, peer);
  };
}
// How x|y's dialler (the smaller id) fell back: the members it could ask when WebRTC
// failed, and how many ms after that it dialled through one and over the pipe (null: never).
function fallbackOf(x, y) {
  const [from, peer] = [x.self, y.self].sort();
  const first = (kind) => steps.find((s) => s.kind === kind && s.from === from && s.peer === peer);
  const failed = first('failed');
  if (!failed) fail('no rtcFailed on the dialling side');
  const after = (kind) => (first(kind) ? Math.round(first(kind).at - failed.at) : null);
  return { vias: failed.vias, via: after('connectVia'), pipe: after('connect') };
}

let n = 0;
const room = (name, extra) => joinRoom({
  appId: 'peerwire-test',
  roomId: `fallback-${process.pid}-${n}`,
  presence: { name },
  fallbackMs: 300,
  RTCPeerConnection: fakePc(name),
  ...extra,
});

// 1. A and B can't link; C links to both. A|B's ICE fails at once, often before C has
// proved the key: the dialler knows C all the same, so signalling waits for it.
n = 1;
block('a1', 'b1');
const [a, b, c] = await Promise.all(['a1', 'b1', 'c1'].map((m) => room(m)));
await until('a full mesh through the fallback', () => [a, b, c].every((r) => r.peers.length === 2), 30000);
const ab = a.route(b.self);
// A member relaying is preferred: signalling joins the dial only if it doesn't answer.
if (ab?.kind !== 'peer' || ab.via !== c.self) fail(`a reaches b ${JSON.stringify(ab)}, not through c`);
const fb1 = fallbackOf(a, b);
if (fb1.via == null || (fb1.pipe != null && fb1.pipe < Room.peerFirstMs)) fail(`3 members: ${JSON.stringify(fb1)}`);
if (a.route(c.self)?.kind !== 'direct' || b.route(c.self)?.kind !== 'direct') fail('a or b not direct to c');
if (b.getPresence(a.self)?.name !== 'a1') fail('presence over the fallback');
const atB = [];
const atA = [];
b.on('message', (d, from) => from === a.self && atB.push(d));
a.on('message', (d, from) => from === b.self && atA.push(d));
a.send('over the fallback', { to: b.self });
b.send({ back: true }, { to: a.self });
await until('messages both ways', () => atB.length === 1 && atA.length === 1);
if (atB[0] !== 'over the fallback' || atA[0]?.back !== true) fail('wrong messages');
console.log(`3 members, a|b blocked: c ${fb1.vias ? 'had' : 'had not yet'} proved the key when WebRTC failed, asked ${fb1.via} ms after, ${fb1.pipe == null ? 'the pipe never dialled' : `the pipe ${fb1.pipe} ms after`}; a reaches b relayed by c, messages and presence both ways`);
await Promise.all([a.leave(), b.leave(), c.leave()]);

// 1b. A|B's ICE never ends, so its link fails at fallbackMs (2 s), as on a hotspot, by
// when C has linked to both and proved the key; C never answers: the dialler asks it
// at once, and signalling joins after Room.peerFirstMs.
n = 'mute';
hang('a1b', 'b1b');
muteVia = true;
const [f, g, h] = await Promise.all(['a1b', 'b1b', 'c1b'].map((m) => room(m, { fallbackMs: 2000 })));
await until('a|b linked over signalling', () => f.peers.includes(g.self) && g.peers.includes(f.self), 30000);
const fb1b = fallbackOf(f, g);
if (fb1b.vias !== 1 || fb1b.via == null || fb1b.via > 50 || fb1b.pipe == null || fb1b.pipe < Room.peerFirstMs - 5) fail(`3 members, no answer: ${JSON.stringify(fb1b)}`);
console.log(`3 members, a|b ICE never ends, c never answers: c asked ${fb1b.via} ms after WebRTC failed, the pipe ${fb1b.pipe} ms after (peerFirstMs ${Room.peerFirstMs}); linked`);
muteVia = false;
await Promise.all([f.leave(), g.leave(), h.leave()]);

// 2. Two members, blocked: only signalling is left.
n = 2;
block('a2', 'b2');
const [d, e] = await Promise.all(['a2', 'b2'].map((m) => room(m)));
await until('a link over signalling', () => d.peers.length === 1 && e.peers.length === 1, 30000);
if (d.route(e.self)?.kind !== 'signaling' || e.route(d.self)?.kind !== 'signaling') fail(`routes ${JSON.stringify([d.route(e.self), e.route(d.self)])}`);
// No member to ask: the pipe at once, not after Room.peerFirstMs.
const fb2 = fallbackOf(d, e);
if (fb2.vias !== 0 || fb2.via != null || fb2.pipe == null || fb2.pipe > 50) fail(`2 members: ${JSON.stringify(fb2)}`);
const big = new Uint8Array(100 * 1024).map((_, i) => i * 7);
const gotBig = [];
e.on('message', (m) => gotBig.push(m));
const t0 = Date.now();
d.send(big);
await until('100 KB over signalling', () => gotBig.length === 1, 30000);
if (gotBig[0].length !== big.length || gotBig[0].some((x, i) => x !== big[i])) fail('100 KB corrupted');
const piped = d.transport.piped;
console.log(`2 members, blocked: the pipe dialled ${fb2.pipe} ms after WebRTC failed (no one else to ask); linked over signalling; 100 KB arrived whole in ${Date.now() - t0} ms (${piped.sent} batches sent)`);
await Promise.all([d.leave(), e.leave()]);

// 3. 'peers' with no third member, and 'off': WebRTC only, so no link.
for (const fallback of ['peers', 'off']) {
  n++;
  block(`a-${fallback}`, `b-${fallback}`);
  const rs = await Promise.all([`a-${fallback}`, `b-${fallback}`].map((m) => room(m, { fallback })));
  await sleep(1500);
  if (rs.some((r) => r.peers.length)) fail(`fallback '${fallback}' linked a blocked pair`);
  await Promise.all(rs.map((r) => r.leave()));
}
console.log("fallback 'peers' alone and 'off': a blocked pair stays apart");

// 4. NB43-1: proven peers that send a 'dg' and never link don't pile up pipes.
{
  const linked = '0'.repeat(64);
  const ep = { id: 'f'.repeat(64), receive() {}, isConnected: (p) => p === linked, set onTransmit(_) {} };
  const sig = { onMessage() {}, send() {}, close() {} };
  const t = new WebRtcTransport(ep, sig, { pipes: true, RTCPeerConnection: fakePc('x') });
  t.allowPipe(linked);
  t.pipeReceive(linked, ['AAAA']);
  for (let i = 1; i <= 1000; i++) {
    const id = i.toString(16).padStart(64, '1');
    t.allowPipe(id);
    t.pipeReceive(id, ['AAAA']);
  }
  if (t.pipeAllowed.size > MAX_PIPE_PEERS || t.pipes.size > MAX_PIPE_PEERS + 1 || t.pipeByPeer.size !== t.pipes.size) fail(`pipes not bounded: allowed ${t.pipeAllowed.size}, pipes ${t.pipes.size}`);
  if (!t.pipeByPeer.has(linked)) fail("an evicted peer's pipe was dropped while its session used it");
  t.close();
  console.log(`1,000 proven peers with a 'dg' each: ${t.pipeAllowed.size} allowed, pipes bounded; a linked peer's pipe kept`);
}

console.log('room_fallback: ok');
process.exit(0);
