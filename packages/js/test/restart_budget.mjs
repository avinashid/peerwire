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
//    after a reconnect, with the peer's ask, restarts at once.
//
//   node packages/js/test/restart_budget.mjs
import { WebRtcTransport, MAX_RESTARTS } from '../src/webrtc.js';

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
    this.localDescription = { ...d, toJSON: () => ({ type: d.type, sdp: d.sdp }) };
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

// Our transport; `from(peer, msg)` hands it a signal, `to(peer, kind)` lists what it sent.
const mk = () => {
  const sent = [];
  const traces = [];
  const sig = { send: (to, msg) => sent.push([to, msg]), onMessage(fn) { this.fn = fn; }, close() {} };
  const ep = { set onTransmit(f) {}, connect() {}, receive() {}, setRelayOnly() {}, isConnected: () => false };
  const t = new WebRtcTransport(ep, sig, { trickle: false, RTCPeerConnection: FakePc });
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

console.log(failed ? `${failed} FAILED` : 'all passed');
process.exit(failed ? 1 : 0);
