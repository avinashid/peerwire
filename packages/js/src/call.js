// Calls: voice and video between the members of a room, each linked to every other (a mesh), up to `max`
// of them (6 by default). room.call() joins the room's call; the app brings the MediaStream.
//
//   const stream = await navigator.mediaDevices.getUserMedia({ audio: true, video: true });
//   const call = room.call(stream);
//   call.on('stream', (peer, s) => (videos[peer].srcObject = s));
//   call.on('ended', (peer) => videos[peer].remove());
//
// Each peer in the call gets an RTCPeerConnection for media alone, apart from the room's link to it
// (webrtc.js), whatever route that link takes, and is signalled over that link: frames of the room's tag
// Call, JSON on the reliable lane, sealed and authenticated like any room message. Nothing goes through the
// signalling relays. The smaller PeerId offers, with one audio and one video transceiver, and the other
// answers on them, so a stream set later (a camera turned on, a screen shared) only replaces tracks and
// never renegotiates. Descriptions follow perfect negotiation (the larger PeerId is polite); only the smaller
// restarts ICE, so our own offers never cross, but a polite side whose offer is rolled back recovers.
//
// Seats. A member in the call says so (`join`, with how long it has been in it, `age`) to every room member,
// and to each that joins the room later, so every member knows who is in the call, in it or not (the
// roster). Joining when `max` are in already is 'full' at once. Otherwise the joiner tells every member,
// and each one in the call links it unless that makes more than `max`: then it answers `full`. A member is
// seated once one of its links has connected and none to a member that was in the call before it (its
// seniors) is still connecting. One not seated gives way: with no seat for a member that joined before it,
// it leaves the call ('full'), and it leaves on a `full` from one that joined before it or within NEAR_MS
// of it. A seated member never does. So the joiner goes, also one that called as soon as it joined the room
// and learns who is in only then; `age` counts only between members not seated, so one claiming it joined
// long ago can't take a seat; and two joining at the same moment for the last seat may both go.
//
// A peer whose media isn't connected CONNECT_MS after its link was made (no direct path, and no TURN server
// in iceServers) fails ('no-path') and stays failed until it or we join the call again. One that connected
// and then lost ICE gets MAX_RESTARTS ICE restarts in RESTART_WINDOW_MS, CONNECT_MS each, then fails ('lost').

import { withoutHostCandidates, candidateFor } from './webrtc.js';

export const DEFAULT_MAX = 6;
export const MAX_CALL = 16;
export const NEAR_MS = 3000;
export const CONNECT_MS = 20000;
export const MAX_RESTARTS = 2;
export const RESTART_WINDOW_MS = 60000;
// A call frame is at most MAX_FRAME bytes, a description in it at most MAX_SDP characters with MAX_SECTIONS
// m-sections, and a candidate at most MAX_CANDIDATE. Each peer's joins, descriptions and candidates come out of
// budgets, BUDGETS[kind] = [burst, ms one takes to refill]: past them a frame is dropped. A join makes a
// connection (and ICE gathering, which asks the STUN servers), a description has us answer it.
export const MAX_FRAME = 64 * 1024;
export const MAX_SDP = 32 * 1024;
export const MAX_SECTIONS = 4;
export const MAX_CANDIDATE = 512;
const MAX_MID = 32;
export const BUDGETS = { join: [6, 5000], sdp: [8, 3000], ice: [64, 250] };

const dec = new TextDecoder();
const KINDS = ['audio', 'video'];
const timer = (ms, fn) => {
  const t = setTimeout(fn, ms);
  return () => clearTimeout(t);
};

// A call frame's body, checked: { t: 'join', age } | { t: 'leave' } | { t: 'full' } | { t: 'sdp', d: { type, sdp } }
// | { t: 'ice', c: { candidate, sdpMid, sdpMLineIndex } }, with no other fields; null if malformed or over a cap.
export function parseCall(bytes) {
  if (bytes.length > MAX_FRAME) return null;
  let m;
  try {
    m = JSON.parse(dec.decode(bytes));
  } catch {
    return null;
  }
  if (typeof m !== 'object' || m === null) return null;
  if (m.t === 'leave' || m.t === 'full') return { t: m.t };
  if (m.t === 'join') return Number.isSafeInteger(m.age) && m.age >= 0 ? { t: 'join', age: m.age } : null;
  if (m.t === 'sdp') {
    const d = m.d;
    if (typeof d !== 'object' || d === null || (d.type !== 'offer' && d.type !== 'answer') || typeof d.sdp !== 'string') return null;
    if (d.sdp.length > MAX_SDP || (d.sdp.match(/^m=/gm)?.length ?? 0) > MAX_SECTIONS) return null;
    return { t: 'sdp', d: { type: d.type, sdp: d.sdp } };
  }
  if (m.t === 'ice') {
    const c = m.c;
    if (typeof c !== 'object' || c === null || typeof c.candidate !== 'string') return null;
    if (c.candidate.length > MAX_CANDIDATE || !c.candidate.startsWith('candidate:')) return null;
    const mid = c.sdpMid ?? null;
    const index = c.sdpMLineIndex ?? null;
    if (mid !== null && (typeof mid !== 'string' || mid.length > MAX_MID)) return null;
    if (index !== null && !(Number.isInteger(index) && index >= 0 && index < MAX_SECTIONS)) return null;
    if (mid === null && index === null) return null;
    return { t: 'ice', c: { candidate: c.candidate, sdpMid: mid, sdpMLineIndex: index } };
  }
  return null;
}

function checkStream(stream) {
  if (stream != null && typeof stream.getTracks !== 'function') throw new TypeError('a call takes a MediaStream, or null to only receive');
}

// A room's calls: the roster, kept whether or not we are in the call, and our call while we are.
// `self` our PeerId; `send(peer, msg)` writes a call frame to a room member; `members()` the room's members;
// `now()` a monotonic clock in ms; `after(ms, fn)` a timer, returning its cancel; `media` { RTCPeerConnection,
// MediaStream, iceServers, privacy } from joinRoom, or null without WebRTC.
export class Calls {
  constructor({ self, send, members, now = () => performance.now(), after = timer, media = null }) {
    Object.assign(this, { self, send, members, now, after, media });
    this.roster = new Map(); // room member in the call -> when it joined the call, by our clock, as it says
    this.budgets = new Map(); // peer -> kind -> { tokens, at }
    this.call = null; // ours, while we are in the call
    this.dropped = 0; // call frames dropped: malformed, or past their budget
  }

  // room.call(): joins the room's call with `stream`.
  open(stream, { max = DEFAULT_MAX } = {}) {
    if (!Number.isInteger(max) || max < 2 || max > MAX_CALL) throw new RangeError(`a call's max is a whole number from 2 to ${MAX_CALL}`);
    checkStream(stream);
    const { RTCPeerConnection: Pc, MediaStream: Ms } = this.media ?? {};
    if (typeof Pc?.prototype?.addTransceiver !== 'function' || typeof Ms !== 'function') {
      throw new TypeError('calls need WebRTC with media (RTCPeerConnection and MediaStream), which this runtime lacks');
    }
    if (this.call) throw new Error('already in the call: call.setStream() changes what we send, call.leave() leaves it');
    const call = new Call(this, stream ?? null, max);
    this.call = call;
    // After this returns, so the app's handlers hear everything.
    queueMicrotask(() => call.start());
    return call;
  }

  // A call frame from a room member.
  receive(peer, bytes) {
    const m = parseCall(bytes);
    if (!m || !this.spend(peer, m.t)) return void this.dropped++;
    if (m.t === 'join') {
      if (!this.roster.has(peer)) this.roster.set(peer, this.now() - m.age);
      this.call?.joined(peer);
    } else if (m.t === 'leave') {
      this.roster.delete(peer);
      this.call?.ended(peer, 'left');
    } else this.call?.signal(peer, m);
  }

  // A member joined the room: it hears that we are in the call.
  joinedRoom(peer) {
    if (this.call?.announced) this.call.announce(peer);
  }

  // A member left the room.
  gone(peer) {
    this.roster.delete(peer);
    this.budgets.delete(peer);
    this.call?.ended(peer, 'gone');
  }

  // We leave the room: our call ends with no word to anyone (the room's goodbye says it).
  close() {
    this.call?.end(null, false);
  }

  // Whether `peer` may send a frame of `kind` now (see BUDGETS).
  spend(peer, kind) {
    const b = BUDGETS[kind];
    if (!b) return true;
    const [burst, every] = b;
    const now = this.now();
    let kinds = this.budgets.get(peer);
    if (!kinds) this.budgets.set(peer, (kinds = {}));
    const s = kinds[kind] ?? (kinds[kind] = { tokens: burst, at: now });
    s.tokens = Math.min(burst, s.tokens + (now - s.at) / every);
    s.at = now;
    if (s.tokens < 1) return false;
    s.tokens--;
    return true;
  }
}

export class Call {
  constructor(calls, stream, max) {
    this.calls = calls;
    this.max = max;
    this.local = stream;
    this.links = new Map(); // peer in the call with us -> its media link
    this.handlers = new Map(); // event -> Set of fns
    this.left = false;
    this.announced = false; // our join went out
    this.started = calls.now();
    this.seniors = new Set(); // the roster when we joined
  }

  get peers() {
    return [...this.links.keys()];
  }

  // 'connecting' | 'connected' | 'failed', or null for a peer not in the call with us.
  state(peer) {
    return this.links.get(peer)?.state ?? null;
  }

  // The peer's MediaStream, once it has come ('stream'), else null.
  stream(peer) {
    const l = this.links.get(peer);
    return l?.shown ? l.remote : null;
  }

  getStats(peer) {
    const l = this.links.get(peer);
    return l && !l.closed ? l.pc.getStats() : Promise.resolve(null);
  }

  // Sends `stream`'s first audio and first video track from now on (null: nothing), on the transceivers each
  // link has: replaceTrack, with no renegotiation.
  async setStream(stream) {
    checkStream(stream);
    if (this.left) throw new Error('left the call');
    this.local = stream ?? null;
    const swaps = [];
    for (const l of this.links.values()) {
      if (!l.tx || l.closed) continue;
      for (const k of KINDS) if (l.tx[k]) swaps.push(l.tx[k].sender.replaceTrack(this.track(k)).catch(() => {}));
    }
    await Promise.all(swaps);
  }

  leave() {
    this.end(null, true);
  }

  on(event, fn) {
    if (!this.handlers.has(event)) this.handlers.set(event, new Set());
    this.handlers.get(event).add(fn);
    return () => this.off(event, fn);
  }

  off(event, fn) {
    this.handlers.get(event)?.delete(fn);
  }

  emit(event, ...args) {
    for (const fn of this.handlers.get(event) ?? []) {
      try {
        fn(...args);
      } catch (e) {
        // A throwing handler must not stall the call: report it on its own.
        setTimeout(() => {
          throw e;
        });
      }
    }
  }

  track(kind) {
    return this.local?.getTracks().find((t) => t.kind === kind) ?? null;
  }

  // We join: 'full' if the roster has `max` in already; else we tell every member and link those in the call.
  start() {
    if (this.left) return;
    const { roster } = this.calls;
    if (roster.size + 1 > this.max) return this.end('full', false);
    for (const p of this.calls.members()) this.announce(p);
    this.announced = true;
    this.seniors = new Set(roster.keys());
    for (const p of this.seniors) this.link(p);
  }

  announce(peer) {
    if (!this.left) this.calls.send(peer, { t: 'join', age: Math.round(this.calls.now() - this.started) });
  }

  // How many would be in the call with `peer` not counted: us and the roster's others.
  seats(peer) {
    let n = 1;
    for (const p of this.calls.roster.keys()) if (p !== peer) n++;
    return n;
  }

  // Whether we hold our seat: a link has connected, and none to a senior is still connecting.
  seated() {
    let connected = false;
    for (const l of this.links.values()) {
      if (l.connected) connected = true;
      else if (l.state === 'connecting' && this.seniors.has(l.peer)) return false;
    }
    return connected;
  }

  // `peer` joined the call (or told us, a newcomer to the room, that it is in it): we link it if there is a seat.
  // If not, and we aren't seated, we go if it joined before us; else it hears `full`.
  joined(peer) {
    if (this.left || this.links.has(peer)) return;
    if (this.seats(peer) < this.max) return this.link(peer);
    if (!this.seated() && this.calls.roster.get(peer) < this.started) return this.end('full', true);
    this.calls.send(peer, { t: 'full' });
  }

  // A description, candidate or `full` from `peer`.
  signal(peer, m) {
    if (this.left) return;
    // `full` from a member in the call: a member not seated goes if that one joined before it (or about then).
    if (m.t === 'full') {
      const at = this.calls.roster.get(peer);
      if (at !== undefined && at <= this.started + NEAR_MS && !this.seated()) this.end('full', true);
      return;
    }
    const l = this.links.get(peer);
    if (!l || l.closed) return;
    // In the order they came, each once the one before it is applied.
    l.chain = l.chain.then(() => (m.t === 'sdp' ? this.described(l, m.d) : this.candidate(l, m.c))).catch(() => {});
  }

  // `peer` left the call ('left') or the room ('gone').
  ended(peer, reason) {
    const l = this.links.get(peer);
    if (!l) return;
    this.links.delete(peer);
    this.close(l);
    this.emit('ended', peer, reason);
  }

  // We leave: `full` says why ('full', or null for leave()), `tell` whether the members hear it.
  end(full, tell) {
    if (this.left) return;
    this.left = true;
    if (tell && this.announced) for (const p of this.calls.members()) this.calls.send(p, { t: 'leave' });
    for (const l of this.links.values()) this.close(l);
    this.links.clear();
    if (this.calls.call === this) this.calls.call = null;
    if (full) this.emit('full', this.max);
  }

  link(peer) {
    const { RTCPeerConnection: Pc, MediaStream: Ms, iceServers = [], privacy = 'direct' } = this.calls.media;
    const pc = new Pc({ iceServers });
    const l = { peer, pc, polite: this.calls.self > peer, remote: new Ms(), tx: null, state: null, shown: false, closed: false,
      makingOffer: false, chain: Promise.resolve(), timer: null, connected: false, restarts: [] };
    this.links.set(peer, l);
    pc.onicecandidate = ({ candidate }) => {
      if (!candidate?.candidate || l.closed) return;
      const c = candidateFor(privacy, candidate.toJSON());
      if (c) this.calls.send(peer, { t: 'ice', c: { candidate: c.candidate, sdpMid: c.sdpMid ?? null, sdpMLineIndex: c.sdpMLineIndex ?? null } });
    };
    pc.ontrack = (e) => this.ontrack(l, e);
    pc.onnegotiationneeded = () => this.offer(l);
    pc.onconnectionstatechange = () => this.connection(l);
    // The impolite side offers both transceivers; the polite one answers on them (attach).
    if (!l.polite) l.tx = Object.fromEntries(KINDS.map((k) => [k, pc.addTransceiver(this.track(k) ?? k, { direction: 'sendrecv' })]));
    this.wait(l, 'no-path');
    this.setState(l, 'connecting');
  }

  // Perfect negotiation: an offer of ours when one is needed (the first, or an ICE restart).
  async offer(l) {
    if (l.closed) return;
    try {
      l.makingOffer = true;
      await l.pc.setLocalDescription();
      if (!l.closed) this.describe(l);
    } catch {
      // Closed, or the peer's offer came first: that one is answered instead.
    } finally {
      l.makingOffer = false;
    }
  }

  describe(l) {
    const d = l.pc.localDescription;
    const sdp = this.calls.media.privacy === 'public-only' ? withoutHostCandidates(d.sdp) : d.sdp;
    this.calls.send(l.peer, { t: 'sdp', d: { type: d.type, sdp } });
  }

  // The peer's description. The impolite side ignores an offer that collides with its own; the polite side takes
  // it, rolling its own back. One that can't be applied fails the link ('error').
  async described(l, d) {
    if (l.closed) return;
    const offer = d.type === 'offer';
    // Descriptions are applied one at a time (signal()), so an offer collides only with one of ours.
    const collision = offer && (l.makingOffer || l.pc.signalingState !== 'stable');
    if (collision && !l.polite) return;
    try {
      await l.pc.setRemoteDescription(d);
      if (offer && !l.closed) {
        this.attach(l);
        await l.pc.setLocalDescription();
        if (!l.closed) this.describe(l);
        if (collision) this.reattach(l);
      }
    } catch {
      return this.fail(l, 'error');
    }
    this.show(l);
  }

  // After an offer is applied: our tracks go on the first audio and the first video transceiver, which send both
  // ways; any other (a newer version's) is inactive, so nothing comes in on it.
  attach(l) {
    const ts = l.pc.getTransceivers();
    const first = Object.fromEntries(KINDS.map((k) => [k, ts.find((t) => t.receiver.track.kind === k)]));
    for (const t of ts) if (t !== first.audio && t !== first.video && !t.stopped) t.direction = 'inactive';
    if (l.tx) return;
    l.tx = first;
    for (const k of KINDS) {
      if (!first[k]) continue;
      first[k].direction = 'sendrecv';
      first[k].sender.replaceTrack(this.track(k)).catch(() => {});
    }
  }

  // Our tracks again, after our offer was rolled back: Chromium (153) stops encoding video on such a sender, sending
  // nothing more on it, until its track is set again.
  reattach(l) {
    for (const k of KINDS) {
      const t = this.track(k);
      const s = l.tx?.[k]?.sender;
      if (t && s) s.replaceTrack(null).then(() => !l.closed && this.track(k) === t && s.replaceTrack(t)).catch(() => {});
    }
  }

  async candidate(l, c) {
    if (l.closed) return;
    try {
      await l.pc.addIceCandidate(c);
    } catch {
      // One for an offer we ignored, or one the browser doesn't take: ICE does without it.
    }
  }

  // A remote track: the first audio and the first video transceiver's go in the peer's stream, once each.
  ontrack(l, e) {
    const kind = e.track.kind;
    const first = l.pc.getTransceivers().find((t) => t.receiver.track.kind === kind);
    if (l.closed || (e.transceiver && e.transceiver !== first) || l.remote.getTracks().some((t) => t.kind === kind)) return;
    l.remote.addTrack(e.track);
  }

  // 'stream', once the peer's stream has its tracks.
  show(l) {
    if (l.shown || l.closed || !l.remote.getTracks().length) return;
    l.shown = true;
    this.emit('stream', l.peer, l.remote);
  }

  connection(l) {
    if (l.closed) return;
    const s = l.pc.connectionState;
    if (s === 'connected') {
      l.timer?.();
      l.timer = null;
      l.connected = true;
      this.setState(l, 'connected');
    } else if (s === 'failed') {
      if (!l.connected) return this.fail(l, 'no-path');
      const now = this.calls.now();
      l.restarts = l.restarts.filter((t) => now - t < RESTART_WINDOW_MS);
      if (l.restarts.length >= MAX_RESTARTS) return this.fail(l, 'lost');
      l.restarts.push(now);
      this.setState(l, 'connecting');
      // The impolite side restarts ICE and the polite one waits for its offer, so the two offers don't cross.
      if (!l.polite) l.pc.restartIce();
      this.wait(l, 'lost');
    }
  }

  // Fails the link with `reason` unless it connects within CONNECT_MS.
  wait(l, reason) {
    l.timer?.();
    l.timer = this.calls.after(CONNECT_MS, () => this.fail(l, reason));
  }

  // The link stays failed, the peer in the call, until it leaves.
  fail(l, reason) {
    if (l.closed) return;
    this.close(l);
    this.setState(l, 'failed', reason);
  }

  close(l) {
    l.closed = true;
    l.timer?.();
    l.timer = null;
    try {
      l.pc.close();
    } catch {
      // closed already
    }
  }

  setState(l, state, reason) {
    if (l.state === state) return;
    l.state = state;
    this.emit('state', l.peer, state, reason);
  }
}
