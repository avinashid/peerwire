// Links with a native peer (a Rust node, an ESP32 board on Wi-Fi) over the room's Nostr relays alone (NB-126): from a
// phone on mobile data, say, to a board at home, with nothing of ours in between. A browser can't dial a native peer
// over WebRTC, and a native peer has no WebRTC: the link's datagrams go as signalling messages instead, in the pipe a
// joinRoom room falls back to (webrtc.js), sealed to the peer and each batch through one relay. It is slow (a relay
// trip per batch, a few batches a second): fine for commands and chat, not for streams.
//
// The room is a native one, the Rust node's, not a joinRoom room. As BluetoothTransport does, this says hello as a
// member of a native room (Rust's traversal.rs `hello`: meta.nb, a proof under the room key), and dials the peer
// itself. It joins the room's topic on the relays, lists the native peers whose hellos prove the room key, and dials
// one over a pipe alone. A native peer takes pipe datagrams only from a member whose hello it holds from the last
// minute, so ours goes on start, every helloEveryMs, on a relay that opens again, and in answer to a peer newly heard
// (at most one each greetMs). Messages are the endpoint's, raw bytes, as a node sends them.
//
//   const pw = await loadPeerwire();
//   const endpoint = pw.endpoint();
//   const link = new RelayTransport(endpoint, { appId: 'peerwire-lan-chat', roomId: 'esp32', onPeer });
//   endpoint.onEvent = (e) => e.type === 'message' && show(new TextDecoder().decode(e.data));
//   async function onPeer(peer, state) {
//     if (state === 'seen') endpoint.send(await link.dial(peer), 'led on'); // resolves once linked
//   }
//
// A hello only names a peer: who is at the other end comes from the handshake, which proves it holds that PeerId.

import { isPeerId, MAX_DATAGRAM } from './peerwire.js';
import { RoomProof } from './proof.js';
import { NostrSignaling, DEFAULT_RELAYS } from './nostr.js';
import { WebRtcTransport } from './webrtc.js';
import { checkRelays } from './policy.js';

// A native peer may read relay messages of 4 KB at most (an ESP32 board's), and an event with one datagram of 1232
// bytes is about 2.8 KB: so a batch to one carries 1232 bytes of datagrams at most (64 small ones make about 3.3 KB).
export const NATIVE_BATCH = MAX_DATAGRAM;
// A peer is listed while its newest hello is under this old: a hello is fresh for a minute (proof.js), and peers say
// one every 30 s at most.
const SEEN_MS = 60000;
const MAX_PEERS = 256; // peers listed; past it, the one heard longest ago goes
// The pipes' clock, as webrtc.js's: a wall clock set back doesn't hold a peer listed.
const clock = () => performance.now();

function failure(code, message) {
  return Object.assign(new Error(`relay: ${message}`), { code });
}

export class RelayTransport {
  // How often our hello goes (a native peer holds one for a minute); the least time between two said in answer to
  // peers newly heard; how long a dial may take (the core gives a handshake 10 s). Tests shorten them.
  static helloEveryMs = 20000;
  static greetMs = 2000;
  static dialMs = 15000;

  // `endpoint`: any module's. `relays`: the room's Nostr relays, DEFAULT_RELAYS by default. `onPeer(peer, state)`:
  // 'seen' when a native peer's hello first proves the room key (and again once one went unheard for a minute),
  // 'open' when a dial links.
  constructor(endpoint, { appId, roomId, relays = DEFAULT_RELAYS, onPeer = null } = {}) {
    if (typeof endpoint?.seal !== 'function' || typeof endpoint?.pw?.nostrKeypair !== 'function') {
      throw new TypeError('RelayTransport needs an Endpoint (pw.endpoint())');
    }
    if (typeof appId !== 'string' || typeof roomId !== 'string' || !appId || !roomId) {
      throw new TypeError('RelayTransport needs an appId and a roomId');
    }
    const name = `${appId}/${roomId}`;
    this.ep = endpoint;
    this.onPeer = onPeer;
    this.closed = false;
    this.seen = new Map(); // peer -> when its newest hello came (clock()), oldest first
    this.dials = new Map(); // peer -> its dial under way: { promise, settle, timer }
    this.greetAt = -Infinity; // when our last hello in answer went
    this.greetTimer = null;
    this.proof = RoomProof.create(name);
    this.signaling = new NostrSignaling(endpoint.pw, endpoint, { room: name, relays: checkRelays(relays) });
    // The pipes, with no WebRTC: an offer goes unanswered.
    const other = endpoint.onTransmit;
    this.rtc = new WebRtcTransport(endpoint, this.signaling, {
      pipes: true, pipeBatch: NATIVE_BATCH, rtc: { direct: false, turn: false }, RTCPeerConnection: null,
    });
    // The endpoint's datagrams to our pipes go over them; others to the transport that was there.
    const piped = endpoint.onTransmit;
    endpoint.onTransmit = (addr, bytes) => (this.rtc.isPipe(addr) ? piped(addr, bytes) : other?.(addr, bytes));
    this.signaling.onHello = (peer, meta) => void this.heard(peer, meta).catch(() => {});
    this.signaling.onReconnect = () => this.hello();
    // After the pipes' own handler, which hands a batch's datagrams to the endpoint (and the pump it schedules): a
    // dial may be linked now.
    this.signaling.onMessage((from) => this.dials.has(from) && queueMicrotask(() => this.check(from)));
    this.timer = setInterval(() => this.hello(), RelayTransport.helloEveryMs);
    this.hello();
  }

  // The native peers heard in the last minute, whose hellos prove the room key.
  get peers() {
    const now = clock();
    return [...this.seen].filter(([, at]) => now - at < SEEN_MS).map(([p]) => p);
  }

  // Our hello, as a member of a native room says it (meta in Rust's key order: nb, role, ts, p). One that fails (the
  // relays closed meanwhile) is lost, as on a relay that drops it.
  async hello() {
    try {
      if (this.closed) return;
      const meta = await (await this.proof).hello(this.ep.id, 'peer');
      if (!this.closed) await this.signaling.hello({ nb: 1, ...meta });
    } catch {
      // lost
    }
  }

  // Tells the app; a handler that throws doesn't break the transport.
  tell(peer, state) {
    try {
      this.onPeer?.(peer, state);
    } catch {
      // the app's
    }
  }

  // Our hello in answer to a peer newly heard, so it holds one of ours by the time we dial it: at most one each
  // greetMs, those asked for meanwhile going as one at its end.
  greet() {
    if (this.closed || this.greetTimer) return;
    const wait = Math.max(0, this.greetAt + RelayTransport.greetMs - clock());
    this.greetTimer = setTimeout(() => {
      this.greetTimer = null;
      this.greetAt = clock();
      this.hello();
    }, wait);
  }

  // A room hello from `peer`: a native peer's (meta.nb) that proves the room key is listed, and may send us datagrams
  // over the pipe. A joinRoom member's (no meta.nb) is left to its room: it links over WebRTC, and takes no bare pipe.
  async heard(peer, meta) {
    if (this.closed || !isPeerId(peer) || peer === this.ep.id || !meta?.nb) return;
    if (!(await (await this.proof).checkHello(peer, meta)) || this.closed) return;
    const now = clock();
    const fresh = !(now - (this.seen.get(peer) ?? -Infinity) < SEEN_MS);
    this.seen.delete(peer);
    this.seen.set(peer, now);
    if (this.seen.size > MAX_PEERS) this.seen.delete(this.seen.keys().next().value);
    this.rtc.allowPipe(peer);
    if (!fresh) return;
    this.greet();
    this.tell(peer, 'seen');
  }

  // Links with `peer` over a pipe: resolves to its PeerId once the handshake is done (at once if the endpoint is
  // linked with it already), within dialMs. Rejects with an Error whose `code` is 'timeout' (not linked in time: the
  // peer isn't online, or not in this room), 'failed' (the endpoint refused the dial) or 'closed' (the transport
  // closed, or drop(peer)); a TypeError for a `peer` that isn't a PeerId. A dial to a peer under way is that one.
  dial(peer) {
    if (!isPeerId(peer)) return Promise.reject(new TypeError('dial: not a peer id (64 lowercase hex characters)'));
    if (this.closed) return Promise.reject(failure('closed', 'the transport is closed'));
    const under = this.dials.get(peer);
    if (under) return under.promise;
    if (this.ep.isConnected(peer)) return Promise.resolve(peer);
    // Its answers come over the pipe; a peer we haven't heard may not hold a hello of ours yet.
    this.rtc.allowPipe(peer);
    if (!this.seen.has(peer)) this.greet();
    const h = this.rtc.pipe(peer);
    const d = {};
    d.promise = new Promise((resolve, reject) => {
      d.settle = (err) => {
        if (this.dials.get(peer) !== d) return;
        clearTimeout(d.timer);
        this.dials.delete(peer);
        if (err) return reject(err);
        resolve(peer);
        this.tell(peer, 'open');
      };
    });
    const late = failure('timeout', 'not linked in time: is the peer online, in this room?');
    d.timer = setTimeout(() => d.settle(late), RelayTransport.dialMs);
    this.dials.set(peer, d);
    if (this.ep.connect(h, peer) < 0) d.settle(failure('failed', 'the endpoint refused the dial'));
    else this.check(peer);
    return d.promise;
  }

  // A dial to `peer` under way is done once the endpoint is linked with it.
  check(peer) {
    if (this.ep.isConnected(peer)) this.dials.get(peer)?.settle(null);
  }

  // Whether `peer` is linked and we have a pipe to it.
  has(peer) {
    return this.rtc.pipeByPeer.has(peer) && this.ep.isConnected(peer);
  }

  // Ends the link to `peer` (a dial under way rejects with 'closed'): the endpoint's goodbye goes over the pipe at
  // once, then the pipe goes. Nothing for a peer we have no pipe to.
  drop(peer) {
    if (!this.rtc.pipeByPeer.has(peer)) return;
    this.dials.get(peer)?.settle(failure('closed', 'dropped'));
    this.ep.disconnect(peer);
    this.rtc.flush(peer);
    this.rtc.dropPipe(peer);
  }

  // Leaves the relays: what waits in a pipe goes first (the endpoint's goodbyes, if it said them), and a dial under
  // way rejects with 'closed'.
  close() {
    if (this.closed) return;
    this.closed = true;
    clearInterval(this.timer);
    clearTimeout(this.greetTimer);
    for (const d of [...this.dials.values()]) d.settle(failure('closed', 'the transport is closed'));
    this.seen.clear();
    this.rtc.close();
  }
}
