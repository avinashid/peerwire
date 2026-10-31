// Calls (call.js, NB-010) in Node, with a fake RTCPeerConnection that negotiates like a browser's (transceivers,
// implicit offers and answers, rollback, ontrack, trickled candidates) and connects only when the test says ICE
// did. In one process, with a fake clock:
// - parseCall: every frame kind, and each cap (frame size, SDP length and m-sections, candidate, mid, index);
//   a description with more than MAX_SDP_CANDIDATES candidates; the budgets (join, sdp, ice; leave and full have
//   none); candidateFor and the description sent per privacy.
// - Two members: the smaller id offers both transceivers and the other answers on them; one 'stream' each with
//   an audio and a video track, then 'connected'. setStream replaces tracks and sends no description. ICE
//   restarted on both ends at once (glare): the larger id rolls its offer back, answers, and sets its tracks
//   again. An offer with more m-sections: the extra ones inactive, the stream still one audio and one video.
//   An offer or an answer that sends nothing still gives a 'stream': the first transceivers' tracks.
// - Failures: no ICE in CONNECT_MS is 'no-path', ICE failed before connecting too; one that connected and failed
//   is restarted by the smaller id only, MAX_RESTARTS times in RESTART_WINDOW_MS, then 'lost', as is a restart
//   that doesn't connect in CONNECT_MS; a description that doesn't apply is 'error'. A failed peer stays in the
//   call until it leaves.
// - Seats: 6 in, a 7th is 'full' at once and sends nothing; an 8th that called before learning who is in links
//   5, then goes on the 6th; the 6 never link it. A seated member keeps its seat against a join claiming an old
//   `age` and a `full`; one not seated gives way to an earlier member, and goes on its `full` (but not on a
//   later one's, nor on one from outside the call) only when its own roster has no seat for it either. Two
//   joining for the last seat at once: nobody holds more than max - 1 links.
// - leave() tells the members and fires nothing more; a member leaving is 'ended' ('left'), leaving the room
//   'ended' ('gone'); the room leaving tells nobody. The API's argument checks.
// Then in real rooms (local signalling, the core in wasm, media still fake): calls signalled over room links,
// a member not in the call keeps the roster, a member joining the room later learns who is in and joins, a
// call frame on the unreliable lane is dropped, and a member leaving the room is 'ended' ('gone').
//
//   node packages/js/test/call.mjs
import { parseCall, Calls, BUDGETS, MAX_FRAME, MAX_SDP, MAX_SECTIONS, MAX_SDP_CANDIDATES, MAX_CANDIDATE, CONNECT_MS, MAX_RESTARTS, RESTART_WINDOW_MS, NEAR_MS, DEFAULT_MAX, MAX_CALL } from '../src/call.js';
import { candidateFor } from '../src/webrtc.js';
import { joinRoom, Lane } from '../src/index.js';

let failed = 0;
const check = (ok, what) => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}`);
  if (!ok) failed++;
};
const throws = (fn, Type, what) => {
  try {
    fn();
  } catch (e) {
    return check(e instanceof Type, `${what} (${e.constructor.name}: ${e.message})`);
  }
  check(false, `${what}: didn't throw`);
};
const enc = new TextEncoder();
const bytes = (m) => enc.encode(typeof m === 'string' ? m : JSON.stringify(m));
const tick = () => new Promise((r) => setImmediate(r));

// ---- A fake WebRTC ----

let nextId = 0;
class FakeTrack {
  constructor(kind) {
    this.kind = kind;
    this.id = `${kind}${++nextId}`;
  }
}
class FakeStream {
  constructor(tracks = []) {
    this.tracks = [...tracks];
  }
  getTracks() {
    return [...this.tracks];
  }
  addTrack(t) {
    this.tracks.push(t);
  }
}
const camera = () => new FakeStream([new FakeTrack('audio'), new FakeTrack('video')]);
const HOST = 'candidate:1 1 udp 2113937151 192.168.1.5 50000 typ host generation 0';
const SRFLX = 'candidate:2 1 udp 1677729535 203.0.113.5 40000 typ srflx raddr 192.168.1.5 rport 50000 generation 0';
// A description with `n` candidates in its one m-section.
const cands = (n) => `v=0\r\nm=audio 9 x\r\n${`a=candidate:${HOST.slice(10)}\r\n`.repeat(n)}a=end-of-candidates\r\n`;
const pcs = [];

class FakePc {
  constructor(config) {
    this.config = config;
    this.ts = [];
    this.signalingState = 'stable';
    this.connectionState = 'new';
    this.localDescription = null;
    this.remoteDescription = null;
    this.stableLocal = null;
    this.ufrag = null;
    this.remoteUfrag = null;
    this.remoteCandidates = [];
    this.needed = false;
    this.restart = false;
    this.restarts = 0; // restartIce() calls
    this.rollbacks = 0;
    this.gathered = new Set();
    pcs.push(this);
  }
  addTransceiver(x, { direction = 'sendrecv' } = {}) {
    const t = transceiver(typeof x === 'string' ? x : x.kind, direction, typeof x === 'string' ? null : x);
    this.ts.push(t);
    this.need();
    return t;
  }
  getTransceivers() {
    return [...this.ts];
  }
  need() {
    this.needed = true;
    queueMicrotask(() => this.negotiate());
  }
  negotiate() {
    if (!this.needed || this.signalingState !== 'stable') return;
    this.needed = false;
    this.onnegotiationneeded?.();
  }
  restartIce() {
    this.restarts++;
    this.restart = true;
    this.need();
  }
  sdp(type) {
    const lines = ['v=0', `o=- 1 1 IN IP4 127.0.0.1`, `a=ice-ufrag:${this.ufrag}`];
    for (const t of this.ts.filter((t) => t.mid !== null)) {
      lines.push(`m=${t.kind} 50000 UDP/TLS/RTP/SAVPF 96`, 'c=IN IP4 192.168.1.5', `a=mid:${t.mid}`, `a=${t.direction}`, `a=candidate:${HOST.slice(10)}`, `a=candidate:${SRFLX.slice(10)}`);
    }
    return { type, sdp: `${lines.join('\r\n')}\r\n` };
  }
  async setLocalDescription() {
    await tick();
    if (this.signalingState === 'closed') throw new Error('InvalidStateError: closed');
    if (this.signalingState === 'have-remote-offer') {
      if (!this.ufrag || this.remoteRestarted) this.ufrag = `u${++nextId}`;
      this.remoteRestarted = false;
      this.restart = false;
      this.localDescription = this.sdp('answer');
      this.become('stable');
    } else {
      for (const t of this.ts) if (t.mid === null) t.mid = String(this.ts.indexOf(t));
      if (!this.ufrag || this.restart) this.ufrag = `u${++nextId}`;
      this.restart = false;
      this.localDescription = this.sdp('offer');
      this.signalingState = 'have-local-offer';
    }
    this.gather();
  }
  // A browser applies a description more slowly than a candidate: two turns here, a candidate one.
  async setRemoteDescription(d) {
    await tick();
    await tick();
    if (this.signalingState === 'closed') throw new Error('InvalidStateError: closed');
    if (d.sdp.includes('a=bad')) throw new Error('OperationError: unparsable');
    const ufrag = /a=ice-ufrag:(\S+)/.exec(d.sdp)[1];
    const sections = [...d.sdp.matchAll(/m=(\w+)[^\n]*\n(?:[^m][^\n]*\n)*?a=mid:(\S+)\r?\n(?:[^m][^\n]*\n)*?a=(sendrecv|sendonly|recvonly|inactive)/g)].map((m) => ({ kind: m[1], mid: m[2], dir: m[3] }));
    if (d.type === 'offer') {
      if (this.signalingState === 'have-local-offer') {
        this.rollbacks++;
        this.localDescription = this.stableLocal;
        this.signalingState = 'stable';
      } else if (this.signalingState !== 'stable') throw new Error(`InvalidStateError: offer in ${this.signalingState}`);
      this.remoteRestarted = this.remoteUfrag !== null && this.remoteUfrag !== ufrag;
      this.remoteOffers = (this.remoteOffers ?? 0) + 1;
    } else if (this.signalingState !== 'have-local-offer') throw new Error(`InvalidStateError: answer in ${this.signalingState}`);
    for (const s of sections) {
      let t = this.ts.find((x) => x.mid === s.mid);
      if (!t) {
        t = transceiver(s.kind, 'recvonly', null);
        t.mid = s.mid;
        this.ts.push(t);
      }
      if ((s.dir === 'sendrecv' || s.dir === 'sendonly') && !t.fired) {
        t.fired = true;
        this.ontrack?.({ track: t.receiver.track, transceiver: t, streams: [] });
      }
    }
    this.remoteUfrag = ufrag;
    this.remoteDescription = d;
    if (d.type === 'offer') this.signalingState = 'have-remote-offer';
    else this.become('stable');
  }
  become(state) {
    this.signalingState = state;
    if (state === 'stable') {
      this.stableLocal = this.localDescription;
      if (this.needed || this.restart) this.need();
    }
  }
  gather() {
    if (this.gathered.has(this.ufrag)) return;
    this.gathered.add(this.ufrag);
    setImmediate(() => {
      for (const c of [HOST, SRFLX]) {
        if (this.signalingState === 'closed') return;
        const j = { candidate: c, sdpMid: '0', sdpMLineIndex: 0, usernameFragment: this.ufrag };
        this.onicecandidate?.({ candidate: { ...j, toJSON: () => ({ ...j }) } });
      }
      this.onicecandidate?.({ candidate: null });
    });
  }
  async addIceCandidate(c) {
    await tick();
    if (!this.remoteDescription) throw new Error('InvalidStateError: no remote description');
    this.remoteCandidates.push(c);
  }
  // ICE as the test says: 'connected', 'failed', ...
  setConnection(s) {
    if (this.connectionState === s || this.signalingState === 'closed') return;
    this.connectionState = s;
    this.onconnectionstatechange?.();
  }
  close() {
    this.signalingState = 'closed';
    this.connectionState = 'closed';
  }
  getStats() {
    return Promise.resolve(new Map([['in', { type: 'inbound-rtp' }]]));
  }
}
function transceiver(kind, direction, track) {
  return { mid: null, kind, direction, stopped: false, fired: false, receiver: { track: new FakeTrack(kind) },
    sender: { track, replaced: [], async replaceTrack(t) { this.track = t; this.replaced.push(t ? t.kind : null); } } };
}
// ICE succeeds between every two connections whose descriptions and candidates have met.
function iceUp() {
  for (const a of pcs) {
    if (a.signalingState !== 'stable' || !a.remoteCandidates.length) continue;
    const b = pcs.find((x) => x !== a && x.ufrag === a.remoteUfrag && x.remoteUfrag === a.ufrag && x.signalingState === 'stable');
    if (b?.remoteCandidates.length) a.setConnection('connected');
  }
}

// ---- One process: members with Calls, frames through a queue, a fake clock ----

const clock = { t: 1000, timers: [] };
const now = () => clock.t;
const after = (ms, fn) => {
  const t = { at: clock.t + ms, fn };
  clock.timers.push(t);
  return () => (t.fn = null);
};
async function advance(ms) {
  const end = clock.t + ms;
  for (;;) {
    const due = clock.timers.filter((t) => t.fn && t.at <= end).sort((a, b) => a.at - b.at)[0];
    if (!due) break;
    clock.t = due.at;
    const fn = due.fn;
    due.fn = null;
    fn();
    await settle();
  }
  clock.t = end;
}

const ID = (c) => c.repeat(64);
const world = { members: new Map(), queue: [], log: [] };
function member(id, { privacy = 'direct', media = true } = {}) {
  const m = { id, room: new Set(), events: [] };
  m.calls = new Calls({
    self: id,
    send: (to, msg) => {
      world.log.push({ from: id, to, msg });
      if (m.room.has(to)) world.queue.push([id, to, bytes(msg)]);
    },
    members: () => [...m.room],
    now,
    after,
    media: media ? { RTCPeerConnection: FakePc, MediaStream: FakeStream, iceServers: [], privacy } : null,
  });
  world.members.set(id, m);
  return m;
}
// Puts members in one room (each lists the others), telling those already in a call about the newcomers.
function meet(...ms) {
  for (const a of ms) for (const b of world.members.values()) {
    if (a === b || a.room.has(b.id)) continue;
    a.room.add(b.id);
    b.room.add(a.id);
    b.calls.joinedRoom(a.id);
    a.calls.joinedRoom(b.id);
  }
}
async function settle() {
  for (let i = 0; i < 60; i++) {
    while (world.queue.length) {
      const [from, to, b] = world.queue.shift();
      world.members.get(to)?.calls.receive(from, b);
    }
    await tick();
  }
}
function join(m, stream = camera(), opts) {
  const call = m.calls.open(stream, opts);
  for (const e of ['state', 'stream', 'ended', 'full']) call.on(e, (...a) => m.events.push([e, ...a]));
  m.call = call;
  return call;
}
const evs = (m, e) => m.events.filter((x) => x[0] === e);
const sentBy = (id, t) => world.log.filter((x) => x.from === id && x.msg.t === t);
const link = (m, peer) => m.call.links.get(peer);
function reset() {
  world.members.clear();
  world.queue.length = 0;
  world.log.length = 0;
  pcs.length = 0;
  clock.timers.length = 0;
}
async function connected(...ms) {
  await settle();
  iceUp();
  await settle();
  return ms.every((m) => m.call.peers.every((p) => m.call.state(p) === 'connected'));
}

// ---- parseCall and the caps ----

{
  const ok = (m) => parseCall(bytes(m));
  check(JSON.stringify(ok({ t: 'join', age: 5, x: 1 })) === '{"t":"join","age":5}', 'join: kept with its age, other fields dropped');
  check(ok({ t: 'join', age: -1 }) === null && ok({ t: 'join', age: 1.5 }) === null && ok({ t: 'join' }) === null && ok({ t: 'join', age: 2 ** 53 }) === null, 'join: a negative, fractional, missing or unsafe age is dropped');
  check(ok({ t: 'leave' })?.t === 'leave' && ok({ t: 'full', extra: [1] })?.t === 'full' && ok({ t: 'what' }) === null, 'leave and full kept; an unknown kind dropped');
  check(parseCall(bytes('{')) === null && parseCall(bytes('[1]')) === null && parseCall(bytes('null')) === null && parseCall(bytes('"x"')) === null, 'not JSON, an array, null or a string: dropped');
  const sdp = (n, len = 0) => `v=0\r\n${'m=audio 9 x\r\n'.repeat(n)}${'a'.repeat(len)}`;
  check(ok({ t: 'sdp', d: { type: 'offer', sdp: sdp(MAX_SECTIONS) } })?.d.type === 'offer' && ok({ t: 'sdp', d: { type: 'answer', sdp: sdp(2) } })?.d.type === 'answer', `sdp: an offer with ${MAX_SECTIONS} m-sections and an answer kept`);
  check(ok({ t: 'sdp', d: { type: 'offer', sdp: sdp(MAX_SECTIONS + 1) } }) === null, `sdp: ${MAX_SECTIONS + 1} m-sections dropped`);
  const long = sdp(1);
  check(ok({ t: 'sdp', d: { type: 'offer', sdp: long + 'a'.repeat(MAX_SDP - long.length) } }) !== null && ok({ t: 'sdp', d: { type: 'offer', sdp: long + 'a'.repeat(MAX_SDP - long.length + 1) } }) === null, `sdp: ${MAX_SDP} characters kept, one more dropped`);
  check(['pranswer', 'rollback', undefined].every((type) => ok({ t: 'sdp', d: { type, sdp: sdp(1) } }) === null) && ok({ t: 'sdp', d: { type: 'offer', sdp: 5 } }) === null && ok({ t: 'sdp', d: null }) === null && ok({ t: 'sdp' }) === null, 'sdp: another type, a non-string sdp, no description: dropped');
  const p = ok({ t: 'sdp', d: { type: 'offer', sdp: sdp(1), __proto__: { x: 1 }, extra: 1 } });
  check(p && Object.keys(p.d).join() === 'type,sdp', 'sdp: only type and sdp copied');
  check(ok({ t: 'sdp', d: { type: 'offer', sdp: cands(MAX_SDP_CANDIDATES) } })?.d.sdp === cands(MAX_SDP_CANDIDATES) && ok({ t: 'sdp', d: { type: 'answer', sdp: cands(MAX_SDP_CANDIDATES + 1) } }) === null, `sdp: ${MAX_SDP_CANDIDATES} candidates kept, one more dropped`);
  const c = (o) => ok({ t: 'ice', c: { candidate: HOST, sdpMid: '0', sdpMLineIndex: 0, ...o } });
  check(c({})?.c.candidate === HOST && c({ sdpMid: null })?.c.sdpMLineIndex === 0 && c({ sdpMLineIndex: undefined })?.c.sdpMid === '0', 'ice: kept with a mid, an index, or both');
  check(c({ sdpMid: null, sdpMLineIndex: null }) === null, 'ice: neither mid nor index: dropped');
  check(c({ sdpMLineIndex: undefined, usernameFragment: 'u' })?.c.sdpMLineIndex === null && Object.keys(c({ extra: 1 }).c).join() === 'candidate,sdpMid,sdpMLineIndex', 'ice: only candidate, sdpMid and sdpMLineIndex copied, one missing as null');
  check(c({ candidate: `candidate:${'x'.repeat(MAX_CANDIDATE - 10)}` }) !== null && c({ candidate: `candidate:${'x'.repeat(MAX_CANDIDATE - 9)}` }) === null, `ice: a candidate of ${MAX_CANDIDATE} characters kept, one more dropped`);
  check(c({ candidate: 'a=candidate:1 1 udp 1 1.2.3.4 5 typ host' }) === null && c({ candidate: '' }) === null && c({ candidate: 7 }) === null, "ice: one not starting 'candidate:', empty, or not a string: dropped");
  check(c({ sdpMid: 'x'.repeat(32) }) !== null && c({ sdpMid: 'x'.repeat(33) }) === null && c({ sdpMid: 3 }) === null, 'ice: a mid of 32 characters kept, 33 or a number dropped');
  check(c({ sdpMLineIndex: MAX_SECTIONS - 1 }) !== null && c({ sdpMLineIndex: MAX_SECTIONS }) === null && c({ sdpMLineIndex: -1 }) === null && c({ sdpMLineIndex: 0.5 }) === null, `ice: an index from 0 to ${MAX_SECTIONS - 1}`);
  const frameOf = (n) => bytes({ t: 'leave', pad: 'x'.repeat(n - 22) }); // {"t":"leave","pad":""} is 22 bytes
  check(frameOf(MAX_FRAME).length === MAX_FRAME && parseCall(frameOf(MAX_FRAME)) !== null && parseCall(frameOf(MAX_FRAME + 1)) === null, `a frame of ${MAX_FRAME} bytes is read, one more dropped unread`);
}

// ---- Budgets ----

{
  reset();
  const m = member(ID('a'));
  const spends = (kind, n) => Array.from({ length: n }, () => m.calls.spend('p', kind)).filter(Boolean).length;
  for (const [kind, [burst, every]] of Object.entries(BUDGETS)) {
    check(spends(kind, burst + 3) === burst, `${kind}: ${burst} at once`);
    clock.t += every - 1;
    check(!m.calls.spend('p', kind), `${kind}: none ${every - 1} ms later`);
    clock.t += 1;
    check(m.calls.spend('p', kind) && !m.calls.spend('p', kind), `${kind}: one more every ${every} ms`);
    check(m.calls.spend('q', kind), `${kind}: per peer`);
  }
  check(spends('leave', 100) === 100 && spends('full', 100) === 100, 'leave and full have no budget');
  m.calls.gone('p');
  check(!m.calls.budgets.has('p'), "a peer gone takes its budgets with it");
  // Through receive(): the frame past the budget is dropped and counted.
  reset();
  const a = member(ID('a'));
  const before = a.calls.dropped;
  for (let i = 0; i < BUDGETS.join[0] + 1; i++) a.calls.receive(ID('b'), bytes({ t: 'join', age: 0 }));
  a.calls.receive(ID('b'), bytes({ t: 'nope' }));
  a.calls.receive(ID('b'), bytes({ t: 'sdp', d: { type: 'offer', sdp: cands(MAX_SDP_CANDIDATES + 1) } }));
  check(a.calls.dropped - before === 3, `receive(): a join past its budget, a malformed frame and a description with ${MAX_SDP_CANDIDATES + 1} candidates are dropped and counted`);
  reset();
  const r = member(ID('a'));
  r.calls.receive(ID('b'), bytes({ t: 'join', age: 0 }));
  const first = r.calls.roster.get(ID('b'));
  clock.t += 1000;
  r.calls.receive(ID('b'), bytes({ t: 'join', age: 10 ** 9 }));
  check(r.calls.roster.get(ID('b')) === first, 'a second join keeps the first claim of when the peer joined');
  r.calls.gone(ID('b'));
  check(!r.calls.roster.has(ID('b')), 'a peer gone leaves the roster');
}

// ---- Privacy ----

{
  const j = (candidate) => ({ candidate, sdpMid: '0', sdpMLineIndex: 0 });
  check(candidateFor('direct', j(HOST)).candidate === HOST && candidateFor('direct', j(SRFLX)).candidate === SRFLX, "'direct': every candidate as it is");
  check(candidateFor('public-only', j(HOST)) === null, "'public-only': no host candidate");
  check(candidateFor('public-only', j(SRFLX)).candidate.includes('raddr 0.0.0.0 rport 0') && !candidateFor('public-only', j(SRFLX)).candidate.includes('192.168'), "'public-only': a server-reflexive one with its related address cleared");
  for (const privacy of ['direct', 'public-only']) {
    reset();
    const [a, b] = [member(ID('a'), { privacy }), member(ID('b'), { privacy })];
    meet(a, b);
    join(a);
    join(b);
    await settle();
    const sent = world.log.filter((x) => x.from === a.id).map((x) => JSON.stringify(x.msg)).join('\n');
    const leaks = sent.includes('192.168.1.5');
    check(privacy === 'direct' ? leaks : !leaks && sent.includes('203.0.113.5') && sent.includes('c=IN IP4 0.0.0.0'), `'${privacy}': ${privacy === 'direct' ? 'the LAN address goes out' : 'no LAN address in any description or candidate sent, the public one does'}`);
  }
}

// ---- Two members: negotiation ----

{
  reset();
  const [a, b] = [member(ID('a')), member(ID('b'))];
  meet(a, b);
  const sa = camera();
  const sb = camera();
  join(a, sa);
  await settle();
  check(a.calls.roster.size === 0 && b.calls.roster.get(a.id) !== undefined, 'a joins: b, not in the call, keeps a in its roster');
  join(b, sb);
  await settle();
  const la = link(a, b.id);
  const lb = link(b, a.id);
  check(!la.polite && lb.polite, 'the smaller id is impolite');
  check(sentBy(a.id, 'sdp').map((x) => x.msg.d.type).join() === 'offer' && sentBy(b.id, 'sdp').map((x) => x.msg.d.type).join() === 'answer', 'the smaller id offers, the other answers: one description each');
  check(sentBy(a.id, 'sdp')[0].msg.d.sdp.match(/a=sendrecv/g)?.length === 2, 'the offer has both transceivers sending both ways');
  check(la.pc.getTransceivers().length === 2 && lb.pc.getTransceivers().length === 2, 'two transceivers each side');
  check(lb.pc.getTransceivers().every((t) => t.direction === 'sendrecv') && lb.tx.audio.sender.track === sb.getTracks()[0] && lb.tx.video.sender.track === sb.getTracks()[1], "the answering side sends its tracks on the offer's transceivers, both ways");
  check(la.tx.audio.sender.track === sa.getTracks()[0] && la.tx.video.sender.track === sa.getTracks()[1], "the offering side's tracks on its own transceivers");
  const st = (m) => evs(m, 'stream');
  check(st(a).length === 1 && st(b).length === 1 && st(a)[0][1] === b.id && st(a)[0][2].getTracks().map((t) => t.kind).join() === 'audio,video' && a.call.stream(b.id) === st(a)[0][2], "one 'stream' each, with an audio and a video track; call.stream(peer) is it");
  check(evs(a, 'state').map((e) => e[2]).join() === 'connecting' && a.call.state(b.id) === 'connecting', "'connecting' until ICE connects");
  check(await connected(a, b), "'connected' on both once ICE is");
  check(evs(b, 'state').map((e) => e.slice(1).join(':')).join() === `${a.id}:connecting:,${a.id}:connected:`, "b's states: connecting, connected, no reason");
  check(JSON.stringify(a.call.peers) === JSON.stringify([b.id]) && (await a.call.getStats(b.id)).size === 1 && (await a.call.getStats(ID('f'))) === null, 'peers, getStats');

  // setStream: replaceTrack, no description.
  const descs = sentBy(a.id, 'sdp').length + sentBy(b.id, 'sdp').length;
  const s2 = camera();
  await a.call.setStream(s2);
  check(la.tx.audio.sender.track === s2.getTracks()[0] && la.tx.video.sender.track === s2.getTracks()[1], 'setStream: the new tracks on the same senders');
  await a.call.setStream(new FakeStream([new FakeTrack('audio')]));
  check(la.tx.video.sender.track === null && la.tx.audio.sender.track.kind === 'audio', 'setStream with no video track: the video sender sends nothing');
  await a.call.setStream(null);
  await settle();
  check(la.tx.audio.sender.track === null && sentBy(a.id, 'sdp').length + sentBy(b.id, 'sdp').length === descs && la.pc.signalingState === 'stable', 'setStream(null): nothing sent, and no description at all');
  await a.call.setStream(sa);

  // Glare: both restart ICE at once.
  const [ua, ub] = [la.pc.ufrag, lb.pc.ufrag];
  lb.tx.video.sender.replaced.length = 0;
  la.pc.restartIce();
  lb.pc.restartIce();
  await settle();
  check(la.pc.signalingState === 'stable' && lb.pc.signalingState === 'stable' && la.pc.ufrag !== ua && lb.pc.ufrag !== ub, 'glare: both settle, stable, with new credentials');
  check(lb.pc.rollbacks === 1 && la.pc.rollbacks === 0, 'glare: the polite side rolled its offer back, the impolite one ignored the other');
  check(lb.tx.video.sender.replaced.join() === ',video' && lb.tx.video.sender.track === sb.getTracks()[1], 'glare: the polite side set its video track again, after null');
  check(evs(a, 'state').length === 2 && a.call.state(b.id) === 'connected', 'glare: no state change');
  check(evs(a, 'stream').length === 1 && evs(b, 'stream').length === 1, "glare: no second 'stream'");
  // An offer that comes while ours is being made, before our signalling state changes, collides too.
  const applied = la.pc.remoteOffers ?? 0;
  la.makingOffer = true;
  a.calls.receive(b.id, bytes({ t: 'sdp', d: { type: 'offer', sdp: lb.pc.localDescription.sdp } }));
  await settle();
  la.makingOffer = false;
  check((la.pc.remoteOffers ?? 0) === applied && la.pc.signalingState === 'stable', 'the impolite side ignores an offer that comes while it makes its own');

  // An offer with an extra m-section (a newer version's): inactive here, the stream unchanged.
  reset();
  const [x, y] = [member(ID('1')), member(ID('2'))];
  meet(x, y);
  join(y);
  await settle();
  const pc = new FakePc({});
  pc.addTransceiver('audio');
  pc.addTransceiver('video');
  pc.addTransceiver('video');
  await pc.setLocalDescription();
  y.calls.receive(x.id, bytes({ t: 'join', age: 0 }));
  y.calls.receive(x.id, bytes({ t: 'sdp', d: { type: 'offer', sdp: pc.localDescription.sdp } }));
  y.calls.receive(x.id, bytes({ t: 'ice', c: { candidate: SRFLX, sdpMid: '0', sdpMLineIndex: 0 } }));
  await settle();
  const ly = link(y, x.id);
  check(ly.pc.remoteCandidates.length === 1, 'a candidate right behind its description is applied after it, not lost');
  const dirs = ly.pc.getTransceivers().map((t) => t.direction).join();
  check(dirs === 'sendrecv,sendrecv,inactive' && y.call.stream(x.id).getTracks().map((t) => t.kind).join() === 'audio,video', `an offer with three m-sections: the third inactive (${dirs}), the stream one audio and one video`);
  check(sentBy(y.id, 'sdp')[0]?.msg.d.type === 'answer' && sentBy(y.id, 'sdp')[0].msg.d.sdp.includes('a=inactive'), 'and the answer says so');

  // The first video section inactive, a later one sending: the stream has the first one's track (silent), and the
  // later one's is left out like any extra one.
  reset();
  const [o1, o2] = [member(ID('1')), member(ID('2'))];
  meet(o1, o2);
  join(o2);
  await settle();
  const odd = new FakePc({});
  odd.addTransceiver('audio');
  odd.addTransceiver('video', { direction: 'inactive' });
  odd.addTransceiver('video');
  await odd.setLocalDescription();
  o2.calls.receive(o1.id, bytes({ t: 'join', age: 0 }));
  o2.calls.receive(o1.id, bytes({ t: 'sdp', d: { type: 'offer', sdp: odd.localDescription.sdp } }));
  await settle();
  const ot = link(o2, o1.id).pc.getTransceivers();
  const got = o2.call.stream(o1.id)?.getTracks() ?? [];
  check(got.length === 2 && got[0] === ot[0].receiver.track && got[1] === ot[1].receiver.track, 'the first video section inactive and a later one sending: the stream has the first audio and the first video section\'s tracks');

  // An offer that sends nothing: 'stream' all the same, with the tracks it could send on (our media reaches it).
  reset();
  const [u, v] = [member(ID('1')), member(ID('2'))];
  meet(u, v);
  join(v);
  await settle();
  const quiet = new FakePc({});
  quiet.addTransceiver('audio', { direction: 'inactive' });
  quiet.addTransceiver('video', { direction: 'inactive' });
  await quiet.setLocalDescription();
  v.calls.receive(u.id, bytes({ t: 'join', age: 0 }));
  v.calls.receive(u.id, bytes({ t: 'sdp', d: { type: 'offer', sdp: quiet.localDescription.sdp } }));
  await settle();
  const vs = evs(v, 'stream');
  check(sentBy(v.id, 'sdp')[0]?.msg.d.type === 'answer' && vs.length === 1 && vs[0][2] === v.call.stream(u.id) && vs[0][2].getTracks().map((t) => t.kind).join() === 'audio,video', "an offer that sends nothing: answered, and one 'stream' with an audio and a video track");

  // An offer with no media section: no track to show, so no 'stream', and call.stream(peer) is null.
  reset();
  const [e1, e2] = [member(ID('1')), member(ID('2'))];
  meet(e1, e2);
  join(e2);
  await settle();
  const bare = new FakePc({});
  await bare.setLocalDescription();
  e2.calls.receive(e1.id, bytes({ t: 'join', age: 0 }));
  e2.calls.receive(e1.id, bytes({ t: 'sdp', d: { type: 'offer', sdp: bare.localDescription.sdp } }));
  await settle();
  check(sentBy(e2.id, 'sdp')[0]?.msg.d.type === 'answer' && !evs(e2, 'stream').length && e2.call.stream(e1.id) === null, "an offer with no media section: answered, no 'stream', call.stream(peer) null");

  // An answer that sends nothing (recvonly) to our offer: 'stream' too, on the offering side.
  reset();
  const g = member(ID('1'));
  join(g);
  await settle();
  const r = ID('2');
  g.room.add(r);
  g.calls.receive(r, bytes({ t: 'join', age: 0 }));
  await settle();
  const deaf = new FakePc({});
  await deaf.setRemoteDescription(sentBy(g.id, 'sdp')[0].msg.d);
  for (const t of deaf.getTransceivers()) t.direction = 'recvonly';
  await deaf.setLocalDescription();
  g.calls.receive(r, bytes({ t: 'sdp', d: deaf.localDescription }));
  await settle();
  const gs = evs(g, 'stream');
  const gt = link(g, r).pc.getTransceivers();
  check(deaf.localDescription.sdp.match(/a=recvonly/g)?.length === 2 && gs.length === 1 && gs[0][1] === r && g.call.stream(r)?.getTracks().length === 2 && g.call.stream(r).getTracks().every((t, i) => t === gt[i].receiver.track), "an answer that sends nothing: one 'stream' on the offering side, with its transceivers' tracks");
}

// ---- Failures ----

{
  reset();
  const [a, b] = [member(ID('a')), member(ID('b'))];
  meet(a, b);
  join(a);
  join(b);
  await settle();
  await advance(CONNECT_MS - 1);
  check(a.call.state(b.id) === 'connecting', `still connecting ${CONNECT_MS - 1} ms after the link`);
  await advance(1);
  check(evs(a, 'state').at(-1).slice(2).join() === 'failed,no-path' && evs(b, 'state').at(-1).slice(2).join() === 'failed,no-path', `no ICE in ${CONNECT_MS} ms: 'failed', 'no-path', on both`);
  check(a.call.peers.includes(b.id) && a.call.state(b.id) === 'failed' && link(a, b.id).pc.signalingState === 'closed' && (await a.call.getStats(b.id)) === null, 'a failed peer stays in the call, its connection closed');
  await advance(CONNECT_MS * 3);
  check(evs(a, 'state').length === 2 && !a.left, 'and nothing is tried again');
  b.call.leave();
  await settle();
  check(evs(a, 'ended').map((e) => e.slice(1).join()).join() === `${b.id},left` && !a.call.peers.length, "it leaves: 'ended' ('left'), out of peers");

  reset();
  const [c, d] = [member(ID('c')), member(ID('d'))];
  meet(c, d);
  join(c);
  join(d);
  await settle();
  link(c, d.id).pc.setConnection('failed');
  check(evs(c, 'state').at(-1).slice(2).join() === 'failed,no-path', "ICE failed before it ever connected: 'no-path' at once");

  // Connected, then lost: the impolite side restarts, MAX_RESTARTS times in RESTART_WINDOW_MS.
  reset();
  const [e, f] = [member(ID('e')), member(ID('f'))];
  meet(e, f);
  join(e);
  join(f);
  await connected(e, f);
  const [le, lf] = [link(e, f.id), link(f, e.id)];
  lf.tx.video.sender.replaced.length = 0;
  for (let i = 1; i <= MAX_RESTARTS; i++) {
    le.pc.setConnection('failed');
    lf.pc.setConnection('failed');
    await settle();
    check(le.pc.restarts === i && lf.pc.restarts === 0 && e.call.state(f.id) === 'connecting' && f.call.state(e.id) === 'connecting', `ICE lost ${i}: the impolite side restarts it, the polite one waits ('connecting')`);
    iceUp();
    le.pc.setConnection('connected');
    lf.pc.setConnection('connected');
    await settle();
    check(e.call.state(f.id) === 'connected', `restart ${i} connects again`);
    le.pc.connectionState = 'connecting';
    lf.pc.connectionState = 'connecting';
  }
  check(!lf.tx.video.sender.replaced.length, 'a restart offer that crosses none of its own: the polite side leaves its tracks as they are');
  le.pc.setConnection('failed');
  await settle();
  check(evs(e, 'state').at(-1).slice(2).join() === 'failed,lost' && le.pc.restarts === MAX_RESTARTS, `ICE lost again after ${MAX_RESTARTS} restarts in ${RESTART_WINDOW_MS} ms: 'lost', no more restarts`);
  await advance(CONNECT_MS);
  check(lf.pc.connectionState === 'connecting' && f.call.state(e.id) === 'connected', 'the polite side holds what its connection says');

  // Restarts older than RESTART_WINDOW_MS don't count.
  reset();
  const [w1, w2] = [member(ID('a')), member(ID('b'))];
  meet(w1, w2);
  join(w1);
  join(w2);
  await connected(w1, w2);
  const lw = link(w1, w2.id);
  for (let i = 1; i <= MAX_RESTARTS; i++) {
    lw.pc.setConnection('failed');
    await settle();
    iceUp();
    lw.pc.setConnection('connected');
    await settle();
  }
  clock.t += RESTART_WINDOW_MS;
  lw.pc.setConnection('failed');
  await settle();
  check(lw.pc.restarts === MAX_RESTARTS + 1 && w1.call.state(w2.id) === 'connecting', `restarts more than ${RESTART_WINDOW_MS} ms old don't count: ICE lost again is restarted`);

  // A restart that doesn't connect in CONNECT_MS is 'lost'.
  reset();
  const [g, h] = [member(ID('a')), member(ID('b'))];
  meet(g, h);
  join(g);
  join(h);
  await connected(g, h);
  const lg = link(g, h.id);
  lg.pc.setConnection('failed');
  await settle();
  await advance(RESTART_WINDOW_MS);
  check(evs(g, 'state').at(-1).slice(2).join() === 'failed,lost', `a restart not connected in ${CONNECT_MS} ms: 'lost'`);

  // A description that doesn't apply.
  reset();
  const [i, k] = [member(ID('a')), member(ID('b'))];
  meet(i, k);
  join(k);
  await settle();
  k.calls.receive(i.id, bytes({ t: 'join', age: 0 }));
  k.calls.receive(i.id, bytes({ t: 'sdp', d: { type: 'offer', sdp: 'v=0\r\na=bad\r\n' } }));
  await settle();
  check(evs(k, 'state').at(-1).slice(2).join() === 'failed,error', "a description that doesn't apply: 'error'");
  k.calls.receive(i.id, bytes({ t: 'sdp', d: { type: 'answer', sdp: 'v=0\r\n' } }));
  await settle();
  check(evs(k, 'state').length === 2, 'frames for a failed link are ignored');
}

// ---- Seats ----

{
  reset();
  const six = 'abcdef'.split('').map((c) => member(ID(c)));
  meet(...six);
  for (const m of six) {
    join(m, null);
    await connected(m);
  }
  await settle();
  iceUp();
  await settle();
  check(six.every((m) => m.call.peers.length === 5 && m.call.peers.every((p) => m.call.state(p) === 'connected')), '6 in the call, each connected to the 5 others');
  clock.t += 5000;
  const g = member(ID('7'));
  meet(g);
  await settle();
  join(g, null);
  await settle();
  check(evs(g, 'full').length === 1 && evs(g, 'full')[0][1] === DEFAULT_MAX && g.call.left && !world.log.some((x) => x.from === g.id), "a 7th that knows who is in: 'full' at once (max 6), sending nothing");
  check(g.calls.call === null, 'and room.call() may be tried again');

  // An 8th calls before it learns who is in: it links 5, goes on the 6th; nobody linked it.
  const h = member(ID('8'));
  join(h, null);
  await settle();
  meet(h);
  await settle();
  iceUp();
  await settle();
  check(evs(h, 'full').length === 1 && h.call.left, "an 8th that called before learning who is in: 'full'");
  const told = sentBy(six[0].id, 'join').find((x) => x.to === h.id)?.msg.age;
  check(told === Math.round(clock.t - six[0].call.started) && told >= 5000, `a member tells a newcomer how long it has been in the call (${told} ms)`);
  check(sentBy(h.id, 'leave').length === 7, 'and it tells the room it left');
  check(six.every((m) => m.call.peers.length === 5 && !m.call.peers.includes(h.id) && !evs(m, 'ended').length && !evs(m, 'full').length), 'the 6 never linked it and keep their 5');
  check(six.every((m) => sentBy(m.id, 'full').some((x) => x.to === h.id)), "each of the 6 answered it 'full'");

  // A join claiming an old age, then a `full`, at the seated 6: nobody moves.
  const x = ID('9');
  for (const m of six) m.room.add(x);
  for (const m of six) m.calls.receive(x, bytes({ t: 'join', age: 10 ** 12 }));
  for (const m of six) m.calls.receive(x, bytes({ t: 'full' }));
  await settle();
  check(six.every((m) => !m.call.left && m.call.peers.length === 5 && !m.call.peers.includes(x)), "a join claiming it was in for years, and its 'full': seated members keep their seats");
  check(six.every((m) => sentBy(m.id, 'full').some((y) => y.to === x)), "it hears 'full' from each");
}

{
  // A member not seated: gives way to an earlier member, goes on its `full`, not on a later one's.
  reset();
  const five = 'abcde'.split('').map((c) => member(ID(c)));
  meet(...five);
  for (const m of five) join(m, null);
  await connected(...five);
  clock.t += 10000;
  const j = member(ID('f'));
  meet(j);
  join(j, null);
  await settle();
  check(j.call.peers.length === 5 && !j.call.seated(), 'a joiner with 5 links still connecting is not seated');
  const x = ID('0');
  j.room.add(x);
  j.calls.receive(x, bytes({ t: 'join', age: 5000 }));
  await settle();
  check(j.call.left && evs(j, 'full').length === 1, 'no seat for a member that joined before it: the joiner goes');

  reset();
  const ms = 'abcde'.split('').map((c) => member(ID(c)));
  meet(...ms);
  for (const m of ms) join(m, null);
  await connected(...ms);
  clock.t += 10000;
  const k = member(ID('f'));
  meet(k);
  join(k, null);
  await settle();
  const late = ID('1');
  const early = ID('2');
  const outside = ID('3');
  for (const p of [late, early, outside]) k.room.add(p);
  clock.t += NEAR_MS + 1;
  k.calls.receive(late, bytes({ t: 'join', age: 0 }));
  k.calls.receive(late, bytes({ t: 'full' }));
  k.calls.receive(outside, bytes({ t: 'full' }));
  await settle();
  check(!k.call.left && sentBy(k.id, 'full').some((y) => y.to === late), `not seated: a later member (by more than ${NEAR_MS} ms) hears 'full', and its 'full', and one from outside the call, are ignored`);
  k.calls.receive(early, bytes({ t: 'join', age: 20000 }));
  k.calls.receive(early, bytes({ t: 'full' }));
  await settle();
  check(k.call.left && evs(k, 'full').length === 1, "not seated: 'full' from an earlier member, and it goes");

  // 'full' from a member that joined within NEAR_MS after us counts too (joins that crossed).
  reset();
  const ns2 = 'abcde'.split('').map((c) => member(ID(c)));
  meet(...ns2);
  for (const m of ns2) join(m, null);
  await connected(...ns2);
  const k2 = member(ID('f'));
  meet(k2);
  join(k2, null);
  await settle();
  const near = ID('1');
  k2.room.add(near);
  clock.t += NEAR_MS;
  k2.calls.receive(near, bytes({ t: 'join', age: 0 }));
  k2.calls.receive(near, bytes({ t: 'full' }));
  await settle();
  check(k2.call.left, `not seated: 'full' from one that joined ${NEAR_MS} ms after it, and it goes`);

  // Connected to one member that was in before it, the others still connecting: not seated yet.
  reset();
  const ss = 'abcde'.split('').map((c) => member(ID(c)));
  meet(...ss);
  for (const m of ss) join(m, null);
  await connected(...ss);
  const n = member(ID('f'));
  meet(n);
  join(n, null);
  await settle();
  link(n, ss[0].id).pc.setConnection('connected');
  link(ss[0], n.id).pc.setConnection('connected');
  await settle();
  check(n.call.state(ss[0].id) === 'connected' && !n.call.seated(), 'connected to one member in before it, the others still connecting: not seated');
  n.calls.receive(ss[1].id, bytes({ t: 'full' }));
  await settle();
  check(!n.call.left, `but with ${DEFAULT_MAX - 1} in, its own roster has a seat for it: a 'full' from one of those is ignored`);
  // A rival for the last seat, later than it, hears 'full'; with no seat in its own roster now, the 'full' counts.
  const rival = ID('9');
  n.room.add(rival);
  n.calls.receive(rival, bytes({ t: 'join', age: 0 }));
  n.calls.receive(ss[1].id, bytes({ t: 'full' }));
  await settle();
  check(n.call.left && sentBy(n.id, 'full').some((y) => y.to === rival), `with ${DEFAULT_MAX} others in its roster, the same 'full' makes it go`);

  // Seated: neither.
  reset();
  const ns = 'abcde'.split('').map((c) => member(ID(c)));
  meet(...ns);
  for (const m of ns) join(m, null);
  await connected(...ns);
  const s = member(ID('f'));
  meet(s);
  join(s, null);
  await connected(s, ...ns);
  check(s.call.seated(), 'once its links connect, the joiner is seated');
  const old = ID('0');
  s.room.add(old);
  s.calls.receive(old, bytes({ t: 'join', age: 10 ** 9 }));
  s.calls.receive(old, bytes({ t: 'full' }));
  await settle();
  check(!s.call.left && sentBy(s.id, 'full').some((y) => y.to === old), "seated: an earlier member with no seat hears 'full', and its 'full' is ignored");
}

{
  // Two join at once for the last seat (crossing joins): no member holds more than max - 1 links.
  reset();
  const five = 'abcde'.split('').map((c) => member(ID(c)));
  meet(...five);
  for (const m of five) join(m, null);
  await connected(...five);
  const [p, q] = [member(ID('p')), member(ID('q'))];
  meet(p, q);
  await settle();
  join(p, null);
  join(q, null);
  await settle();
  iceUp();
  await settle();
  const all = [...five, p, q];
  const most = Math.max(...all.filter((m) => !m.call.left).map((m) => m.call.peers.length));
  check(most <= DEFAULT_MAX - 1 && five.every((m) => !m.call.left), `two joining for the last seat: at most ${DEFAULT_MAX - 1} links each (${most}), the 5 stay (${[p, q].map((m) => (m.call.left ? 'full' : 'in')).join(', ')})`);

  // max: 2 makes a call of two.
  reset();
  const [a, b, c] = [member(ID('a')), member(ID('b')), member(ID('c'))];
  meet(a, b, c);
  join(a, null, { max: 2 });
  await settle();
  join(b, null, { max: 2 });
  await connected(a, b);
  join(c, null, { max: 2 });
  await settle();
  check(c.call.left && evs(c, 'full')[0][1] === 2 && a.call.peers.length === 1, "max 2: a third gets 'full' (2)");
}

// ---- Leaving ----

{
  reset();
  const [a, b, c] = [member(ID('a')), member(ID('b')), member(ID('c'))];
  meet(a, b, c);
  for (const m of [a, b, c]) join(m);
  await connected(a, b, c);
  const n = b.events.length;
  b.call.leave();
  b.call.leave();
  await settle();
  check(sentBy(b.id, 'leave').length === 2 && b.call.left && !b.call.peers.length && b.events.length === n, 'leave(): the members hear it, once; no events after');
  check([a, c].every((m) => evs(m, 'ended').map((e) => e.slice(1).join()).join() === `${b.id},left` && !m.calls.roster.has(b.id)), "the others: 'ended' ('left'), out of the roster");
  check(link(a, c.id) && a.call.state(c.id) === 'connected', 'the rest of the call goes on');
  a.calls.gone(c.id);
  check(evs(a, 'ended').at(-1).slice(1).join() === `${c.id},gone` && !a.call.peers.length, "a member leaving the room: 'ended' ('gone')");
  const sent = world.log.length;
  c.calls.close();
  check(c.call.left && world.log.length === sent, 'the room leaving ends the call with no frame (its goodbye says it)');
  await b.call.setStream(null).then(() => check(false, 'setStream after leave'), (e) => check(e instanceof Error, 'setStream after leaving throws'));
}

// ---- The API's checks ----

{
  reset();
  const a = member(ID('a'));
  throws(() => a.calls.open(null, { max: 1 }), RangeError, 'max 1');
  throws(() => a.calls.open(null, { max: MAX_CALL + 1 }), RangeError, `max ${MAX_CALL + 1}`);
  throws(() => a.calls.open(null, { max: 2.5 }), RangeError, 'max 2.5');
  throws(() => a.calls.open({}, {}), TypeError, 'a stream that is no MediaStream');
  a.calls.open(null, { max: MAX_CALL });
  throws(() => a.calls.open(null), Error, 'a second call while in one');
  throws(() => member(ID('b'), { media: false }).calls.open(null), TypeError, 'no WebRTC media');
}

// ---- Real rooms: local signalling, the core in wasm; media still fake ----

{
  reset();
  const opts = { appId: 'peerwire-test', roomId: `call-${process.pid}` };
  const until = async (what, cond, ms = 15000) => {
    const t = Date.now();
    while (!cond()) {
      if (Date.now() - t > ms) return check(false, `timed out: ${what}`);
      await new Promise((r) => setTimeout(r, 10));
    }
    return true;
  };
  const media = { RTCPeerConnection: FakePc, MediaStream: FakeStream, iceServers: [], privacy: 'direct' };
  const rooms = await Promise.all([0, 1, 2].map(() => joinRoom(opts)));
  for (const r of rooms) r.calls.media = media;
  const [a, b, c] = rooms;
  await until('a full mesh', () => rooms.every((r) => r.peers.length === 2));
  const got = new Map(rooms.map((r) => [r.self, []]));
  const ca = a.call(camera());
  const cb = b.call(camera());
  for (const [r, call] of [[a, ca], [b, cb]]) for (const e of ['stream', 'ended']) call.on(e, (...x) => got.get(r.self).push([e, ...x]));
  await until('the streams', () => got.get(a.self).some((e) => e[0] === 'stream') && got.get(b.self).some((e) => e[0] === 'stream'));
  iceUp();
  await until('connected', () => ca.state(b.self) === 'connected' && cb.state(a.self) === 'connected');
  check(true, 'two rooms: a call signalled over their room link, streams both ways, connected');
  check(c.calls.roster.has(a.self) && c.calls.roster.has(b.self) && !c.calls.call, 'a member not in the call keeps the roster');
  const d = await joinRoom(opts);
  d.calls.media = media;
  await until('d to join', () => d.peers.length === 3);
  await until('d to learn who is in', () => d.calls.roster.size === 2);
  check(d.calls.roster.has(a.self) && d.calls.roster.has(b.self), 'a member joining the room later learns who is in the call');
  const cd = d.call(null);
  await until('d in the call', () => cd.peers.length === 2 && ca.peers.includes(d.self));
  iceUp();
  await until('d connected', () => cd.peers.every((p) => cd.state(p) === 'connected'));
  check(true, 'it joins the call and connects to both');
  // A call frame on the unreliable lane is dropped.
  c.write(a.self, Uint8Array.of(9, ...bytes({ t: 'join', age: 0 })), Lane.Unreliable);
  await new Promise((r) => setTimeout(r, 300));
  check(!a.calls.roster.has(c.self), 'a call frame on the unreliable lane is dropped');
  c.write(a.self, Uint8Array.of(9, ...bytes({ t: 'join', age: 0 })), Lane.Reliable);
  await until("c's join on the reliable lane", () => a.calls.roster.has(c.self));
  await b.leave();
  await until("a and d see b's leave", () => [ca, cd].every((x) => !x.peers.includes(b.self)));
  check(cb.left && !b.calls.call, "leaving the room leaves its call");
  check(got.get(a.self).some((e) => e[0] === 'ended' && e[1] === b.self && e[2] === 'gone'), "b leaving the room: 'ended' ('gone') on a");
  throws(() => b.call(null), Error, 'room.call() after leaving the room');
  await Promise.all([a, c, d].map((r) => r.leave()));
}

console.log(failed ? `FAIL: ${failed} check(s)` : 'PASS');
process.exit(failed ? 1 : 0);
