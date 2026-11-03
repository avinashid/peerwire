// NB-084: the room's own birthday punch (src/punch.js), with a fake in-process
// RTCPeerConnection. Each member is a page with a fake class of its own (so a punch slot
// of its own) and a NAT that its connections' server-reflexive candidates show: 'cone'
// (one port at its IP), 'symmetric' (two ports far apart: no prediction), 'sequential'
// (two ports 2 apart: a prediction) or 'none' (no STUN answer). Room links between blocked
// members fail, so the room falls back. A punch's connections meet as real ones do
// (birthday_fleet.mjs runs those in Chromium): the cone side's connection reaches a fleet
// connection only through a guess it sprayed at the hard side's IP, and only one that was
// aimed at the cone side's own public address (its checks opened its NAT); with `hit`, the
// HIT_AT-th guess lands on a mapping. Checks:
// - cone × symmetric, either side dialling, over signalling and relayed by a member: the
//   cone side sprays, the symmetric side opens a fleet of fleetSize aimed at the cone side's
//   address, both adopt the channel that forms (route direct, adopted), messages go over
//   it, and only that connection stays open; it closing never brings a second punch;
// - no punch for cone × cone, symmetric × symmetric, a sequential symmetric NAT (a
//   prediction), a NAT that showed nothing, or one public IP on both sides;
// - teardown: a punch that misses ends at punchMs and closes everything, either side
//   leaving mid-punch closes everything on both;
// - the opt-out: `birthday: false` on the cone side starts none, on the symmetric side
//   refuses the offer at once and opens nothing;
// - a slot that is busy puts the punch off (busyMs), and it goes ahead once it frees, or
//   gives up after the last of busyMs; a page in two punches opens one fleet at a time;
// - the NATs a failed link showed outlive the link, but not the peer;
// - the symmetric side refuses an offer whose address is private (even one the peer's
//   candidates named), isn't one they named, or is malformed, and an oversized description;
//   drops frames on the unreliable lane and what isn't a Punch step; the same offer, well
//   formed, opens a fleet, a second one meanwhile gets nothing, and one after it a 'no';
// - no punch either for a symmetric NAT at a carrier-NAT IP, or a pair linked directly (whose
//   open link nat() reads);
// - a cone side that stops waiting for the answer stops the symmetric side, which opens no
//   fleet though its certificate comes after; a channel the room doesn't take closes on both
//   sides; a second answer, or a 'no' after the answer, changes nothing; a punch connection
//   with no public address sends no offer; a cone side that lost the first offer punches
//   once a retry's failed link shows the NATs; the slot: one holder, freed by it alone, shut
//   gapMs after a punch that spent something;
// - per destination IP (NB84-1): three peers behind one public IP, as PeerIds a room-key holder
//   can mint, get 2 punches, not 3, on the symmetric side (fleets) and on the cone side (sprays);
//   a peer at another IP still gets one; the slot counts only punches that spent something and
//   named an IP, forgets them after ipWindowMs, and when full refuses a new IP, forgetting none.
//
//   node packages/js/test/room_birthday.mjs
import { joinRoom, Room, peerwireModule } from '../src/room.js';
import { WebRtcTransport } from '../src/webrtc.js';
import { Punches, punchSlot, MAX_IPS } from '../src/punch.js';
import { SPRAY_PORTS } from '../src/birthday.js';
import { Lane } from '../src/peerwire.js';

const fail = (m) => {
  console.error(`FAIL ${m}`);
  if (process.env.DEBUG) for (const t of traces) console.error(JSON.stringify(t));
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

Punches.afterMs = 100;
Punches.answerMs = 3000;
Punches.punchMs = 1500;
Punches.gapMs = 0;
Punches.busyMs = [300, 300];
const FLEET = Punches.fleetSize; // the real 256: fakes are cheap
const HIT_AT = 5;
let hit = true;
let certDelay = 0; // ms a fake certificate takes
let dropOfferTo = null; // the page whose transport loses the next first offer (not a retry's)

const blocked = new Set(); // 'a|b', both orders
const block = (x, y) => (blocked.add(`${x}|${y}`), blocked.add(`${y}|${x}`));
const nats = new Map(); // owner -> { ip, kind }
const made = []; // every fake connection
const peak = new Map(); // owner -> the most fleet connections it had open and unlinked at once
const pcs = new Map(); // token -> live fake connection
let tokens = 0;
let ports = 20000;

class FakeDc {
  constructor() {
    this.readyState = 'connecting';
    this.bufferedAmount = 0;
    this.other = null;
    this.sent = 0;
  }
  send(bytes) {
    if (this.readyState !== 'open') throw new Error('not open');
    this.sent++;
    const o = this.other;
    const copy = bytes.slice().buffer;
    setTimeout(() => o.readyState === 'open' && o.onmessage?.({ data: copy }));
  }
  open() {
    this.readyState = 'open';
    this.onopen?.();
  }
  close() {
    for (const d of [this, this.other]) {
      if (!d || d.readyState === 'closed') continue;
      d.readyState = 'closed';
      setTimeout(() => d.onclose?.());
    }
  }
}

// The server-reflexive candidate lines a connection of `owner` gathers, by its NAT (`extra`:
// one more, at another IP, as a second interface gives).
function srflx(owner, port) {
  const { ip, kind, extra } = nats.get(owner);
  const line = (p, f, at = ip) => `a=candidate:${f} 1 udp 1686052607 ${at} ${p} typ srflx raddr 0.0.0.0 rport 0`;
  if (kind === 'cone') return [line(port + 10000, 1), ...(extra ? [line(port + 20000, 4, extra)] : [])];
  if (kind === 'symmetric') return [line(30000 + (port % 997), 1), line(50000 + (port % 991), 2)];
  if (kind === 'sequential') return [line(port + 10000, 1), line(port + 10002, 2)];
  return [];
}

// An RTCPeerConnection class for member (page) `owner`.
const fakePc = (owner) => class FakePc {
  static owner = owner;
  static async generateCertificate() {
    if (certDelay) await sleep(certDelay);
    return { owner };
  }
  constructor(config = {}) {
    this.owner = owner;
    this.token = `t${++tokens}`;
    this.port = ++ports; // its socket
    this.cert = config.certificates?.[0] ?? null; // a fleet connection
    this.connectionState = 'new';
    this.iceGatheringState = 'complete';
    this.localDescription = null;
    this.remoteDescription = null;
    this.remote = null;
    this.cands = []; // remote candidates added
    this.sprayed = 0;
    this.closed = false;
    made.push(this);
    pcs.set(this.token, this);
    if (this.cert) peak.set(owner, Math.max(peak.get(owner) ?? 0, made.filter((f) => f.owner === owner && f.cert && !f.closed && f.connectionState !== 'connected').length));
  }
  // Our public address as our srflx candidate names it ('ip port'), if one.
  get addr() {
    return / (\d+\.\d+\.\d+\.\d+ \d+) typ srflx/.exec(this.sdp())?.[1] ?? null;
  }
  sdp() {
    return ['v=0', `a=pw-token:${this.token}`, `a=ice-ufrag:u${this.token}`, `a=ice-pwd:${'p'.repeat(22)}${this.token}`,
      'm=application 9 UDP/DTLS/SCTP webrtc-datachannel', 'c=IN IP4 0.0.0.0', ...srflx(owner, this.port),
      `a=candidate:3 1 udp 2122260223 10.9.0.1 ${this.port} typ host`, ''].join('\r\n');
  }
  createDataChannel() {
    this.dc = new FakeDc();
    return this.dc;
  }
  async createOffer() {
    return { type: 'offer', sdp: this.sdp() };
  }
  async createAnswer() {
    return { type: 'answer', sdp: this.sdp() };
  }
  async setLocalDescription(d) {
    this.localDescription = { ...d, toJSON: () => ({ type: d.type, sdp: d.sdp }) };
  }
  async setRemoteDescription(d) {
    const remote = pcs.get(/a=pw-token:(\S+)/.exec(d.sdp ?? '')?.[1]);
    if (!remote) throw new Error('unknown remote');
    this.remote = remote;
    this.remoteDescription = { type: d.type, sdp: d.sdp };
    // A room link connects on its answer; a punch only through the spray.
    if (d.type === 'answer' && !remote.cert) setTimeout(() => this.connect(remote), 5);
  }
  // `punched`: a guess landed, which no block stops (the block is the NATs, that the punch gets past).
  connect(r, punched = false) {
    if (this.closed || r.closed) return;
    if (!punched && blocked.has(`${this.owner}|${r.owner}`)) {
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
  removeEventListener() {}
  async addIceCandidate(c) {
    this.cands.push(c.candidate);
    // A guess the cone side sprays: at the hard side's IP, the HIT_AT-th lands on the mapping
    // of a fleet connection whose checks went to our address.
    const m = / (\d+\.\d+\.\d+\.\d+) (\d+) typ host/.exec(c.candidate);
    if (!/^candidate:pwb/.test(c.candidate) || !this.remote?.cert || !m) return;
    if (m[1] !== nats.get(this.remote.owner).ip) fail(`a guess at ${m[1]}, not the hard side's IP`);
    if (++this.sprayed !== HIT_AT || !hit) return;
    const aimed = made.filter((f) => f.cert && f.remote === this && !f.closed && f.cands.some((x) => x.includes(` ${this.addr} typ srflx`)));
    if (aimed[2]) setTimeout(() => this.connect(aimed[2], true), 5);
  }
  close() {
    this.closed = true;
    this.connectionState = 'closed';
    pcs.delete(this.token);
    this.dc?.close();
  }
};

// Every birthday trace, with the page it came from.
const traces = [];
const tr = WebRtcTransport.prototype.tr;
WebRtcTransport.prototype.tr = function (peer, what, d) {
  if (what === 'birthday') traces.push({ owner: this.Pc.owner, peer, ...d });
  return tr.call(this, peer, what, d);
};
const onSignal = WebRtcTransport.prototype.onSignal;
WebRtcTransport.prototype.onSignal = function (from, msg) {
  if (this.Pc.owner !== dropOfferTo || msg?.kind !== 'offer' || msg.retry) return onSignal.call(this, from, msg);
  dropOfferTo = null;
  return Promise.resolve();
};
const steps = (owner, peer) => traces.filter((t) => t.owner === owner && (!peer || t.peer === peer)).map((t) => t.step + (t.why ? `(${t.why})` : '')).join(' ');

// Two secrets, the first one's PeerId below the second's (the smaller id dials).
const pw = await peerwireModule();
function secrets() {
  const s = [0, 1].map(() => crypto.getRandomValues(new Uint8Array(32)));
  const id = (k) => {
    const ep = pw.endpoint({ secret: k });
    const out = ep.id;
    ep.close();
    return out;
  };
  return id(s[0]) < id(s[1]) ? s : [s[1], s[0]];
}

let n = 0;
const room = (owner, nat, opts = {}) => {
  nats.set(owner, nat);
  return joinRoom({ appId: 'peerwire-test', roomId: `bday-${process.pid}-${n}`, presence: { name: owner }, fallbackMs: 300, RTCPeerConnection: fakePc(owner), ...opts });
};
const cone = (ip) => ({ ip, kind: 'cone' });
const sym = (ip) => ({ ip, kind: 'symmetric' });
const isAdopted = (r) => r?.kind === 'direct' && r.adopted === true;
const open = (owner) => made.filter((pc) => pc.owner === owner && !pc.closed);
// The open connections of `owner` that are neither a room link nor an adopted channel.
const loose = (r, owner) => open(owner).filter((pc) => ![...r.transport.links.values()].some((l) => l.pc === pc) && !r.peers.some((p) => r.transport.carrier(p)?.pc === pc));
const fleets = (owner) => made.filter((pc) => pc.owner === owner && pc.cert);
const linked = (what, x, y, kind) => until(what, () => x.peers.includes(y.self) && y.peers.includes(x.self) && x.route(y.self)?.kind === kind && y.route(x.self)?.kind === kind, 20000);
const leaveAll = (...rs) => Promise.all(rs.map((r) => r.leave()));

// 1, 2. Cone × symmetric over signalling, the cone side dialling, then the symmetric one.
for (const coneDials of [true, false]) {
  n++;
  const [lo, hi] = secrets();
  const [c, s] = [`c${n}`, `s${n}`];
  block(c, s);
  const [a, b] = await Promise.all([room(c, cone('203.0.113.10'), { secret: coneDials ? lo : hi }), room(s, sym('198.51.100.20'), { secret: coneDials ? hi : lo })]);
  if ((a.self < b.self) !== coneDials) fail('secrets in the wrong order');
  await linked('a link over signalling', a, b, 'signaling');
  await until('both ends adopt the punched channel', () => isAdopted(a.route(b.self)) && isAdopted(b.route(a.self)), 10000);
  if (steps(c) !== 'start spray end(channel open) adopted' || steps(s) !== `start fleet end(channel open) adopted`) fail(`steps: ${c}: ${steps(c)} / ${s}: ${steps(s)}`);
  const roles = traces.filter((t) => t.step === 'start' && (t.owner === c || t.owner === s)).map((t) => `${t.owner} ${t.role}`).sort().join(', ');
  if (roles !== `${c} easy, ${s} hard`) fail(`roles ${roles}`);
  // The fleet: fleetSize connections, each given exactly the cone side's spray address; the
  // cone side's guesses: SPRAY_PORTS, all at the symmetric side's IP (checked as they came).
  const spray = made.find((pc) => pc.owner === c && pc.remote?.cert);
  const fleet = fleets(s);
  if (fleet.length !== FLEET || !fleet.every((f) => f.cands.length === 1 && f.cands[0].includes(` ${spray.addr} typ srflx`))) fail(`fleet of ${fleet.length}, aimed ${fleet.map((f) => f.cands.join()).slice(0, 2)}`);
  if (spray.sprayed !== SPRAY_PORTS) fail(`${spray.sprayed} guesses`);
  // Only the channel stays open of what the punch opened.
  if (loose(a, c).length || loose(b, s).length || fleet.filter((f) => !f.closed).length !== 1) fail(`left open: ${loose(a, c).length} / ${loose(b, s).length}, fleet ${fleet.filter((f) => !f.closed).length}`);
  const got = [];
  b.on('message', (m) => got.push(m));
  const ch = a.transport.carrier(b.self);
  const before = ch.dc.sent;
  a.send('over the punched channel');
  await until('a message over the channel', () => got.includes('over the punched channel'), 5000);
  if (ch.dc.sent === before) fail('the message did not go over the channel');
  console.log(`cone × symmetric over signalling, the ${coneDials ? 'cone' : 'symmetric'} side dialling: ${c} sprayed ${spray.sprayed} guesses at ${nats.get(s).ip}, ${s} opened ${fleet.length} aimed at ${spray.addr}; both adopted the channel, the rest closed`);
  // The channel closes: back on signalling, and no second punch.
  ch.pc.close();
  await linked('the link back on signalling', a, b, 'signaling');
  a.punches.consider(b.self);
  await sleep(Punches.afterMs + 300);
  if (traces.filter((t) => t.owner === c && t.step === 'start').length !== 1) fail('a second punch with the same peer');
  console.log('  the channel closed: back on signalling, no second punch');
  await leaveAll(a, b);
  if (open(c).length || open(s).length) fail(`open after leave: ${open(c).length} / ${open(s).length}`);
}

// 3. Relayed by a member: a (cone) and b (symmetric) can't link, m links both.
{
  n++;
  const peerFirst = Room.peerFirstMs;
  Room.peerFirstMs = 60000; // no pipe: the link stays relayed
  block('ra', 'rb');
  const [a, b, m] = await Promise.all([room('ra', cone('203.0.113.11')), room('rb', sym('198.51.100.21')), room('rm', cone('192.0.2.31'))]);
  await linked('a link relayed by m', a, b, 'peer');
  await until('both ends adopt the punched channel', () => isAdopted(a.route(b.self)) && isAdopted(b.route(a.self)), 10000);
  if (steps('rm') !== '' || steps('ra') !== 'start spray end(channel open) adopted') fail(`steps: ${steps('ra')} / m: ${steps('rm')}`);
  const got = [];
  a.on('message', (x) => got.push(x));
  b.send('relayed no more', { to: a.self });
  await until('a message over the channel', () => got.includes('relayed no more'), 5000);
  console.log('cone × symmetric relayed by a member: both adopted the punched channel; the member, linked direct to both, took no part');
  Room.peerFirstMs = peerFirst;
  await leaveAll(a, b, m);
}

// 4. No punch for any other pair.
for (const [what, x, y, how = 'signaling'] of [
  ['cone × cone', cone('203.0.113.12'), cone('198.51.100.22')],
  ['symmetric × symmetric', sym('203.0.113.13'), sym('198.51.100.23')],
  ['a sequential symmetric NAT (a prediction) × cone', { ip: '203.0.113.14', kind: 'sequential' }, cone('198.51.100.24')],
  ['no STUN answer × symmetric', { ip: '203.0.113.15', kind: 'none' }, sym('198.51.100.25')],
  ['one public IP, cone × symmetric', cone('203.0.113.16'), sym('203.0.113.16')],
  ['a private "public" IP, cone × symmetric', cone('10.1.2.3'), sym('198.51.100.26')],
  ['cone × symmetric at a carrier-NAT IP', cone('203.0.113.25'), sym('100.64.1.2')],
  ['cone × symmetric linked directly', cone('203.0.113.26'), sym('198.51.100.34'), 'direct'],
]) {
  n++;
  if (how !== 'direct') block(`nx${n}`, `ny${n}`);
  const [a, b] = await Promise.all([room(`nx${n}`, x), room(`ny${n}`, y)]);
  await linked(`a link, ${how}`, a, b, how);
  // Before any link has failed, nat() reads the link there is.
  if (how === 'direct' && (!a.transport.nat(b.self)?.mine.has(x.ip) || !a.transport.nat(b.self).theirs.has(y.ip))) fail("nat() doesn't read the open link");
  await sleep(Punches.afterMs + 400);
  if (steps(`nx${n}`) || steps(`ny${n}`) || loose(a, `nx${n}`).length || loose(b, `ny${n}`).length) fail(`${what}: ${steps(`nx${n}`)} / ${steps(`ny${n}`)}`);
  console.log(`no punch: ${what}`);
  await leaveAll(a, b);
}

// 5. A punch that misses ends at punchMs; everything it opened closes, on both sides.
{
  n++;
  hit = false;
  block('ma', 'mb');
  const [a, b] = await Promise.all([room('ma', cone('203.0.113.17')), room('mb', sym('198.51.100.27'))]);
  await linked('a link over signalling', a, b, 'signaling');
  await until('the fleet open', () => fleets('mb').length === FLEET);
  const t0 = Date.now();
  await until('both sides end the punch', () => /end/.test(steps('ma')) && /end/.test(steps('mb')), 10000);
  await sleep(50);
  if (loose(a, 'ma').length || loose(b, 'mb').length || fleets('mb').some((f) => !f.closed)) fail(`left open after a miss: ${loose(a, 'ma').length} / ${loose(b, 'mb').length}`);
  if (a.route(b.self)?.kind !== 'signaling' || a.peers.length !== 1) fail('the link moved or went');
  console.log(`a miss: ended after ${Date.now() - t0} ms (${steps('ma')} / ${steps('mb')}), all ${FLEET + 1} punch connections closed, the link stays on signalling`);
  // What the failed link showed outlives it (a retry's link, say, starts with nothing).
  a.transport.dropRtc(b.self);
  const seen = a.transport.nat(b.self);
  if (!seen?.mine.has('203.0.113.17') || seen.theirs.get('198.51.100.27')?.size !== 2) fail(`the failed link's view went with it: ${JSON.stringify(seen && [...seen.theirs])}`);
  hit = true;
  await leaveAll(a, b);
}

// 6. Either side leaving mid-punch closes everything, on both sides.
for (const hardLeaves of [true, false]) {
  n++;
  hit = false;
  const [c, s] = [`lc${n}`, `ls${n}`];
  block(c, s);
  const [a, b] = await Promise.all([room(c, cone('203.0.113.18')), room(s, sym('198.51.100.28'))]);
  await linked('a link over signalling', a, b, 'signaling');
  await until('the fleet open, the spray on', () => fleets(s).length === FLEET && /spray/.test(steps(c)));
  const [goes, stays, gone, kept] = hardLeaves ? [b, a, s, c] : [a, b, c, s];
  await goes.leave();
  if (open(gone).length) fail(`${open(gone).length} connections open on the side that left`);
  await until('the punch over on the side that stays', () => /end\(the peer is gone\)/.test(steps(kept)), 5000);
  if (loose(stays, kept).length) fail(`${loose(stays, kept).length} left open on the side that stayed`);
  if (stays.transport.nat(goes.self) !== null) fail('what the link to the peer showed outlived the peer');
  console.log(`the ${hardLeaves ? 'symmetric' : 'cone'} side left mid-punch: all its connections closed at once; the other side ended its punch and closed its own`);
  hit = true;
  await stays.leave();
}

// 7. The opt-out.
{
  n++;
  block('oa', 'ob');
  const [a, b] = await Promise.all([room('oa', cone('203.0.113.19'), { birthday: false }), room('ob', sym('198.51.100.29'))]);
  await linked('a link over signalling', a, b, 'signaling');
  await sleep(Punches.afterMs + 400);
  if (steps('oa') || steps('ob') || loose(a, 'oa').length) fail(`the cone side opted out but: ${steps('oa')} / ${steps('ob')}`);
  console.log('birthday: false on the cone side: no punch');
  await leaveAll(a, b);
  n++;
  block('pa', 'pb');
  const [c, d] = await Promise.all([room('pa', cone('203.0.113.20')), room('pb', sym('198.51.100.30'), { birthday: false })]);
  await linked('a link over signalling', c, d, 'signaling');
  const t0 = Date.now();
  await until('the cone side refused', () => /end\(refused\)/.test(steps('pa')), 5000);
  if (Date.now() - t0 > Punches.answerMs / 2 || fleets('pb').length || steps('pb') || loose(c, 'pa').length) fail(`the symmetric side opted out but: ${steps('pa')} / ${steps('pb')}, fleet ${fleets('pb').length}`);
  console.log(`birthday: false on the symmetric side: its peer's offer refused (${steps('pa')}), no fleet`);
  await leaveAll(c, d);
}

// 8. A busy slot puts the punch off, and it goes ahead once it frees.
{
  n++;
  block('ba', 'bb');
  const held = { busy: true };
  const [a, b] = await Promise.all([room('ba', cone('203.0.113.21')), room('bb', sym('198.51.100.31'))]);
  if (!punchSlot(b.punches.Pc).take(held)) fail('the slot was taken');
  await linked('a link over signalling', a, b, 'signaling');
  await until('put off once', () => /end\(refused\)/.test(steps('ba')), 5000);
  punchSlot(b.punches.Pc).free(held, false);
  await until('both ends adopt the punched channel', () => isAdopted(a.route(b.self)) && isAdopted(b.route(a.self)), 10000);
  if (steps('ba') !== 'start end(refused) start spray end(channel open) adopted') fail(`steps: ${steps('ba')}`);
  console.log(`a busy slot: put off (${steps('ba')}), then linked`);
  await leaveAll(a, b);
  // A slot held throughout: tried again after each of busyMs, then no more.
  n++;
  block('bc', 'bd');
  const [c, d] = await Promise.all([room('bc', cone('203.0.113.27')), room('bd', sym('198.51.100.35'))]);
  if (!punchSlot(d.punches.Pc).take(held)) fail('the slot was taken');
  await linked('a link over signalling', c, d, 'signaling');
  const tries = 1 + Punches.busyMs.length;
  await until(`${tries} tries`, () => steps('bc') === Array(tries).fill('start end(refused)').join(' '), 5000);
  await sleep(Punches.busyMs.at(-1) + 400);
  if (steps('bc') !== Array(tries).fill('start end(refused)').join(' ') || steps('bd')) fail(`a slot held throughout: ${steps('bc')} / ${steps('bd')}`);
  console.log(`a slot held throughout: ${tries} tries, then no more`);
  punchSlot(d.punches.Pc).free(held, false);
  await leaveAll(c, d);
}

// 9. One page, two punches: one fleet at a time (its slot, and gapMs after one).
{
  n++;
  Punches.gapMs = 200;
  block('p1', 'ps');
  block('p2', 'ps');
  block('p1', 'p2');
  const [x, y, s] = await Promise.all([room('p1', cone('203.0.113.22')), room('p2', cone('203.0.113.23')), room('ps', sym('198.51.100.32'))]);
  await until('both adopt', () => isAdopted(s.route(x.self)) && isAdopted(s.route(y.self)), 15000);
  const most = peak.get('ps');
  if (fleets('ps').length !== 2 * FLEET || most !== FLEET) fail(`${fleets('ps').length} fleet connections made, ${most} open at once`);
  console.log(`one page, two punches: ${fleets('ps').length / FLEET} fleets one after the other (at most ${most} unlinked open at once; ${steps('p1')} / ${steps('p2')}); both links adopted`);
  Punches.gapMs = 0;
  await leaveAll(x, y, s);
}

// 10. The symmetric side refuses an offer it can't trust, and drops what isn't a step.
{
  n++;
  block('fa', 'fb');
  // The cone side's candidates also name a private IP (a second interface): an offer at it is refused all the same.
  const [a, b] = await Promise.all([room('fa', { ...cone('203.0.113.24'), extra: '10.0.0.5' }, { birthday: false }), room('fb', sym('198.51.100.33'))]);
  await linked('a link over signalling', a, b, 'signaling');
  const replies = [];
  const receive = a.punches.receive.bind(a.punches);
  a.punches.receive = (peer, body) => (replies.push(JSON.parse(new TextDecoder().decode(body))), receive(peer, body));
  const Pc = a.punches.Pc;
  const pc = new Pc();
  pc.createDataChannel();
  await pc.setLocalDescription(await pc.createOffer());
  const sdp = pc.localDescription.sdp.replace(/^a=candidate:.*\r\n/gm, '');
  const addr = pc.addr.replace(' ', ':');
  // Sends `m` (or the `raw` frame), then a probe the symmetric side always refuses (a private
  // address); `refused`: `m` gets a 'no' of its own, else nothing. The replies can't be told
  // apart, so they are counted: all of them in, and a moment more for any stray one.
  const frame = (x) => Uint8Array.of(10, ...new TextEncoder().encode(JSON.stringify(x)));
  const ask = async (what, m, refused, { lane = Lane.Reliable, raw = null } = {}) => {
    const had = replies.length;
    const want = refused ? 2 : 1;
    a.write(b.self, raw ?? frame(m), lane);
    a.write(b.self, frame({ p: 'offer', sdp, addr: '10.0.0.5:4000' }), Lane.Reliable);
    await until(`the replies after ${what}`, () => replies.length - had >= want);
    await sleep(200);
    const got = replies.slice(had);
    if (got.length !== want || got.some((r) => r.p !== 'no' || r.busy)) fail(`${what}: ${JSON.stringify(got)}`);
    if (fleets('fb').length || steps('fb')) fail(`${what}: ${fleets('fb').length} fleet connections, ${steps('fb')}`);
  };
  await ask('a private address', { p: 'offer', sdp, addr: '10.0.0.5:4000' }, true);
  await ask("an address the peer's own candidates never named", { p: 'offer', sdp, addr: '192.0.2.77:4000' }, true);
  await ask('a malformed address', { p: 'offer', sdp, addr: `${addr} ` }, true);
  await ask('no address', { p: 'offer', sdp, addr: 7 }, true);
  await ask('an oversized description', { p: 'offer', sdp: `${sdp}a=x:${'y'.repeat(16 * 1024)}\r\n`, addr }, true);
  await ask('a good offer on the unreliable lane', { p: 'offer', sdp, addr }, false, { lane: Lane.Unreliable });
  await ask('not JSON', null, false, { raw: Uint8Array.of(10, 123, 34) });
  await ask('not an object', 'offer', false);
  await ask('an unknown step', { p: 'fleet', sdp, addr }, false);
  await ask('an answer with no punch', { p: 'answer', sdp }, false);
  await ask('a stop with no punch', { p: 'stop' }, false);
  // The same offer, well formed, on the reliable lane: the fleet opens and answers (no probe:
  // during a punch, a second offer gets no reply).
  const had = replies.length;
  a.write(b.self, frame({ p: 'offer', sdp, addr }), Lane.Reliable);
  await until('the fleet and its answer', () => fleets('fb').length === FLEET && /fleet/.test(steps('fb')) && replies.length > had, 5000);
  const got = replies.slice(had);
  if (got.length !== 1 || got[0].p !== 'answer') fail(`a good offer: ${JSON.stringify(got)}`);
  if (!fleets('fb').every((f) => f.cands.length === 1 && f.cands[0].includes(` ${pc.addr} typ srflx`))) fail('the fleet aimed elsewhere');
  // A second offer while the punch runs: nothing.
  a.write(b.self, frame({ p: 'offer', sdp, addr }), Lane.Reliable);
  await sleep(400);
  if (replies.length !== had + 1 || fleets('fb').length !== FLEET) fail(`a second offer during the punch: ${JSON.stringify(replies.slice(had))}, ${fleets('fb').length} fleet connections`);
  a.write(b.self, frame({ p: 'stop' }), Lane.Reliable);
  await until('stop', () => /end\(the peer stopped\)/.test(steps('fb')), 5000);
  if (fleets('fb').some((f) => !f.closed)) fail(`stop left ${fleets('fb').filter((f) => !f.closed).length} open`);
  // And once it has been tried, the same offer again is refused.
  const before = replies.length;
  a.write(b.self, frame({ p: 'offer', sdp, addr }), Lane.Reliable);
  await until('a reply to the offer after the punch', () => replies.length > before, 5000);
  await sleep(200);
  if (replies.length !== before + 1 || replies.at(-1).p !== 'no' || fleets('fb').length !== FLEET) fail(`an offer after the punch: ${JSON.stringify(replies.slice(before))}`);
  console.log('the symmetric side refused a private address (one the peer named too), one the peer never named, a malformed one, none, and an oversized description; dropped frames on the unreliable lane and what is not a step; a good offer opened the fleet, a second got nothing, stop closed it, and the offer again was refused');
  pc.close();
  await leaveAll(a, b);
}

// 12. The cone side gives up waiting for the answer while the symmetric side still makes its
// certificate: told to stop, the symmetric side opens no fleet.
{
  n++;
  Punches.answerMs = 300;
  certDelay = 800;
  block('sa', 'sb');
  const [a, b] = await Promise.all([room('sa', cone('203.0.113.28')), room('sb', sym('198.51.100.36'))]);
  await linked('a link over signalling', a, b, 'signaling');
  await until('both ends', () => /end/.test(steps('sa')) && /end/.test(steps('sb')), 5000);
  await sleep(certDelay + 300);
  if (steps('sa') !== 'start end(timed out)' || steps('sb') !== 'start end(the peer stopped)' || fleets('sb').length || loose(a, 'sa').length) fail(`stop while the fleet is made: ${steps('sa')} / ${steps('sb')}, ${fleets('sb').length} fleet connections`);
  console.log(`no answer in time: the cone side stopped (${steps('sa')}), and the symmetric side, still making its certificate, opened no fleet (${steps('sb')})`);
  Punches.answerMs = 3000;
  certDelay = 0;
  await leaveAll(a, b);
}

// 13. A channel the room doesn't take closes, on both sides.
{
  n++;
  block('na', 'nb');
  const [a, b] = await Promise.all([room('na', cone('203.0.113.29')), room('nb', sym('198.51.100.37'))]);
  a.adoptChannel = async () => false;
  await linked('a link over signalling', a, b, 'signaling');
  await until('neither adopts', () => /not-adopted/.test(steps('na')) && /not-adopted/.test(steps('nb')), 10000);
  await sleep(50);
  if (loose(a, 'na').length || loose(b, 'nb').length || a.route(b.self)?.kind !== 'signaling' || b.route(a.self)?.kind !== 'signaling') fail(`a channel not taken: ${loose(a, 'na').length} / ${loose(b, 'nb').length} left open`);
  console.log(`a channel the room doesn't take: closed on both sides (${steps('na')} / ${steps('nb')}), the link stays on signalling`);
  await leaveAll(a, b);
}

// 14. A hostile symmetric side: a second answer is ignored, and so is a 'no' after the answer;
// 'stop' ends the punch.
{
  n++;
  block('ka', 'kb');
  const [a, b] = await Promise.all([room('ka', cone('203.0.113.30')), room('kb', sym('198.51.100.38'))]);
  b.punches.receive = () => {}; // its own punches play no part: the test answers for it
  await linked('a link over signalling', a, b, 'signaling');
  const isLink = (pc) => [...a.transport.links.values()].some((l) => l.pc === pc);
  await until('the offer out', () => /start/.test(steps('ka')) && made.some((pc) => pc.owner === 'ka' && !pc.closed && !isLink(pc) && pc.localDescription), 5000);
  const spray = made.find((pc) => pc.owner === 'ka' && !pc.closed && !isLink(pc));
  const Pc = b.punches.Pc;
  const f = new Pc({ certificates: [{}] });
  f.createDataChannel();
  await f.setRemoteDescription({ type: 'offer', sdp: spray.localDescription.sdp });
  const answer = (await f.createAnswer()).sdp;
  const say = (m) => b.write(a.self, Uint8Array.of(10, ...new TextEncoder().encode(JSON.stringify(m))), Lane.Reliable);
  say({ p: 'answer', sdp: answer });
  await until('the spray', () => spray.sprayed === SPRAY_PORTS, 5000);
  say({ p: 'answer', sdp: answer });
  say({ p: 'no' });
  await sleep(400);
  if (spray.sprayed !== SPRAY_PORTS || steps('ka') !== 'start spray' || spray.closed) fail(`a second answer, then a 'no': ${spray.sprayed} guesses, ${steps('ka')}`);
  say({ p: 'stop' });
  await until('stopped', () => steps('ka') === 'start spray end(the peer stopped)' && spray.closed, 5000);
  console.log(`a second answer and a 'no' after the answer change nothing (${spray.sprayed} guesses); 'stop' ends it`);
  f.close();
  await leaveAll(a, b);
}

// 15. The cone side's punch connection has no public address (STUN didn't answer it): no offer.
{
  n++;
  Punches.afterMs = 500;
  block('ua', 'ub');
  const [a, b] = await Promise.all([room('ua', cone('203.0.113.31')), room('ub', sym('198.51.100.39'))]);
  await linked('a link over signalling', a, b, 'signaling');
  nats.get('ua').kind = 'none'; // for connections made from now on
  await until('the cone side gave up', () => /end/.test(steps('ua')), 5000);
  await sleep(200);
  if (steps('ua') !== 'start end(no public address)' || steps('ub') || fleets('ub').length || loose(a, 'ua').length) fail(`no public address: ${steps('ua')} / ${steps('ub')}`);
  console.log(`no public address for the punch connection: ${steps('ua')}, no offer, no fleet`);
  Punches.afterMs = 100;
  await leaveAll(a, b);
}

// 16. The cone side never got the first offer, so it knew nothing of the NATs when the peer
// joined; a retry's link that fails shows them, and the punch follows.
{
  n++;
  const [lo, hi] = secrets();
  block('ra2', 'rb2');
  dropOfferTo = 'ra2';
  const [a, b] = await Promise.all([room('ra2', cone('203.0.113.32'), { secret: hi }), room('rb2', sym('198.51.100.40'), { secret: lo })]);
  await linked('a link over signalling', a, b, 'signaling');
  await sleep(Punches.afterMs + 300);
  if (steps('ra2') || steps('rb2') || a.transport.nat(b.self)) fail(`before the retry: ${steps('ra2')} / ${steps('rb2')}`);
  b.retryDirect(a.self); // the dialling side
  await until('both ends adopt the punched channel', () => isAdopted(a.route(b.self)) && isAdopted(b.route(a.self)), 10000);
  console.log(`the first offer lost: no punch on joining; a retry's failed link showed the NATs, and the punch linked them (${steps('ra2')})`);
  await leaveAll(a, b);
}

// 17. The slot itself: one holder; only it frees the slot; after a punch that spent something,
// shut for gapMs; one that spent nothing frees it at once. One slot per RTCPeerConnection class.
{
  Punches.gapMs = 200;
  const Pc = class {};
  const slot = punchSlot(Pc);
  const [x, y] = [{}, {}];
  if (punchSlot(Pc) !== slot || punchSlot(class {}) === slot) fail('one slot per class');
  if (!slot.take(x) || slot.take(y) || !slot.busy) fail('a second holder');
  slot.free(y);
  if (slot.take(y)) fail('freed by one not holding it');
  slot.free(x, false);
  if (slot.busy || !slot.take(y)) fail('one that spent nothing left a gap');
  slot.free(y);
  if (!slot.busy || slot.take(x)) fail('no gap after one that spent something');
  await sleep(Punches.gapMs + 50);
  if (slot.busy || !slot.take(x)) fail('still shut after the gap');
  console.log('the slot: one holder, freed only by it, shut for gapMs after a punch that spent something');
  Punches.gapMs = 0;
}

// 18. The slot per destination IP (NB84-1): ipMax punches that spent something at one IP per
// ipWindowMs; one that spent nothing, or named no IP, doesn't count; another IP goes; full, it
// refuses a new IP but not one it holds, and forgets each once the window has passed.
{
  const window = Punches.ipWindowMs;
  Punches.ipWindowMs = 300;
  const slot = punchSlot(class {});
  const run = (ip, spent = true) => {
    const who = {};
    if (!slot.take(who, ip)) return false;
    slot.free(who, spent);
    return true;
  };
  const ip = '192.0.2.1';
  if (!run(ip) || !run(ip, false) || !run(ip)) fail('refused within ipMax');
  if (run(ip)) fail('a third punch at one IP');
  if (!run('192.0.2.2') || !run(null) || slot.busy) fail('another IP, or none, refused');
  await sleep(Punches.ipWindowMs + 50);
  if (!run(ip)) fail('still refused once the window passed');
  for (let i = 1; i < MAX_IPS; i++) if (!run(`198.18.0.${i}`)) fail(`refused the ${i + 1}th IP`);
  if (run('198.18.1.1')) fail(`a new IP past ${MAX_IPS}`);
  if (!run(ip)) fail('full, it refused an IP it holds');
  if (!run(null)) fail('full, it refused a punch naming no IP');
  await sleep(Punches.ipWindowMs + 50);
  if (!run('198.18.1.1')) fail('full still once the window passed');
  console.log(`the slot per destination IP: ${Punches.ipMax} punches that spent something per window, then refused; another IP goes; full at ${MAX_IPS} IPs, a new one refused; all forgotten after the window`);
  Punches.ipWindowMs = window;
}

// 19, 20. Per destination IP in rooms (NB84-1): three peers behind one public IP get 2 punches
// within the window, not 3; then a peer at another IP gets one. As the symmetric side, a page
// opens 2 fleets at the IP and refuses the third offer (busy, so the peer puts it off and gives
// up); as the cone side, it sprays the IP twice and never offers to the third. They join one at
// a time; every pair is blocked, so each is reached over signalling.
for (const symPage of [true, false]) {
  n++;
  const busyMs = Punches.busyMs;
  Punches.busyMs = [200, 200];
  const tag = symPage ? 'h' : 'e';
  const [one, other] = symPage ? [cone('203.0.113.40'), cone('203.0.113.41')] : [sym('198.51.100.41'), sym('198.51.100.42')];
  const page = `${tag}0`;
  const names = [page, `${tag}1`, `${tag}2`, `${tag}3`, `${tag}4`];
  for (const x of names) for (const y of names) if (x < y) block(x, y);
  const p = await room(page, symPage ? sym('198.51.100.40') : cone('203.0.113.42'));
  const rs = [];
  const punched = async (i) => {
    await until(`${names[i]} punched`, () => isAdopted(p.route(rs[i - 1].self)) && isAdopted(rs[i - 1].route(p.self)), 10000);
    await until('the slot free', () => !p.punches.slot.busy, 5000);
  };
  for (let i = 1; i <= 4; i++) {
    rs.push(await room(names[i], i === 4 ? other : one));
    if (i !== 3) {
      await punched(i);
      continue;
    }
    // The third at the same IP: it puts the punch off after each refusal, then gives up (as
    // the cone side, the page does that itself, and never offers).
    const [easy, hard] = symPage ? [rs[2], p] : [p, rs[2]];
    const gaveUp = () => easy.punches.putOff.get(hard.self) === Punches.busyMs.length && !easy.punches.waits.has(hard.self) && !easy.punches.runs.has(hard.self);
    await until('the third put off to the end', gaveUp, 5000);
    await sleep(100);
    const tries = symPage ? Array(1 + Punches.busyMs.length).fill('start end(refused)').join(' ') : '';
    if (steps(names[3]) !== tries || steps(page, rs[2].self) || p.route(rs[2].self)?.kind !== 'signaling') fail(`the third at one IP: ${steps(names[3])} / ${steps(page, rs[2].self)}`);
  }
  // The fleets (each connection given the cone side's address) and the sprays, by IP.
  const fleet = symPage ? fleets(page) : names.slice(1).flatMap(fleets);
  const aimed = (ip) => fleet.filter((f) => f.cands[0].includes(` ${ip} `)).length / FLEET;
  const sprayed = (ip) => traces.filter((t) => names.includes(t.owner) && t.step === 'spray' && t.ip === ip).length;
  const [at, again] = symPage ? [aimed(one.ip), aimed(other.ip)] : [sprayed(one.ip), sprayed(other.ip)];
  if (at !== 2 || again !== 1 || fleet.length !== 3 * FLEET) fail(`at ${one.ip}: ${at}, at ${other.ip}: ${again}; ${fleet.length} fleet connections`);
  console.log(`per destination IP, as the ${symPage ? 'symmetric side: 2 fleets at' : 'cone side: 2 sprays at'} ${one.ip} for three peers there (the third ${symPage ? 'refused, busy, until it gave up' : 'never offered to'}), then one ${symPage ? 'fleet' : 'spray'} at ${other.ip}`);
  Punches.busyMs = busyMs;
  await leaveAll(p, ...rs);
}

console.log('room_birthday: ok');
process.exit(0);
