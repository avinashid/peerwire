// Automatic birthday punch (NB-084). A room member that the room reaches only by fallback
// (relayed by another member, or over signalling) gets one browser birthday punch
// (birthday.js) when the WebRTC link that failed showed one side behind a cone NAT and
// the other behind a symmetric one with no port prediction: the pair WebRTC can't link on
// its own, and the one Net Lab's button punches by the NAT types. The punch is
// coordinated over the room's own encrypted link (Punch frames, room.js), and a channel it
// makes takes the link over (room.adoptChannel). The fallback carries the link meanwhile,
// so nobody waits on the punch.
//
// The cone ("easy") side starts: it opens its one connection and offers it, with its public
// address. The symmetric ("hard") side opens its fleet only if what it saw itself agrees
// (it is symmetric with no prediction, the peer a cone), and only at that address if the
// peer's own candidates named its IP; then it answers. The easy side sprays the hard
// side's public IP as the failed link named it, never an address from a Punch frame.
//
//   easy -> hard   { p: 'offer', sdp, addr }   its offer (no candidates) and public ip:port
//   hard -> easy   { p: 'answer', sdp }        the fleet's one shared answer (no candidates)
//   hard -> easy   { p: 'no', busy? }          not now; with `busy`, later may do
//   either         { p: 'stop' }               the punch ended on this side
//
// Bounds: one punch at a time per WebRTC implementation (a page's RTCPeerConnection: all
// its rooms, and Net Lab's button), as a fleet is FLEET_SIZE connections and Chrome allows
// a page about 500; after one that opened a fleet or sprayed, the next waits gapMs. One
// try per peer for the room's life. A punch whose channel hasn't opened by punchMs ends;
// everything it opened closes then, when the peer goes, and when we leave.

import { openFleet, openSpray, randomCredentials, sprayPorts, stripCandidates, isPublicV4, FLEET_SIZE, SPRAY_PORTS } from './birthday.js';
import { natOfPorts } from './webrtc.js';

const MAX_SDP = 16 * 1024; // a Punch frame's description; a data-channel-only one is about 600 bytes
const MAX_TRIED = 1024; // peers punched with in one room; past that, no more punches
const dec = new TextDecoder();

const slots = new WeakMap(); // RTCPeerConnection class -> its punch slot

// The punch slot of `Pc` (a page's RTCPeerConnection): one punch holds it at a time, and after
// one that spent something (`spent`: a fleet, a spray) it stays shut for Punches.gapMs. Net Lab's
// button takes it too, so a page never holds two fleets.
export function punchSlot(Pc) {
  let s = slots.get(Pc);
  if (s) return s;
  let holder = null;
  let shutUntil = 0;
  s = {
    get busy() {
      return holder !== null || performance.now() < shutUntil;
    },
    take(who) {
      if (s.busy) return false;
      holder = who;
      return true;
    },
    free(who, spent = true) {
      if (holder !== who) return;
      holder = null;
      if (spent) shutUntil = performance.now() + Punches.gapMs;
    },
  };
  slots.set(Pc, s);
  return s;
}

// Our first public server-reflexive IPv4 address in `sdp`, as ip:port, or null.
function publicAddr(sdp) {
  for (const m of (sdp ?? '').matchAll(/^a=candidate:\S+ \d+ udp \d+ (\d+\.\d+\.\d+\.\d+) (\d+) typ srflx/gim)) {
    if (isPublicV4(m[1])) return `${m[1]}:${m[2]}`;
  }
  return null;
}

function shut(pc) {
  try {
    pc?.close();
  } catch {
    // already closed
  }
}

// A room's punches. `room` gives { send(peer, m), route(peer), member(peer), nat(peer),
// adopt(peer, pc, dc), trace(peer, what, detail) } (see Room). `on` false, or a WebRTC that
// can't make a certificate (one not a browser's): none of ours, and a peer's offer is refused.
export class Punches {
  static afterMs = 2000; // a peer just on a fallback waits this long (the fallback settles first)
  static answerMs = 15000; // the easy side waits this long for the fleet's answer
  static punchMs = 60000; // no channel by then: the punch ends (at ICE's ~20 checks/s, ~1200 sprayed ports)
  static gapMs = 30000; // between punches in a page, after one that spent something
  static busyMs = [15000, 30000, 60000, 120000]; // a punch put off (busy) tries again after these, then no more
  static fleetSize = FLEET_SIZE; // tests use fewer

  constructor({ RTCPeerConnection, iceServers = [], on = true, room }) {
    this.Pc = RTCPeerConnection;
    this.iceServers = iceServers;
    this.on = on && typeof RTCPeerConnection?.generateCertificate === 'function';
    this.room = room;
    this.slot = punchSlot(RTCPeerConnection);
    this.tried = new Set(); // peers punched with, or being punched with
    this.putOff = new Map(); // peer -> how often its punch was put off as busy
    this.waits = new Map(); // peer -> timer of a punch to begin
    this.runs = new Map(); // peer -> its punch under way
    this.closed = false;
  }

  trace(peer, step, d = {}) {
    this.room.trace(peer, 'birthday', { step, ...d });
  }

  // `peer` may want a punch now: it joined (its route is set by then), or a WebRTC link to it
  // failed, a retry's too (its NATs are known by then). As the cone side, one begins afterMs
  // on, if it still calls for one then.
  consider(peer) {
    if (!this.on || this.closed || this.tried.has(peer) || this.runs.has(peer) || this.waits.has(peer) || !this.view(peer, 'easy')) return;
    this.later(peer, Punches.afterMs);
  }

  later(peer, ms) {
    this.waits.set(peer, setTimeout(() => {
      this.waits.delete(peer);
      this.start(peer).catch(() => {});
    }, ms));
  }

  // Whether what our side saw calls for a punch with `peer`, us as `role`: it is a member the
  // room reaches only by fallback, and the WebRTC link that failed (transport.nat) showed a
  // cone ('easy': ours) and a symmetric NAT with no prediction, at public IPv4s, none of them
  // shared (one NAT, which doesn't hairpin: Net Lab skips that pair too). Gives { ip, theirs }:
  // the symmetric side's public IP (the spray aims there) and the peer's addresses. undefined
  // while it can't say yet (no fallback route, no link seen); null if it doesn't.
  view(peer, role) {
    const r = this.room.route(peer);
    const v = this.room.nat(peer);
    if (!this.room.member(peer) || (r !== 'peer' && r !== 'signaling') || !v) return undefined;
    if ([...v.mine.keys()].some((ip) => v.theirs.has(ip))) return null;
    const [cone, sym] = role === 'easy' ? [v.mine, v.theirs] : [v.theirs, v.mine];
    const s = natOfPorts(sym);
    if (natOfPorts(cone).type !== 'cone' || s.type !== 'symmetric' || s.next || ![...cone.keys()].some(isPublicV4)) return null;
    const ip = [...sym].find(([, ports]) => ports.size > 1)[0];
    return isPublicV4(ip) ? { ip, theirs: v.theirs } : null;
  }

  // Takes the slot and starts `run` with `peer`: it ends unless something moves it on within `ms`.
  begin(peer, run, ms) {
    if (!this.slot.take(run)) return false;
    this.tried.add(peer);
    this.runs.set(peer, run);
    run.timer = setTimeout(() => this.end(peer, run, 'timed out'), ms);
    this.trace(peer, 'start', { role: run.role });
    return true;
  }

  // As the easy side: our one connection, offered with our public address.
  async start(peer) {
    const v = this.view(peer, 'easy');
    if (!v || this.closed || this.tried.has(peer) || this.runs.has(peer) || this.tried.size >= MAX_TRIED) return;
    const run = { role: 'easy', ip: v.ip, easy: null, answered: false, spent: false, timer: null };
    if (!this.begin(peer, run, Punches.answerMs)) return this.busy(peer);
    let easy;
    try {
      easy = await openSpray({ RTCPeerConnection: this.Pc, iceServers: this.iceServers });
    } catch (e) {
      return this.end(peer, run, `error: ${e?.message}`);
    }
    if (this.runs.get(peer) !== run) return shut(easy.pc); // ended meanwhile
    run.easy = easy;
    const addr = publicAddr(easy.offerSdp);
    // With no public address of ours, the fleet would have nothing to aim at.
    if (!addr) return this.end(peer, run, 'no public address');
    this.room.send(peer, { p: 'offer', sdp: stripCandidates(easy.offerSdp), addr });
    easy.done.then(({ pc, dc }) => this.won(peer, run, pc, dc)).catch(() => {});
  }

  // A Punch frame from member `peer`. Malformed ones, and steps out of turn, are dropped.
  receive(peer, body) {
    let m;
    try {
      m = JSON.parse(dec.decode(body));
    } catch {
      return;
    }
    if (!m || typeof m !== 'object') return;
    if (m.p === 'offer') return void this.offered(peer, m).catch(() => {});
    const run = this.runs.get(peer);
    if (!run) return;
    if (m.p === 'stop') return this.end(peer, run, 'the peer stopped', { quiet: true });
    if (run.role !== 'easy') return;
    if (m.p === 'answer') return void this.answered(peer, run, m).catch(() => {});
    if (m.p === 'no' && !run.answered) this.end(peer, run, 'refused', { quiet: true, busy: m.busy === true });
  }

  // As the hard side: the easy side's offer. The fleet opens only if our own view agrees, aimed
  // at the one address the peer gave, if public and one its own candidates named. Else 'no'
  // ('busy' when later may do: the slot is held, or we can't say yet).
  async offered(peer, m) {
    if (this.runs.has(peer)) return; // one punch per peer at a time
    const no = (busy = false) => this.room.send(peer, busy ? { p: 'no', busy } : { p: 'no' });
    if (!this.on || this.closed || this.tried.has(peer) || this.tried.size >= MAX_TRIED) return no();
    if (typeof m.sdp !== 'string' || m.sdp.length > MAX_SDP || typeof m.addr !== 'string') return no();
    const v = this.view(peer, 'hard');
    if (!v) return no(v === undefined);
    const ip = /^(\d{1,3}(?:\.\d{1,3}){3}):\d{1,5}$/.exec(m.addr)?.[1];
    if (!isPublicV4(ip) || !v.theirs.has(ip)) return no();
    const run = { role: 'hard', fleet: null, spent: true, timer: null };
    if (!this.begin(peer, run, Punches.punchMs)) return no(true);
    try {
      const cert = await this.Pc.generateCertificate({ name: 'ECDSA', namedCurve: 'P-256' });
      if (this.runs.get(peer) !== run) return;
      const fleet = await openFleet({ RTCPeerConnection: this.Pc, cert, creds: randomCredentials(), offerSdp: m.sdp, size: Punches.fleetSize, easyAddr: m.addr });
      if (this.runs.get(peer) !== run) return void fleet.conns.forEach((c) => shut(c.pc)); // ended meanwhile
      run.fleet = fleet;
    } catch (e) {
      return this.end(peer, run, `error: ${e?.message}`); // openFleet closed what it opened
    }
    this.trace(peer, 'fleet', { n: run.fleet.conns.length });
    this.room.send(peer, { p: 'answer', sdp: run.fleet.answerSdp });
    run.fleet.done.then(({ pc, dc }) => this.won(peer, run, pc, dc)).catch(() => {});
  }

  // As the easy side: the fleet's answer. We spray the hard side's IP from here on.
  async answered(peer, run, m) {
    if (run.answered || !run.easy || typeof m.sdp !== 'string' || m.sdp.length > MAX_SDP) return;
    run.answered = run.spent = true;
    clearTimeout(run.timer);
    run.timer = setTimeout(() => this.end(peer, run, 'timed out'), Punches.punchMs);
    const ports = sprayPorts(SPRAY_PORTS);
    this.trace(peer, 'spray', { ip: run.ip, n: ports.length });
    try {
      await run.easy.applyAnswer(m.sdp, run.ip, ports);
    } catch (e) {
      this.end(peer, run, `error: ${e?.message}`);
    }
  }

  // The punch made a channel: it leaves the punch's hands (the fleet closed the rest), and both
  // sides hand it to the room. One the room doesn't take closes. The peer isn't told to stop: its
  // own side of the channel may not have opened yet.
  async won(peer, run, pc, dc) {
    if (this.runs.get(peer) !== run) return;
    run.easy = null;
    if (run.fleet) run.fleet.conns = [];
    this.end(peer, run, 'channel open', { quiet: true });
    const adopted = await this.room.adopt(peer, pc, dc).catch(() => false);
    this.trace(peer, adopted ? 'adopted' : 'not-adopted', { role: run.role });
    if (!adopted) shut(pc);
  }

  // Ends `peer`'s punch, if `run` is still it: what it opened closes, the slot frees, and unless
  // `quiet` the peer is told to stop. `busy`: put off, not a try; it begins again later.
  end(peer, run, why, { quiet = false, busy = false } = {}) {
    if (this.runs.get(peer) !== run) return;
    this.runs.delete(peer);
    clearTimeout(run.timer);
    this.slot.free(run, run.spent);
    shut(run.easy?.pc);
    for (const c of run.fleet?.conns ?? []) shut(c.pc);
    if (!quiet) this.room.send(peer, { p: 'stop' });
    this.trace(peer, 'end', { role: run.role, why });
    if (!busy) return;
    this.tried.delete(peer);
    this.busy(peer);
  }

  // A punch put off as busy begins again after the next of busyMs, if any is left.
  busy(peer) {
    const n = this.putOff.get(peer) ?? 0;
    if (this.closed || n >= Punches.busyMs.length) return;
    this.putOff.set(peer, n + 1);
    this.later(peer, Punches.busyMs[n]);
  }

  // `peer` left the room or its link went: its punch ends.
  gone(peer) {
    clearTimeout(this.waits.get(peer));
    this.waits.delete(peer);
    this.putOff.delete(peer);
    const run = this.runs.get(peer);
    if (run) this.end(peer, run, 'the peer is gone', { quiet: true });
  }

  // We left the room: every punch ends.
  close() {
    this.closed = true;
    for (const t of this.waits.values()) clearTimeout(t);
    this.waits.clear();
    for (const [peer, run] of [...this.runs]) this.end(peer, run, 'left the room', { quiet: true });
  }
}
