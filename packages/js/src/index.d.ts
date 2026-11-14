// Types for @peerwire/js.

/** A peer's id: its 32-byte Peerwire PeerId as 64 hex characters. */
export type PeerId = string;

/** Free-form data a peer shares with the room (a nickname, a colour); must survive JSON. */
export type Presence = any;

/** `reliable`: ordered, retransmitted. `unreliable`: latest-wins datagrams, one packet at most.
 * `urgent`: reliable, each fragment sent twice, ordered only among urgent messages (it overtakes queued
 * `reliable` data); arrives as `reliable`. Urgent messages are capped at 64 KiB encoded: `send` throws a
 * RangeError above that. */
export type LaneName = 'reliable' | 'unreliable' | 'urgent';

/** What `room.send` accepts. Strings arrive as strings, bytes as a Uint8Array, anything else as parsed JSON. */
export type RoomData = string | Uint8Array | ArrayBuffer | ArrayBufferView | number | boolean | null | object;

export interface SignalingAdapter {
  readonly id: string;
  /** Resolves once the adapter can publish (relays connected). */
  readonly ready?: Promise<unknown>;
  /** Called with each peer that announces itself in the room. */
  onHello: (peer: PeerId, meta?: unknown) => void;
  hello(meta?: unknown): void | Promise<void>;
  /** `lossy`: the sender resends what `body` carries if it is lost (a signalling-pipe batch), so it may go one way
   * rather than every way the adapter has (the Nostr adapter sends it through one relay, not each). */
  send(to: PeerId, body: unknown, options?: { lossy?: boolean }): void | Promise<void>;
  /** Milliseconds until the adapter can take one more message (0: it can now), for one that paces its sends, as the
   * Nostr adapter does. A signalling pipe waits this long, at most a second at a time, so its batches fill meanwhile.
   * Without it, the pipes send at their own rate, 6 batches a second with a burst of 12; an adapter that also ignores
   * `lossy` and sends to several relays puts all of that on each of them. */
  wait?(): number;
  /** `from` must be the sender's PeerId, authenticated by the adapter (as the Nostr adapter's sealed messages are;
   * BroadcastChannel is same-origin): WebRTC offers, answers, ICE candidates and restarts, retry requests, and the
   * signalling pipe and its stale drop all trust it. */
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}

export interface SignalingContext {
  endpoint: Endpoint;
  peerwire: Peerwire;
  /** `${appId}/${roomId}` */
  room: string;
}

/** A path a room's link may take: WebRTC without TURN (host and STUN candidates, port prediction, birthday punches),
 * WebRTC through a TURN server in `iceServers`, relayed by another member, or over the signalling relays. */
export type RoomPath = 'direct' | 'turn' | 'peers' | 'signaling';

/** A room's connection policy: `joinRoom`'s `policy`, and `room.setPolicy`, which takes only the keys that change. */
export interface RoomPolicy {
  /** The paths a link may take, in the order to try them; a path missing is off. Default (and `fallback: 'auto'`)
   * `['direct', 'turn', 'peers', 'signaling']`; `fallback: 'peers'` leaves out `'signaling'`, and `'off'` both.
   * - The side that dials (the smaller PeerId) tries the first path, and each later one when those before it fail. A
   *   WebRTC attempt fails when its DataChannel isn't open `fallbackMs` after its offer, or its ICE fails; a dial
   *   through members or signalling, when it has no answer in 10 s, or at once with nothing to dial (no member
   *   linked to us directly).
   * - `'direct'` and `'turn'` next to each other are one WebRTC attempt (ICE prefers the direct pairs); `'peers'` and
   *   `'signaling'` next to each other are one fallback, the first dialled 1 s before the other. So `['direct',
   *   'peers', 'signaling', 'turn']` uses TURN only once the fallback fails. With `'turn'` and no `'direct'`, WebRTC
   *   takes relay candidates alone (`iceTransportPolicy: 'relay'`), and only relay candidates are sent. A WebRTC
   *   attempt without `'turn'` asks the peer to leave its TURN servers out too.
   * - A link moves only to a path earlier than its own: WebRTC retries, a DataChannel that opens late and birthday
   *   punches need a WebRTC path before its own, and a member takes over a link on signalling only with `'peers'`
   *   before `'signaling'`. So `['peers', 'direct']` keeps a relayed link relayed.
   * - It governs this end: a peer whose policy differs can still dial us over a path ours has off. Such a link
   *   moves to a path ours allows (a member's link to signalling, a signalling link to a member, if the peer takes
   *   it), or drops: `'peer-leave'` with reason `'policy'`.
   * - The order binds the side that dials. Answering a WebRTC offer, this end uses every WebRTC path its policy
   *   allows, whatever comes first in its order: `['signaling', 'direct']` still sends our addresses to a peer that
   *   dials WebRTC first, and `'turn'` last still gathers on our TURN server.
   * TypeError for an empty order, an unknown path, or one given twice. */
  order?: RoomPath[];
  /** STUN and TURN servers: urls of `stun:`, `stuns:`, `turn:` or `turns:`. Default: none for `'local'`, Google and
   * Cloudflare STUN otherwise. TURN ones are used only with `'turn'` in the order and STUN ones only with
   * `'direct'`, by the room's links, its calls and its birthday punches. TypeError for any other url, naming the
   * entry by its index alone, never a url or a credential. */
  iceServers?: RTCIceServer[];
  /** Nostr relays, for `signaling: 'nostr'`: `ws://` or `wss://` urls. Members find each other, and link over
   * signalling, only through a relay they share. TypeError for an empty list, or a url that doesn't parse, isn't
   * `ws:` or `wss:`, or has a fragment, naming it by its index alone. A url the browser still refuses (a blocked
   * port) is a relay that can't be reached: dialled again now and then, as one that closed. */
  relays?: string[];
  /** As `joinRoom`'s. */
  fallbackMs?: number;
  /** As `joinRoom`'s. */
  sameNatFallbackMs?: number;
  /** As `joinRoom`'s; it needs `'direct'` in the order too. */
  birthday?: boolean;
  /** As `joinRoom`'s; set when the room is joined, so `setPolicy` takes only the room's own. */
  privacy?: 'direct' | 'public-only';
}

export interface JoinRoomOptions {
  /** Names your app; rooms of different apps never meet. */
  appId: string;
  /**
   * The room's name, and its only secret: peers prove they know it, so the topic
   * relays see lets no one in. A guessable name is a public lobby; for a closed
   * room use at least 128 random bits, e.g. `randomRoomId()`.
   */
  roomId: string;
  /**
   * How peers find each other.
   * - `'local'` (default): BroadcastChannel; tabs of one browser, or one Node process.
   * - `'nostr'`: public Nostr relays (`wss://nos.lol`, `wss://relay.primal.net`, `wss://nostr.mom`), across the internet.
   * - a factory returning your own adapter.
   */
  signaling?: 'local' | 'nostr' | ((ctx: SignalingContext) => SignalingAdapter);
  /** Shared with every peer when it joins; change it with `room.setPresence`. Default `{}`. `joinRoom` rejects
   * one over 1 MiB as JSON (RangeError) or one JSON can't encode, such as a function or a BigInt (TypeError). */
  presence?: Presence;
  /** Queue events for `room.poll()`/`room.receive()` from the start; otherwise the first call starts the queue. */
  queue?: boolean;
  /** 32 bytes restoring an identity from `room.endpoint.secret()`; a fresh one by default. */
  secret?: Uint8Array;
  /** Where to load `peerwire_ffi.wasm` from: a URL (in a browser, relative and root-relative ones such as a bundler's
   * `?url` import are fetched from the page's origin), a file path or `file:` URL (Node) or the bytes. Default: next to
   * this package. */
  wasm?: string | URL | BufferSource;
  /** The connection policy: which paths a link may take, in what order, and the servers they use. See
   * {@link RoomPolicy}. The options below that it also holds (`relays`, `iceServers`, `privacy`, `fallbackMs`,
   * `sameNatFallbackMs`, `birthday`, and `fallback` for its `order`) go either here or there: TypeError for one
   * given both ways. Change it later with `room.setPolicy`. */
  policy?: RoomPolicy;
  /** Nostr relays, for `signaling: 'nostr'`. */
  relays?: string[];
  /** Default: none for `'local'`, Google and Cloudflare STUN otherwise. TURN servers are used as the policy's order
   * allows. */
  iceServers?: RTCIceServer[];
  /** Which of your addresses peers see in ICE candidates. `'direct'` (default): all of them.
   * `'public-only'`: no LAN or loopback (`host`) candidates, only what STUN reports (and no local address in
   * a candidate's related address or the SDP's `c=` line either), so tabs of one
   * machine with no STUN server can't link. Relay-only is not available for rooms: use
   * `WebRtcTransport` with `privacy: 'relay-only'`.
   * Once the origin has a stored microphone or camera grant (it lasts across visits and covers every page of the
   * origin, even one that never asks), browsers list every network interface to connections made after the grant
   * (RFC 8828 mode 1). With `'direct'`, members then get each one's real address (LAN, hotspot and IPv6 ones),
   * which is what lets a phone and a laptop on its hotspot link directly. `'public-only'` withholds those, and that
   * direct path with them, but still sends each interface's public address as STUN sees it (the Wi-Fi's and the
   * mobile network's, or the real network's under a VPN). Voice and video apps ask for that permission anyway. */
  privacy?: 'direct' | 'public-only';
  /** What to do for a peer WebRTC can't link (symmetric NAT on both sides, no TURN):
   * - `'auto'` (default): dial it through every member linked to both of you, and over the
   *   signalling relays themselves, at once; the first path to answer wins. Signalling is
   *   slow (a relay round trip per batch, a few batches a second) but works on any network
   *   that reaches the relays.
   * - `'peers'`: through other members only.
   * - `'off'`: WebRTC only.
   * Every path carries the same end-to-end encrypted session. See `room.route(peer)`. Shorthand for the policy's
   * `order`: `'auto'` is every path, `'peers'` all but `'signaling'`, `'off'` `['direct', 'turn']`. */
  fallback?: 'auto' | 'peers' | 'off';
  /** How long a WebRTC link may take to open before the fallback starts. Default 8000 ms. */
  fallbackMs?: number;
  /** The same for a peer behind your own public IPv4 (its ICE candidates name a public address yours do), such
   * as a laptop on a phone's hotspot, where browsers find no direct path unless they show their LAN addresses.
   * Default 3000 ms (at most `fallbackMs`). ICE goes on checking: if the link's DataChannel opens later (two
   * desktops on one Wi-Fi), the link moves onto it and `route(peer)` says `'direct'` again. */
  sameNatFallbackMs?: number;
  /** While a peer is reached by fallback, the room tries WebRTC again with a new connection (see `retryDirect`),
   * and also when the page is granted the microphone or camera: browsers show their local addresses only to
   * connections made after such a grant, so with privacy `'direct'` the retry offers them to that peer. `false`
   * keeps a grant from making new connections (Net Lab's global room rejoins as `'public-only'` instead). Has no
   * effect with `'public-only'`, which never offers them. Default true. */
  retryOnGrant?: boolean;
  /** A peer reached by fallback whose failed WebRTC link showed one of you behind a cone NAT and the other behind
   * a symmetric one with no port prediction gets one birthday punch, run over the room's own encrypted link: the
   * symmetric side opens 256 connections, the cone side sprays up to 1024 ports of its public IP, and a channel
   * that forms takes the link over (`route(peer)` says `{ kind: 'direct', adopted: true }`). The fallback carries
   * the link meanwhile. One punch at a time per page (per `RTCPeerConnection` class: give every room in a page the
   * same one), at most 2 at any one IP per 5 minutes, at most once per peer while you are in the room, 60 s at
   * most; it needs STUN in `iceServers` and a browser's WebRTC. The peer's own candidates name the address your
   * connections check, so a member can name a third party's. `false` turns it off, and refuses peers' punches.
   * Default true. */
  birthday?: boolean;
  /** A WebRTC implementation for runtimes without one (Node: node-datachannel's polyfill).
   * Without WebRTC, `'local'` rooms send datagrams over the BroadcastChannel. */
  RTCPeerConnection?: typeof globalThis.RTCPeerConnection;
}

export interface MessageInfo {
  lane: 'reliable' | 'unreliable';
  /** How it was sent: a string, bytes, or any other value as JSON. */
  kind: 'text' | 'bytes' | 'json';
}

/** A queued event from `room.poll()`/`room.receive()`: plain values only, for hosts such as Godot's JavaScriptBridge. */
export type RoomEvent =
  | { type: 'peer-join'; peer: PeerId; presence: Presence }
  | { type: 'presence'; peer: PeerId; presence: Presence }
  | { type: 'peer-leave'; peer: PeerId; reason: string }
  | { type: 'message'; peer: PeerId; data: string | ArrayBuffer | any; kind: 'text' | 'bytes' | 'json'; lane: 'reliable' | 'unreliable' };

export interface RoomEvents {
  /** A peer connected and shared its presence (the member's JSON, which can nest deeply: see `message`). */
  'peer-join': (peer: PeerId, presence: Presence) => void;
  /** A peer left, timed out or lost its connection. A peer that leaves (`leave()`, or its tab closing) shows
   * as 'left' or 'closed' at once; one that crashes or loses its network shows as 'timeout' after the idle timeout (30 s).
   * 'policy': its link was on a path the policy has off and couldn't move (see `setPolicy`). */
  'peer-leave': (peer: PeerId, reason: 'left' | 'closed' | 'failed' | 'timeout' | 'replaced' | 'violation' | 'policy') => void;
  /** A joined peer changed its presence. It is the member's JSON, which can nest deeply: see `message`. */
  presence: (peer: PeerId, presence: Presence) => void;
  /** What a joined peer sent: a string, a Uint8Array, or any other value as JSON. A member's JSON can nest about
   * 500000 levels deep in 1 MiB, and some engines' JSON.stringify throws past a few thousand (Node 22's does):
   * check its depth before you stringify it. */
  message: (data: any, peer: PeerId, info: MessageInfo) => void;
}

export interface SendOptions {
  /** One peer or several; default every joined peer. */
  to?: PeerId | PeerId[];
  /** Default `'reliable'`. A number must be 0, 1 or 2 (reliable, unreliable, urgent); anything else throws
   * TypeError. */
  lane?: LaneName | number;
}

export declare class Room {
  /** Our own PeerId. */
  readonly self: PeerId;
  /** Joined peers, not counting us. */
  readonly peers: PeerId[];
  /** The Peerwire endpoint underneath, for stats, sealing and identity. */
  readonly endpoint: Endpoint;
  readonly signaling: SignalingAdapter;
  /** Subscribes; returns a function that unsubscribes. */
  on<E extends keyof RoomEvents>(event: E, fn: RoomEvents[E]): () => void;
  off<E extends keyof RoomEvents>(event: E, fn: RoomEvents[E]): void;
  /** Sends to every joined peer, or to `to`. Returns how many peers it was queued for.
   * Throws RangeError if the message can't fit the lane (reliable: 1 MiB; unreliable: one datagram; urgent:
   * 64 KiB), also while earlier messages wait for the send window, and then sends it to no one. */
  send(data: RoomData, options?: SendOptions): number;
  /** `send` with positional arguments; `peer` '' or null sends to everyone. */
  sendTo(peer: PeerId | '' | null, data: RoomData, lane?: LaneName | number): number;
  /** Every queued event, oldest first, and clears the queue. The first call starts queueing. */
  poll(): RoomEvent[];
  /** The next queued event, or null. The first call starts queueing. */
  receive(): RoomEvent | null;
  /** Our presence, or a joined peer's (a member's JSON, which can nest deeply: see `message`). */
  getPresence(peer?: PeerId): Presence | undefined;
  /** Throws RangeError for a presence over 1 MiB as JSON, TypeError for one JSON can't encode (`undefined`, a
   * function, a Symbol, a BigInt, a cycle), and keeps the old one. */
  setPresence(presence: Presence): void;
  /** How `peer` is reached: its own WebRTC link, a channel handed to `adoptChannel` (`adopted`), relayed by
   * member `via`, or over the signalling relays; null while it isn't linked. A `'peer'` or `'signaling'` route
   * can become `'direct'` later, with the same session (no `peer-leave` or `peer-join`): ICE goes on checking,
   * the room retries WebRTC (see `retryDirect`), or a channel is adopted. A `'signaling'` route also moves to a
   * member linked directly to both of you once that member is seen to relay both ways, and back if it goes; a
   * `'peer'` route to another such member once it is clearly faster (20 % and 15 ms, in 3 of 5 probe rounds 2 s
   * apart). Each only as the policy allows: a link moves only to a path earlier in its `order`. `turn`: our end of
   * the WebRTC link runs through a TURN server (its selected candidate pair's local candidate is a relay one);
   * unset while this WebRTC can't say. `rttMs`: the round trip over this path, measured since the link moved there;
   * null until then. */
  route(peer: PeerId): ({ kind: 'direct'; adopted?: true; turn?: true } | { kind: 'peer'; via: PeerId } | { kind: 'signaling' }) & { rttMs: number | null } | null;
  /** The policy as it stands, every key set: `relays` null without Nostr signalling, and `iceServers` as their
   * urls alone, never a username or credential (an entry from here given back to `setPolicy` keeps its own). */
  readonly policy: Readonly<{
    order: RoomPath[];
    iceServers: { urls: string | string[] }[];
    relays: string[] | null;
    fallbackMs: number;
    sameNatFallbackMs: number;
    birthday: boolean;
    privacy: 'direct' | 'public-only';
  }>;
  /** Changes the policy: the keys given, the rest as they were. New dials follow it at once, and so do links:
   * - one on a path now off moves where the room can move it, within 10 s: from a member to signalling, or from
   *   signalling to a member, if the peer takes it. Else it drops (`'peer-leave'` reason `'policy'`), and the two
   *   dial again under the new order. A WebRTC link on a path now off (over our TURN with `'turn'` off, or not with
   *   `'direct'` off) drops at once the same way;
   * - new `iceServers` go to every WebRTC connection for its next ICE restart, a link over our TURN restarts ICE
   *   at once (from the side that dialled it), and peers on a fallback try WebRTC again. With `'direct'` turned off,
   *   a link kept on our TURN restarts ICE the same way, and every connection sends relay candidates alone;
   * - a call's connections take the new servers and candidate types too, and restart ICE (on our side) when
   *   `'direct'` or `'turn'` goes off or the servers change. With no WebRTC path left, the call fails (`'no-path'`),
   *   and so does one still on a path now off 20 s after its restart. Addresses sent before the change can't be
   *   taken back;
   * - new `relays`: the room dials those it hasn't, says hello there, and closes those dropped. Links stay; what
   *   was on its way through a relay dropped is lost, and the session resends it;
   * - `fallbackMs`, `sameNatFallbackMs` and `birthday` count for links and punches from then on. `privacy` is set
   *   when the room is joined.
   * Throws TypeError as `joinRoom` does, for `relays` without Nostr signalling, and for a `privacy` other than the
   * room's; nothing changes when it throws (every check comes first). */
  setPolicy(policy: RoomPolicy): void;
  /** Tries WebRTC again, now, with `peer` (or every peer) the room reaches only by fallback: a new connection,
   * so one made after a network change, or a microphone or camera grant (privacy `'direct'`), offers the new
   * addresses. If its DataChannel opens, the link moves onto it and `route(peer)` says `'direct'`; if not, it
   * closes quietly and the link stays on its fallback. The room also retries on its own: 20 s after a peer goes
   * on a fallback, then 40, 80, 160 s, then every 5 min (each ±20 %), and on `online`, a new connection type
   * and a grant (see `retryOnGrant`). Retries take turns, one at a time per room, at least 2 s apart; only
   * the dialling side (the smaller id) offers, so the other side asks it. Returns how many peers are to be
   * retried. */
  retryDirect(peer?: PeerId): number;
  /** Moves `peer`'s link onto `dc`, an open DataChannel to it made outside the room (`pc` its connection; a
   * birthday punch's, say), when the room reaches `peer` only by fallback and the peer adopts its end too.
   * True once the channel carries the link; false, leaving the channel to the caller, if the link is direct
   * already or the peer didn't adopt within 5 s. When the channel closes, the link goes back to its fallback.
   * The room's own birthday punches (see the `birthday` option of `joinRoom`) hand their channel to it. */
  adoptChannel(peer: PeerId, pc: RTCPeerConnection, dc: RTCDataChannel): Promise<boolean>;
  /** The shared document `name` (1 to 128 characters), opened on the first call and the same object until it is
   * closed: the same name on every member is the same document. `options` count when it opens. Throws after
   * `leave()`, which closes the room's docs, TypeError for a `persist` it can't use or `relays: true` without Nostr
   * signalling, and RangeError for a name with `persist` or `relays` that isn't well-formed (a lone surrogate) or for
   * `relays: true` with a roomId that hasn't `randomRoomId()`'s shape. See {@link Doc}. */
  doc(name: string, options?: DocOptions): Doc;
  /** Joins the room's voice and video call with `stream` (null only receives): every member in it sends us its
   * stream and gets ours, each over a WebRTC connection of its own, signalled over the room's links. The app gets the
   * stream (getUserMedia); the library never asks for the camera or microphone. One call per room: throws while
   * ours is open (see `Call.setStream`, `Call.leave`) and after `leave()`; TypeError where WebRTC has no media
   * (Node), or for a stream that isn't a MediaStream; RangeError for a `max` that isn't 2 to 16. See {@link Call}. */
  call(stream: MediaStream | null, options?: CallOptions): Call;
  /** Smoothed round-trip time to a peer in ms, or null before the first sample. */
  rtt(peer: PeerId): number | null;
  stats(): EndpointStats;
  /** Tells every peer we're going, then closes the links, all before it returns (so it works from `pagehide`,
   * where it also runs); the promise resolves once docs kept on this device (`persist`) have saved, however long
   * the store takes (one that never settles holds it; the links are closed by then). Reliable messages not yet
   * acknowledged may not arrive: the goodbye can overtake them. Leaves the call too. */
  leave(): Promise<void>;
}

export interface CallOptions {
  /** The most in the call, us included: 2 to 16, default 6. Each member sends its stream to every other, so its
   * upload grows with each one (see docs/project/performance.md §6). A member with no seat for a joiner tells it the call
   * is full; the joiner goes only if its own count of who is in agrees. */
  max?: number;
}

/** Why a peer's media failed: 'no-path', it never connected within 20 s (no direct path between the two, and no
 * TURN server in `iceServers`); 'lost', it connected, then ICE failed and 2 restarts in a minute didn't bring it
 * back; 'error', its description couldn't be applied. */
export type CallFailure = 'no-path' | 'lost' | 'error';

export interface CallEvents {
  /** A peer's media connection: 'connecting' once its link is made (and again while an ICE restart runs),
   * 'connected', or 'failed' with why. A failed peer is not tried again until it or we join again. */
  state: (peer: PeerId, state: 'connecting' | 'connected' | 'failed', reason?: CallFailure) => void;
  /** A peer's stream, once per peer, when its description is applied: one audio and one video track, whatever it
   * sends. Its tracks stay the same when the peer changes what it sends (`setStream`); they are silent or black
   * while it sends nothing, so say "muted" or "camera off" in presence rather than guessing from frames. A peer
   * gets our media from 'connected' on, so draw its tile from its first 'state'. */
  stream: (peer: PeerId, stream: MediaStream) => void;
  /** A peer in `call.peers` is out of the call: it left the call ('left') or the room ('gone'). */
  ended: (peer: PeerId, reason: 'left' | 'gone') => void;
  /** The call had no seat for us: we are out of it (`call.left`), and `room.call()` may try again. `max` is ours. */
  full: (max: number) => void;
}

/** The room's voice and video call, as `room.call()` joined it: a mesh, each member linked to every other. */
export interface Call {
  /** The peers in the call with us, failed ones too (see `state`), until 'ended'. */
  readonly peers: PeerId[];
  /** True after `leave()`, 'full', or the room's `leave()`. Nothing fires after it but 'full'. */
  readonly left: boolean;
  /** Subscribes; returns a function that unsubscribes. */
  on<E extends keyof CallEvents>(event: E, fn: CallEvents[E]): () => void;
  off<E extends keyof CallEvents>(event: E, fn: CallEvents[E]): void;
  /** The peer's media state, or null for a peer not in the call with us. */
  state(peer: PeerId): 'connecting' | 'connected' | 'failed' | null;
  /** The peer's stream once 'stream' has fired, else null. */
  stream(peer: PeerId): MediaStream | null;
  /** The peer's connection stats (inbound-rtp's framesDecoded, say), or null for a peer not linked or failed. */
  getStats(peer: PeerId): Promise<RTCStatsReport | null>;
  /** Sends `stream`'s first audio and first video track from now on, or nothing for a kind it lacks (null: nothing
   * at all): a camera turned on, a screen shared. It replaces tracks and never renegotiates. Throws after leaving. */
  setStream(stream: MediaStream | null): Promise<void>;
  /** Leaves the call and tells the members; the room stays. */
  leave(): void;
}

/** A map key, set item or log entry as `entries()` lists it. Values come back frozen. A value is member data: check
 * every field you render. `from` is the writer as the doc was told: a member relaying a catch-up can name any
 * writer, so use it to colour or group, not as proof. */
export interface DocMapEntry {
  readonly key: string;
  readonly value: unknown;
  readonly from: PeerId;
}
export interface DocItem {
  /** The item's id: opaque, the same on every member. */
  readonly id: string;
  readonly value: unknown;
  readonly from: PeerId;
}

/** What `doc.on('change')` reports: one event per key or item a write or a member's frame changed. */
export interface DocChange {
  /** The collection's name, as given to `doc.map()`, `doc.set()` or `doc.log()`. */
  name: string;
  type: 'map' | 'set' | 'log';
  /** A map's key. */
  key?: string;
  /** A set item's or log entry's id. */
  id?: string;
  /** What the key or item holds now; undefined once it is gone. */
  value: unknown;
  /** The key or item is gone: deleted, or trimmed from a log. */
  deleted?: true;
  /** The writer of the value now shown or, for a deletion, the member it came from (us for our own writes), or
   * 'relays' for one in a save joined from the relays (`relays: true`). */
  from: PeerId | 'relays';
  /** Our own write. */
  local: boolean;
}

/** Where a doc's catching up stands: `doc.sync`, and what `doc.on('sync')` reports when it changes. As good as the
 * members that told us: use it for a status line, not to decide what the doc holds. */
export interface DocSync {
  /** 'alone': no other member to catch up from. 'loading': catching up from `from`. 'synced': caught up (an
   * answer ended from a member that had caught up itself, or every member has answered). */
  readonly state: 'alone' | 'loading' | 'synced';
  /** The member being caught up from, while 'loading'; null when it waits to ask one, and in the other states. */
  readonly from: PeerId | null;
  /** Entries of `from`'s answer that have landed, newest first, of the `total` it carries (as it says): 0 of 0
   * until its first piece lands, and in the other states. `have` may stop short of `total`: the last piece ends
   * the answer. */
  readonly have: number;
  readonly total: number;
  /** Where the doc came from: 'members' once caught up from them; else 'relays' once it joined a save from the relays
   * (`relays: true`), as of `at`; null before either. */
  readonly source: 'members' | 'relays' | null;
  /** With `source` 'relays': when the newest save joined was made (ms since the epoch), as its saver claims it (one
   * more than 15 minutes past this device's clock counts as the time it was read). For display only ("as of
   * 10:42"). Null otherwise. */
  readonly at: number | null;
}

/** One value per key. A `set` replaces the values its writer has seen; of values set at the same time, the one
 * with the later Lamport stamp shows, then the higher writer. A `delete` removes the values its writer has seen,
 * so a set made elsewhere at the same time wins. */
export interface DocMap {
  /** True; false, with a warning, when members have filled the doc or claimed every write of ours (see the README).
   * Throws RangeError for a key over 256 characters or a value over 64 KiB as JSON (or nested over 64 deep),
   * TypeError for a value that isn't JSON. */
  set(key: string, value: unknown): boolean;
  get(key: string): unknown;
  has(key: string): boolean;
  /** False when the key holds nothing. */
  delete(key: string): boolean;
  /** By key. The same array until the map changes. */
  entries(): readonly DocMapEntry[];
}

/** Items, each with its own id; `delete(id)` removes one (undo: delete your item). */
export interface DocSet {
  /** The new item's id; null, with a warning, when `DocMap.set` would return false. Throws as it does. */
  add(value: unknown): string | null;
  get(id: string): unknown;
  has(id: string): boolean;
  /** False when no such item is held. */
  delete(id: string): boolean;
  /** Oldest first, by Lamport stamp then writer: the same order on every member (a whiteboard's z-order). */
  entries(): readonly DocItem[];
}

/** Append-only, keeping the newest `keep` entries. */
export interface DocLog {
  /** The new entry's id; null, with a warning, when `DocMap.set` would return false. Throws as it does. */
  append(value: unknown): string | null;
  get(id: string): unknown;
  /** Oldest first, by Lamport stamp then writer: an entry appended after seeing another sorts after it. */
  entries(): readonly DocItem[];
}

/** Where a doc kept on this device keeps its copy: a store of bytes by key, e.g. over files. `get` gives back what
 * `set` stored, undefined only when there is none, and rejects when it can't tell; `set` stores all of it or
 * nothing (over files: a temp file, then a rename). */
export interface DocStore {
  get(key: string): Promise<Uint8Array | ArrayBuffer | undefined>;
  set(key: string, bytes: Uint8Array): Promise<void>;
  delete(key: string): Promise<void>;
}

export interface DocOptions {
  /** Keeps a copy of the doc on this device, loaded when it opens, before members are asked: `true` for IndexedDB
   * (browsers; TypeError where there is none), or a {@link DocStore}: make it once and pass the same object each
   * time, as a doc closed and opened again at once loads what the closing one saved only through the same store.
   * Saved when the page is hidden, on `close()` and `leave()`, and while the doc changes, one save at a time: a
   * second after a change, and after the last save by its size at 256 KiB/s. A save the store refuses (a full
   * disk) is tried again with the next change, a second after, then 2, 4 … up to 64 s apart while it keeps
   * refusing, until one succeeds; hiding the page, `close()` and `leave()` still save at once. The copy is sealed
   * under a key from the room key, so the storage shows neither the room, the doc nor what it holds; anyone who
   * knows the roomId can read it, as they could join the room. */
  persist?: boolean | DocStore;
  /** Keeps a sealed copy of the doc on the Nostr relays the room signals over (`signaling: 'nostr'`), so a member that
   * opens it while no other member is online still gets it: from the relays, then live from members as ever.
   *
   * **Use a roomId from `randomRoomId()`.** Anyone with the roomId can read this copy at any time, from any relay,
   * without joining the room and with no member online: nobody sees it read. Relays serve stored events to anyone,
   * and mirrors and archives may keep them, whatever expiry or deletion asks, so anything saved can be copied and
   * kept, and deleting from the doc doesn't remove old saves. There is no revoking a link: only a new roomId cuts off
   * someone who once had it. Its key is also a stored test for a guessed roomId. So `relays: true` throws RangeError
   * for a roomId that hasn't `randomRoomId()`'s shape (22 or more base64url characters); the check catches mistakes,
   * not a guessable roomId of that shape. Anyone with the roomId can also overwrite or wipe the copy: members
   * restore it at their next save, and it is best effort, the copies on members' devices (`persist`) being the main
   * store. Relays keep it 30 days after its last save (opening the doc within a week of that saves it again), or
   * less.
   *
   * Saved by one member at a time, behind signalling: a while after the doc changes (10 s without a change, then up
   * to 30 s more, at most once in 5 minutes, after reading what the relays hold), and by the last member online
   * when the page hides. At most 8 pieces of 44 KiB deflated (a few MB of JSON); a larger doc keeps none, with a
   * warning. Throws TypeError without Nostr signalling, or where there is no `CompressionStream('deflate-raw')`. */
  relays?: boolean;
}

/** A shared document from `room.doc(name)`: maps, sets and logs that every member holds and that converge. Writes
 * reach every member at once on the reliable lane; members that join later, or come back after a partition, catch
 * up, from the best-routed member first, in pieces, the newest entries first. Limits: 10,000 entries and 4 MiB of
 * JSON text per doc; names 128 characters, keys 256, values 64 KiB as JSON. Members are trusted alike: any member
 * can overwrite, delete or fill a doc. */
export interface Doc {
  readonly name: string;
  /** Where catching up stands now; `'sync'` fires when it changes. */
  readonly sync: DocSync;
  /** True once the doc has caught up, and from then on, also once the other members have gone ('alone'). */
  readonly synced: boolean;
  /** What opening found on this device: 'loaded' (its copy is in the doc), 'empty' (none, or one that couldn't be
   * read, which the next save replaces), 'blocked' (the storage refused, as in some private modes, hadn't loaded
   * the copy within 5 s, or holds one from another version of this package: the copy is left as it is, and the doc
   * goes on without one, with a warning) or 'off' (no `persist`). */
  readonly loaded: Promise<'loaded' | 'empty' | 'blocked' | 'off'>;
  map(name: string): DocMap;
  set(name: string): DocSet;
  /** `keep`: the newest entries kept, 1 to 10,000 (default 1000). Give every member the same, right after
   * `room.doc()`. Calling again with another `keep` trims to it. */
  log(name: string, options?: { keep?: number }): DocLog;
  /** Subscribes; returns a function that unsubscribes. A handler that throws is reported on its own. */
  on(event: 'change', fn: (change: DocChange) => void): () => void;
  on(event: 'sync', fn: (sync: DocSync) => void): () => void;
  off(event: 'change', fn: (change: DocChange) => void): void;
  off(event: 'sync', fn: (sync: DocSync) => void): void;
  /** Deletes the copy this device keeps (`persist`), and keeps none until the doc is opened with `persist` again.
   * With `relays: true`, also wipes the copy on the relays, best effort, **for every member** until another member
   * saves (a relay or archive may still keep what it copied), and saves none there from now on. What the doc holds
   * stays. Resolves once the copy is gone and the relays have answered; rejects if the storage fails. */
  forget(): Promise<void>;
  /** Stops syncing and forgets the doc; `room.doc(name)` later opens it afresh and catches up. Writes after it
   * throw. A doc kept on this device saves first: the promise resolves once the store has the copy, however long
   * that takes (closed while a store takes more than 5 s to load the copy, it resolves then, saving nothing). */
  close(): Promise<void>;
}

/** Joins a room: `const room = await joinRoom({ appId: 'my-app', roomId: 'lobby' })`. */
export declare function joinRoom(options: JoinRoomOptions): Promise<Room>;

/** 128 random bits, base64url (22 characters): a roomId nobody can guess. */
export declare function randomRoomId(): string;

/** A {@link DocStore} in this process's memory, for Node and tests: its copies go with the process. */
export declare function memoryStore(): DocStore;

export interface InboxOptions {
  appId: string;
  /** 32 bytes restoring the identity whose inbox it is (`room.endpoint.secret()`); a fresh one by default, which can
   * only send, as nobody knows its PeerId. */
  secret?: Uint8Array;
  /** Nostr relays to leave and read messages on. Default {@link DEFAULT_RELAYS}. */
  relays?: string[];
  /** Remembers which messages were delivered, across sessions: `true` for IndexedDB (browsers), or a
   * {@link DocStore}. Without it, a message still on the relays is delivered again in each new session. Needs
   * `secret`; sealed under a key from it, so the storage shows nothing of what was delivered. */
  persist?: boolean | DocStore;
  /** Where to load `peerwire_ffi.wasm` from, as for `joinRoom`. */
  wasm?: string | URL | BufferSource;
}

export interface InboxMessageInfo {
  /** How it was sent: a string, bytes, or any other value as JSON. */
  kind: 'text' | 'bytes' | 'json';
  /** When it was sent, in ms, by the sender's clock. */
  at: number;
  /** The message's id, 32 hex characters, from its sender and the id the sender gave it: the same if the message
   * comes again (an inbox remembers 4096 messages, and without `persist` one session's), so the app can drop it. */
  id: string;
}

/** Messages to one peer by its PeerId while it is offline, from `openInbox()`: sealed to the recipient and left on
 * Nostr relays, read the next time it opens its inbox. A relay sees neither recipient nor sender, nor which messages
 * go together: each is signed by a fresh key and padded, and every inbox of the app reads every message of the app,
 * opening only its own. The relay does see the IP of whoever leaves or reads messages, and when. */
export interface Inbox {
  /** Our PeerId: give it to those who may write to us. */
  readonly id: PeerId;
  /** `data` as sent, `from` the sender's PeerId (authenticated: it signed the message, and nobody else can read
   * it). Each message is delivered once (across sessions with `persist`), to every listener, oldest first. The
   * first listener starts reading: at once, then every minute; nothing is read while nobody listens. A message left
   * for another app, or a JSON value nested deeper than 64, is dropped. A listener that throws is reported on its
   * own. Returns a function that removes it. */
  on(event: 'message', fn: (data: any, from: PeerId, info: InboxMessageInfo) => void): () => void;
  off(event: 'message', fn: (data: any, from: PeerId, info: InboxMessageInfo) => void): void;
  /** Leaves `data` (a string, bytes, or any other value as JSON: 16 KiB at most, and a value nested 64 deep at most,
   * else RangeError) for `to` on the relays, for `ttl` seconds (1 to 7 days, the default; NIP-40 asks relays to drop
   * it after). Resolves to the number of relays that took it, once each has answered. Anyone can leave a message for
   * any PeerId: check `from`. */
  send(to: PeerId, data: any, options?: { ttl?: number }): Promise<number>;
  /** Reads the relays now; resolves to the number of messages delivered. Each relay is read up to 100 messages a
   * check, so a flood delays messages without costing more, except that 100 or more left in a message's second and
   * ordered before it on a relay (by event id) hide it there: a relay can't be asked for the rest of a second. Its
   * copies on other relays are still read. */
  check(): Promise<number>;
  /** Stops reading and closes the relay connections. */
  close(): void;
}

/** Opens an inbox: `const inbox = await openInbox({ appId: 'my-app', secret, persist: true })`. */
export declare function openInbox(options: InboxOptions): Promise<Inbox>;
/** The largest message `inbox.send` takes, in bytes (16 KiB). */
export declare const MAX_MESSAGE: number;
/** The longest a message stays on the relays, in seconds (7 days). */
export declare const MAX_TTL_S: number;

export declare const DEFAULT_ICE_SERVERS: RTCIceServer[];
export declare const DEFAULT_RELAYS: string[];
export declare const RECONNECT_MS: number;
export declare const MAX_RECONNECT_MS: number;

// Lower layers.

export declare const Lane: { readonly Reliable: 0; readonly Unreliable: 1; readonly Urgent: 2 };
export declare const EventKind: {
  readonly Connected: 1;
  readonly Message: 2;
  readonly Disconnected: 3;
  readonly ConnectFailed: 4;
  readonly PathChanged: 5;
  readonly AddrObserved: 6;
};

export type Path = { direct: number } | { via: PeerId };

export type EndpointEvent =
  | { type: 'connected'; peer: PeerId; initiator: boolean; path: Path }
  | { type: 'message'; peer: PeerId; lane: number; data: Uint8Array }
  | { type: 'disconnected'; peer: PeerId; reason: 'timeout' | 'closed' | 'replaced' | 'violation' }
  | { type: 'connectFailed'; peer: PeerId }
  | { type: 'pathChanged'; peer: PeerId; path: Path }
  | { type: 'addrObserved'; peer: PeerId; addr: number }
  | { type: 'unknown'; kind: number };

export interface EndpointStats {
  txPackets: number;
  rxPackets: number;
  rxMalformed: number;
  rxUnknownSession: number;
  rxAuthFailed: number;
  rxReplayed: number;
  rxHandshakeDropped: number;
  retransmits: number;
  unreliableDropped: number;
  roams: number;
  relayed: number;
  relayDropped: number;
  punchesSent: number;
  punchSucceeded: number;
  punchFailed: number;
  directFallbacks: number;
}

export interface Peerwire {
  nostrKeypair(): { secret: Uint8Array; pubkey: Uint8Array };
  nostrSign(secret: Uint8Array, msg32: Uint8Array): Uint8Array;
  endpoint(options?: { secret?: Uint8Array | null; shareAddresses?: boolean; relayOnly?: boolean }): Endpoint;
}

/** Loads the module from a URL (fetched; in a browser, relative and root-relative ones too), an absolute path or
 * `file:` URL (read from disk, in Node only) or its bytes. A fetch with an HTTP error status rejects with it. */
export declare function loadPeerwire(source: string | URL | BufferSource): Promise<Peerwire>;

/** Every method that takes a peer throws a TypeError unless it is 32 bytes or 64 lowercase hex
 * characters: ids from signalling are untrusted and never reach wasm memory unchecked. */
export declare class Endpoint {
  readonly peerId: Uint8Array;
  readonly id: PeerId;
  /** Called with (transport handle, datagram) for each datagram to send. */
  onTransmit: (addr: number, bytes: Uint8Array) => void;
  onEvent: (event: EndpointEvent) => void;
  /** Called at the end of a pump that handled incoming datagrams: ACKs may have freed send
   * window, so a sender that got -3 (would block) can try again. Room flushes its outbox here. */
  onDrain: (() => void) | null;
  /** Datagrams lost because `onTransmit` threw; the endpoint keeps running and retransmits. */
  readonly transmitErrors: number;
  /** The 32-byte identity secret; null after `close()`. */
  secret(): Uint8Array | null;
  /** Throws `Error('connect: endpoint closed')` after `close()`. */
  connect(addr: number, peer: PeerId | Uint8Array): number;
  /** Throws `Error('connectVia: endpoint closed')` after `close()`. */
  connectVia(relay: PeerId | Uint8Array, peer: PeerId | Uint8Array): number;
  /** A direct address for a relayed (or dialling) peer: punches to it and moves the link there once the peer
   * answers from it. Does nothing for a peer linked directly or unknown, while relay-only for one linked
   * through a relay or being dialled with one (nothing is sent to `addr`), or after `close()`. */
  addCandidate(addr: number, peer: PeerId | Uint8Array): void;
  /** Marks `addr` as a slow address of the linked `peer` (a signalling pipe), or clears the mark (`null`). Paths
   * rank slow address, then relay, then direct. On it the link moves to a relay offered with `addRelay` once a
   * probe through it is answered, and goes back to it ('pathChanged') when that relay's link goes, the peer's
   * traffic comes over it again, the relay carries nothing for 15 s, or data sent through it goes unacknowledged
   * for 5 s. Then the link holds that relay off for a minute, twice as long each time it stalls the link again (up
   * to 16 minutes). Does nothing for a peer not linked, or after `close()`. */
  setSlow(peer: PeerId | Uint8Array, addr: number | null): void;
  /** Offers `relay`, a peer linked directly, to carry the link to `peer` while that link is on its slow address
   * (`setSlow`): a sealed probe goes through `relay` every half second for 5 s, and the link moves there
   * ('pathChanged') once `peer` answers through it, which it does only if it would use the relay too. The first
   * probe goes out after the current task, with any others offered in it. On a link relayed already, the relay
   * is raced against the one it is on (a probe round every 2 s, 5 rounds), and the end with the smaller
   * PeerId moves the link to it once it is clearly faster; the other follows. A relay that leaves the link's data
   * unacknowledged for 5 s, on a link with no slow address, is held off as `setSlow` describes and raced at once,
   * and either end moves the link to the relay that wins. Does nothing while relay-only, for a link that is
   * direct, for a relay the link holds off (see `setSlow`), or after `close()`. */
  addRelay(relay: PeerId | Uint8Array, peer: PeerId | Uint8Array): void;
  /** Ends the session with `peer` and sends it a sealed close at once (pumps), so it sees 'disconnected'
   * (reason 'closed') now rather than after its idle timeout. No local event for `peer`, though links it relayed
   * go back to their slow addresses ('pathChanged'); does nothing after `close()`. */
  disconnect(peer: PeerId | Uint8Array): void;
  /** Relayed links never punch to a direct path, dials through a relay alone (given the relay before any
   * direct address of their own, or with a relay when this is turned on) send nothing direct, relays forward
   * without our address, and a direct handshake is answered only from a peer we are dialling directly or already
   * have a direct link with. A direct dial later given a relay keeps its direct addresses and may complete direct,
   * until the next `setRelayOnly(true)`: every such call, even with relay-only already on, sends each dial that has
   * a relay through it alone. */
  setRelayOnly(on: boolean): void;
  /** After `close()`, the counters as they were when it closed. */
  stats(): EndpointStats;
  /** seal, open and rttMs return null, and isConnected false, after `close()`. */
  seal(peer: PeerId | Uint8Array, bytes: Uint8Array): Uint8Array | null;
  open(bytes: Uint8Array): { from: PeerId; data: Uint8Array } | null;
  isConnected(peer: PeerId | Uint8Array): boolean;
  rttMs(peer: PeerId | Uint8Array): number | null;
  /** Round-trip time over the path the link to `peer` is on now, measured since it moved there; null until
   * measured, or after `close()`. */
  pathRttMs(peer: PeerId | Uint8Array): number | null;
  /** 0, or a negative PW_ERR code (-1 not connected, -2 too large: over 1100 B unreliable, 64 KiB urgent,
   * 1 MiB reliable; -3 would block).
   * Throws `Error('send: endpoint closed')` after `close()`: that is a bug in the app, not a lost packet. */
  send(peer: PeerId | Uint8Array, data: string | Uint8Array, lane?: number): number;
  /** send() for a caller that tries the same message again after -3: a message over 16 KiB stays
   * in wasm memory between tries instead of being copied in each time. `data` is matched by
   * identity and must not change between tries; one message is kept at a time. */
  sendRetry(peer: PeerId | Uint8Array, data: string | Uint8Array, lane?: number): number;
  /** A datagram that arrived on handle `addr`. `peer`: the only peer that sends on that handle (its DataChannel,
   * its signalling pipe), if there is one: a packet there for another peer's link, or a handshake from anyone else,
   * is then dropped unread, so a peer that relays for others can't move their links onto its own channel. Ignored
   * after `close()`: late datagrams are normal. */
  receive(addr: number, bytes: Uint8Array, peer?: PeerId | Uint8Array | null): void;
  pump(): void;
  /** Frees the endpoint; a second call does nothing. Close the transport first, so that no
   * datagram arrives for a closed endpoint (`room.leave()` does this). */
  close(): void;
  /** True once `close()` has run. */
  readonly closed: boolean;
}

export declare function decodeEvent(bytes: Uint8Array): EndpointEvent;
export declare function toHex(bytes: Uint8Array): string;
/** Removes `host` candidates (LAN, loopback, mDNS) from an SDP. */
export declare function withoutHostCandidates(sdp: string): string;
/** Counts an SDP's ICE candidates by type and family; `mdns` are host candidates named `*.local`. */
export declare function candidateSummary(sdp: string): { host4: number; host6: number; mdns: number; srflx4: number; srflx6: number; prflx: number; relay: number };
/** A NAT as the server-reflexive candidates in an SDP show it; a sequential symmetric one comes with its prediction. */
export interface NatInfo {
  type: 'symmetric' | 'cone' | 'unknown';
  /** With `next` and `step`: the NAT's public IPv4 address, its likely next port, and the step between ports. */
  ip?: string;
  next?: number;
  step?: number;
}
/** What the server-reflexive candidates in an SDP (or candidate lines) say about the NAT. */
export declare function natFromSdp(sdp: string): NatInfo;
/** ICE candidates at the ports around a peer's prediction, to add with `addIceCandidate`; `[]` if it doesn't check out. */
export declare function predictedCandidates(nat: { ip: string; next: number; step: number }, sdpMid?: string): RTCIceCandidateInit[];
export declare function fromHex(hex: string): Uint8Array;
/** Whether `id` is a PeerId as text: 64 lowercase hex characters. Check ids from signalling with it. */
export declare function isPeerId(id: unknown): id is PeerId;
/** A PeerId as 32 bytes; throws a TypeError unless `id` is 32 bytes or 64 lowercase hex characters. */
export declare function peerBytes(id: PeerId | Uint8Array): Uint8Array;

export interface Transport {
  dial(peer: PeerId): void | Promise<void>;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

export declare class WebRtcTransport implements Transport {
  constructor(
    endpoint: Endpoint,
    signaling: SignalingAdapter,
    options?: {
      iceServers?: RTCIceServer[];
      /** `'failed'`: the DataChannel isn't open `fallbackMs` after its offer or answer (`sameNatFallbackMs` for a
       * peer behind our public IPv4), or ICE failed; ICE goes on checking, and a channel that opens later takes
       * over a link the endpoint made by other means (a relayed one punches over it; a pipe's datagrams go over it,
       * and the far end's core moves the link there). `'carrier'`: an adopted channel started or stopped carrying
       * the peer's link (see `adopt`). With `'closed'`, `handle` is the closed link's endpoint handle.
       * `'retry-failed'`: a retry's connection (see `retry`) didn't open; `'retry'`: the other side asks us, the
       * dialling side, for one. */
      onPeer?: (peer: PeerId, state: 'open' | 'closed' | 'failed' | 'carrier' | 'retry-failed' | 'retry', handle?: number) => void;
      /** Default 8000 ms. */
      fallbackMs?: number;
      /** Default 3000 ms, at most `fallbackMs`. */
      sameNatFallbackMs?: number;
      loss?: number;
      trickle?: boolean;
      /** `'relay-only'` needs `relay`: WebRTC then runs only with that peer. */
      privacy?: 'direct' | 'public-only' | 'relay-only';
      relay?: PeerId;
      /** Offers carry `p: await sign(to)`; an offer is answered only if `check(from, p)` holds. */
      offerProof?: { sign(to: PeerId): Promise<string>; check(from: PeerId, p: unknown): Promise<boolean> };
      /** Default false. Lets the transport carry datagrams as signalling messages (a pipe) for peers the room allows.
       * `joinRoom` turns it on for `fallback: 'auto'` and opens the pipes itself. Off, pipe batches that arrive are
       * refused (`badSignals`). */
      pipes?: boolean;
      /** The bytes of datagrams a pipe batch carries at most, 1 to 16384 (the default), else TypeError; a pipe queues 8
       * batches' worth. {@link RelayTransport} uses {@link NATIVE_BATCH}. */
      pipeBatch?: number;
      /** The WebRTC paths allowed: STUN servers go to a connection with `direct`, TURN ones with `turn`, and one
       * without `direct` takes relay candidates alone. Ours follow what `dial` is given; one we answer, these, without
       * TURN when the offer says `turn: false`; with neither, an offer goes unanswered. Default both. */
      rtc?: { direct: boolean; turn: boolean };
      RTCPeerConnection?: typeof globalThis.RTCPeerConnection;
    },
  );
  /** Datagrams dropped by the simulated `loss` option. */
  readonly dropped: number;
  /** Datagrams dropped because the DataChannel had over 1 MiB queued or `send` threw. */
  readonly congested: number;
  /**
   * Signals refused: a `from` that isn't a PeerId or an empty message, an offer whose `offerProof` fails or whose
   * SDP doesn't apply, a retry offer that would make a peer's second new connection within 2 s, an `ice-batch`
   * that isn't a list of at most 32, or one that failed unexpectedly, and a pipe batch from a peer not allowed one
   * (or with `pipes` off) or that isn't a list of 1 to 64 base64 datagrams of 1 to 1232 bytes each (a batch is taken
   * whole or not at all). Answers and
   * ICE candidates that don't apply are not counted (copies and races are normal). The WebRTC counterpart of
   * NostrSignaling's `stats.rejected`.
   */
  readonly badSignals: number;
  /** Offers sent again for want of an answer, and answers sent again for a repeated offer. */
  readonly resent: number;
  /**
   * Offers, answers, copies of either and `restart` asks not sent because the peer's publish budget was spent:
   * 6 at once, then 1 per 5 s per peer, so no peer can keep us publishing to the signalling relays. The ICE
   * candidates of an answer not sent wait with it, until a copy of its offer finds budget.
   */
  readonly throttled: number;
  /**
   * ICE restarts begun as the dialling side (a link that was open lost ICE). At most 3 per link a minute, whether
   * or not ICE reconnects in between (while it stays down, the next is made once the minute frees); the other
   * side's ask is heeded only while our own ICE is down too.
   */
  readonly restarts: number;
  /** Retries begun as the dialling side (see `retry`). */
  readonly retried: number;
  /**
   * Diagnostics: called for each link's signalling (`offer-sent`, `offer-received`, `answer-sent`, `answer-received`,
   * `ice-batch-sent`, `ice-batch-received`, with `candidateSummary` counts, `resend: true` on a copy and
   * `restart: true` for an ICE restart, `retry: true` for a retry's offer), retries (`retry` with `dialer`,
   * `retry-failed`), ICE (`ice-state` with `state`, `candidate-error` with the STUN server `url`,
   * `code`, `text`, `address`, `ice-restart` with `n`, `reason`, `dialer`), port prediction (`guesses` with the
   * peer's `ip`, `next`, `step` and the `n` candidates added), `same-nat` (the peer's candidates name our
   * public IPv4 `ip`, so the link falls back after `ms`), `throttled` (a message of `kind` not sent: see
   * `throttled`) and the room's birthday punches (`birthday` with `step`: `start` with our `role`, `'easy'`
   * (cone) or `'hard'` (symmetric), `fleet` with the `n` connections opened, `spray` with the `ip` and the `n`
   * ports, `end` with `role` and `why`, then `adopted` or `not-adopted` once a channel opened).
   */
  trace: ((peer: PeerId, what: string, detail: Record<string, unknown>) => void) | null;
  /**
   * The network changed: links that were open but whose ICE isn't connected restart ICE now, rather than after
   * the 2 s grace. `joinRoom` calls it on `online` and on a `navigator.connection` change.
   */
  networkChanged(): void;
  /**
   * Carries `peer`'s link over `dc`, an open DataChannel to it made outside the transport, once the far end
   * adopts its end too (each sends a one-byte marker until it hears the other's). With `pipe`, the peer's pipe
   * datagrams go over `dc` instead of signalling; otherwise `dc` gets its own handle, for
   * `endpoint.addCandidate`. Resolves to that handle, or -1 (the channel left to the caller) if the far end
   * didn't adopt within 5 s or `dc` closed first. Once carrying, `dc` closing or `pc` failing ends it, as do
   * `drop(peer)`, `close()` and a newer `adopt(peer)`; ending it closes `pc`. `room.adoptChannel` uses it.
   */
  adopt(peer: PeerId, pc: RTCPeerConnection, dc: RTCDataChannel, options?: { pipe?: boolean }): Promise<number>;
  /** The adopted channel carrying `peer`'s link now, or null. */
  carrier(peer: PeerId): { pc: RTCPeerConnection; dc: RTCDataChannel } | null;
  /**
   * What the newest WebRTC link to `peer` that failed showed of the two NATs (before one has, the link now, once
   * it has the peer's description): the server-reflexive IPv4 addresses our candidates (`mine`) and the peer's
   * (`theirs`) named, each with its ports; null before that. The room's birthday punches read it.
   */
  nat(peer: PeerId): { mine: Map<string, Set<number>>; theirs: Map<string, Set<number>> } | null;
  /**
   * As the dialling side, dials `peer` again with a new RTCPeerConnection while its link runs by other means:
   * the link's own connection, which never opened, is closed, and the offer says `retry`, so the other side
   * drops its own (only one that never opened). If the new DataChannel opens, the link moves onto it; if not,
   * it closes quietly (`'retry-failed'`, never `'failed'`). False if the link is open or a retry is under way.
   * `use`: its WebRTC paths, the `rtc` option's by default.
   */
  retry(peer: PeerId, use?: { direct: boolean; turn: boolean }): boolean;
  /** Asks `peer`, the dialling side, for a retry (it gets `onPeer(peer, 'retry')`). */
  askRetry(peer: PeerId): void;
  /** Opens a DataChannel to `peer` over the WebRTC paths `use` (the `rtc` option's by default; an offer without
   * `turn` says `turn: false`) and, once open, runs the handshake. `replace`: the offer says `retry`, so the peer
   * drops a link of its own that never opened, but ours fails as `'failed'` (a later stage of a room's policy). */
  dial(peer: PeerId, options?: { retry?: boolean; replace?: boolean; use?: { direct: boolean; turn: boolean } }): Promise<void>;
  /** Whether our end of `peer`'s WebRTC link runs through a TURN server (its selected candidate pair's local
   * candidate is a relay one): null while this WebRTC can't say, or with no link. */
  relayed(peer: PeerId): boolean | null;
  /** New iceServers, and the WebRTC paths allowed: connections made from now on use them, and every connection
   * takes them (`setConfiguration`) for its next ICE restart, within the paths it was made for. */
  configure(iceServers: RTCIceServer[], rtc: { direct: boolean; turn: boolean }): void;
  /** Restarts ICE on `peer`'s open link, if we dialled it, within the restart budget (see `restarts`). */
  restart(peer: PeerId): void;
  /** Pipes on or off. Off: no new pipe, and only a peer whose pipe is open now may still send over it. */
  setPipes(on: boolean): void;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

/** The module with Bluetooth (`peerwire_ffi_ble.wasm`, next to this package's own): `loadPeerwire(BLUETOOTH_WASM)`.
 * It is the default module and `pw_ble_*`, about 50 KB more, so only a page that uses Bluetooth loads it. */
export declare const BLUETOOTH_WASM: URL;
/** The GATT service a Peerwire peer serves over Bluetooth LE, and its `rx` (written) and `tx` (notified) characteristics. */
export declare const SERVICE_UUID: string;
export declare const RX_UUID: string;
export declare const TX_UUID: string;

/** Why a Bluetooth dial failed: see {@link BluetoothTransport.dial}. */
export type BluetoothFailure = 'not-available' | 'cancelled' | 'not-allowed' | 'not-member' | 'refused' | 'wrong-peer'
  | 'violation' | 'timeout' | 'failed' | 'closed';

/**
 * Bluetooth LE in the browser (Web Bluetooth: Chrome and Edge, Chrome on Android), as a central only: the user picks
 * a device nearby that runs Peerwire (a Linux machine running `ble_chat`, an ESP32 board), and the page links with
 * the room member there over Bluetooth alone, with no network. Two browsers can't link this way, and Firefox, Safari
 * and every iPhone browser have no Web Bluetooth (`dial` rejects with `'not-available'`).
 *
 * The room is a native one, a Rust node's (`ble_led`, `ble_chat`), not a `joinRoom` room: the page sends the member a
 * room hello over the connection and dials it there, and messages are the endpoint's, raw bytes
 * (`endpoint.send(peer, 'led on')`, a `message` event back). The protocol runs in the wasm (`BleLink`); this moves
 * GATT values, 20 bytes at most each, as Web Bluetooth doesn't say the ATT MTU.
 *
 * ```js
 * const pw = await loadPeerwire(BLUETOOTH_WASM);
 * const endpoint = pw.endpoint();
 * const bt = new BluetoothTransport(endpoint, { appId: 'peerwire-lan-chat', roomId: 'esp32' });
 * endpoint.onEvent = (e) => e.type === 'message' && show(new TextDecoder().decode(e.data));
 * button.onclick = async () => endpoint.send(await bt.dial(), 'led on');
 * ```
 *
 * It has the {@link Transport} methods, but its `dial` takes no peer (or one to insist on) and resolves to the PeerId
 * it linked with.
 */
export declare class BluetoothTransport {
  /** `endpoint`: one of the module with Bluetooth, else TypeError. `bluetooth`: Web Bluetooth, `navigator.bluetooth`
   * by default. `onPeer`: `'open'` once a link is up, `'closed'` once its connection went (the endpoint's link then
   * times out, unless it has another path). */
  constructor(
    endpoint: Endpoint,
    options: {
      appId: string;
      roomId: string;
      bluetooth?: { requestDevice(options: object): Promise<any>; getAvailability?(): Promise<boolean> } | null;
      onPeer?: ((peer: PeerId, state: 'open' | 'closed') => void) | null;
    },
  );
  /** Whether this browser has Web Bluetooth and a radio it can use now. */
  available(): Promise<boolean>;
  /**
   * Asks the user to pick a device (call it from a click or another user gesture; the picker lists Peerwire devices
   * alone), connects, proves the room and links with the member there: resolves to its PeerId once the link is up
   * (within 15 s). With `peer`, only if the device proves it is that peer. A connection whose room challenge doesn't
   * come within 2 s of subscribing (Chrome drops a notification that comes too early) is made again, 3 times in all.
   * Rejects with an Error whose `code` is a {@link BluetoothFailure}: `'not-member'` when the device closes the
   * connection on our answer to its room challenge (another room's name), `'refused'` when it closes it before
   * challenging us (full), `'wrong-peer'` when it proves it is another peer than `peer` or one linked over another
   * connection.
   */
  dial(peer?: PeerId | null): Promise<PeerId>;
  has(peer: PeerId): boolean;
  /** Ends the link to `peer`: the endpoint's goodbye goes over its connection, which then closes. */
  drop(peer: PeerId): void;
  /** Closes every connection; a dial under way rejects with `'closed'`. */
  close(): void;
}

/** The bytes of datagrams a pipe batch to a native peer carries at most (1232): a board reads relay messages of 4 KB at
 * most, and one such batch makes an event of about 3 KB. */
export declare const NATIVE_BATCH: number;

/** Why a {@link RelayTransport} dial failed. */
export type RelayFailure = 'timeout' | 'failed' | 'closed';

/**
 * Links with a native room member (a Rust node, a board on Wi-Fi) over the room's Nostr relays alone, from anywhere:
 * the link's datagrams go as signalling messages, on {@link WebRtcTransport}'s pipe, sealed to the peer and each
 * batch through one relay. Slow (a relay trip each way, a few batches a second): for commands and chat, not streams.
 *
 * The room is a native one, as with {@link BluetoothTransport}: the transport says hello as a native member does
 * (`meta.nb`, a proof under the room key) on start, every 20 s, on a relay that opens again and in answer to a peer
 * newly heard (at most one each 2 s), lists the native peers whose hellos prove the room key, and dials one over a
 * pipe alone, taking pipe batches from those peers alone. Messages are the endpoint's, raw bytes. A hello only names a
 * peer: the handshake proves who it is. The room name is the board's password: over the internet, use one nobody can
 * guess ({@link randomRoomId}).
 *
 * ```js
 * const endpoint = (await loadPeerwire(wasmUrl)).endpoint();
 * const link = new RelayTransport(endpoint, { appId: 'peerwire-lan-chat', roomId: 'esp32',
 *   onPeer: async (peer, state) => state === 'seen' && endpoint.send(await link.dial(peer), 'led on') });
 * ```
 *
 * It has the {@link Transport} methods, but its `dial` resolves to the PeerId once linked.
 */
export declare class RelayTransport {
  /** `endpoint`: any module's, else TypeError, as for a missing `appId` or `roomId`. `relays`: `ws://` or `wss://`
   * urls with no fragment ({@link DEFAULT_RELAYS} by default), else a TypeError naming one by its index. `onPeer`:
   * `'seen'` when a native peer's hello first proves the room key (and again once one went unheard for a minute),
   * `'open'` when a dial links. `role`: Rust's `Role`, said in our hello and proved under the room key: `'peer'` (the
   * default) links with peers and hosts, `'host'` with peers and clients, `'client'` with hosts alone, else a
   * TypeError. Only peers in a role ours links with are listed and send us pipe batches: a page that drives a board
   * says `'client'` to a board that joined as `Role::Host`, so two such pages don't dial each other. */
  constructor(
    endpoint: Endpoint,
    options: {
      appId: string;
      roomId: string;
      relays?: string[];
      onPeer?: ((peer: PeerId, state: 'seen' | 'open') => void) | null;
      role?: 'peer' | 'host' | 'client';
    },
  );
  /** The native peers heard in the last minute, in a role ours links with, whose hellos prove the room key. */
  readonly peers: PeerId[];
  /** The relays it says hello on and carries the pipes over (`stats`). */
  readonly signaling: NostrSignaling;
  /**
   * Links with `peer` over a pipe: resolves to its PeerId once the handshake is done (at once if the endpoint is linked
   * with it already), within 15 s. Its answers are taken once its hello proves the room key: a peer not heard yet is
   * sent our hello, which a native peer answers with its own. Rejects with an Error whose `code` is a
   * {@link RelayFailure}: `'timeout'` (the peer isn't online, or not in this room), `'failed'` (the endpoint refused
   * the dial), `'closed'` (`close()`, or `drop(peer)`); with a TypeError for a `peer` that isn't a PeerId. A dial to a
   * peer under way is that one.
   */
  dial(peer: PeerId): Promise<PeerId>;
  /** Whether `peer` is linked and we have a pipe to it. */
  has(peer: PeerId): boolean;
  /** Ends the link to `peer`: the endpoint's goodbye goes over the pipe at once, then the pipe goes. */
  drop(peer: PeerId): void;
  /** Sends what waits in the pipes, then leaves the relays; a dial under way rejects with `'closed'`. */
  close(): void;
}

/** What a {@link BleLink} reports. */
export type BleEvent =
  | { type: 'proven' }
  | { type: 'peer'; peer: PeerId }
  | { type: 'signal'; inner: number; payload: Uint8Array }
  | { type: 'closed'; reason: 'wrong-answer' | 'wrong-proof' | 'early' | 'drops' | 'wrong-identity' | 'late' | 'closed' };

/**
 * One Bluetooth LE connection's protocol in the module with Bluetooth (`pw_ble_*`), beside an endpoint of that module:
 * the room proof, identity proofs, sealed signals and the framing. `BluetoothTransport` drives it over Web Bluetooth;
 * another host moves the GATT values itself: `push` each value that came, send each of `values()`, call `timeout()`
 * again by the time it gives, and hand `datagram` what the endpoint sends to `addr`. Datagrams pass only once the
 * other end proved who it is (`'peer'`), and then only as from that peer.
 */
export declare class BleLink {
  /** `central`: this end connected (default true). `mtu`: the ATT MTU values are cut to (default 23: 20-byte values).
   * `addr`: the endpoint handle of the connection's datagrams. TypeError for an endpoint of a module without
   * Bluetooth. */
  constructor(endpoint: Endpoint, options: { appId: string; roomId: string; central?: boolean; mtu?: number; addr: number });
  /** Takes a value that came; the events it brought. As the room is proven, our identity proof goes. */
  push(value: Uint8Array | ArrayBuffer | ArrayBufferView): BleEvent[];
  events(): BleEvent[];
  /** The values to send, oldest first, each taken off the queue. */
  values(): Uint8Array[];
  /** Seals `payload` as a signal (`inner` 1 a room hello, 2 a message sealed to the peer): 0, or -1 before the room is
   * proven, -2 over 1,207 bytes, -3 with the queue full, -4 for another kind. */
  signal(inner: number, payload: Uint8Array): number;
  /** A datagram the endpoint sent to `addr`: 0, or -1 before the peer proved who it is, -2 too large, -3 queue full. */
  datagram(bytes: Uint8Array): number;
  /** Closes the link if 3 s went without the room and the peer's identity proven; ms until the next call, or null. */
  timeout(): number | null;
  state(): 'proving' | 'answered' | 'proven' | 'identified' | 'closed';
  setMtu(mtu: number): void;
  free(): void;
}

export declare class LocalTransport implements Transport {
  constructor(endpoint: Endpoint, signaling: BroadcastSignaling);
  dial(peer: PeerId): void;
  has(peer: PeerId): boolean;
  drop(peer: PeerId): void;
  close(): void;
}

export declare class BroadcastSignaling implements SignalingAdapter {
  constructor(id: string, room?: string);
  readonly id: string;
  onHello: (peer: PeerId, meta?: unknown) => void;
  hello(meta?: unknown): void;
  send(to: PeerId, body: unknown): void;
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}

export declare class NostrSignaling implements SignalingAdapter {
  constructor(pw: Peerwire, endpoint: Endpoint, options: { room: string; relays?: string[] });
  readonly id: string;
  readonly ready: Promise<void>;
  /**
   * `relaysOpen` counts sockets open now; `reconnects` counts relays that opened again after a close or a redial;
   * `backoffs`, relays held for their rate or stopped for a ban; `resent`, events sent to a relay again after it
   * refused them for its rate; `unsent`, events dropped from a relay's queue (past 64 waiting, or waiting over 10 s);
   * `redials`, sockets closed and dialled again for hearing nothing at all for 10 s after a signal (20 s, then 30 s,
   * for the same relay; at most 3 until it answers within 5 s on a socket dialled 30 s ago or more).
   */
  readonly stats: { published: number; received: number; rejected: number; relaysOpen: number; reconnects: number; backoffs: number; resent: number; unsent: number; redials: number; lastRejection?: string };
  onHello: (peer: PeerId, meta?: unknown) => void;
  /**
   * A relay opened again after its socket closed or heard nothing (see `stats.redials`). A closed one is redialled on
   * its own after `RECONNECT_MS` (1 s), doubling to `MAX_RECONNECT_MS` (30 s), and back to 1 s only once a socket has
   * stayed open 30 s, so a relay that drops each connection as it opens fires this twice a minute at most (a `Room`
   * says hello through every relay each time); at once, starting over, on going online, a new connection type or the
   * page showing again (not on a `navigator.connection` 'change' of the same type: a new speed estimate); going online
   * redials every relay. Also when a relay `setRelays` added opens.
   */
  onReconnect: (url: string) => void;
  /** Moves to the relays at `urls`, keeping the sockets of those listed already: one no longer listed is closed (what
   * waited for it is lost), one newly listed is dialled and subscribed as the others. */
  setRelays(urls: string[]): void;
  hello(meta?: unknown): Promise<void>;
  /** `lossy`: through one relay, the relays taking turns (one that holds us for its rate, has banned us or isn't open
   * is skipped, and so is one slowed behind what waits for it and, while any relay that answers is left, one that has
   * answered nothing for 5 s since it was sent an event), not each. */
  send(to: PeerId, body: unknown, options?: { lossy?: boolean }): Promise<void>;
  /** Milliseconds until the relay the next lossy event goes to could take it (0: it can now). Each relay is paced on
   * its own, so lossy sends that wait this long move at about the relays' rates together. */
  wait(): number;
  onMessage(fn: (from: PeerId, body: any) => void): void;
  close(): void;
}
