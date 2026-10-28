// NB46-1: a peer can't keep us publishing offers and answers to the signalling relays,
// which rate-limit and ban busy clients. One WebRtcTransport on a scripted fake
// RTCPeerConnection (no browser); the test plays the peer. Checks:
// 1. a 'restart' ask while our ICE is connected is ignored: no restart, nothing sent;
// 2. a peer looping 'restart' (our ICE down, it answers each offer, ICE reconnects, it
//    asks again) gets MAX_RESTARTS restarts (a reconnect doesn't start the count over)
//    and at most the publish budget of messages;
// 3. an offer and 200 copies of it get at most the budget of answers, the rest counted
//    in `throttled`; another peer's budget is its own; it refills at publishEveryMs, and
//    peers whose budget is whole again are forgotten;
// 4. a linked peer's 50 new offers (ICE-restart offers) get at most the budget of answers;
// 5. a genuine ICE failure still restarts: 'failed' at once as the dialling side (an
//    offer with restart: true) and as the other side (a 'restart' ask); a second drop
//    after a reconnect, with the peer's ask, restarts at once;
// 6. candidates of an answer past the budget wait with it (NB46-1a): a linked peer's 50
//    new offers get the budget of answers and only their candidates, trickled ('ice') or
//    late ('ice-batch'); a copy that releases a held answer sends it made again, with the
//    candidates found meanwhile in it and none sent twice, and those found after flow;
// 7. a copy of a new offer that comes while it is being answered gets no answer to the
//    last one (NB46-1b), whether that one went out or was held;
// 8. the restart window slides: a minute after the 3rd restart a 4th goes through (NB46-T1),
//    and one refused while ICE is down is made once the window frees (NB46-1c).
//
//   node packages/js/test/restart_budget.mjs
import { WebRtcTransport, MAX_RESTARTS, RESTART_WINDOW_MS } from '../src/webrtc.js';

let failed = 0;
const ok = (cond, what) => {
  console.log(`${cond ? 'ok  ' : 'FAIL'} ${what}`);
  if (!cond) failed++;
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const id = (n) => n.toString(16).padStart(2, '0').repeat(32);
const [ME, PEER, OTHER, THIRD] = [id(0x0a), id(0x0b), id(0x0c), id(0x0d)]; // we dial PEER
const BURST = WebRtcTransport.publishBurst;
const EVERY = WebRtcTransport.publishEveryMs;

let seq = 0;
const sdp = (tag) => `v=0\r\na=ice-ufrag:${tag}\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n`;
const offer = (tag, extra = {}) => ({ kind: 'offer', sdp: { type: 'offer', sdp: sdp(tag) }, ...extra });
const answer = (tag) => ({ kind: 'answer', sdp: { type: 'answer', sdp: sdp(tag) } });

// Enough of RTCPeerConnection for the signalling: descriptions apply at once, gathering is
// complete, and the test sets ICE.
class FakePc extends EventTarget {
  constructor() {
    super();
    this.iceGatheringState = 'complete';
    this.iceConnectionState = 'new';
    this.connectionState = 'new';
    this.signalingState = 'stable';
    this.localDescription = null;
    this.remoteDescription = null;
  }
  createDataChannel() { return { binaryType: '', readyState: 'connecting', send() {}, close() {} }; }
  async createOffer() { return { type: 'offer', sdp: sdp(`o${++seq}`) }; }
  async createAnswer() { return { type: 'answer', sdp: sdp(`a${++seq}`) }; }
  async setLocalDescription(d) {
    this.localDescription = { ...d, toJSON() { return { type: this.type, sdp: this.sdp }; } };
    this.signalingState = d.type === 'offer' ? 'have-local-offer' : 'stable';
  }
  async setRemoteDescription(d) {
    if (typeof d?.sdp !== 'string') throw new Error('malformed');
    this.remoteDescription = d;
    this.signalingState = d.type === 'offer' ? 'have-remote-offer' : 'stable';
  }
  async addIceCandidate() {}
  close() { this.signalingState = 'closed'; }
  // ICE goes `state`; `quiet`: no event, the transport sees it when it next looks.
  ice(state, quiet = false) {
    this.iceConnectionState = state;
    if (!quiet) this.dispatchEvent(new Event('iceconnectionstatechange'));
  }
}

// A server-reflexive candidate.
const cand = (n) => ({ candidate: `candidate:${n} 1 udp 1 203.0.113.5 ${50000 + n} typ srflx raddr 0.0.0.0 rport 0`, sdpMid: '0',
  toJSON() { return { candidate: this.candidate, sdpMid: '0' }; } });
// Gathering that finds `k` candidates a moment after each answer is set (as a browser does
// once an ICE-restart offer is applied), then completes. `found(c)` adds one to the local
// description, as a browser does, and hands it to the transport.
const gathering = (k) => class extends FakePc {
  async setLocalDescription(d) {
    await super.setLocalDescription(d);
    if (d.type !== 'answer') return;
    setTimeout(() => {
      for (let i = 0; i < k; i++) this.found(cand(seq * 10 + i));
      this.onicecandidate?.({ candidate: null });
    }, 1);
  }
  found(c) {
    this.localDescription = { ...this.localDescription, sdp: `${this.localDescription.sdp}a=${c.candidate}\r\n` };
    this.onicecandidate?.({ candidate: c });
  }
};
// The candidates a message published to the peer carries, in its description or as 'ice'
// or 'ice-batch'.
const carried = (m) => (m.kind === 'ice' ? [m.candidate.candidate] : m.kind === 'ice-batch' ? m.c.map((c) => c.candidate)
  : (m.sdp?.sdp ?? '').split('\r\n').filter((a) => a.startsWith('a=candidate:')).map((a) => a.slice(2)));

// Our transport; `from(peer, msg)` hands it a signal, `to(peer, kind)` lists what it sent.
const mk = (trickle = false, Pc = FakePc) => {
  const sent = [];
  const traces = [];
  const sig = { send: (to, msg) => sent.push([to, msg]), onMessage(fn) { this.fn = fn; }, close() {} };
  const ep = { set onTransmit(f) {}, connect() {}, receive() {}, setRelayOnly() {}, isConnected: () => false };
  const t = new WebRtcTransport(ep, sig, { trickle, RTCPeerConnection: Pc });
  t.trace = (peer, what, d) => traces.push({ peer, what, ...d });
  const from = (peer, msg) => sig.fn(peer, msg);
  const to = (peer, kind) => sent.filter(([p, m]) => p === peer && (!kind || m.kind === kind));
  return { t, traces, from, to };
};
const open = (l) => {
  l.dc.readyState = 'open';
  l.dc.onopen();
  l.pc.ice('connected');
};
// A link we dialled to PEER (we are the dialling side), open, ICE connected.
const dialled = async (s) => {
  await s.t.dial(PEER);
  const l = s.t.links.get(s.t.byPeer.get(PEER));
  s.from(PEER, answer('peer0'));
  await sleep(5);
  open(l);
  return l;
};
// A link `peer` dialled to us (we answer), open, ICE connected.
const answered = async (s, peer) => {
  s.from(peer, offer(`${peer.slice(0, 2)}0`));
  await sleep(5);
  const l = s.t.links.get(s.t.byPeer.get(peer));
  open(l);
  return l;
};

// 1. A 'restart' ask while our ICE is connected: nothing.
{
  const s = mk();
  await dialled(s);
  const n0 = s.to(PEER).length;
  for (let i = 0; i < 20; i++) s.from(PEER, { kind: 'restart' });
  await sleep(20);
  ok(s.t.restarts === 0 && s.to(PEER).length === n0, `20 'restart' asks while our ICE is connected: ${s.t.restarts} restarts, ${s.to(PEER).length - n0} messages sent`);
  ok(s.t.badSignals === 0, 'no signal failed');
  s.t.close();
}

// 2. A peer looping 'restart': each round our ICE is down, it asks, answers our offer, and
// ICE reconnects.
{
  const s = mk();
  const l = await dialled(s);
  for (let i = 1; i <= 20; i++) {
    l.pc.ice('disconnected', true); // quiet: no grace timer, only the ask
    s.from(PEER, { kind: 'restart' });
    await sleep(5);
    if (l.pc.signalingState === 'have-local-offer') s.from(PEER, answer(`peer${i}`));
    await sleep(5);
    l.pc.ice('connected');
  }
  const asked = s.traces.filter((t) => t.what === 'ice-restart' && t.reason === 'asked').length;
  ok(s.t.restarts === MAX_RESTARTS && asked === MAX_RESTARTS, `a peer looping 'restart' 20 times (answering each, ICE reconnecting): ${s.t.restarts} restarts, want ${MAX_RESTARTS} (a reconnect doesn't start the count over)`);
  const out = s.to(PEER).length;
  ok(out <= BURST, `${out} messages published to it in all (the dial's offer and ${s.to(PEER, 'offer').length - 1} restart offers), budget ${BURST}`);
  ok(s.t.badSignals === 0, 'no signal failed');
  s.t.close();
}

// 3. An offer and 200 copies of it: the answer, then copies of it within the budget.
{
  const s = mk();
  const o = offer('copied');
  s.from(PEER, o);
  await sleep(5);
  const flooded = Date.now(); // a copy is answered (or not) within its from(): no await before
  for (let i = 0; i < 200; i++) s.from(PEER, structuredClone(o));
  await sleep(20);
  const answers = s.to(PEER, 'answer').length;
  ok(answers === BURST && s.t.resent === BURST - 1, `an offer and 200 copies: ${answers} answers (${s.t.resent} resent), want ${BURST}`);
  ok(s.t.throttled === 200 - (BURST - 1), `the other ${s.t.throttled} copies are counted in throttled, want ${200 - (BURST - 1)}`);
  ok(s.traces.filter((t) => t.what === 'throttled' && t.kind === 'answer').length === s.t.throttled, "each traced as 'throttled'");
  // Another peer's budget is its own.
  s.from(OTHER, offer('other'));
  await sleep(5);
  ok(s.to(OTHER, 'answer').length === 1, "another peer's offer is still answered");
  // It refills: one a publishEveryMs.
  WebRtcTransport.publishEveryMs = 200;
  await sleep(250);
  for (let i = 0; i < 20; i++) s.from(PEER, structuredClone(o));
  await sleep(10);
  const more = s.to(PEER, 'answer').length - answers;
  const most = Math.floor((Date.now() - flooded) / 200); // tokens back since the flood, at most
  ok(more >= 1 && more <= most, `${Date.now() - flooded} ms on, at one per 200 ms: ${more} more answers to 20 copies (1 to ${most})`);
  // Peers whose budget is whole again are forgotten.
  WebRtcTransport.publishEveryMs = 10;
  await sleep(100);
  s.from(THIRD, offer('third'));
  await sleep(5);
  ok(s.t.budgets.size === 1 && s.t.budgets.has(THIRD), `budgets kept: ${s.t.budgets.size} (only the peer just answered)`);
  WebRtcTransport.publishEveryMs = EVERY;
  ok(s.t.badSignals === 0, 'no signal failed');
  s.t.close();
}

// 4. A linked peer's 50 new offers (ICE-restart offers from the dialling side).
{
  const s = mk();
  await answered(s, PEER);
  for (let i = 1; i <= 50; i++) s.from(PEER, offer(`restart${i}`, { restart: true }));
  await sleep(50);
  const answers = s.to(PEER, 'answer').length;
  ok(answers === BURST, `a linked peer's first offer and 50 new ones: ${answers} answers, want ${BURST}`);
  ok(s.t.badSignals === 0, 'no signal failed');
  s.t.close();
}

// 5. A genuine ICE failure still restarts.
{
  const s = mk();
  const l = await dialled(s);
  const o0 = s.to(PEER, 'offer').length;
  l.pc.ice('failed');
  await sleep(20);
  ok(s.t.restarts === 1 && s.to(PEER, 'offer').slice(o0).some(([, m]) => m.restart === true), `ICE 'failed' as the dialling side: a restart offer at once (${s.t.restarts} restart)`);
  s.from(PEER, answer('peer1'));
  await sleep(5);
  l.pc.ice('connected');
  // ICE drops again and the peer asks before our grace is over: a restart now, the
  // second within the window.
  l.pc.ice('disconnected');
  s.from(PEER, { kind: 'restart' });
  await sleep(20);
  const asked = s.traces.filter((t) => t.what === 'ice-restart' && t.reason === 'asked').length;
  ok(s.t.restarts === 2 && asked === 1 && l.pc.signalingState === 'have-local-offer', `after a reconnect, ICE 'disconnected' and the peer asks: restarted at once (${s.t.restarts} restarts, ${asked} asked)`);
  ok(s.t.badSignals === 0, 'no signal failed');
  s.t.close();

  const s2 = mk();
  const l2 = await answered(s2, PEER);
  l2.pc.ice('failed');
  await sleep(20);
  ok(s2.to(PEER, 'restart').length === 1, `ICE 'failed' as the answering side: it asks the peer to restart (${s2.to(PEER, 'restart').length} ask)`);
  s2.t.close();
}

// 6. Candidates of an answer past the budget wait with it (NB46-1a): a linked peer's 50 new
// offers, our gathering finding candidates after each answer.
for (const [trickle, k, kind] of [[true, 3, 'ice'], [false, 1, 'ice-batch']]) {
  WebRtcTransport.publishEveryMs = 1e9; // no refill while the offers come, however slow the host
  const s = mk(trickle, gathering(k));
  await answered(s, PEER);
  for (let i = 1; i <= 50; i++) {
    s.from(PEER, offer(`new${i}`, { restart: true }));
    await sleep(3);
  }
  await sleep(50);
  const [answers, cands] = [s.to(PEER, 'answer').length, s.to(PEER, kind).length];
  const want = trickle ? k * BURST : BURST;
  ok(answers === BURST && cands === want, `${trickle ? 'trickle' : 'no trickle'}: a linked peer's first offer and 50 new ones: ${answers} answers and ${cands} '${kind}', want ${BURST} and ${want}`);
  ok(s.t.badSignals === 0, 'no signal failed');
  WebRtcTransport.publishEveryMs = EVERY;
  s.t.close();
}
// A held answer released by a copy of its offer once the budget refills: made again with
// the candidates found meanwhile, none of them sent twice, and those found after it flow.
for (const trickle of [true, false]) {
  const how = trickle ? 'trickle' : 'no trickle';
  WebRtcTransport.publishBurst = 1;
  const s = mk(trickle, gathering(2));
  const l = await answered(s, PEER); // spends the one token
  const n0 = s.to(PEER).length;
  const o = offer('held', { restart: true });
  s.from(PEER, o);
  await sleep(20);
  ok(l.unsent && s.to(PEER).length === n0, `${how}: an answer past the budget is held, and its 2 candidates with it (${s.to(PEER).length - n0} published)`);
  WebRtcTransport.publishEveryMs = 10;
  await sleep(30);
  s.from(PEER, structuredClone(o));
  await sleep(5);
  l.pc.found(cand(999));
  l.pc.onicecandidate({ candidate: null });
  await sleep(5);
  const after = s.to(PEER).slice(n0).map(([, m]) => m);
  const [answerOut, next] = [after[0] ?? {}, after[1] ?? {}];
  ok(after.length === 2 && answerOut.kind === 'answer' && carried(answerOut).length === 2 && !l.unsent,
    `${how}: a copy of its offer once the budget refills releases it, made again with the 2 candidates found meanwhile (${after.map((m) => m.kind)}; ${carried(answerOut).length} in the answer)`);
  ok(next.kind === (trickle ? 'ice' : 'ice-batch') && carried(next).join() === cand(999).candidate,
    `${how}: a candidate found after it goes out, alone (${carried(next).length} in its '${next.kind}')`);
  WebRtcTransport.publishBurst = BURST;
  WebRtcTransport.publishEveryMs = EVERY;
  s.t.close();
}

// 7. A copy of a new offer that comes while it is being answered (NB46-1b): no answer to
// the last offer, whether that went out or was held for the budget.
{
  const s = mk();
  await answered(s, PEER);
  const n0 = s.to(PEER, 'answer').length;
  const last = s.to(PEER, 'answer')[0][1].sdp.sdp;
  const o2 = offer('second', { restart: true });
  s.from(PEER, o2);
  s.from(PEER, structuredClone(o2)); // the dialler's copy, or a relay's duplicate
  await sleep(20);
  const after = s.to(PEER, 'answer').slice(n0).map(([, m]) => m.sdp.sdp);
  ok(after.length === 1 && after[0] !== last, `a copy of a new offer while it is answered, the last answer sent: ${after.length} answers, ${after.filter((a) => a === last).length} to the last offer (want 1 and 0)`);
  s.t.close();
}
{
  WebRtcTransport.publishBurst = 1;
  const s = mk();
  const l = await answered(s, PEER); // spends the one token
  s.from(PEER, offer('first', { restart: true }));
  await sleep(10);
  const last = l.pc.localDescription.sdp; // its answer, held
  WebRtcTransport.publishEveryMs = 10;
  await sleep(30);
  const n0 = s.to(PEER, 'answer').length;
  const o2 = offer('second', { restart: true });
  s.from(PEER, o2);
  s.from(PEER, structuredClone(o2));
  await sleep(20);
  const after = s.to(PEER, 'answer').slice(n0).map(([, m]) => m.sdp.sdp);
  ok(after.length === 1 && after[0] === l.pc.localDescription.sdp && !l.unsent,
    `a copy of a new offer while it is answered, the last answer held: ${after.length} answers, ${after.filter((a) => a === last).length} to the last offer (want 1 and 0)`);
  WebRtcTransport.publishBurst = BURST;
  WebRtcTransport.publishEveryMs = EVERY;
  s.t.close();
}

// 8. The restart window slides (NB46-T1): ICE fails as the dialling side, we restart, the
// peer answers and ICE connects, MAX_RESTARTS + 1 times; then the clock moves on a minute.
{
  const realNow = Date.now;
  const s = mk();
  const l = await dialled(s);
  const round = async () => {
    l.pc.ice('failed');
    await sleep(10);
    if (l.pc.signalingState === 'have-local-offer') s.from(PEER, answer(`round${++seq}`));
    await sleep(5);
    l.pc.ice('connected');
  };
  for (let i = 0; i <= MAX_RESTARTS; i++) await round();
  ok(s.t.restarts === MAX_RESTARTS, `ICE failing ${MAX_RESTARTS + 1} times in a row: ${s.t.restarts} restarts, want ${MAX_RESTARTS}`);
  Date.now = () => realNow() + RESTART_WINDOW_MS + 1000;
  await round();
  Date.now = realNow;
  ok(s.t.restarts === MAX_RESTARTS + 1, `${(RESTART_WINDOW_MS + 1000) / 1000} s later, ICE failing again: ${s.t.restarts} restarts, want ${MAX_RESTARTS + 1}`);
  s.t.close();
}
// One refused while ICE is down is made once the window frees, with no other event (NB46-1c).
{
  const realNow = Date.now;
  const s = mk();
  const l = await dialled(s);
  for (let i = 0; i < MAX_RESTARTS; i++) {
    l.pc.ice('failed');
    await sleep(10);
    s.from(PEER, answer(`round${++seq}`));
    await sleep(5);
    l.pc.ice('connected');
  }
  // The clock moves on to 30 ms before the first leaves the window, and ICE fails for good.
  const skew = l.restartAt[0] + RESTART_WINDOW_MS - 30 - realNow();
  Date.now = () => realNow() + skew;
  l.pc.ice('failed');
  await sleep(150);
  Date.now = realNow;
  ok(s.t.restarts === MAX_RESTARTS + 1 && l.pc.signalingState === 'have-local-offer',
    `ICE failing a ${MAX_RESTARTS + 1}th time, 30 ms before the window frees: ${s.t.restarts} restarts once it does, want ${MAX_RESTARTS + 1}`);
  s.t.close();
}

console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
