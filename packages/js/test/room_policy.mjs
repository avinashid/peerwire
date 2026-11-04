// The room's connection policy (joinRoom's `policy`, room.setPolicy(), src/policy.js), over a fake
// in-process RTCPeerConnection that links any two members except blocked pairs, whose ICE fails, unless
// one end has a TURN server and the pair is one TURN can link. Checks:
// - validation: unknown paths, an empty or repeated order, fallback and policy.order together, a bad
//   ICE server (the message names no URL and no credential), relays without Nostr, privacy after join;
//   room.policy shows no credential, and an entry from it given back keeps its own;
// - the default: every path, as `fallback: 'auto'`, connections configured with every server and no
//   `turn` in offers; `fallback: 'peers'` and `'off'` as orders;
// - ['direct'] only, with a member that could relay: the blocked pair stays apart, nothing dialled
//   through the member or over signalling, TURN servers given to no connection;
// - TURN after a fallback (['direct', 'peers', 'turn']): the first attempt leaves TURN out on both ends,
//   the TURN attempt comes once the fallback has nothing, relay-only, and route() says `turn: true`; with
//   a member to relay, no TURN attempt, and retries leave TURN out; ['turn'] with no TURN server dials nothing;
//   after a pipe the peer doesn't take (['direct', 'signaling', 'turn']), TURN once the core's dial gives up;
// - a fallback first: ['signaling', 'direct'] links over signalling with no WebRTC at all, a peer whose
//   policy has no WebRTC path answers no offer, and a link on 'peers' under ['peers', 'direct'] is never
//   retried over WebRTC;
// - a peer that dials a path ours has off (through a member, ours without 'peers'): the link moves to
//   signalling; with 'signaling' before 'peers', a member isn't dialled while the pipe gets its head start,
//   and a peer whose policy offers a member doesn't pull the pipe link onto it;
// - live: dropping 'peers' moves a relayed link to signalling (and it stays there), or drops it
//   ('peer-leave' reason 'policy') with no signalling, or when the peer won't take the pipe, after
//   Room.moveMs; dropping 'signaling' moves a pipe link to a member, or drops it (the peer sees it go at
//   once, and no pipe is made after); new iceServers go to
//   every connection with an ICE restart for a link over TURN; turning 'turn' off drops a TURN link,
//   which links again over what is left;
// - live relays (Nostr, an in-process relay per URL): a swap keeps the link and its messages, keeps the
//   socket of a relay in both sets, closes the one dropped, and says hello on the one added.
//
//   node packages/js/test/room_policy.mjs
import { joinRoom, Room, Endpoint } from '../src/index.js';
import { peerwireModule } from '../src/room.js';
import { WebRtcTransport } from '../src/webrtc.js';
import { hasTurn } from '../src/policy.js';

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
const rejects = async (what, p, check = () => true) => {
  try {
    await (typeof p === 'function' ? p() : p);
  } catch (e) {
    if (!(e instanceof TypeError)) fail(`${what}: ${e?.name} ${e?.message}, not a TypeError`);
    if (!check(e)) fail(`${what}: ${e.message}`);
    return e;
  }
  fail(`${what}: no TypeError`);
};

Room.moveMs = 1500;
Room.peerFirstMs = 300;

const blocked = new Set(); // 'a|b', both orders: direct ICE fails
const turnable = new Set(); // blocked pairs that link through a TURN server on either end
const hung = new Set(); // the same, for pairs whose ICE never ends: their link fails at fallbackMs
const pcs = new Map(); // token -> FakePc
const made = []; // every FakePc made
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
    if (this.readyState === 'open') return;
    this.readyState = 'open';
    this.onopen?.();
  }
}

const relayOnly = (pc) => pc.config.iceTransportPolicy === 'relay';
const turnIn = (pc) => hasTurn(pc.config.iceServers ?? []);

// An RTCPeerConnection class for member `owner`.
const fakePc = (owner) => class FakePc {
  constructor(config = {}) {
    this.owner = owner;
    this.token = String(++tokens);
    this.config = config;
    this.configs = [config];
    this.turn = false; // our end of the selected pair is a relay one
    this.connectionState = 'new';
    this.iceGatheringState = 'complete';
    this.localDescription = null;
    this.restarts = 0;
    pcs.set(this.token, this);
    made.push(this);
  }
  get sctp() {
    const pc = this;
    return { transport: { iceTransport: { getSelectedCandidatePair: () => (pc.dc?.readyState === 'open' ? { local: { type: pc.turn ? 'relay' : 'host' } } : null) } } };
  }
  setConfiguration(c) {
    this.config = c;
    this.configs.push(c);
  }
  createDataChannel() {
    this.dc = new FakeDc();
    return this.dc;
  }
  async createOffer(o) {
    if (o?.iceRestart) this.restarts++;
    return { type: 'offer', sdp: `fake:${this.token}:${this.restarts}` };
  }
  async createAnswer() {
    return { type: 'answer', sdp: `fake:${this.token}:0` };
  }
  async setLocalDescription(d) {
    this.localDescription = { ...d, toJSON: () => ({ type: d.type, sdp: d.sdp }) };
  }
  async setRemoteDescription(d) {
    const remote = pcs.get(d.sdp.split(':')[1]);
    if (!remote) throw new Error('unknown remote');
    this.remote = remote;
    if (d.type === 'answer') setTimeout(() => this.connect(remote), 5);
  }
  // ICE: direct unless the pair is blocked or an end takes relay candidates alone; else through TURN, if the
  // ends that need it have a TURN server (a blocked pair: either end, if TURN can link it).
  connect(r) {
    if (this.dc.readyState === 'open' || hung.has(`${this.owner}|${r.owner}`)) return;
    let ok = true;
    let mine = false;
    let theirs = false;
    if (relayOnly(this) || relayOnly(r)) {
      ok = (!relayOnly(this) || turnIn(this)) && (!relayOnly(r) || turnIn(r));
      [mine, theirs] = [relayOnly(this), relayOnly(r)];
    } else if (blocked.has(`${this.owner}|${r.owner}`)) {
      ok = turnable.has(`${this.owner}|${r.owner}`) && (turnIn(this) || turnIn(r));
      [mine, theirs] = [turnIn(this), !turnIn(this)];
    }
    if (!ok) {
      for (const pc of [this, r]) {
        pc.connectionState = 'failed';
        pc.onconnectionstatechange?.();
      }
      return;
    }
    this.turn = mine;
    r.turn = theirs;
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
const block = (x, y, turn = false) => {
  for (const k of [`${x}|${y}`, `${y}|${x}`]) {
    blocked.add(k);
    if (turn) turnable.add(k);
  }
};
const hang = (x, y) => (hung.add(`${x}|${y}`), hung.add(`${y}|${x}`));
const unblock = (x, y) => {
  for (const k of [`${x}|${y}`, `${y}|${x}`]) blocked.delete(k);
};
const ofOwner = (o) => made.filter((pc) => pc.owner === o);

// Every dial through a member (connectVia) or by address (connect: for a blocked pair, a pipe).
const steps = [];
for (const kind of ['connect', 'connectVia']) {
  const dial = Endpoint.prototype[kind];
  Endpoint.prototype[kind] = function (x, peer) {
    steps.push({ kind, from: this.id, peer, x });
    return dial.call(this, x, peer);
  };
}
// The offers each transport publishes.
const offers = [];
const publish = WebRtcTransport.prototype.publish;
WebRtcTransport.prototype.publish = function (peer, msg) {
  if (msg.kind === 'offer') offers.push({ from: this.ep.id, to: peer, msg });
  return publish.call(this, peer, msg);
};

// Two identities, the one with the smaller PeerId (it dials) first.
const pw = await peerwireModule();
const ordered = () => {
  const keys = [0, 1].map(() => crypto.getRandomValues(new Uint8Array(32)));
  const ids = keys.map((k) => {
    const ep = pw.endpoint({ secret: k });
    const id = ep.id;
    ep.close();
    return id;
  });
  return ids[0] < ids[1] ? keys : [keys[1], keys[0]];
};

const STUN = { urls: 'stun:stun.test:3478' };
const TURN = { urls: ['turn:turn.test:3478', 'turns:turn.test:5349'], username: 'u-1', credential: 'SECRET-1' };
let n = 0;
const room = (name, extra) => joinRoom({
  appId: 'peerwire-test',
  roomId: `policy-${process.pid}-${n}`,
  presence: { name },
  fallbackMs: 300,
  RTCPeerConnection: fakePc(name),
  ...extra,
});
const leaves = (r) => {
  const out = [];
  r.on('peer-leave', (p, why) => out.push([p, why]));
  return out;
};
// A message each way between x and y arrives.
const talk = async (x, y, what) => {
  const got = [];
  const off = [y.on('message', (d, from) => from === x.self && got.push(['xy', d])), x.on('message', (d, from) => from === y.self && got.push(['yx', d]))];
  x.send(`${what} xy`, { to: y.self });
  y.send(`${what} yx`, { to: x.self });
  await until(`${what}: a message each way`, () => got.length === 2, 20000);
  off.forEach((f) => f());
};
const both = (rs) => Promise.all(rs.map((r) => r.leave()));

// 1. Validation.
{
  await rejects('an empty order', room('v', { policy: { order: [] } }));
  await rejects('an unknown path', room('v', { policy: { order: ['direct', 'udp'] } }), (e) => /'udp'/.test(e.message));
  await rejects('a path twice', room('v', { policy: { order: ['direct', 'direct'] } }));
  await rejects('an order not an array', room('v', { policy: { order: 'direct' } }));
  await rejects('fallback and policy.order', room('v', { fallback: 'off', policy: { order: ['direct'] } }));
  await rejects('iceServers both ways', room('v', { iceServers: [STUN], policy: { iceServers: [STUN] } }));
  await rejects('an unknown policy key', room('v', { policy: { orders: ['direct'] } }));
  await rejects('policy.relays without Nostr', room('v', { policy: { relays: ['wss://r.test'] } }));
  const bad = { urls: 'https://turn.test/?token=URLSECRET', username: 'USERSECRET', credential: 'CREDSECRET' };
  const e = await rejects('a server that is not STUN or TURN', room('v', { iceServers: [STUN, bad] }));
  if (/SECRET|https/.test(e.message)) fail(`the error names the server: ${e.message}`);

  n++;
  const r = await room('v1', { iceServers: [STUN, TURN] });
  const p = r.policy;
  if (p.order.join() !== 'direct,turn,peers,signaling' || p.fallbackMs !== 300 || p.birthday !== true || p.privacy !== 'direct' || p.relays !== null) fail(`default policy ${JSON.stringify(p)}`);
  if (JSON.stringify(p).includes('SECRET') || JSON.stringify(p).includes('u-1')) fail('room.policy shows a credential');
  if (p.iceServers[1].urls.join() !== TURN.urls.join()) fail('room.policy lost the urls');
  // An entry room.policy gave, given back, keeps its credentials.
  r.setPolicy({ iceServers: [...r.policy.iceServers, { urls: 'stun:other.test' }] });
  if (r.plan.iceServers[1].credential !== 'SECRET-1' || r.transport.iceServers[1] !== TURN) fail('an entry given back lost its credential');
  const before = JSON.stringify(r.policy);
  await rejects('setPolicy: an unknown path', () => r.setPolicy({ order: ['direct', 'bogus'] }));
  await rejects('setPolicy: privacy', () => r.setPolicy({ privacy: 'public-only' }));
  await rejects('setPolicy: relays with local signalling', () => r.setPolicy({ relays: ['wss://r.test'] }));
  await rejects('setPolicy: fallbackMs', () => r.setPolicy({ fallbackMs: -1 }));
  if (JSON.stringify(r.policy) !== before) fail('a setPolicy that threw changed the policy');
  r.setPolicy({ privacy: 'direct', fallbackMs: 500 });
  if (r.transport.fallbackMs !== 500 || r.transport.sameNatMs !== 500) fail(`fallbackMs live: ${r.transport.fallbackMs}, ${r.transport.sameNatMs}`);
  await r.leave();
  for (const [fallback, order] of [['peers', 'direct,turn,peers'], ['off', 'direct,turn']]) {
    const f = await room('v2', { fallback });
    if (f.policy.order.join() !== order || f.transport.pipesOn !== (fallback === 'auto')) fail(`fallback '${fallback}' is ${f.policy.order}`);
    await f.leave();
  }
  console.log('validation: TypeError for each, naming no server; room.policy shows urls only and an entry given back keeps its credential; fallback as orders');
}

// 2. The default: connections get every server, offers say nothing of TURN.
{
  n = 'default';
  made.length = 0;
  offers.length = 0;
  const [a, b] = await Promise.all(['d1', 'd2'].map((m) => room(m, { iceServers: [STUN, TURN] })));
  await until('a default link', () => a.peers.length === 1 && b.peers.length === 1);
  if (made.some((pc) => pc.config.iceServers.length !== 2 || pc.config.iceServers[1] !== TURN || 'iceTransportPolicy' in pc.config)) fail('a default connection is configured otherwise');
  if (offers.some((o) => 'turn' in o.msg)) fail('a default offer says turn');
  if (a.route(b.self)?.kind !== 'direct' || a.route(b.self).turn) fail(`default route ${JSON.stringify(a.route(b.self))}`);
  await both([a, b]);
  console.log('the default: every server on every connection, nothing of TURN in offers, a direct route');
}

// 3. ['direct'] only, a|b blocked, c could relay: a|b stays apart, nothing else dialled, no TURN anywhere.
{
  n = 3;
  made.length = 0;
  steps.length = 0;
  block('a3', 'b3', true);
  const policy = { order: ['direct'], iceServers: [STUN, TURN] };
  const [a, b, c] = await Promise.all(['a3', 'b3', 'c3'].map((m) => room(m, { policy })));
  await until('c linked to both', () => c.peers.length === 2 && a.peers.includes(c.self) && b.peers.includes(c.self));
  await sleep(1500);
  if (a.peers.includes(b.self) || b.peers.includes(a.self)) fail("['direct'] linked a blocked pair");
  // Only a DataChannel's own handshake dials by address, and none opened between a and b.
  const ab = steps.filter((s) => (s.from === a.self && s.peer === b.self) || (s.from === b.self && s.peer === a.self));
  if (ab.length) fail(`['direct'] dialled a|b: ${JSON.stringify(ab.map((s) => s.kind))}`);
  if (a.transport.pipesOn || a.transport.pipes.size || made.some((pc) => turnIn(pc))) fail('pipes on, or a TURN server given');
  // Nor to calls or birthday punches.
  if (hasTurn(a.calls.media.iceServers) || a.calls.media.iceTransportPolicy || hasTurn(a.punches.iceServers)) fail('a call or a punch gets TURN');
  await both([a, b, c]);
  console.log("['direct'] with a member that could relay: the blocked pair stays apart, nothing through c or signalling, no TURN server given");
}

// 4. TURN after a fallback. Two members blocked direct (TURN links them): the direct attempt leaves TURN out on both
// ends, the fallback has no member, so the TURN attempt follows: relay-only, offered as a retry, route turn.
{
  n = 4;
  made.length = 0;
  offers.length = 0;
  block('a4', 'b4', true);
  const policy = { order: ['direct', 'peers', 'turn'], iceServers: [STUN, TURN] };
  const [a, b] = await Promise.all(['a4', 'b4'].map((m) => room(m, { policy })));
  await until('a|b over TURN', () => a.peers.length === 1 && b.peers.length === 1);
  const [x, y] = a.self < b.self ? [a, b] : [b, a];
  const mine = ofOwner(x === a ? 'a4' : 'b4');
  const first = offers.filter((o) => o.from === x.self);
  if (first.length < 2 || first[0].msg.turn !== false || first[0].msg.retry || first[1].msg.turn === false || first[1].msg.retry !== true) fail(`offers ${JSON.stringify(first.map((o) => [o.msg.turn, o.msg.retry]))}`);
  if (turnIn(mine[0]) || !relayOnly(mine[1]) || !turnIn(mine[1]) || mine[1].config.iceServers.some((s) => [s.urls].flat().some((u) => u.startsWith('stun:')))) fail('the dialler configured its attempts otherwise');
  const theirs = ofOwner(y === a ? 'a4' : 'b4');
  if (turnIn(theirs[0]) || !turnIn(theirs[1])) fail("the answerer didn't follow the offers' turn");
  if (!x.route(y.self)?.turn || x.route(y.self).kind !== 'direct') fail(`route ${JSON.stringify(x.route(y.self))}`);
  await talk(a, b, 'over TURN');
  await both([a, b]);

  // With a member to relay, the fallback links and no TURN attempt is made; a retry leaves TURN out too. The pair's
  // ICE never ends, so its link fails at fallbackMs, by when c has linked to both: a stage with nothing to dial
  // fails at once (as 'peers' alone always has), and TURN would follow.
  n = '4b';
  made.length = 0;
  hang('a4b', 'b4b');
  const [d, e, f] = await Promise.all(['a4b', 'b4b', 'c4b'].map((m) => room(m, { policy, fallbackMs: 1500 })));
  await until('a|b relayed by c', () => d.route(e.self)?.kind === 'peer' && e.route(d.self)?.kind === 'peer', 20000);
  await sleep(500);
  if (made.some((pc) => turnIn(pc))) fail('a TURN attempt with a member relaying');
  const [dx, dy] = d.self < e.self ? [d, e] : [e, d];
  const was = made.length;
  dx.retryDirect(dy.self);
  await until('a retry', () => made.length > was);
  if (turnIn(made[was])) fail('a retry of a relayed link used TURN, which comes after peers');
  await both([d, e, f]);

  // ['turn'] with no TURN server: nothing to dial.
  n = '4c';
  made.length = 0;
  const [g, h] = await Promise.all(['a4c', 'b4c'].map((m) => room(m, { policy: { order: ['turn'], iceServers: [STUN] } })));
  if (g.calls.media.iceTransportPolicy !== 'relay' || g.calls.media.iceServers.length) fail("a call under ['turn'] isn't relay-only");
  await sleep(800);
  if (made.length || g.peers.length) fail("['turn'] with no TURN server dialled");
  await both([g, h]);

  // ['direct', 'signaling', 'turn'], dialling a peer that takes no pipe: the pipe dial gets no answer, the core gives
  // up on it (10 s), and the TURN attempt follows.
  n = '4d';
  made.length = 0;
  steps.length = 0;
  block('a4d', 'b4d', true);
  const [lo, hi] = ordered();
  const t0 = Date.now();
  const [x4, y4] = await Promise.all([
    room('a4d', { secret: lo, policy: { order: ['direct', 'signaling', 'turn'], iceServers: [STUN, TURN] } }),
    room('b4d', { secret: hi, policy: { order: ['direct', 'turn'], iceServers: [STUN, TURN] } }),
  ]);
  await until('a|b over TURN after the pipe', () => x4.peers.length === 1 && y4.peers.length === 1, 30000);
  const piped = steps.some((s) => s.kind === 'connect' && s.from === x4.self && s.peer === y4.self && x4.transport.isPipe(s.x));
  if (!piped || !x4.route(y4.self)?.turn || Date.now() - t0 < 10000) fail(`pipe ${piped}, route ${JSON.stringify(x4.route(y4.self))}, ${Date.now() - t0} ms`);
  await both([x4, y4]);
  console.log("TURN after a fallback: the first attempt without TURN on both ends, then one relay-only (route turn); with a member relaying none, and retries without TURN; ['turn'] with no server dials nothing; after a pipe the peer doesn't take, once the core gave up");
}

// 5. A fallback first: over signalling with no WebRTC at all, never moving to it; on a member under
// ['peers', 'direct'], never retried.
{
  n = 5;
  made.length = 0;
  const [a, b] = await Promise.all(['a5', 'b5'].map((m) => room(m, { policy: { order: ['signaling', 'direct'] } })));
  await until('a|b over signalling', () => a.route(b.self)?.kind === 'signaling' && b.route(a.self)?.kind === 'signaling');
  await sleep(500);
  if (made.length || a.retryDirect() || b.retryDirect()) fail('WebRTC with signaling first');
  await talk(a, b, 'signalling first');
  await both([a, b]);

  // A peer with no WebRTC path answers no offer: the default side dials it, its WebRTC attempt goes unanswered and
  // fails, and its fallback links over signalling.
  n = '5c';
  made.length = 0;
  const [lo, hi] = ordered();
  const [x5, y5] = await Promise.all([room('a5c', { secret: lo }), room('b5c', { secret: hi, policy: { order: ['signaling'] } })]);
  await until('a|b over signalling', () => x5.route(y5.self)?.kind === 'signaling' && y5.peers.includes(x5.self) && x5.peers.includes(y5.self), 20000);
  if (ofOwner('b5c').length || ofOwner('a5c').length !== 1) fail(`connections made: ${ofOwner('a5c').length} by the dialler, ${ofOwner('b5c').length} by the peer with no WebRTC path`);
  await both([x5, y5]);

  n = '5b';
  block('a5b', 'b5b');
  const [d, e, f] = await Promise.all(['a5b', 'b5b', 'c5b'].map((m) => room(m)));
  await until('a|b relayed by c', () => d.route(e.self)?.kind === 'peer' && e.route(d.self)?.kind === 'peer', 20000);
  for (const r of [d, e]) r.setPolicy({ order: ['peers', 'direct'] });
  unblock('a5b', 'b5b');
  const tried = d.transport.retried + e.transport.retried;
  if (d.retryDirect(e.self) || e.retryDirect(d.self) || d.transport.has(e.self) || e.transport.has(d.self)) fail('a link on peers under [peers, direct] retried, or its failed attempt kept');
  await sleep(1000);
  if (d.transport.retried + e.transport.retried !== tried || d.route(e.self)?.kind !== 'peer') fail('it moved to direct');
  await both([d, e, f]);
  console.log("a fallback first: ['signaling', 'direct'] links with no WebRTC; a peer with no WebRTC path answers no offer; a link on peers under ['peers', 'direct'] is never retried");
}

// 6. Live: 'peers' off. a|b relayed by c.
{
  // a. To signalling: both ends move, and stay, past the core's punch time.
  n = 6;
  block('a6', 'b6');
  const [a, b, c] = await Promise.all(['a6', 'b6', 'c6'].map((m) => room(m)));
  await until('a|b relayed by c', () => a.route(b.self)?.kind === 'peer' && b.route(a.self)?.kind === 'peer', 20000);
  const left = leaves(a);
  a.setPolicy({ order: ['direct', 'turn', 'signaling'] });
  await until('a|b over signalling', () => a.route(b.self)?.kind === 'signaling' && b.route(a.self)?.kind === 'signaling', 10000);
  await sleep(6000); // b still offers c (its policy has peers): a mustn't take it
  if (a.route(b.self)?.kind !== 'signaling' || left.length) fail(`a went back to ${JSON.stringify(a.route(b.self))}, or left ${JSON.stringify(left)}`);
  await talk(a, b, 'moved to signalling');
  await both([a, b, c]);

  // b. With no signalling: dropped at once, reason 'policy', and apart after.
  n = '6b';
  block('a6b', 'b6b');
  const [d, e, f] = await Promise.all(['a6b', 'b6b', 'c6b'].map((m) => room(m)));
  await until('a|b relayed by c', () => d.route(e.self)?.kind === 'peer' && d.peers.includes(e.self), 20000);
  const dl = leaves(d);
  const el = leaves(e);
  d.setPolicy({ order: ['direct'] });
  if (dl.length !== 1 || dl[0][0] !== e.self || dl[0][1] !== 'policy') fail(`d's leaves ${JSON.stringify(dl)}`);
  await until('e sees d go', () => el.length === 1, 5000);
  if (el[0][1] !== 'closed') fail(`e saw d go for ${el[0][1]}`);
  await sleep(2500);
  if (d.peers.includes(e.self) || e.peers.includes(d.self) || f.peers.length !== 2) fail('linked again, or c lost one');
  await both([d, e, f]);

  // c. The peer takes no pipe (its policy has no signalling): dropped after Room.moveMs.
  n = '6c';
  block('a6c', 'b6c');
  const [g, h, i] = await Promise.all([room('a6c'), room('b6c', { fallback: 'peers' }), room('c6c')]);
  await until('a|b relayed by c', () => g.route(h.self)?.kind === 'peer' && g.peers.includes(h.self), 20000);
  const gl = leaves(g);
  const t0 = Date.now();
  g.setPolicy({ order: ['direct', 'turn', 'signaling'] });
  await until('g drops h', () => gl.length === 1, 10000);
  const took = Date.now() - t0;
  if (gl[0][1] !== 'policy' || took < Room.moveMs - 50) fail(`dropped in ${took} ms for ${gl[0][1]}`);
  await both([g, h, i]);
  console.log(`'peers' off live: a relayed link moved to signalling and stayed; with no signalling dropped at once ('policy'); with a peer that takes no pipe, after ${took} ms`);

  // d. A peer dials us through a member, a path ours has off: the link moves to signalling, and stays linked.
  n = '6d';
  hang('a6d', 'b6d');
  const [lo, hi] = ordered();
  Room.peerFirstMs = 5000; // so the dialler's pipe doesn't race its member
  const [j, k, m] = await Promise.all([
    room('a6d', { secret: lo, fallbackMs: 1500 }),
    room('b6d', { secret: hi, fallbackMs: 1500, policy: { order: ['direct', 'turn', 'signaling'] } }),
    room('c6d', { fallbackMs: 1500 }),
  ]);
  await until('k on signalling with j', () => k.route(j.self)?.kind === 'signaling' && k.peers.includes(j.self) && j.peers.includes(k.self), 20000);
  if (!steps.some((s) => s.kind === 'connectVia' && s.from === j.self && s.peer === k.self)) fail('j never dialled k through m');
  await talk(j, k, 'landed on peers, moved');
  Room.peerFirstMs = 300;
  await both([j, k, m]);
  console.log("a peer dialled through a member, ours without 'peers': the link moved to signalling and stayed linked");
}

// 7. Live: 'signaling' off. a|b on the pipe, and a member that could relay under signaling-before-peers.
{
  // a dials b, signalling before members; c is linked to both by the time a|b fails (its ICE never ends). The pipe
  // goes at once and the member not within its head start. b's policy (the default) offers c for the pipe link,
  // which a doesn't take: peers comes after signaling.
  n = 7;
  steps.length = 0;
  hang('a7', 'b7');
  const [lo, hi] = ordered();
  Room.peerFirstMs = 2000;
  const [a, b, c] = await Promise.all([
    room('a7', { secret: lo, fallbackMs: 1500, policy: { order: ['direct', 'turn', 'signaling', 'peers'] } }),
    room('b7', { secret: hi, fallbackMs: 1500 }),
    room('c7', { fallbackMs: 1500 }),
  ]);
  await until('a|b over signalling', () => a.route(b.self)?.kind === 'signaling' && b.route(a.self)?.kind === 'signaling' && a.peers.includes(b.self) && b.peers.includes(a.self), 20000);
  const dials = steps.filter((s) => s.from === a.self && s.peer === b.self).map((s) => s.kind);
  if (dials.join() !== 'connect') fail(`a dialled b: ${dials}`);
  Room.peerFirstMs = 300;
  await sleep(2500); // b probes through c: a doesn't answer
  if (a.route(b.self)?.kind !== 'signaling' || b.route(a.self)?.kind !== 'signaling') fail(`moved to a later path: ${JSON.stringify([a.route(b.self), b.route(a.self)])}`);
  const left = leaves(a);
  for (const r of [a, b]) r.setPolicy({ order: ['direct', 'turn', 'peers'] });
  await until('a|b moved to c', () => a.route(b.self)?.kind === 'peer' && b.route(a.self)?.kind === 'peer', 10000);
  if (left.length || a.transport.pipes.size) fail(`left ${JSON.stringify(left)}, ${a.transport.pipes.size} pipes`);
  await talk(a, b, 'moved to c');
  await both([a, b, c]);

  // With no member: dropped.
  n = '7b';
  block('a7b', 'b7b');
  const [d, e] = await Promise.all(['a7b', 'b7b'].map((m) => room(m)));
  await until('a|b over signalling', () => d.route(e.self)?.kind === 'signaling' && d.peers.includes(e.self), 20000);
  const dl = leaves(d);
  const el = leaves(e);
  d.setPolicy({ order: ['direct', 'turn', 'peers'] });
  if (dl.length !== 1 || dl[0][1] !== 'policy') fail(`d's leaves ${JSON.stringify(dl)}`);
  // Its sealed close went at once, over the pipe; and no pipe is made from now on.
  await until('e sees d go', () => el.length === 1, 3000);
  if (d.transport.pipe('e'.repeat(64)) !== null) fail('a pipe made with signalling off');
  await both([d, e]);
  console.log("signalling before members: the pipe dialled at once, the member not, and a's pipe link not pulled onto c; 'signaling' off live: moved to c; with no member, dropped ('policy'), the peer told at once, and no pipe made after");
}

// 8. Live iceServers and 'turn'. a|b linked over TURN.
{
  n = 8;
  made.length = 0;
  block('a8', 'b8', true);
  const [a, b] = await Promise.all(['a8', 'b8'].map((m) => room(m, { iceServers: [STUN, TURN] })));
  await until('a|b over TURN', () => a.peers.length === 1 && b.peers.length === 1);
  const [x, y] = a.self < b.self ? [a, b] : [b, a];
  if (!x.route(y.self)?.turn) fail(`not over TURN: ${JSON.stringify(x.route(y.self))}`);
  const TURN2 = { urls: 'turn:turn2.test:3478', username: 'u-2', credential: 'SECRET-2' };
  const restarts = x.transport.restarts;
  for (const r of [a, b]) r.setPolicy({ iceServers: [STUN, TURN2] });
  const live = made.filter((pc) => pc.connectionState === 'connected');
  if (live.length !== 2 || live.some((pc) => pc.config.iceServers[1] !== TURN2)) fail('a connection kept its old servers');
  await until('the dialler restarted ICE', () => x.transport.restarts === restarts + 1, 3000);
  // 'turn' off: the TURN link drops and links again over what is left (signalling: two members).
  const left = leaves(x);
  x.setPolicy({ order: ['direct', 'peers', 'signaling'] });
  if (left.length !== 1 || left[0][1] !== 'policy') fail(`x's leaves ${JSON.stringify(left)}`);
  await until('linked again, over signalling', () => x.route(y.self)?.kind === 'signaling' && y.peers.includes(x.self), 20000);
  await talk(a, b, "after 'turn' off");
  await both([a, b]);
  console.log("new iceServers: every connection took them, the dialler restarted ICE on its TURN link; 'turn' off: dropped ('policy') and linked again over signalling");
}

// 9. Live relays over Nostr: an in-process relay per URL.
{
  class RelaySocket {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    static open = new Set();
    static events = []; // [url, tag] of every EVENT published
    constructor(url) {
      this.url = url;
      this.readyState = RelaySocket.CONNECTING;
      setTimeout(() => {
        if (this.readyState !== RelaySocket.CONNECTING) return;
        this.readyState = RelaySocket.OPEN;
        RelaySocket.open.add(this);
        this.onopen?.();
      });
    }
    send(raw) {
      if (this.readyState !== RelaySocket.OPEN) return;
      const [type, x, y] = JSON.parse(raw);
      if (type === 'REQ') this.req = { sub: x, tags: y['#p'] };
      if (type !== 'EVENT') return;
      RelaySocket.events.push([this.url, x.tags[0][1]]);
      setTimeout(() => this.readyState === RelaySocket.OPEN && this.onmessage?.({ data: JSON.stringify(['OK', x.id, true, '']) }));
      for (const ws of RelaySocket.open) {
        if (ws === this || ws.url !== this.url || !ws.req?.tags.includes(x.tags[0][1])) continue;
        const data = JSON.stringify(['EVENT', ws.req.sub, x]);
        setTimeout(() => ws.readyState === RelaySocket.OPEN && ws.onmessage?.({ data }));
      }
    }
    close() {
      this.readyState = RelaySocket.CLOSED;
      RelaySocket.open.delete(this);
      setTimeout(() => this.onclose?.());
    }
  }
  const Real = globalThis.WebSocket;
  globalThis.WebSocket = RelaySocket;
  n = 9;
  block('a9', 'b9');
  const [a, b] = await Promise.all(['a9', 'b9'].map((m) => room(m, { signaling: 'nostr', relays: ['wss://r1.test', 'wss://r2.test'] })));
  await until('a|b over signalling', () => a.route(b.self)?.kind === 'signaling' && a.peers.includes(b.self) && b.peers.includes(a.self), 20000);
  const left = [leaves(a), leaves(b)];
  const r2 = a.signaling.sockets.find((s) => s.url === 'wss://r2.test');
  const r1 = a.signaling.sockets.find((s) => s.url === 'wss://r1.test');
  a.setPolicy({ relays: ['wss://r2.test', 'wss://r3.test'] });
  b.setPolicy({ relays: ['wss://r3.test'] });
  const urls = a.signaling.sockets.map((s) => s.url).sort().join();
  if (urls !== 'wss://r2.test,wss://r3.test' || !a.signaling.sockets.includes(r2) || r1.readyState !== RelaySocket.CLOSED) fail(`a's sockets ${urls}`);
  if (a.policy.relays.join() !== 'wss://r2.test,wss://r3.test') fail(`a's policy relays ${a.policy.relays}`);
  await until('a says hello on r3', () => RelaySocket.events.some(([u, tag]) => u === 'wss://r3.test' && tag === a.signaling.roomTag), 3000);
  await talk(a, b, 'after the relay swap');
  if (left.some((l) => l.length) || a.route(b.self)?.kind !== 'signaling') fail(`the swap dropped the link: ${JSON.stringify(left)}`);
  // A member on r3 alone finds both.
  const c = await room('c9', { signaling: 'nostr', relays: ['wss://r3.test'] });
  await until('c finds a and b on r3', () => c.peers.length === 2, 20000);
  await both([a, b, c]);
  globalThis.WebSocket = Real;
  console.log("live relays: a swap kept the link and its messages, kept r2's socket, closed r1's, said hello on r3, where a newcomer found both");
}

console.log('room_policy: ok');
process.exit(0);
