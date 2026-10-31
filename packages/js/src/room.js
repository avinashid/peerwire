// joinRoom: the high-level API. A room is a full mesh of Peerwire links between
// everyone who joined the same appId and roomId, found through a signalling
// adapter and carried over WebRTC DataChannels.
//
//   const room = await joinRoom({ appId: 'my-app', roomId: 'lobby' });
//   room.on('message', (data, peer) => ...);
//   room.send('hello');
//
// Every room message starts with one tag byte (see T). Presence travels over
// the encrypted links, never through signalling: a peer counts as joined once
// its presence has arrived, so 'peer-join' always carries it.
//
// Rooms are closed by their name (proof.js): hellos, WebRTC offers and the first
// message on every link carry an HMAC under the room key, which the topic relays
// see doesn't give away. A peer is dialled only on a hello that proves the key,
// an offer is answered only if it proves it, and a link that doesn't prove it
// within PROOF_MS is dropped, never told our presence. Use roomIds with at least
// 128 random bits (randomRoomId()): a guessable name is a public lobby.
//
// When WebRTC can't link two members (symmetric NAT on both sides, and no TURN in
// iceServers), the room falls back on its own, per peer: the dialer dials the
// peer through every member it is linked to directly (the core relays for linked
// peers) and over the signalling channel itself (a pipe, see webrtc.js), at once,
// and the first path to answer wins. Every path carries the same Noise session,
// so relays and signalling only ever see sealed datagrams. room.route(peer) says
// which path a peer is on; a link over the pipe moves to a member that links to
// both later, once the core sees it relay both ways, and back to the pipe if that
// member goes. `fallback: 'peers'` skips the pipe, 'off' both. A peer
// behind our own public IPv4 (a laptop on a phone's hotspot) falls back after
// sameNatFallbackMs, not fallbackMs. The link's own DataChannel opening later takes
// the link back (webrtc.js), and one found by other means (a birthday punch,
// birthday.js) can take a fallback link over: room.adoptChannel(). Meanwhile the room
// tries WebRTC again now and then, and on a network change or a microphone or camera
// grant (retry.js); a retry whose DataChannel opens takes the link over the same way.
//
// Hosts that can't take callbacks well (Godot's JavaScriptBridge) pull instead:
// room.poll() / room.receive() return queued events as plain objects (strings,
// numbers, ArrayBuffers), and room.sendTo(peer, data, lane) takes no options.
//
// room.doc(name) opens a shared document (doc.js) that every member holds and that
// catches up members who join later. Its frames have their own tags, plain or deflated,
// so they never show as messages; an older peer drops a tag it doesn't know.

import { loadPeerwire, Lane, toHex, fromHex, isPeerId } from './peerwire.js';
import { WebRtcTransport, BroadcastSignaling } from './webrtc.js';
import { NostrSignaling } from './nostr.js';
import { LocalTransport } from './local.js';
import { RoomProof } from './proof.js';
import { DirectRetries } from './retry.js';
import { Doc, unopened } from './doc.js';
import { docCopy, storeOf } from './store.js';
import { deflate, inflate, deflates } from './zip.js';
import { RelayCopy } from './relays.js';

export const DEFAULT_ICE_SERVERS = [{ urls: 'stun:stun.l.google.com:19302' }, { urls: 'stun:stun.cloudflare.com:3478' }];
const LANES = { reliable: Lane.Reliable, unreliable: Lane.Unreliable, urgent: Lane.Urgent };
// Nudge carries nothing (peers ignore tags they don't know): see the 'open' handler. Doc
// carries a shared document's frame as JSON (doc.js), on the reliable lane; DocZ the same,
// deflated (raw deflate).
const T = { Bytes: 0, Text: 1, Json: 2, Presence: 3, Bye: 4, Proof: 5, Nudge: 6, Doc: 7, DocZ: 8 };
// Doc frames of DEFLATE_MIN bytes or more go deflated to members whose doc reads them, where
// this platform has 'deflate-raw' streams. One goes plain if it deflates to under 1/DEFLATE_RATIO
// of its size, and one is inflated only to the lane's limit and DEFLATE_RATIO times its own size,
// so what a member sends never makes us read more than that many times it (zip.js stops
// inflating past the limit).
const DEFLATE_MIN = 1024;
const DEFLATE_RATIO = 16;
// Presence is re-announced this often, so a lost hello only delays a join.
const HELLO_EVERY_MS = 15000;
// A link must prove the room key this soon after it connects.
const PROOF_MS = 10000;
// In a fallback, members get this long to relay a peer before signalling, the
// slowest path, joins the dial. With no other member that could, it is dialled at once.
const PEER_FIRST_MS = 1000;
// Frames held from one peer while its link proof is checked (and until our presence
// has gone to it).
const MAX_HELD = 256;
// How long a pagehide handler that said goodbye holds the page. The browser may end
// the renderer as soon as the handler returns, before its WebRTC thread has put the
// Bye on the wire; holding the main thread a few ms lets that thread send it.
const PAGEHIDE_GRACE_MS = 20;
const NOT_CONNECTED = -1;
const TOO_LARGE = -2;
const WOULD_BLOCK = -3;
// The core's limit per lane on a message, here a frame (tag byte and data): reliable
// Config::max_message, unreliable MAX_UNRELIABLE, urgent MAX_URGENT. write() checks it
// itself, since a frame that waits in the outbox meets the core only in flush().
const MAX_FRAME = { [Lane.Reliable]: 1 << 20, [Lane.Unreliable]: 1100, [Lane.Urgent]: 64 << 10 };
const tooLarge = (f) => new RangeError(`message of ${f.length - 1} bytes is too large for this lane`);

const enc = new TextEncoder();
const dec = new TextDecoder();
const modules = new Map(); // wasm source -> Promise<Peerwire>, shared by rooms and inboxes

// The Peerwire module from `wasm` (a URL, path or bytes; the package's own by default), loaded once.
export async function peerwireModule(wasm) {
  const src = wasm ?? new URL('../pkg/peerwire_ffi.wasm', import.meta.url);
  const key = String(src);
  if (!modules.has(key)) modules.set(key, loadPeerwire(src).catch((e) => (modules.delete(key), Promise.reject(e))));
  return modules.get(key);
}

function frame(tag, body) {
  const b = new Uint8Array(body.length + 1);
  b[0] = tag;
  b.set(body, 1);
  return b;
}

function encode(data) {
  if (typeof data === 'string') return frame(T.Text, enc.encode(data));
  if (data instanceof Uint8Array) return frame(T.Bytes, data);
  if (data instanceof ArrayBuffer) return frame(T.Bytes, new Uint8Array(data));
  if (ArrayBuffer.isView(data)) return frame(T.Bytes, new Uint8Array(data.buffer, data.byteOffset, data.byteLength));
  return frame(T.Json, enc.encode(JSON.stringify(data)));
}

// A lane by name or number. Only the three named ones: the core sends any other
// number as unreliable, but MAX_FRAME wouldn't know its limit.
function laneOf(lane) {
  const n = typeof lane === 'number' ? lane : LANES[lane];
  if (n !== Lane.Reliable && n !== Lane.Unreliable && n !== Lane.Urgent) throw new TypeError(`unknown lane ${String(lane)}: use 'reliable', 'unreliable' or 'urgent' (0, 1 or 2)`);
  return n;
}

// Our presence as a frame, checked before anything stores it: one over the reliable
// lane's limit would fail every later welcome, and so every later join. JSON.stringify
// gives undefined, not an error, for undefined, a function or a Symbol: that frame would
// go out empty, no peer could parse it, and a room joining later would never list us.
function presenceFrame(presence) {
  const json = JSON.stringify(presence);
  if (json === undefined) throw new TypeError('presence must be a JSON value');
  const f = frame(T.Presence, enc.encode(json));
  if (f.length > MAX_FRAME[Lane.Reliable]) throw tooLarge(f);
  return f;
}

// A queued event: only strings, numbers, plain JSON values and ArrayBuffers.
function plainEvent(type, args) {
  if (type === 'message') {
    const [data, peer, { kind, lane }] = args;
    return { type, peer, data: data instanceof Uint8Array ? data.slice().buffer : data, kind, lane };
  }
  const [peer, x] = args;
  if (type === 'peer-leave') return { type, peer, reason: x };
  return { type, peer, presence: x };
}

export async function joinRoom({
  appId,
  roomId,
  signaling = 'local',
  presence = {},
  queue = false,
  secret,
  wasm,
  relays,
  iceServers,
  privacy = 'direct',
  fallback = 'auto',
  fallbackMs,
  sameNatFallbackMs,
  retryOnGrant = true,
  RTCPeerConnection = globalThis.RTCPeerConnection,
} = {}) {
  if (!appId || !roomId) throw new TypeError('joinRoom needs an appId and a roomId');
  // A room dials every member, so relay-only (one WebRTC link, to the relay) can't apply.
  if (privacy !== 'direct' && privacy !== 'public-only') throw new TypeError(`joinRoom privacy must be 'direct' or 'public-only', not ${privacy}`);
  if (!['auto', 'peers', 'off'].includes(fallback)) throw new TypeError(`joinRoom fallback must be 'auto', 'peers' or 'off', not ${fallback}`);
  presenceFrame(presence);
  const pw = await peerwireModule(wasm);
  const endpoint = pw.endpoint({ secret });
  const name = `${appId}/${roomId}`;
  const proof = await RoomProof.create(name);
  let sig;
  if (signaling === 'local') sig = new BroadcastSignaling(endpoint.id, `peerwire/${name}`);
  else if (signaling === 'nostr') sig = new NostrSignaling(pw, endpoint, { room: name, relays });
  else if (typeof signaling === 'function') sig = signaling({ endpoint, peerwire: pw, room: name });
  else throw new TypeError(`unknown signaling ${signaling}: use 'local', 'nostr' or a factory`);
  let transport;
  if (RTCPeerConnection) {
    // Remote peers need STUN to find a path; tabs of one browser don't.
    const ice = iceServers ?? (signaling === 'local' ? [] : DEFAULT_ICE_SERVERS);
    // Relays charge per event: one offer and one answer, each with every candidate.
    const offerProof = {
      sign: (to) => proof.pair('pw-rtc-offer', endpoint.id, to),
      check: (from, p) => proof.checkPair('pw-rtc-offer', from, endpoint.id, p),
    };
    const pipes = fallback === 'auto';
    transport = (onPeer) => new WebRtcTransport(endpoint, sig, { iceServers: ice, trickle: signaling === 'local', onPeer, privacy, offerProof, pipes, fallbackMs, sameNatFallbackMs, RTCPeerConnection });
  } else if (signaling === 'local') {
    transport = () => new LocalTransport(endpoint, sig);
  } else {
    endpoint.close();
    sig.close();
    throw new Error('no WebRTC in this runtime: pass RTCPeerConnection (e.g. from node-datachannel) or use signaling: \'local\'');
  }
  // A grant shows local addresses only to new connections, and only 'direct' offers them.
  const room = new Room(endpoint, sig, transport, presence, proof, { fallback, grants: privacy === 'direct' && retryOnGrant, roomId, pw });
  if (queue) room.queue = [];
  await sig.ready;
  await room.hello();
  return room;
}

export class Room {
  static proofMs = PROOF_MS; // tests shorten it
  static peerFirstMs = PEER_FIRST_MS;
  constructor(endpoint, signaling, makeTransport, presence, proof, { fallback = 'off', grants = false, roomId = null, pw = null } = {}) {
    this.endpoint = endpoint;
    this.signaling = signaling;
    this.roomId = roomId; // for room.doc(name, { relays })
    this.pw = pw;
    this.self = endpoint.id;
    this.me = presence;
    this.linked = new Set(); // peers with a Peerwire session
    this.members = new Map(); // joined peers -> presence
    this.handlers = new Map(); // event -> Set of fns
    this.outbox = new Map(); // peer -> [frame, lane] waiting for send window
    this.left = false;
    this.queue = null; // events for poll()/receive(), once asked for
    this.greeted = new Set(); // peers whose hello we answered
    this.proof = proof;
    this.proven = new Set(); // linked peers that proved the room key
    this.proving = new Map(); // peer -> frames held while its proof is checked
    this.proofSent = new Map(); // linked peer -> resolves once our link proof is written
    this.deadlines = new Map(); // peer -> timer that drops it unless it proves the key
    this.fallback = fallback;
    this.routes = new Map(); // linked peer -> { kind: 'direct' | 'peer' | 'signaling', via?, adopted? }
    this.paths = new Map(); // linked peer -> the endpoint's path to it
    this.fallingBack = new Set(); // peers we dial by fallback since WebRTC failed
    this.docs = new Map(); // name -> open Doc
    this.docOut = new Map(); // peer -> the doc frames to it, in order, while one waits to be deflated
    this.docIn = new Map(); // peer -> the doc frames from it, in order, while one waits to be inflated
    this.deflated = new WeakMap(); // doc frame -> its deflated bytes, made once for all the members it goes to
    this.transport = makeTransport((peer, state, handle) => {
      if (state === 'closed') this.rtcClosed(peer, handle);
      else if (state === 'failed') this.rtcFailed(peer);
      else if (state === 'carrier') this.reroute(peer);
      else if (state === 'open' || state === 'retry-failed') this.retries.done(peer);
      // A link over signalling moves onto a channel that opened late (a retry's, say) with
      // its first datagram there: one now (its ACK moves the far end's too), not at the next
      // keepalive, up to 10 s on.
      if (state === 'open' && this.routes.get(peer)?.kind === 'signaling' && this.proven.has(peer)) this.write(peer, Uint8Array.of(T.Nudge), Lane.Reliable);
      else if (state === 'retry' && this.self < peer) this.retries.ask(peer); // only the dialling side is asked
    });
    // Peers on a fallback: WebRTC is tried again now and then (see retryDirect).
    this.retries = new DirectRetries((peer) => this.retryNow(peer), { holdMs: (this.transport.fallbackMs ?? 8000) + 10000 });
    this.unwatch = []; // removes the permission listeners
    if (grants) this.watchGrants();
    endpoint.onEvent = (e) => this.onEvent(e);
    // ACKs free send window: flush the outbox as they arrive (the timer is a fallback).
    endpoint.onDrain = () => this.outbox.size && this.flush('drain');
    signaling.onHello = (peer, meta) => this.onHello(peer, meta);
    // A relay back after a network change: announce us now, not at the next interval.
    if ('onReconnect' in signaling) signaling.onReconnect = () => this.hello();
    this.helloTimer = setInterval(() => this.hello(), HELLO_EVERY_MS);
    this.onPageHide = () => {
      const linked = this.linked.size;
      this.leave();
      // A wait that blocks: no timer or promise would run before the page goes.
      for (const end = performance.now() + PAGEHIDE_GRACE_MS; linked && performance.now() < end;);
    };
    globalThis.addEventListener?.('pagehide', this.onPageHide);
    // A page hidden may not come back (a phone ends background tabs): docs kept on this
    // device save now.
    this.onHidden = () => {
      if (globalThis.document.visibilityState !== 'hidden') return;
      for (const d of this.docs.values()) {
        d.save();
        d.relays?.hidden();
      }
    };
    globalThis.document?.addEventListener?.('visibilitychange', this.onHidden);
    // A network switch: links whose ICE dropped restart it now (only those, so a new
    // speed estimate, which fires 'change' too, costs nothing), and peers on a fallback
    // are retried (only on a new network: 'online', or a new connection type).
    this.netType = globalThis.navigator?.connection?.type;
    this.onNetwork = (e) => {
      this.transport.networkChanged?.();
      const type = globalThis.navigator?.connection?.type;
      if (e?.type === 'online' || type !== this.netType) this.retries.soon();
      this.netType = type;
    };
    globalThis.addEventListener?.('online', this.onNetwork);
    globalThis.navigator?.connection?.addEventListener?.('change', this.onNetwork);
  }

  get peers() {
    return [...this.members.keys()];
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
    if (this.queue) this.queue.push(plainEvent(event, args));
    for (const fn of this.handlers.get(event) ?? []) {
      try {
        fn(...args);
      } catch (e) {
        // A throwing handler must not stall the endpoint: report it on its own.
        setTimeout(() => {
          throw e;
        });
      }
    }
  }

  // Sends to every joined peer, or to `to` (one id or several). Returns how
  // many peers it was queued for.
  send(data, { to, lane = 'reliable' } = {}) {
    if (this.left) throw new Error('left the room');
    const l = laneOf(lane);
    const f = encode(data);
    const targets = to === undefined ? this.peers : [to].flat().filter((p) => this.members.has(p));
    // A message too large for the lane throws at the first target, before any is written.
    for (const p of targets) this.write(p, f, l);
    return targets.length;
  }

  // send() without an options object: `peer` '' or null means everyone.
  sendTo(peer, data, lane = 'reliable') {
    return this.send(data, peer ? { to: peer, lane } : { lane });
  }

  // Every event since the last call, oldest first. The first call starts the
  // queue (or pass `queue: true` to joinRoom to catch events from the start).
  poll() {
    if (!this.queue) this.queue = [];
    const q = this.queue;
    this.queue = [];
    return q;
  }

  // The next event, or null.
  receive() {
    if (!this.queue) this.queue = [];
    return this.queue.shift() ?? null;
  }

  getPresence(peer = this.self) {
    return peer === this.self ? this.me : this.members.get(peer);
  }

  setPresence(presence) {
    const f = presenceFrame(presence);
    this.me = presence;
    for (const p of this.proven) this.write(p, f, Lane.Reliable);
  }

  // How `peer` is reached: { kind: 'direct' } (its own WebRTC link), { kind: 'direct',
  // adopted: true } (a channel handed to adoptChannel), { kind: 'peer', via } (relayed
  // by member `via`), { kind: 'signaling' } (over the signalling relays), or null while
  // not linked; with `rttMs`, the round trip over that path (null until measured).
  route(peer) {
    const r = this.routes.get(peer);
    return r ? { ...r, rttMs: this.endpoint.pathRttMs(peer) } : null;
  }

  // Tries WebRTC again, now, with `peer` (or every peer) that the room reaches only by
  // fallback: a new connection, so one made after a microphone or camera grant offers the
  // local addresses (privacy 'direct'), and one after a network change its addresses.
  // If its DataChannel opens, the link moves onto it (route() says 'direct'), the same
  // session; if not, nothing changes. Retries take turns: one at a time per room, at
  // least 2 s apart. Returns how many peers are to be retried.
  retryDirect(peer) {
    return this.left ? 0 : this.retries.soon(peer);
  }

  // Begins a retry with `peer`: the dialling side dials it afresh, the other asks it to.
  retryNow(peer) {
    if (this.left || !this.linked.has(peer)) return false;
    if (this.self < peer) return !!this.transport.retry?.(peer);
    this.transport.askRetry?.(peer);
    return false;
  }

  // Browsers show their local addresses only to connections made after a microphone or
  // camera grant, so on one, peers on a fallback are retried.
  watchGrants() {
    for (const name of ['microphone', 'camera']) {
      (async () => {
        const st = await globalThis.navigator?.permissions?.query?.({ name });
        if (!st?.addEventListener || this.left) return;
        const on = () => st.state === 'granted' && this.retries.soon();
        st.addEventListener('change', on);
        this.unwatch.push(() => st.removeEventListener('change', on));
      })().catch(() => {}); // a permission this browser can't query
    }
  }

  // Moves `peer`'s link onto `dc`, an open DataChannel to it made outside the room
  // (`pc` its connection; a birthday punch's, say), when the room reaches `peer` only by
  // fallback (relayed by a member, or over signalling) and the peer adopts its end too.
  // Resolves true once the channel carries the link: a link over signalling moves at
  // once; a relayed one once the endpoint's punch over the channel is answered. route()
  // then says { kind: 'direct', adopted: true }. Resolves false, leaving the channel to
  // the caller, if the link is direct already or the peer didn't adopt in time. When
  // the channel closes, the link goes back to its fallback route.
  async adoptChannel(peer, pc, dc) {
    const r = this.routes.get(peer);
    if (this.left || !r || r.kind === 'direct' || !this.transport.adopt) return false;
    const pipe = r.kind === 'signaling';
    const h = await this.transport.adopt(peer, pc, dc, { pipe });
    if (h < 0) return false;
    if (!pipe) this.endpoint.addCandidate(h, peer);
    return true;
  }

  // The shared document `name`, opened on first call: the same name on every member is
  // the same document. See doc.js. `persist` keeps a copy on this device, loaded when it
  // opens (store.js): true for IndexedDB, or a store. `relays: true` keeps one on the
  // signalling relays (relays.js), only for a roomId of randomRoomId()'s shape: anyone with
  // the roomId can read that copy, offline and for good. Options count when the doc opens.
  doc(name, { persist, relays = false } = {}) {
    if (this.left) throw new Error('left the room');
    let d = this.docs.get(name);
    if (d) return d;
    if (typeof relays !== 'boolean') throw new TypeError('relays is true or false');
    if (relays && (typeof this.signaling.query !== 'function' || typeof this.signaling.store !== 'function' || !this.pw)) {
      throw new TypeError("relays: true keeps the doc on the room's Nostr relays: it needs signaling: 'nostr'");
    }
    if (relays && !deflates) throw new TypeError("relays: true needs CompressionStream with 'deflate-raw', which this platform lacks");
    if (relays && !/^[A-Za-z0-9_-]{22,}$/.test(this.roomId ?? '')) {
      throw new RangeError('relays: true needs a roomId nobody can guess, from randomRoomId(): anyone with the roomId can read the copy on the relays');
    }
    const store = persist ? docCopy(storeOf(persist), this.proof, name) : null;
    const copy = relays ? new RelayCopy(this.signaling, this.pw, this.proof, name) : null;
    d = new Doc(name, {
      self: this.self,
      peers: () => this.peers,
      send: (peer, m, z) => this.writeDoc(peer, m, z),
      after: (ms, fn) => {
        const t = setTimeout(fn, ms);
        return () => clearTimeout(t);
      },
      now: () => performance.now(),
      closed: (doc) => this.docs.get(name) === doc && this.docs.delete(name),
      route: (peer) => this.routes.get(peer)?.kind ?? null,
      deflate: deflates,
      store,
      relays: copy,
    });
    this.docs.set(name, d);
    d.open();
    return d;
  }

  // Doc frames go to a peer in order. One of DEFLATE_MIN bytes or more, to a member whose doc
  // reads deflated frames (`z`), goes deflated where we can, which takes a moment: the frames
  // after it wait for it.
  writeDoc(peer, m, z) {
    const body = enc.encode(JSON.stringify(m));
    const zip = z && deflates && body.length >= DEFLATE_MIN;
    const before = this.docOut.get(peer);
    if (!zip && !before) return this.sendDoc(peer, T.Doc, body);
    const next = (before ?? Promise.resolve()).then(async () => {
      if (!zip) return this.sendDoc(peer, T.Doc, body);
      let made = this.deflated.get(m);
      if (!made) this.deflated.set(m, (made = deflate(body)));
      const c = await made;
      if (c && c.length * DEFLATE_RATIO >= body.length) this.sendDoc(peer, T.DocZ, c);
      else this.sendDoc(peer, T.Doc, body);
    }).catch(() => {});
    this.docOut.set(peer, next);
    next.then(() => this.docOut.get(peer) === next && this.docOut.delete(peer));
  }

  sendDoc(peer, tag, body) {
    if (this.left) return;
    try {
      this.write(peer, frame(tag, body), Lane.Reliable);
    } catch {
      // A doc frame stays under half the lane's limit, and a peer gone meanwhile is no error here.
    }
  }

  // A doc frame from a member, in the order sent: one deflated waits to be inflated, and the
  // frames after it wait for it. With no doc open, one deflated is dropped unread: a member
  // deflates only for a doc whose ask said it reads them, so it is for a doc we closed.
  onDoc(peer, tag, body) {
    if (tag === T.DocZ && !this.docs.size) return;
    const before = this.docIn.get(peer);
    if (tag === T.Doc && !before) return this.docFrame(peer, body);
    const data = body.slice(); // the endpoint's buffer is its own once this returns
    const next = (before ?? Promise.resolve()).then(async () => {
      const text = tag === T.Doc ? data : await inflate(data, Math.min(MAX_FRAME[Lane.Reliable], DEFLATE_RATIO * data.length));
      if (text && !this.left && this.members.has(peer)) this.docFrame(peer, text);
    }).catch(() => {});
    this.docIn.set(peer, next);
    next.then(() => this.docIn.get(peer) === next && this.docIn.delete(peer));
  }

  // A doc frame's JSON text, for the open doc it names. An ask for a doc we don't have open is
  // answered with nothing (see unopened), and anything else for one is dropped. It never throws
  // into the room.
  docFrame(peer, text) {
    try {
      const m = JSON.parse(dec.decode(text));
      const d = typeof m?.d === 'string' ? this.docs.get(m.d) : undefined;
      if (d) return d.receive(peer, m);
      const none = unopened(m);
      if (none) this.writeDoc(peer, none, false);
    } catch {
      // Malformed: dropped.
    }
  }

  rtt(peer) {
    return this.endpoint.rttMs(peer);
  }

  stats() {
    return this.endpoint.stats();
  }

  // Says goodbye to every peer, so they see 'peer-leave' at once, then closes. All of it
  // happens before this returns, with no timer or await, because on pagehide the page
  // may be gone before either runs. The Bye goes on the unreliable lane, which no full
  // send window or unacknowledged reliable data holds back, and the core's sealed close
  // follows it; each is handed to the transport (dc.send) by the pump in the same call.
  // The promise resolves once docs kept on this device have saved.
  async leave() {
    if (this.left) return;
    this.left = true;
    clearInterval(this.helloTimer);
    clearTimeout(this.flushTimer);
    globalThis.removeEventListener?.('pagehide', this.onPageHide);
    globalThis.removeEventListener?.('online', this.onNetwork);
    globalThis.navigator?.connection?.removeEventListener?.('change', this.onNetwork);
    globalThis.document?.removeEventListener?.('visibilitychange', this.onHidden);
    this.retries.close();
    for (const off of this.unwatch) off();
    const saved = [...this.docs.values()].map((d) => d.close());
    this.docOut.clear();
    this.docIn.clear();
    const bye = Uint8Array.of(T.Bye);
    for (const p of this.linked) this.endpoint.send(p, bye, Lane.Unreliable);
    this.endpoint.pump();
    for (const p of this.linked) this.endpoint.disconnect(p);
    this.outbox.clear();
    this.transport.close();
    this.endpoint.close();
    this.members.clear();
    this.linked.clear();
    this.proven.clear();
    this.proving.clear();
    this.proofSent.clear();
    for (const t of this.deadlines.values()) clearTimeout(t);
    this.deadlines.clear();
    this.routes.clear();
    this.paths.clear();
    this.fallingBack.clear();
    await Promise.all(saved);
  }

  // Announces us on the room topic, with a proof of the room key.
  async hello() {
    if (!this.left) this.signaling.hello(await this.proof.hello(this.self));
  }

  async onHello(peer, meta) {
    // A custom signalling adapter may pass anything along: only PeerIds are dialled.
    // `meta.nb` marks a Rust UDP peer (traversal.rs `hello`), which WebRTC can't dial;
    // `meta.pw`, which this once checked by mistake, is taken as the same mark.
    if (this.left || !isPeerId(peer) || peer === this.self || meta?.nb || meta?.pw) return;
    // Knowing the topic isn't enough: only hellos that prove the room key count.
    if (!(await this.proof.checkHello(peer, meta))) return;
    // A member may reach us over signalling once WebRTC fails, whichever side dials.
    this.transport.allowPipe?.(peer);
    if (this.left || this.transport.has(peer) || this.fallingBack.has(peer) || this.endpoint.isConnected(peer)) return;
    // Answer a newcomer's hello once, so it learns about us too.
    if (!this.greeted.has(peer)) {
      this.greeted.add(peer);
      this.hello();
    }
    // One side dials: the smaller id.
    if (this.self < peer) this.transport.dial(peer);
  }

  onEvent(e) {
    if (this.left) return;
    if (e.type === 'connected') {
      const peer = e.peer;
      this.linked.add(peer);
      this.paths.set(peer, e.path);
      this.reroute(peer);
      this.fallingBack.delete(peer);
      // Prove the room key first; presence follows once the peer has proved it too.
      this.proofSent.set(peer, this.proof.pair('pw-link', this.self, peer).then((p) => {
        if (this.linked.has(peer)) this.write(peer, frame(T.Proof, fromHex(p)), Lane.Reliable);
      }));
      clearTimeout(this.deadlines.get(peer));
      this.deadlines.set(peer, setTimeout(() => {
        if (!this.proven.has(peer)) this.lost(peer, 'unproven');
      }, Room.proofMs));
    } else if (e.type === 'message') {
      this.onMessage(e.peer, e.data, e.lane);
    } else if (e.type === 'pathChanged') {
      if (this.linked.has(e.peer)) {
        this.paths.set(e.peer, e.path);
        this.reroute(e.peer);
      }
    } else if (e.type === 'disconnected') {
      this.lost(e.peer, e.reason);
    } else if (e.type === 'connectFailed') {
      this.lost(e.peer, 'failed');
    }
  }

  onMessage(peer, data, lane) {
    const tag = data[0];
    const body = data.subarray(1);
    if (!this.proven.has(peer)) return this.unproven(peer, tag, body, data, lane);
    if (tag === T.Presence) {
      let p;
      try {
        p = JSON.parse(dec.decode(body));
      } catch {
        return;
      }
      const joined = this.members.has(peer);
      this.members.set(peer, p);
      this.emit(joined ? 'presence' : 'peer-join', peer, p);
      if (!joined) for (const d of this.docs.values()) d.joined(peer);
      return;
    }
    if (tag === T.Bye) return this.lost(peer, 'left');
    // Before its presence, a peer hasn't joined: only unreliable data can race it.
    if (!this.members.has(peer)) return;
    if (tag === T.Doc || tag === T.DocZ) return this.onDoc(peer, tag, body);
    let value;
    if (tag === T.Bytes) value = body;
    else if (tag === T.Text) value = dec.decode(body);
    else if (tag === T.Json) {
      try {
        value = JSON.parse(dec.decode(body));
      } catch {
        return;
      }
    } else return;
    const kind = tag === T.Bytes ? 'bytes' : tag === T.Text ? 'text' : 'json';
    this.emit('message', value, peer, { lane: lane === Lane.Unreliable ? 'unreliable' : 'reliable', kind });
  }

  // A frame from a peer that hasn't proved the room key: only its proof (or a
  // goodbye) is acted on, and what follows the proof waits for the check and our presence.
  unproven(peer, tag, body, data, lane) {
    const held = this.proving.get(peer);
    if (held) {
      if (held.length >= MAX_HELD) return this.lost(peer, 'unproven');
      return void held.push([data, lane]);
    }
    if (tag === T.Bye) return this.lost(peer, 'left');
    if (tag !== T.Proof || !this.linked.has(peer)) return;
    const rest = [];
    this.proving.set(peer, rest);
    this.proof.checkPair('pw-link', peer, this.self, toHex(body)).then((ok) => {
      // Lost (and maybe linked again) meanwhile: this check is stale.
      if (this.left || this.proving.get(peer) !== rest) return;
      if (!ok) return this.lost(peer, 'unproven');
      // Our presence goes after our own proof, or the peer drops it as unproven and never
      // lists us: signing that proof can finish after this check (WebCrypto keeps no order).
      // The held frames go after our presence, and frames keep being held until then: a
      // peer-join handler that sends to the peer must reach it after our presence, or the
      // peer drops that as from a peer that hasn't joined. Should our proof never be sent,
      // the peer stays unproven and the deadline drops it.
      this.proofSent.get(peer).then(() => {
        if (this.left || this.proving.get(peer) !== rest) return;
        this.proving.delete(peer);
        clearTimeout(this.deadlines.get(peer));
        this.deadlines.delete(peer);
        this.proven.add(peer);
        this.relayPending(peer);
        this.offerRelays();
        this.write(peer, frame(T.Presence, enc.encode(JSON.stringify(this.me))), Lane.Reliable);
        for (const [d, l] of rest) this.onMessage(peer, d, l);
      });
    });
  }

  routeOf(path) {
    if (path.via) return { kind: 'peer', via: path.via };
    if (this.transport.carries?.(path.direct)) return { kind: 'direct', adopted: true };
    return { kind: this.transport.isPipe?.(path.direct) ? 'signaling' : 'direct' };
  }

  // The route to `peer` on `path`. The core learns whether a direct path is the slow
  // signalling pipe, which a member relay beats (offerRelays).
  setRoute(peer, path) {
    const r = this.routeOf(path);
    this.routes.set(peer, r);
    this.retries.track(peer, r.kind !== 'direct', this.self < peer); // the dialling side keeps the backoff
    if (path.direct !== undefined) this.endpoint.setSlow(peer, r.kind === 'signaling' ? path.direct : null);
  }

  // The route to a linked `peer` again: its path changed, or an adopted channel took
  // its link or gave it back.
  reroute(peer) {
    const path = this.paths.get(peer);
    if (!path || !this.linked.has(peer)) return;
    this.setRoute(peer, path);
    this.offerRelays();
    for (const d of this.docs.values()) d.rerouted();
  }

  // Every proven member linked to us directly is offered as a relay for every proven
  // peer we reach over signalling or through another member: the core probes through it
  // and moves the link there once the peer answers (from signalling), or once it is
  // clearly faster than the member relaying now; the peer answers only if it would use
  // that member too.
  offerRelays() {
    const members = [...this.routes].filter(([p, r]) => r.kind === 'direct' && this.proven.has(p)).map(([p]) => p);
    for (const [p, r] of this.routes) {
      if (r.kind !== 'direct' && this.proven.has(p)) for (const m of members) this.endpoint.addRelay(m, p);
    }
  }

  // A DataChannel closed: the peer is lost if that was its path, or if no fallback
  // is under way; otherwise only the WebRTC link goes. `handle`, the closed link's: a
  // link that isn't the peer's path now (an old one, while the path still names a
  // relayed channel that just ended, say) never drops the peer (NB48b-5).
  rtcClosed(peer, handle) {
    const r = this.routes.get(peer);
    const stale = handle !== undefined && this.paths.get(peer)?.direct !== handle;
    if (r ? r.kind !== 'direct' || r.adopted || stale : this.fallingBack.has(peer)) return this.transport.dropRtc(peer);
    this.lost(peer, 'closed');
  }

  // WebRTC couldn't link `peer`. The side that dials (the smaller id) dials it again
  // through every member linked to us directly (and any that links later) and, unless
  // one answers within PEER_FIRST_MS, over signalling too; the core keeps the first
  // path to answer. With no other member that could (two in the room, say: a laptop on
  // a phone's hotspot), signalling is dialled at once. The other side just answers.
  rtcFailed(peer) {
    if (this.left || this.fallback === 'off' || this.self > peer || this.endpoint.isConnected(peer)) return;
    const vias = [...this.routes].filter(([p, r]) => p !== peer && r.kind === 'direct' && this.proven.has(p)).map(([p]) => p);
    const pipe = this.transport.pipe?.(peer);
    // Nothing else to try: drop the dead link, so a later hello dials WebRTC again.
    if (!vias.length && pipe == null) return this.lost(peer, 'failed');
    this.fallingBack.add(peer);
    for (const v of vias) this.endpoint.connectVia(v, peer);
    if (pipe == null) return;
    // Another peer we heard but haven't linked yet may still link and prove the key, and
    // relayPending dial through it: only with no such peer either is signalling at once.
    const others = [...this.greeted, ...this.linked].some((p) => p !== peer && (!this.linked.has(p) || this.routes.get(p)?.kind === 'direct'));
    if (!others) return this.endpoint.connect(pipe, peer);
    setTimeout(() => !this.left && this.fallingBack.has(peer) && this.endpoint.connect(pipe, peer), Room.peerFirstMs);
  }

  // A member just linked directly and proved the key: it can relay the peers we are
  // still dialling by fallback (the core adds a path to a dial under way).
  relayPending(member) {
    if (this.routes.get(member)?.kind !== 'direct') return;
    for (const p of this.fallingBack) if (p !== member && !this.endpoint.isConnected(p)) this.endpoint.connectVia(member, p);
  }

  lost(peer, reason) {
    this.transport.drop(peer);
    this.linked.delete(peer);
    this.proven.delete(peer);
    this.proving.delete(peer);
    this.proofSent.delete(peer);
    clearTimeout(this.deadlines.get(peer));
    this.deadlines.delete(peer);
    this.outbox.delete(peer);
    this.greeted.delete(peer);
    this.routes.delete(peer);
    this.paths.delete(peer);
    this.fallingBack.delete(peer);
    this.retries.forget(peer);
    this.docOut.delete(peer);
    this.docIn.delete(peer);
    // Out of the room before the session ends: disconnect() pumps, and a 'disconnected'
    // already queued (its sealed close came in the Bye's pipe batch) is not a second leave.
    const member = this.members.delete(peer);
    // Its sealed close may never come now that its transport is gone: end the session
    // here, so links it relayed go back to signalling at once.
    if (reason === 'left') this.endpoint.disconnect(peer);
    if (member) {
      for (const d of this.docs.values()) d.left(peer);
      this.emit('peer-leave', peer, reason);
    }
  }

  // Throws RangeError for a frame over its lane's limit before queueing or sending it,
  // whether or not the outbox holds frames for `peer`: flush() could only drop it. A
  // frame sent to several peers throws at the first, so none of them is sent it.
  write(peer, f, lane) {
    if (f.length > MAX_FRAME[lane]) throw tooLarge(f);
    const q = this.outbox.get(peer);
    if (q) return void q.push([f, lane]);
    const r = this.endpoint.sendRetry(peer, f, lane);
    if (r === WOULD_BLOCK) {
      this.outbox.set(peer, [[f, lane]]);
      this.flushSoon();
    } else if (r === TOO_LARGE) {
      throw tooLarge(f);
    } else if (r < 0 && r !== NOT_CONNECTED) {
      throw new Error(`send failed (${r})`);
    }
  }

  // Sends what the send window takes, oldest first. `from` says who asked: 'drain'
  // (the endpoint handled incoming datagrams, ACKs among them) or 'timer'.
  flush(from) {
    if (this.left) return;
    for (const [peer, q] of this.outbox) {
      while (q.length && this.endpoint.sendRetry(peer, q[0][0], q[0][1]) !== WOULD_BLOCK) q.shift();
      if (!q.length) this.outbox.delete(peer);
    }
    if (!this.outbox.size) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
  }

  // The fallback while the outbox holds frames, should no datagram arrive to drain it.
  flushSoon() {
    if (this.flushTimer) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = null;
      this.flush('timer');
      if (this.outbox.size && !this.left) this.flushSoon();
    }, 2);
  }
}
